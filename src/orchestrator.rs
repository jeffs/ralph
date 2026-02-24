use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
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

const WS_DIR: &str = ".ralph";

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
    cleanup_stale_workspaces().await;
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
                None,
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

        // Resume interrupted in-flight tasks before
        // scheduling new work.
        let made_progress = resume_inflight(
            &tasks,
            conn,
            config,
            &registry,
            &mut cumulative_cost,
        )
        .await?;
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

        // Partition into parallelizable groups
        let groups = scheduler::partition_independent(&ready);

        for group in groups {
            let use_workspaces = group.len() > 1;

            if use_workspaces {
                run_group_with_workspaces(
                    &group,
                    conn,
                    config,
                    &registry,
                    &mut cumulative_cost,
                )
                .await?;
            } else {
                run_group_singleton(
                    &group,
                    conn,
                    config,
                    &registry,
                    &mut cumulative_cost,
                )
                .await?;
            }

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

            // Reload tasks from db to see updated phases,
            // then advance any tasks now at Testing/Reviewing.
            let fresh_tasks = db::list_active_tasks(conn)?;
            resume_inflight(
                &fresh_tasks,
                conn,
                config,
                &registry,
                &mut cumulative_cost,
            )
            .await?;

            if registry.is_shutdown() {
                eprintln!("[ralph] shutdown requested.");
                return Ok(());
            }

            // Checkpoint: seal the current working-copy change
            // and start a fresh one for the next group.
            let group_ids: Vec<String> = group.iter().map(|t| t.id.clone()).collect();
            let detail = checkpoint_description(&group_ids, conn);
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
    let result = agent::invoke_agent(AgentRole::Triager, &ctx, config, None, registry, 0).await?;

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

/// Group tasks by disjoint file sets for parallel execution.
/// Tasks with overlapping files_changed are placed in the same group
/// to avoid parallel work on the same files.
fn group_by_disjoint_files<'a>(tasks: &[&'a Task]) -> Vec<Vec<&'a Task>> {
    let mut groups: Vec<(HashSet<PathBuf>, Vec<&'a Task>)> = Vec::new();

    for &task in tasks {
        let files: HashSet<PathBuf> = task.files_changed.iter().cloned().collect();

        let mut merged = false;
        for (group_files, group_tasks) in &mut groups {
            if files.is_disjoint(group_files) {
                group_files.extend(files.iter().cloned());
                group_tasks.push(task);
                merged = true;
                break;
            }
        }
        if !merged {
            groups.push((files, vec![task]));
        }
    }

    groups.into_iter().map(|(_, tasks)| tasks).collect()
}

