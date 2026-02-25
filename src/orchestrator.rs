use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::process::Command as TokioCommand;

use crate::agent::{
    self, AgentContext, AgentRole, AgentStatus, FEEDBACK_MAX_LEN, ProcessRegistry,
    build_feedback_history, truncate_feedback,
};
use crate::config::Config;
use crate::db;
use crate::nit::truncate_with_ellipsis;
use crate::scheduler;
use crate::task::{Phase, Task, TaskDef, unix_now};

/// Install a background task that listens for SIGINT and SIGTERM,
/// kills all registered process groups, then exits.
pub(crate) fn spawn_signal_handler(registry: ProcessRegistry) {
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n[ralph] interrupted, cleaning up agents...");
            }
            _ = sigterm.recv() => {
                eprintln!("\n[ralph] terminated, cleaning up agents...");
            }
        }
        registry.request_shutdown();
        registry.kill_all().await;
    });
}

/// Determine the post-failure phase. Returns Failed if the
/// attempt budget is exhausted, otherwise Pending for retry.
fn reset_or_fail_phase(attempts: u32, config: &Config) -> Phase {
    if attempts >= config.max_attempts {
        Phase::Failed
    } else {
        Phase::Pending
    }
}


/// Format guidance entries as a bullet list for prompt injection.
fn build_guidance(entries: &[String]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    entries
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Record full agent response text as feedback so the
/// implementer can see what went wrong on the next attempt.
/// When `stderr` is non-empty, appends it under a heading
/// so the next attempt can see CLI diagnostics.
fn push_feedback(
    conn: &Connection,
    task_id: &str,
    phase_label: &str,
    attempts: u32,
    full_text: &str,
    stderr: &[String],
) -> Result<()> {
    let prefix = format!("[{phase_label} · attempt {attempts}]");
    let mut body = truncate_feedback(full_text, FEEDBACK_MAX_LEN);
    if !stderr.is_empty() {
        // Budget: leave room for the stderr section within FEEDBACK_MAX_LEN.
        let stderr_budget = FEEDBACK_MAX_LEN
            .saturating_sub(body.len())
            .saturating_sub(32);
        if stderr_budget > 0 {
            let stderr_text = stderr.join("\n");
            let truncated = truncate_feedback(&stderr_text, stderr_budget);
            body.push_str("\n\n[stderr]\n");
            body.push_str(&truncated);
        }
    }
    db::push_feedback(conn, task_id, &format!("{prefix}\n{body}"))
}

/// Record a nit to the database.
fn record_nit(
    conn: &Connection,
    source_task: &str,
    source_role: &str,
    attempt: u32,
    suggestions: &str,
) -> Result<()> {
    let id = db::next_nit_id(conn)?;
    let summary = crate::nit::summarize(suggestions);
    let nit = crate::nit::Nit {
        id,
        source_task: source_task.to_string(),
        source_role: source_role.to_string(),
        attempt,
        content: suggestions.to_string(),
        summary,
        status: crate::nit::NitStatus::Open,
        promoted_to: None,
        created_at: unix_now(),
    };
    db::insert_nit(conn, &nit)
}

/// Extract proposed tasks from an agent result, assign IDs,
/// deduplicate, and insert into the database.
///
/// Returns the number of tasks actually added.
fn ingest_new_tasks(
    result: &agent::AgentResult,
    conn: &Connection,
    source_label: &str,
) -> Result<usize> {
    let proposals = result.parse_new_tasks();
    if proposals.is_empty() {
        return Ok(0);
    }
    Ok(materialize_proposed_tasks(&proposals, conn, source_label)?
        .into_iter()
        .filter(|id| id.is_some())
        .count())
}

/// Parse a numbered-list failure reason into tasks and insert them.
///
/// Used as a fallback when the final reviewer returns a failure
/// reason without structured `NEW_TASKS:` output.
fn ingest_from_failure_reason(
    reason: &str,
    conn: &Connection,
    source_label: &str,
) -> Result<usize> {
    let proposals = agent::tasks_from_numbered_list(reason);
    if proposals.is_empty() {
        return Ok(0);
    }
    Ok(materialize_proposed_tasks(&proposals, conn, source_label)?
        .into_iter()
        .filter(|id| id.is_some())
        .count())
}

/// Extract the first sentence (or first ~120 chars) of a failure reason
/// to use as a task title.
fn truncate_for_title(reason: &str) -> String {
    // Collapse to a single line.
    let oneline: String = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    // Try to cut at the first sentence boundary.
    if let Some(pos) = oneline.find(". ")
        && pos < 120
    {
        return oneline[..=pos].to_string();
    }
    truncate_with_ellipsis(&oneline, 120)
}

/// Shared logic: validate, deduplicate, assign IDs, and insert proposed tasks.
///
/// Returns a `Vec` positionally aligned with `proposals`: `Some(id)` for each proposal
/// that was materialized as a new task, `None` for proposals skipped due to deduplication.
fn materialize_proposed_tasks(
    proposals: &[agent::ProposedTask],
    conn: &Connection,
    source_label: &str,
) -> Result<Vec<Option<String>>> {
    let existing = db::list_all_tasks(conn)?;
    let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
    let existing_titles: HashSet<&str> = existing.iter().map(|t| t.title.as_str()).collect();
    let max_priority = existing.iter().map(|t| t.priority).max().unwrap_or(0);

    let gen_id_str = db::next_generated_id(conn)?;
    let mut gen_counter: u32 = gen_id_str
        .strip_prefix("GEN-")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        - 1;

    let mut new_tasks = Vec::new();
    let mut new_ids: HashSet<String> = HashSet::new();
    let mut result_ids: Vec<Option<String>> = Vec::with_capacity(proposals.len());

    for p in proposals {
        // Deduplicate by title — skip if an identical task already exists.
        if existing_titles.contains(p.title.as_str()) {
            result_ids.push(None);
            continue;
        }

        let id = match &p.id {
            Some(id)
                if !id.trim().is_empty()
                    && !existing_ids.contains(id.as_str())
                    && !new_ids.contains(id) =>
            {
                id.clone()
            }
            _ => {
                gen_counter += 1;
                format!("GEN-{gen_counter}")
            }
        };
        new_ids.insert(id.clone());
        result_ids.push(Some(id.clone()));
        new_tasks.push(Task::from_def(&TaskDef {
            id,
            title: p.title.clone(),
            description: p.description.clone(),
            priority: p.priority.unwrap_or(max_priority + 1),
            blocked_by: p.blocked_by.clone(),
        }));
    }

    if !new_tasks.is_empty() {
        db::insert_tasks(conn, &new_tasks)?;
        for t in &new_tasks {
            eprintln!(
                "[ralph] {source_label} → new task: [{}] {} (pri={})",
                t.id, t.title, t.priority
            );
        }
    }
    Ok(result_ids)
}

/// Main orchestration loop. Iterates until convergence
/// (all tasks done + reviewer approves), stagnation
/// (max attempts exceeded), or iteration cap.
pub async fn run_loop(conn: &Connection, max_iterations: usize, config: &Config) -> Result<()> {
    let registry = ProcessRegistry::new(config.kill_grace_secs);
    spawn_signal_handler(registry.clone());
    isolate_dirty_tree().await;
    let mut cumulative_cost: f64 = 0.0;
    let mut triage_rounds: u32 = 0;

    for iteration in 1..=max_iterations {
        if registry.is_shutdown() {
            eprintln!("[ralph] shutdown requested.");
            return Ok(());
        }

        eprintln!("\n[ralph] === iteration {iteration} === (${cumulative_cost:.4} spent)");

        registry.audit_and_kill_orphans().await;

        // Fast-path convergence check: if no non-terminal tasks remain,
        // skip the full task deserialisation and go straight to final review.
        if db::count_non_terminal(conn)? == 0 {
            eprintln!("[ralph] all tasks done, final review...");
            let final_diff = agent::jj_diff_git().await.unwrap_or_default();
            let final_summary = agent::jj_changed_files()
                .await
                .unwrap_or_default()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let review = agent::invoke_agent(
                AgentRole::Reviewer,
                &AgentContext::Review {
                    task_id: "final".to_string(),
                    task_title: "Final review".to_string(),
                    task_description: "Review the full project for \
                         correctness."
                        .to_string(),
                    diff_summary: final_summary,
                    diff: final_diff,
                },
                config,
                &registry,
                0,
            )
            .await?;

            cumulative_cost += review.cost_usd.unwrap_or(0.0);
            if config.max_cost_usd.is_some_and(|max| cumulative_cost > max) {
                eprintln!(
                    "[ralph] cost budget exceeded (${cumulative_cost:.4} > ${:.4}), stopping.",
                    config.max_cost_usd.unwrap()
                );
                return Ok(());
            }

            if registry.is_shutdown() {
                eprintln!("[ralph] shutdown requested.");
                return Ok(());
            }

            match review.status {
                AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                    if let AgentStatus::ApprovedWithNits { suggestions } = &review.status {
                        eprintln!("[ralph] final review nits: {suggestions}");
                        if let Err(e) = record_nit(conn, "final", "final_review", 1, suggestions) {
                            eprintln!("[ralph] failed to record nit: {e}");
                        }
                    }

                    if config.auto_triage && triage_rounds < config.max_triage_rounds {
                        let promoted =
                            triage_open_nits(conn, config, &registry, &mut cumulative_cost)
                                .await?;
                        if promoted > 0 {
                            triage_rounds += 1;
                            eprintln!(
                                "[ralph] triage round {triage_rounds}: \
                                 {promoted} nit(s) promoted, continuing..."
                            );
                            continue;
                        }
                    }

                    eprintln!(
                        "[ralph] final review passed — \
                         converged!"
                    );
                    return Ok(());
                }
                AgentStatus::Failure { ref reason } | AgentStatus::NeedsRetry { ref reason } => {
                    eprintln!("[ralph] reviewer found issues: {reason}");

                    // First, try structured NEW_TASKS from the reviewer output.
                    let mut added = ingest_new_tasks(&review, conn, "final review")?;

                    // For Tiers 2 & 3, use the full reviewer output rather than
                    // the reason string, which may be a generic parse-failure
                    // message (e.g. "no STATUS line in agent output").
                    let findings = if review.text.trim().is_empty() {
                        reason.as_str()
                    } else {
                        review.text.as_str()
                    };

                    // Tier 2: parse numbered items from reviewer findings.
                    if added == 0 {
                        added = ingest_from_failure_reason(findings, conn, "final review")?;
                    }

                    // Tier 3: wrap prose as a single task.
                    if added == 0 && !findings.trim().is_empty() {
                        let desc = truncate_feedback(findings, FEEDBACK_MAX_LEN);
                        let fallback = vec![agent::ProposedTask {
                            id: None,
                            title: truncate_for_title(findings),
                            description: desc,
                            priority: None,
                            blocked_by: vec![],
                        }];
                        added = materialize_proposed_tasks(
                            &fallback,
                            conn,
                            "final review (synthesized)",
                        )?
                        .into_iter()
                        .filter(|id| id.is_some())
                        .count();
                    }

                    if added > 0 {
                        eprintln!("[ralph] {added} new task(s) queued from final review");
                        continue; // Re-enter the main loop
                    }

                    eprintln!("[ralph] no tasks extracted from review, stopping.");
                    return Ok(());
                }
            }
        }

        // CLI commands (skip/fail/reset) mutate the DB directly,
        // so a fresh load picks up any changes.
        let tasks = db::list_active_tasks(conn)?;
        db::validate_deps(conn)?;

        // Reset any tasks stuck in transient phases from a
        // prior interrupted run.
        let made_progress = recover_interrupted(&tasks, conn)?;
        if registry.is_shutdown() {
            eprintln!("[ralph] shutdown requested.");
            return Ok(());
        }
        if made_progress {
            // Re-evaluate from the top — deps may have
            // unblocked.
            continue;
        }

        // Check stagnation
        let stagnant: Vec<&str> = tasks
            .iter()
            .filter(|t| t.phase == Phase::Failed)
            .map(|t| t.id.as_str())
            .collect();

        if !stagnant.is_empty() {
            eprintln!(
                "[ralph] stagnant tasks (max attempts): {}",
                stagnant.join(", ")
            );
        }

        // Find ready tasks (Pending phase only)
        let ready = scheduler::ready_tasks(&tasks, config);
        if ready.is_empty() {
            if stagnant.is_empty() {
                eprintln!(
                    "[ralph] no ready tasks and no \
                     stagnation — possible dependency \
                     deadlock"
                );
            }
            eprintln!("[ralph] nothing to do, stopping.");
            break;
        }

        eprintln!("[ralph] {} task(s) ready", ready.len());

        // Run each task through its full lifecycle serially.
        // Each task completes implement → test → review before
        // the next one starts, so later tasks build on verified work.
        for task in &ready {
            run_task(
                task,
                conn,
                config,
                &registry,
                &mut cumulative_cost,
            )
            .await?;

            if config.max_cost_usd.is_some_and(|max| cumulative_cost > max) {
                eprintln!(
                    "[ralph] cost budget exceeded (${cumulative_cost:.4} > ${:.4}), stopping.",
                    config.max_cost_usd.unwrap()
                );
                return Ok(());
            }

            if registry.is_shutdown() {
                eprintln!("[ralph] shutdown requested.");
                return Ok(());
            }

            // Checkpoint after each task's full lifecycle.
            let detail = checkpoint_description(&[task.id.clone()], conn);
            if let Err(e) = jj_checkpoint(&detail).await {
                eprintln!("[ralph] jj commit skipped: {e}");
            }
        }
    }

    eprintln!("[ralph] loop finished.");
    Ok(())
}

// ── Nit triage ────────────────────────────────────────────

/// A single triage decision emitted by the triager agent.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct TriageDecision {
    pub nit_id: String,
    pub decision: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Parse triage decisions from agent output text.
/// Each line that parses as a `TriageDecision` JSON object is collected.
pub(crate) fn parse_triage_decisions(text: &str) -> Vec<TriageDecision> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('{') {
                serde_json::from_str::<TriageDecision>(trimmed).ok()
            } else {
                None
            }
        })
        .collect()
}

/// Invoke the triager agent on open nits, apply decisions,
/// and return the number of promoted nits.
pub async fn triage_open_nits(
    conn: &Connection,
    config: &Config,
    registry: &ProcessRegistry,
    cumulative_cost: &mut f64,
) -> Result<usize> {
    let nits = db::list_nits(conn, false)?;

    if nits.is_empty() {
        return Ok(0);
    }

    eprintln!("[ralph] triaging {} open nit(s)...", nits.len());

    // Build context for the triager
    let nits_json = nits
        .iter()
        .map(|n| serde_json::to_string(n).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");

    let tasks = db::list_active_tasks(conn)?;
    let tasks_summary = tasks
        .iter()
        .map(|t| format!("[{}] {} ({:?})", t.id, t.title, t.phase))
        .collect::<Vec<_>>()
        .join("\n");

    let ctx = AgentContext::triage(nits_json, tasks_summary);
    let result = agent::invoke_agent(AgentRole::Triager, &ctx, config, registry, 0).await?;

    *cumulative_cost += result.cost_usd.unwrap_or(0.0);

    if registry.is_shutdown() {
        return Ok(0);
    }

    let decisions = parse_triage_decisions(&result.text);
    if decisions.is_empty() {
        eprintln!("[ralph] triager returned no decisions");
        return Ok(0);
    }

    let mut promoted_count = 0usize;
    let mut proposals = Vec::new();
    let mut promoted_nit_ids: Vec<String> = Vec::new();

    for d in &decisions {
        let nit_entry = nits.iter().find(|n| n.id == d.nit_id);
        let Some(nit_entry) = nit_entry else {
            eprintln!("[ralph] triager referenced unknown nit '{}'", d.nit_id);
            continue;
        };
        if nit_entry.status != crate::nit::NitStatus::Open {
            continue;
        }

        match d.decision.as_str() {
            "promote" => {
                let title = d
                    .title
                    .clone()
                    .unwrap_or_else(|| crate::nit::summarize(&nit_entry.content));
                let description = d
                    .description
                    .clone()
                    .unwrap_or_else(|| nit_entry.content.clone());
                proposals.push(agent::ProposedTask {
                    id: None,
                    title,
                    description,
                    priority: None,
                    blocked_by: vec![],
                });
                promoted_nit_ids.push(d.nit_id.clone());
                promoted_count += 1;
                eprintln!("[ralph] triager: promote {}", d.nit_id);
            }
            "dismiss" => {
                db::update_nit_status(
                    conn,
                    &d.nit_id,
                    crate::nit::NitStatus::Dismissed,
                    None,
                )?;
                let reason = d.reason.as_deref().unwrap_or("triager dismissed");
                eprintln!("[ralph] triager: dismiss {} — {reason}", d.nit_id);
            }
            other => {
                eprintln!(
                    "[ralph] triager: unknown decision '{}' for {}",
                    other, d.nit_id
                );
            }
        }
    }

    // Materialize promoted nits as tasks, then update each nit's promoted_to field.
    if !proposals.is_empty() {
        match materialize_proposed_tasks(&proposals, conn, "triager") {
            Ok(task_ids) => {
                let added = task_ids.iter().filter(|id| id.is_some()).count();
                eprintln!("[ralph] triager created {added} task(s)");
                for (nit_id, task_id_opt) in promoted_nit_ids.iter().zip(task_ids.iter()) {
                    db::update_nit_status(
                        conn,
                        nit_id,
                        crate::nit::NitStatus::Promoted,
                        task_id_opt.as_deref(),
                    )?;
                }
            }
            Err(e) => {
                eprintln!("[ralph] failed to materialize triager tasks: {e}");
                // Still mark nits as promoted even if materialization failed.
                for nit_id in &promoted_nit_ids {
                    db::update_nit_status(
                        conn,
                        nit_id,
                        crate::nit::NitStatus::Promoted,
                        None,
                    )?;
                }
            }
        }
    }

    Ok(promoted_count)
}

/// Reset any tasks stuck in transient phases from a prior
/// interrupted run. Returns true if any task was reset.
fn recover_interrupted(tasks: &[Task], conn: &Connection) -> Result<bool> {
    let mut progressed = false;

    for t in tasks {
        let reset = match t.phase {
            Phase::Implementing | Phase::Testing | Phase::Reviewing => true,
            _ => false,
        };
        if reset {
            eprintln!(
                "[ralph] {} stuck in {:?}, resetting to Pending",
                t.id, t.phase
            );
            db::update_phase(conn, &t.id, Phase::Pending, unix_now())?;
            progressed = true;
        }
    }

    Ok(progressed)
}

/// Build a checkpoint commit message summarizing what the
/// group accomplished, e.g. "ralph: BUILD-1 (testing), UI-2 (done)".
fn checkpoint_description(group_ids: &[String], conn: &Connection) -> String {
    let parts: Vec<String> = group_ids
        .iter()
        .map(|id| {
            let phase_label = db::get_task(conn, id)
                .ok()
                .flatten()
                .map(|t| match t.phase {
                    Phase::Pending => "pending",
                    Phase::Implementing => "implementing",
                    Phase::Testing => "testing",
                    Phase::Reviewing => "reviewing",
                    Phase::Done => "done",
                    Phase::Failed => "failed",
                    Phase::Skipped => "skipped",
                })
                .unwrap_or("unknown");
            format!("{id} ({phase_label})")
        })
        .collect();
    format!("ralph: {}", parts.join(", "))
}

/// Seal the current working-copy change and start a fresh
/// one, but only if there are actual changes to commit.
async fn jj_checkpoint(description: &str) -> Result<()> {
    let files = agent::jj_changed_files().await?;
    if files.is_empty() {
        return Ok(());
    }
    TokioCommand::new("jj")
        .args(["commit", "-m", description])
        .status()
        .await
        .context("jj commit")?;
    Ok(())
}

/// If the working copy has pre-existing changes, isolate
/// them by creating a new empty change on top. This keeps
/// ralph's work separate without disturbing the user's
/// in-progress modifications.
async fn isolate_dirty_tree() {
    if let Ok(files) = agent::jj_changed_files().await
        && !files.is_empty()
    {
        eprintln!(
            "[ralph] {} pre-existing dirty file(s) — \
             isolating with `jj new`",
            files.len()
        );
        let _ = TokioCommand::new("jj").arg("new").status().await;
    }
}

/// Run a single task through its full lifecycle:
/// implement → test → review. Returns early if any phase
/// fails (task is reset to Pending or marked Failed).
async fn run_task(
    t: &Task,
    conn: &Connection,
    config: &Config,
    registry: &ProcessRegistry,
    cumulative_cost: &mut f64,
) -> Result<()> {
    // ── Implement ──────────────────────────────────────────
    let pre_files = agent::jj_changed_files().await.unwrap_or_default();

    db::update_phase(conn, &t.id, Phase::Implementing, unix_now())?;

    let guidance = build_guidance(&t.guidance);
    let guidance = if guidance.is_empty() {
        None
    } else {
        Some(guidance)
    };
    let feedback_history = build_feedback_history(&t.feedback, FEEDBACK_MAX_LEN);
    let feedback_history = if feedback_history.is_empty() {
        None
    } else {
        Some(feedback_history)
    };
    let attempt = t.attempts + 1;
    let ctx = AgentContext::implement(
        &t.id,
        &t.title,
        &t.description,
        guidance.as_deref(),
        feedback_history.as_deref(),
    );
    let result = agent::invoke_agent(
        AgentRole::Implementer,
        &ctx,
        config,
        registry,
        attempt,
    )
    .await;

    let new_attempts = t.attempts + 1;
    db::update_attempts(conn, &t.id, new_attempts)?;

    let impl_ok = match result {
        Ok(r) => {
            *cumulative_cost += r.cost_usd.unwrap_or(0.0);
            if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status
                && let Err(e) = record_nit(conn, &t.id, "implementer", new_attempts, suggestions)
            {
                eprintln!("[ralph] failed to record nit: {e}");
            }
            if let Err(e) = ingest_new_tasks(&r, conn, &format!("implementer/{}", t.id)) {
                eprintln!("[ralph] failed to ingest new tasks from implementer: {e}");
            }
            match &r.status {
                AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                    db::update_last_error(conn, &t.id, None)?;
                    true
                }
                AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                    push_feedback(conn, &t.id, "Implementer", new_attempts, &r.text, &r.stderr_lines)?;
                    let new_phase = reset_or_fail_phase(new_attempts, config);
                    db::update_phase(conn, &t.id, new_phase, unix_now())?;
                    db::update_last_error(conn, &t.id, Some(reason))?;
                    eprintln!("[ralph] {} implement failed: {reason}", t.id);
                    false
                }
            }
        }
        Err(e) => {
            let new_phase = reset_or_fail_phase(new_attempts, config);
            db::update_phase(conn, &t.id, new_phase, unix_now())?;
            db::update_last_error(conn, &t.id, Some(&e.to_string()))?;
            eprintln!("[ralph] agent error for {}: {e}", t.id);
            false
        }
    };

    // Attribute files via pre/post snapshot (even on failure,
    // so diagnostics can see what the agent changed).
    let post_files = agent::jj_changed_files().await.unwrap_or_default();
    let new_files: Vec<PathBuf> = post_files
        .into_iter()
        .filter(|f| !pre_files.contains(f))
        .collect();
    let mut all_files = t.files_changed.clone();
    all_files.extend(new_files);
    all_files.sort();
    all_files.dedup();
    db::update_files_changed(conn, &t.id, &all_files)?;

    if !impl_ok || registry.is_shutdown() {
        return Ok(());
    }

    // ── Test ───────────────────────────────────────────────
    db::update_phase(conn, &t.id, Phase::Testing, unix_now())?;

    let files = all_files;
    let ctx = AgentContext::test(&t.id, &t.title, &t.description, files.clone());
    let test_result = agent::invoke_agent(AgentRole::Tester, &ctx, config, registry, new_attempts).await;

    let test_ok = match test_result {
        Ok(r) => {
            *cumulative_cost += r.cost_usd.unwrap_or(0.0);
            if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status
                && let Err(e) = record_nit(conn, &t.id, "tester", new_attempts, suggestions)
            {
                eprintln!("[ralph] failed to record nit: {e}");
            }
            if let Err(e) = ingest_new_tasks(&r, conn, &format!("tester/{}", t.id)) {
                eprintln!("[ralph] failed to ingest new tasks from tester: {e}");
            }
            match r.status {
                AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                    db::update_last_error(conn, &t.id, None)?;
                    true
                }
                AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                    push_feedback(conn, &t.id, "Tester", new_attempts, &r.text, &r.stderr_lines)?;
                    let new_phase = reset_or_fail_phase(new_attempts, config);
                    db::update_phase(conn, &t.id, new_phase, unix_now())?;
                    db::update_last_error(conn, &t.id, Some(&reason))?;
                    eprintln!("[ralph] {} tests failed: {reason}", t.id);
                    false
                }
            }
        }
        Err(e) => {
            push_feedback(conn, &t.id, "Tester", new_attempts, &e.to_string(), &[])?;
            let new_phase = reset_or_fail_phase(new_attempts, config);
            db::update_phase(conn, &t.id, new_phase, unix_now())?;
            db::update_last_error(conn, &t.id, Some(&e.to_string()))?;
            eprintln!("[ralph] tester error for {}: {e}", t.id);
            false
        }
    };

    if !test_ok || registry.is_shutdown() {
        return Ok(());
    }

    // ── Review ─────────────────────────────────────────────
    db::update_phase(conn, &t.id, Phase::Reviewing, unix_now())?;

    let diff = agent::jj_diff_git().await.unwrap_or_default();
    let diff_summary = files
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let ctx = AgentContext::review(&t.id, &t.title, &t.description, diff_summary, diff);
    let review_result = agent::invoke_agent(AgentRole::Reviewer, &ctx, config, registry, 0).await;

    match review_result {
        Ok(r) => {
            *cumulative_cost += r.cost_usd.unwrap_or(0.0);
            if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status
                && let Err(e) = record_nit(conn, &t.id, "reviewer", new_attempts, suggestions)
            {
                eprintln!("[ralph] failed to record nit: {e}");
            }
            if let Err(e) = ingest_new_tasks(&r, conn, &format!("reviewer/{}", t.id)) {
                eprintln!("[ralph] failed to ingest new tasks from reviewer: {e}");
            }
            match r.status {
                AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                    db::update_phase(conn, &t.id, Phase::Done, unix_now())?;
                    db::update_last_error(conn, &t.id, None)?;
                    eprintln!("[ralph] {} — done!", t.id);
                }
                AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                    push_feedback(conn, &t.id, "Reviewer", new_attempts, &r.text, &r.stderr_lines)?;
                    let new_phase = reset_or_fail_phase(new_attempts, config);
                    db::update_phase(conn, &t.id, new_phase, unix_now())?;
                    db::update_last_error(conn, &t.id, Some(&reason))?;
                    eprintln!("[ralph] {} review issues: {reason}", t.id);
                }
            }
        }
        Err(e) => {
            push_feedback(conn, &t.id, "Reviewer", new_attempts, &e.to_string(), &[])?;
            let new_phase = reset_or_fail_phase(new_attempts, config);
            db::update_phase(conn, &t.id, new_phase, unix_now())?;
            db::update_last_error(conn, &t.id, Some(&e.to_string()))?;
            eprintln!("[ralph] reviewer error for {}: {e}", t.id);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_triage_decisions_valid() {
        let text = r#"Looking at the nits...
{"nit_id":"NIT-1","decision":"promote","title":"Fix naming","description":"Rename foo to bar"}
{"nit_id":"NIT-2","decision":"dismiss","reason":"Stylistic preference"}
STATUS: SUCCESS"#;
        let decisions = parse_triage_decisions(text);
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].nit_id, "NIT-1");
        assert_eq!(decisions[0].decision, "promote");
        assert_eq!(decisions[0].title.as_deref(), Some("Fix naming"));
        assert_eq!(decisions[1].nit_id, "NIT-2");
        assert_eq!(decisions[1].decision, "dismiss");
        assert_eq!(decisions[1].reason.as_deref(), Some("Stylistic preference"));
    }

    #[test]
    fn parse_triage_decisions_empty() {
        let text = "No JSON here.\nSTATUS: SUCCESS\n";
        let decisions = parse_triage_decisions(text);
        assert!(decisions.is_empty());
    }

    #[test]
    fn truncate_for_title_short_reason() {
        let reason = "Fix the lint error";
        assert_eq!(truncate_for_title(reason), "Fix the lint error");
    }

    #[test]
    fn truncate_for_title_first_sentence() {
        let reason = "`cargo clippy` fails with 2 pedantic lint errors in `src/parser.rs`. \
                       All tests pass but the project's clippy policy is violated.";
        let title = truncate_for_title(reason);
        assert_eq!(
            title,
            "`cargo clippy` fails with 2 pedantic lint errors in `src/parser.rs`."
        );
    }

    #[test]
    fn truncate_for_title_long_no_sentence() {
        let reason = "a]".repeat(100); // 200 chars, no sentence boundary
        let title = truncate_for_title(&reason);
        assert!(title.len() <= 124); // 120 + ellipsis (up to 4 bytes)
        assert!(title.ends_with('…'));
    }

    #[test]
    fn truncate_for_title_collapses_whitespace() {
        let reason = "line one\n  line two\n  line three. rest";
        let title = truncate_for_title(reason);
        assert_eq!(title, "line one line two line three.");
    }

    #[test]
    fn parse_triage_decisions_mixed_lines() {
        let text = r#"Here's my analysis:
Not JSON
{"nit_id":"NIT-3","decision":"promote","title":"Add tests"}
Some commentary
{"nit_id":"NIT-4","decision":"dismiss"}
STATUS: SUCCESS"#;
        let decisions = parse_triage_decisions(text);
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].nit_id, "NIT-3");
        assert_eq!(decisions[1].nit_id, "NIT-4");
        assert!(decisions[1].reason.is_none());
    }

    /// When the reviewer omits a STATUS line, `reason` is the generic
    /// "no STATUS line in agent output" — useless for task creation.
    /// The fix uses `review.text` (the full output) for Tiers 2 & 3.
    /// This test verifies that `tasks_from_numbered_list` (the Tier 2
    /// extractor) can recover tasks from realistic reviewer output that
    /// lacks a STATUS line.
    #[test]
    fn tier2_extracts_tasks_from_full_review_text() {
        let review_text = "\
I found several issues:\n\
1. The `parse_config` function doesn't validate the `timeout` field\n\
2. Missing unit test for the error branch in `handle_request`\n\
3. `unwrap()` on line 47 of lib.rs should propagate with `?`\n";

        let tasks = agent::tasks_from_numbered_list(review_text);
        assert_eq!(tasks.len(), 3);
        assert!(tasks[0].title.contains("parse_config"));
        assert!(tasks[1].title.contains("unit test"));
        assert!(tasks[2].title.contains("unwrap"));

        // The generic reason would yield nothing:
        let generic = "no STATUS line in agent output";
        let from_generic = agent::tasks_from_numbered_list(generic);
        assert!(from_generic.is_empty());
    }
}