/// Advance all tasks stuck at Testing or Reviewing.
/// Returns true if any task moved forward.
///
/// Testing tasks with disjoint file sets run in parallel.
/// Reviewing tasks always run in parallel (read-only).
async fn resume_inflight(
    tasks: &[Task],
    conn: &Connection,
    config: &Config,
    registry: &ProcessRegistry,
    cumulative_cost: &mut f64,
) -> Result<bool> {
    let mut progressed = false;

    // Implementing phase → task was mid-implementation when Ralph
    // restarted. Reset to Pending so the scheduler picks it up again.
    let implementing: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.phase == Phase::Implementing)
        .collect();
    for t in implementing {
        eprintln!("[ralph] {} stuck in Implementing, resetting to Pending", t.id);
        db::update_phase(conn, &t.id, Phase::Pending, unix_now())?;
        progressed = true;
    }

    // Testing phase → run tester (parallel for disjoint file sets)
    let testing: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.phase == Phase::Testing)
        .collect();

    let test_groups = group_by_disjoint_files(&testing);
    for group in test_groups {
        let mut handles = Vec::new();
        for t in &group {
            let files = t.files_changed.clone();
            let ctx = AgentContext::test(&t.id, &t.title, &t.description, files);
            let cfg = config.clone();
            let reg = registry.clone();
            let id_owned = t.id.clone();
            handles.push(tokio::spawn(async move {
                let result =
                    agent::invoke_agent(AgentRole::Tester, &ctx, &cfg, None, &reg, 0).await;
                (id_owned, result)
            }));
        }

        for handle in handles {
            let (id, result) = handle.await?;
            eprintln!("[ralph] resuming test for {id}...");
            let attempts = tasks.iter().find(|t| t.id == id).map_or(0, |t| t.attempts);
            match result {
                Ok(r) => {
                    *cumulative_cost += r.cost_usd.unwrap_or(0.0);
                    if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status
                        && let Err(e) = record_nit(conn, &id, "tester", attempts, suggestions)
                    {
                        eprintln!("[ralph] failed to record nit: {e}");
                    }
                    // Ingest any new tasks proposed by the tester.
                    if let Err(e) = ingest_new_tasks(&r, conn, &format!("tester/{id}")) {
                        eprintln!("[ralph] failed to ingest new tasks from tester: {e}");
                    }
                    match r.status {
                        AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                            db::update_phase(conn, &id, Phase::Reviewing, unix_now())?;
                            db::update_last_error(conn, &id, None)?;
                            progressed = true;
                        }
                        AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                            push_feedback(conn, &id, "Tester", attempts, &r.text, &r.stderr_lines)?;
                            let new_phase = reset_or_fail_phase(attempts, config);
                            db::update_phase(conn, &id, new_phase, unix_now())?;
                            db::update_last_error(conn, &id, Some(&reason))?;
                            progressed = true;
                            eprintln!("[ralph] {id} tests failed: {reason}");
                        }
                    }
                }
                Err(e) => {
                    push_feedback(conn, &id, "Tester", attempts, &e.to_string(), &[])?;
                    let new_phase = reset_or_fail_phase(attempts, config);
                    db::update_phase(conn, &id, new_phase, unix_now())?;
                    db::update_last_error(conn, &id, Some(&e.to_string()))?;
                    progressed = true;
                    eprintln!("[ralph] tester error for {id}: {e}");
                }
            }
            if config
                .max_cost_usd
                .is_some_and(|max| *cumulative_cost > max)
            {
                eprintln!(
                    "[ralph] cost budget exceeded (${:.4} > ${:.4}), stopping.",
                    *cumulative_cost,
                    config.max_cost_usd.unwrap()
                );
                return Ok(progressed);
            }
        }
    }

    // Reviewing phase → run reviewer (always parallel — read-only)
    let reviewing: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.phase == Phase::Reviewing)
        .collect();

    if !reviewing.is_empty() {
        // Compute diff once for all reviewers.
        let diff = agent::jj_diff_git().await.unwrap_or_default();

        let mut handles = Vec::new();
        for t in &reviewing {
            let diff_summary = t
                .files_changed
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let ctx = AgentContext::review(
                &t.id,
                &t.title,
                &t.description,
                diff_summary,
                diff.clone(),
            );
            let cfg = config.clone();
            let reg = registry.clone();
            let id_owned = t.id.clone();
            handles.push(tokio::spawn(async move {
                let result =
                    agent::invoke_agent(AgentRole::Reviewer, &ctx, &cfg, None, &reg, 0).await;
                (id_owned, result)
            }));
        }

        for handle in handles {
            let (id, result) = handle.await?;
            eprintln!("[ralph] resuming review for {id}...");
            let attempts = tasks.iter().find(|t| t.id == id).map_or(0, |t| t.attempts);
            match result {
                Ok(r) => {
                    *cumulative_cost += r.cost_usd.unwrap_or(0.0);
                    if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status
                        && let Err(e) = record_nit(conn, &id, "reviewer", attempts, suggestions)
                    {
                        eprintln!("[ralph] failed to record nit: {e}");
                    }
                    // Ingest any new tasks proposed by the reviewer.
                    if let Err(e) = ingest_new_tasks(&r, conn, &format!("reviewer/{id}")) {
                        eprintln!("[ralph] failed to ingest new tasks from reviewer: {e}");
                    }
                    match r.status {
                        AgentStatus::Success => {
                            db::update_phase(conn, &id, Phase::Done, unix_now())?;
                            db::update_last_error(conn, &id, None)?;
                            progressed = true;
                            eprintln!("[ralph] {id} — done!");
                        }
                        AgentStatus::ApprovedWithNits { .. } => {
                            db::update_phase(conn, &id, Phase::Done, unix_now())?;
                            db::update_last_error(conn, &id, None)?;
                            progressed = true;
                            eprintln!("[ralph] {id} — done (with nits)");
                        }
                        AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                            push_feedback(
                                conn,
                                &id,
                                "Reviewer",
                                attempts,
                                &r.text,
                                &r.stderr_lines,
                            )?;
                            let new_phase = reset_or_fail_phase(attempts, config);
                            db::update_phase(conn, &id, new_phase, unix_now())?;
                            db::update_last_error(conn, &id, Some(&reason))?;
                            progressed = true;
                            eprintln!("[ralph] {id} review issues: {reason}");
                        }
                    }
                }
                Err(e) => {
                    push_feedback(conn, &id, "Reviewer", attempts, &e.to_string(), &[])?;
                    let new_phase = reset_or_fail_phase(attempts, config);
                    db::update_phase(conn, &id, new_phase, unix_now())?;
                    db::update_last_error(conn, &id, Some(&e.to_string()))?;
                    progressed = true;
                    eprintln!("[ralph] reviewer error for {id}: {e}");
                }
            }
            if config
                .max_cost_usd
                .is_some_and(|max| *cumulative_cost > max)
            {
                eprintln!(
                    "[ralph] cost budget exceeded (${:.4} > ${:.4}), stopping.",
                    *cumulative_cost,
                    config.max_cost_usd.unwrap()
                );
                return Ok(progressed);
            }
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

// ── Workspace management ──────────────────────────────────

/// Remove any `ralph-*` workspaces left over from interrupted
/// runs. Parses `jj workspace list` and forgets/removes each.
async fn cleanup_stale_workspaces() {
    let Ok(output) = TokioCommand::new("jj")
        .args(["workspace", "list"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Format: "workspace-name: <change-id> <description>"
        let Some(name) = line.split(':').next().map(str::trim) else {
            continue;
        };
        if !name.starts_with("ralph-") {
            continue;
        }
        eprintln!("[ralph] cleaning up stale workspace {name}");
        let _ = TokioCommand::new("jj")
            .args(["workspace", "forget", name])
            .status()
            .await;
        let ws_dir = PathBuf::from(WS_DIR).join(format!("ws-{}", &name["ralph-".len()..]));
        if ws_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&ws_dir).await;
        }
    }
}

/// Create a jj workspace for a task. Returns the workspace
/// directory path. Symlinks `config.workspace.shared` entries
/// from the project root.
async fn create_workspace(task_id: &str, config: &Config) -> Result<PathBuf> {
    let ws_name = format!("ralph-{task_id}");
    let ws_dir = PathBuf::from(WS_DIR).join(format!("ws-{task_id}"));

    let status = TokioCommand::new("jj")
        .args([
            "workspace",
            "add",
            &ws_dir.to_string_lossy(),
            "--name",
            &ws_name,
        ])
        .status()
        .await
        .context("jj workspace add")?;

    if !status.success() {
        bail!("jj workspace add failed for {ws_name}");
    }

    // Symlink shared paths from the project root
    let project_root = std::env::current_dir().context("getting project root")?;
    for shared in &config.workspace.shared {
        let src = project_root.join(shared);
        let dst = ws_dir.join(shared);
        if src.exists() {
            // Ensure parent dirs exist
            if let Some(parent) = dst.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            // Remove existing entry if any (workspace add may
            // have checked out the file)
            let _ = tokio::fs::remove_file(&dst).await;
            let _ = tokio::fs::remove_dir_all(&dst).await;
            tokio::fs::symlink(&src, &dst)
                .await
                .with_context(|| format!("symlinking {} → {}", src.display(), dst.display()))?;
        }
    }

    let abs = tokio::fs::canonicalize(&ws_dir)
        .await
        .unwrap_or_else(|_| project_root.join(&ws_dir));
    Ok(abs)
}

/// Tear down a workspace: forget it and optionally abandon its
/// changes, then remove the directory.
async fn teardown_workspace(task_id: &str, abandon: bool) {
    let ws_name = format!("ralph-{task_id}");
    if abandon {
        let _ = TokioCommand::new("jj")
            .args(["abandon", &format!("{ws_name}@")])
            .status()
            .await;
    }
    let _ = TokioCommand::new("jj")
        .args(["workspace", "forget", &ws_name])
        .status()
        .await;
    let ws_dir = PathBuf::from(WS_DIR).join(format!("ws-{task_id}"));
    if ws_dir.exists() {
        let _ = tokio::fs::remove_dir_all(&ws_dir).await;
    }
}

// ── Group execution strategies ────────────────────────────

/// Run a multi-task group with per-task jj workspaces for
/// isolation. Each agent gets its own working copy; after
/// completion, successful changes are squashed back into the
/// default workspace.
async fn run_group_with_workspaces(
    group: &[&Task],
    conn: &Connection,
    config: &Config,
    registry: &ProcessRegistry,
    cumulative_cost: &mut f64,
) -> Result<()> {
    // Create workspaces and spawn agents
    let mut handles = Vec::new();
    let mut created_ws: Vec<String> = Vec::new();

    for &t in group {
        let ws_path = match create_workspace(&t.id, config).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[ralph] workspace creation failed for {}: {e}", t.id);
                db::update_attempts(conn, &t.id, t.attempts + 1)?;
                let new_phase = reset_or_fail_phase(t.attempts + 1, config);
                db::update_phase(conn, &t.id, new_phase, unix_now())?;
                db::update_last_error(
                    conn,
                    &t.id,
                    Some(&format!("workspace creation: {e}")),
                )?;
                continue;
            }
        };
        created_ws.push(t.id.clone());

        db::update_phase(conn, &t.id, Phase::Implementing, unix_now())?;

        let id = t.id.clone();
        let title = t.title.clone();
        let desc = t.description.clone();
        let guidance = build_guidance(&t.guidance);
        let guidance = if guidance.is_empty() {
            None
        } else {
            Some(guidance)
        };
        let fb = build_feedback_history(&t.feedback, FEEDBACK_MAX_LEN);
        let fb = if fb.is_empty() { None } else { Some(fb) };
        let attempt = t.attempts + 1;
        let cfg = config.clone();
        let reg = registry.clone();
        handles.push(tokio::spawn(async move {
            let ctx =
                AgentContext::implement(&id, &title, &desc, guidance.as_deref(), fb.as_deref());
            let result = agent::invoke_agent(
                AgentRole::Implementer,
                &ctx,
                &cfg,
                Some(&ws_path),
                &reg,
                attempt,
            )
            .await;
            (id, result)
        }));
    }

    // Collect results
    struct Outcome {
        id: String,
        success: bool,
    }
    let mut outcomes = Vec::new();

    for handle in handles {
        let (id, result) = handle.await?;
        let task = group.iter().find(|t| t.id == id);
        let prev_attempts = task.map_or(0, |t| t.attempts);
        let new_attempts = prev_attempts + 1;
        db::update_attempts(conn, &id, new_attempts)?;

        let success = match result {
            Ok(r) => {
                *cumulative_cost += r.cost_usd.unwrap_or(0.0);
                if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status
                    && let Err(e) = record_nit(conn, &id, "implementer", new_attempts, suggestions)
                {
                    eprintln!("[ralph] failed to record nit: {e}");
                }
                // Ingest any new tasks proposed by the implementer.
                if let Err(e) = ingest_new_tasks(&r, conn, &format!("implementer/{id}")) {
                    eprintln!("[ralph] failed to ingest new tasks from implementer: {e}");
                }
                match &r.status {
                    AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                        db::update_phase(conn, &id, Phase::Testing, unix_now())?;
                        db::update_last_error(conn, &id, None)?;
                        true
                    }
                    AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                        let new_phase = reset_or_fail_phase(new_attempts, config);
                        db::update_phase(conn, &id, new_phase, unix_now())?;
                        db::update_last_error(conn, &id, Some(reason))?;
                        eprintln!("[ralph] {id} implement failed: {reason}");
                        false
                    }
                }
            }
            Err(e) => {
                let new_phase = reset_or_fail_phase(new_attempts, config);
                db::update_phase(conn, &id, new_phase, unix_now())?;
                db::update_last_error(conn, &id, Some(&e.to_string()))?;
                eprintln!("[ralph] agent error for {id}: {e}");
                false
            }
        };
        outcomes.push(Outcome { id, success });
    }

    // Merge successful workspaces, abandon failed ones.
    // Always capture files_changed (even on failure) for diagnostics.
    for outcome in &outcomes {
        if !created_ws.contains(&outcome.id) {
            continue;
        }

        // Attribute files from the workspace, regardless of outcome.
        let rev = format!("ralph-{}@", outcome.id);
        let files = agent::jj_changed_files_for(&rev).await.unwrap_or_default();
        let task = group.iter().find(|t| t.id == outcome.id);
        let mut all_files = task.map(|t| t.files_changed.clone()).unwrap_or_default();
        all_files.extend(files);
        all_files.sort();
        all_files.dedup();
        db::update_files_changed(conn, &outcome.id, &all_files)?;

        if outcome.success {
            // Squash workspace changes into default working copy
            let squash_status = TokioCommand::new("jj")
                .args(["squash", "--from", &rev, "--into", "@"])
                .status()
                .await;
            if let Err(e) = squash_status {
                eprintln!("[ralph] squash failed for {}: {e}", outcome.id);
            }
            teardown_workspace(&outcome.id, false).await;
        } else {
            teardown_workspace(&outcome.id, true).await;
        }
    }

    Ok(())
}

/// Run a singleton group directly in the default workspace
/// (no workspace overhead).
async fn run_group_singleton(
    group: &[&Task],
    conn: &Connection,
    config: &Config,
    registry: &ProcessRegistry,
    cumulative_cost: &mut f64,
) -> Result<()> {
    let t = group[0];
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
        None,
        registry,
        attempt,
    )
    .await;

    let new_attempts = t.attempts + 1;
    db::update_attempts(conn, &t.id, new_attempts)?;
    match result {
        Ok(r) => {
            *cumulative_cost += r.cost_usd.unwrap_or(0.0);
            if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status
                && let Err(e) = record_nit(conn, &t.id, "implementer", new_attempts, suggestions)
            {
                eprintln!("[ralph] failed to record nit: {e}");
            }
            // Ingest any new tasks proposed by the implementer.
            if let Err(e) = ingest_new_tasks(&r, conn, &format!("implementer/{}", t.id)) {
                eprintln!("[ralph] failed to ingest new tasks from implementer: {e}");
            }
            match &r.status {
                AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                    db::update_phase(conn, &t.id, Phase::Testing, unix_now())?;
                    db::update_last_error(conn, &t.id, None)?;
                }
                AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                    let new_phase = reset_or_fail_phase(new_attempts, config);
                    db::update_phase(conn, &t.id, new_phase, unix_now())?;
                    db::update_last_error(conn, &t.id, Some(reason))?;
                    eprintln!("[ralph] {} implement failed: {reason}", t.id);
                }
            }
        }
        Err(e) => {
            let new_phase = reset_or_fail_phase(new_attempts, config);
            db::update_phase(conn, &t.id, new_phase, unix_now())?;
            db::update_last_error(conn, &t.id, Some(&e.to_string()))?;
            eprintln!("[ralph] agent error for {}: {e}", t.id);
        }
    }

    // Attribute files via pre/post snapshot (even on failure,
    // so diagnostics can see what the agent changed).
    {
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
