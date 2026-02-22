use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command as TokioCommand;

use crate::agent::{
    self, AgentContext, AgentResult, AgentRole, AgentStatus, FEEDBACK_MAX_LEN, ProcessRegistry,
    build_feedback_history, truncate_feedback,
};
use crate::config::Config;
use crate::nit::truncate_with_ellipsis;
use crate::scheduler;
use crate::state::{ExecutionState, Phase};
use crate::task::{self, Task};

use crate::state::TaskExecution;

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

const STATE_PATH: &str = ".ralph/state.json";

/// Set a task back to Pending, or to Failed if it has
/// exhausted its attempt budget.
fn reset_or_fail(exec: &mut TaskExecution, config: &Config) {
    if exec.attempts >= config.max_attempts {
        exec.phase = Phase::Failed;
    } else {
        exec.phase = Phase::Pending;
    }
}

/// Decide whether a failure is worth retrying based on its
/// classification. Timeouts and unknown errors are retryable;
/// build errors and test failures are retryable (the implementer
/// gets feedback). Review rejections are always retryable.
fn should_retry(kind: agent::FailureKind) -> bool {
    match kind {
        agent::FailureKind::Timeout => true,
        agent::FailureKind::BuildError => true,
        agent::FailureKind::TestFailure => true,
        agent::FailureKind::ReviewRejection => true,
        agent::FailureKind::Unknown => true,
    }
}

/// Like `reset_or_fail` but consults failure classification.
/// Non-retryable failures go straight to Failed regardless of
/// attempt count.
#[allow(dead_code)]
fn reset_or_fail_classified(exec: &mut TaskExecution, config: &Config, reason: &str) {
    let kind = agent::classify_failure(reason);
    if !should_retry(kind) || exec.attempts >= config.max_attempts {
        exec.phase = Phase::Failed;
    } else {
        exec.phase = Phase::Pending;
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
fn push_feedback(exec: &mut TaskExecution, phase_label: &str, full_text: &str, stderr: &[String]) {
    let prefix = format!("[{phase_label} · attempt {}]", exec.attempts);
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
    exec.feedback.push(format!("{prefix}\n{body}"));
}

/// Record a nit to `.ralph/nits.jsonl`.
async fn record_nit(
    source_task: &str,
    source_role: &str,
    attempt: u32,
    suggestions: &str,
) -> Result<()> {
    let path = PathBuf::from(".ralph/nits.jsonl");
    let nits = crate::nit::load_nits(&path).await.unwrap_or_default();
    let id = crate::nit::next_nit_id(&nits);
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
        created_at: crate::state::unix_now(),
    };
    crate::nit::append_nit(&path, &nit).await
}

/// Extract proposed tasks from an agent result, assign IDs,
/// deduplicate, and append to the task file.
///
/// Returns the number of tasks actually added.
async fn ingest_new_tasks(
    result: &AgentResult,
    tasks_path: &Path,
    source_label: &str,
) -> Result<usize> {
    let proposals = result.parse_new_tasks();
    if proposals.is_empty() {
        return Ok(0);
    }
    materialize_proposed_tasks(&proposals, tasks_path, source_label).await
}

/// Parse a numbered-list failure reason into tasks and append them.
///
/// Used as a fallback when the final reviewer returns a failure
/// reason without structured `NEW_TASKS:` output.
async fn ingest_from_failure_reason(
    reason: &str,
    tasks_path: &Path,
    source_label: &str,
) -> Result<usize> {
    let proposals = agent::tasks_from_numbered_list(reason);
    if proposals.is_empty() {
        return Ok(0);
    }
    materialize_proposed_tasks(&proposals, tasks_path, source_label).await
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

/// Shared logic: validate, deduplicate, assign IDs, and append proposed tasks.
async fn materialize_proposed_tasks(
    proposals: &[agent::ProposedTask],
    tasks_path: &Path,
    source_label: &str,
) -> Result<usize> {
    let existing = task::load_tasks(tasks_path).await.unwrap_or_default();
    let existing_ids: HashSet<&str> = existing.iter().map(|t| t.id.as_str()).collect();
    let existing_titles: HashSet<&str> = existing.iter().map(|t| t.title.as_str()).collect();
    let max_priority = existing.iter().map(|t| t.priority).max().unwrap_or(0);

    let mut gen_counter = existing
        .iter()
        .filter_map(|t| t.id.strip_prefix("GEN-")?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);

    let mut new_tasks = Vec::new();
    let mut new_ids: HashSet<String> = HashSet::new();

    for p in proposals {
        // Deduplicate by title — skip if an identical task already exists.
        if existing_titles.contains(p.title.as_str()) {
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
        new_tasks.push(Task {
            id,
            title: p.title.clone(),
            description: p.description.clone(),
            priority: p.priority.unwrap_or(max_priority + 1),
            blocked_by: p.blocked_by.clone(),
        });
    }

    let count = new_tasks.len();
    if count > 0 {
        task::append_tasks(tasks_path, &new_tasks).await?;
        for t in &new_tasks {
            eprintln!(
                "[ralph] {source_label} → new task: [{}] {} (pri={})",
                t.id, t.title, t.priority
            );
        }
    }
    Ok(count)
}

/// Main orchestration loop. Iterates until convergence
/// (all tasks done + reviewer approves), stagnation
/// (max attempts exceeded), or iteration cap.
pub async fn run_loop(tasks_path: &Path, max_iterations: usize, config: &Config) -> Result<()> {
    let state_path = PathBuf::from(STATE_PATH);
    let registry = ProcessRegistry::new(config.kill_grace_secs);
    spawn_signal_handler(registry.clone());
    isolate_dirty_tree().await;
    cleanup_stale_workspaces().await;
    let mut cumulative_cost: f64 = 0.0;
    let mut triage_rounds: u32 = 0;

    for iteration in 1..=max_iterations {
        if registry.is_shutdown() {
            eprintln!("[ralph] shutdown requested, saving state...");
            return Ok(());
        }

        eprintln!("\n[ralph] === iteration {iteration} === (${cumulative_cost:.4} spent)");

        registry.audit_and_kill_orphans().await;

        let tasks = task::load_tasks(tasks_path).await?;
        let archive_path = std::path::PathBuf::from(".ralph/archive.jsonl");
        let archived = task::load_archive(&archive_path).await?;
        let archived_ids: std::collections::HashSet<&str> =
            archived.iter().map(|t| t.id.as_str()).collect();
        task::validate_deps(&tasks, &archived_ids)?;
        let mut state = ExecutionState::load(&state_path).await?;
        let task_ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();

        // Drain sideband directives (from ralph skip/fail/reset)
        match crate::state::drain_directives().await {
            Ok(directives) if !directives.is_empty() => {
                for d in &directives {
                    eprintln!("[ralph] applying directive: {:?} {}", d.action, d.task_id);
                }
                state.apply_directives(&directives, &task_ids);
                state.save(&state_path).await?;
            }
            Err(e) => {
                eprintln!("[ralph] failed to drain directives: {e}");
            }
            _ => {}
        }

        // Check convergence: all done → final review
        if state.all_done(&task_ids) {
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
                state.save(&state_path).await?;
                return Ok(());
            }

            if registry.is_shutdown() {
                eprintln!("[ralph] shutdown requested, saving state...");
                state.save(&state_path).await?;
                return Ok(());
            }

            match review.status {
                AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                    if let AgentStatus::ApprovedWithNits { suggestions } = &review.status {
                        eprintln!("[ralph] final review nits: {suggestions}");
                        if let Err(e) = record_nit("final", "final_review", 1, suggestions).await {
                            eprintln!("[ralph] failed to record nit: {e}");
                        }
                    }

                    if config.auto_triage && triage_rounds < config.max_triage_rounds {
                        let promoted =
                            triage_open_nits(tasks_path, config, &registry, &mut cumulative_cost)
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
                    let mut added = ingest_new_tasks(&review, tasks_path, "final review").await?;

                    // Fallback: parse numbered items from the failure reason.
                    if added == 0 {
                        added =
                            ingest_from_failure_reason(reason, tasks_path, "final review").await?;
                    }

                    // Last resort: wrap the prose failure reason as a single task.
                    if added == 0 && !reason.trim().is_empty() {
                        let fallback = vec![agent::ProposedTask {
                            id: None,
                            title: truncate_for_title(reason),
                            description: reason.to_string(),
                            priority: None,
                            blocked_by: vec![],
                        }];
                        added = materialize_proposed_tasks(
                            &fallback,
                            tasks_path,
                            "final review (synthesized)",
                        )
                        .await?;
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

        // Resume interrupted in-flight tasks before
        // scheduling new work.
        let made_progress = resume_inflight(
            &tasks,
            &mut state,
            config,
            &registry,
            &mut cumulative_cost,
            tasks_path,
        )
        .await?;
        if registry.is_shutdown() {
            eprintln!("[ralph] shutdown requested, saving state...");
            state.save(&state_path).await?;
            return Ok(());
        }
        if made_progress {
            state.save(&state_path).await?;
            // Re-evaluate from the top — deps may have
            // unblocked.
            continue;
        }

        // Check stagnation
        let stagnant: Vec<&str> = tasks
            .iter()
            .filter(|t| {
                state
                    .tasks
                    .get(&t.id)
                    .is_some_and(|e| e.phase == Phase::Failed)
            })
            .map(|t| t.id.as_str())
            .collect();

        if !stagnant.is_empty() {
            eprintln!(
                "[ralph] stagnant tasks (max attempts): {}",
                stagnant.join(", ")
            );
        }

        // Find ready tasks (Pending phase only)
        let ready = scheduler::ready_tasks(&tasks, &state, config);
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
        let groups = scheduler::partition_independent(&ready, &state);

        for group in groups {
            let use_workspaces = group.len() > 1;

            if use_workspaces {
                run_group_with_workspaces(
                    &group,
                    &mut state,
                    config,
                    &registry,
                    &mut cumulative_cost,
                    tasks_path,
                )
                .await?;
            } else {
                run_group_singleton(
                    &group,
                    &mut state,
                    config,
                    &registry,
                    &mut cumulative_cost,
                    tasks_path,
                )
                .await?;
            }

            state.save(&state_path).await?;

            if config.max_cost_usd.is_some_and(|max| cumulative_cost > max) {
                eprintln!(
                    "[ralph] cost budget exceeded (${cumulative_cost:.4} > ${:.4}), stopping.",
                    config.max_cost_usd.unwrap()
                );
                return Ok(());
            }

            if registry.is_shutdown() {
                eprintln!("[ralph] shutdown requested, saving state...");
                return Ok(());
            }

            // Advance any tasks now at Testing/Reviewing
            resume_inflight(
                &tasks,
                &mut state,
                config,
                &registry,
                &mut cumulative_cost,
                tasks_path,
            )
            .await?;
            state.save(&state_path).await?;

            if registry.is_shutdown() {
                eprintln!("[ralph] shutdown requested, saving state...");
                return Ok(());
            }

            // Checkpoint: seal the current working-copy change
            // and start a fresh one for the next group.
            let detail = checkpoint_description(&group, &state);
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
async fn triage_open_nits(
    tasks_path: &Path,
    config: &Config,
    registry: &ProcessRegistry,
    cumulative_cost: &mut f64,
) -> Result<usize> {
    let nits_path = PathBuf::from(".ralph/nits.jsonl");
    let mut nits = crate::nit::load_nits(&nits_path).await.unwrap_or_default();

    let open: Vec<&crate::nit::Nit> = nits
        .iter()
        .filter(|n| n.status == crate::nit::NitStatus::Open)
        .collect();

    if open.is_empty() {
        return Ok(0);
    }

    eprintln!("[ralph] triaging {} open nit(s)...", open.len());

    // Build context for the triager
    let nits_json = open
        .iter()
        .map(|n| serde_json::to_string(n).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");

    let tasks = task::load_tasks(tasks_path).await.unwrap_or_default();
    let state_path = PathBuf::from(STATE_PATH);
    let exec_state = ExecutionState::load(&state_path).await?;
    let tasks_summary = tasks
        .iter()
        .map(|t| {
            let phase = exec_state
                .tasks
                .get(&t.id)
                .map(|e| format!("{:?}", e.phase))
                .unwrap_or_else(|| "Pending".to_string());
            format!("[{}] {} ({})", t.id, t.title, phase)
        })
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

    for d in &decisions {
        let nit_entry = nits.iter_mut().find(|n| n.id == d.nit_id);
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
                nit_entry.status = crate::nit::NitStatus::Promoted;
                promoted_count += 1;
                eprintln!("[ralph] triager: promote {}", d.nit_id);
            }
            "dismiss" => {
                nit_entry.status = crate::nit::NitStatus::Dismissed;
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

    // Save nit status updates
    if let Err(e) = crate::nit::save_nits(&nits_path, &nits).await {
        eprintln!("[ralph] failed to save nit updates: {e}");
    }

    // Materialize promoted nits as tasks
    if !proposals.is_empty() {
        match materialize_proposed_tasks(&proposals, tasks_path, "triager").await {
            Ok(added) => {
                eprintln!("[ralph] triager created {added} task(s)");
            }
            Err(e) => {
                eprintln!("[ralph] failed to materialize triager tasks: {e}");
            }
        }
    }

    Ok(promoted_count)
}

/// Group Testing tasks by disjoint file sets for parallel execution.
/// Tasks with overlapping files_changed are placed in the same group
/// to avoid parallel testing of the same files.
fn group_by_disjoint_files(ids: &[String], state: &ExecutionState) -> Vec<Vec<String>> {
    let mut groups: Vec<(std::collections::HashSet<PathBuf>, Vec<String>)> = Vec::new();

    for id in ids {
        let files: std::collections::HashSet<PathBuf> = state
            .tasks
            .get(id)
            .map(|e| e.files_changed.iter().cloned().collect())
            .unwrap_or_default();

        let mut merged = false;
        for (group_files, group_ids) in &mut groups {
            if files.is_disjoint(group_files) {
                group_files.extend(files.iter().cloned());
                group_ids.push(id.clone());
                merged = true;
                break;
            }
        }
        if !merged {
            groups.push((files, vec![id.clone()]));
        }
    }

    groups.into_iter().map(|(_, ids)| ids).collect()
}

/// Advance all tasks stuck at Testing or Reviewing.
/// Returns true if any task moved forward.
///
/// Testing tasks with disjoint file sets run in parallel.
/// Reviewing tasks always run in parallel (read-only).
async fn resume_inflight(
    tasks: &[Task],
    state: &mut ExecutionState,
    config: &Config,
    registry: &ProcessRegistry,
    cumulative_cost: &mut f64,
    tasks_path: &Path,
) -> Result<bool> {
    let mut progressed = false;

    // Implementing phase → task was mid-implementation when Ralph
    // restarted. Reset to Pending so the scheduler picks it up again.
    let implementing: Vec<String> = state
        .tasks
        .iter()
        .filter(|(_, e)| e.phase == Phase::Implementing)
        .map(|(id, _)| id.clone())
        .collect();
    for id in implementing {
        eprintln!("[ralph] {id} stuck in Implementing, resetting to Pending");
        let exec = state.entry(&id);
        exec.phase = Phase::Pending;
        progressed = true;
    }

    // Testing phase → run tester (parallel for disjoint file sets)
    let testing: Vec<String> = state
        .tasks
        .iter()
        .filter(|(_, e)| e.phase == Phase::Testing)
        .map(|(id, _)| id.clone())
        .collect();

    let test_groups = group_by_disjoint_files(&testing, state);
    for group in test_groups {
        let mut handles = Vec::new();
        for id in &group {
            let files = state
                .tasks
                .get(id)
                .map(|e| e.files_changed.clone())
                .unwrap_or_default();
            let ctx = AgentContext::test(id, files);
            let mut cfg = config.clone();
            if !config.workspace.isolate_env.is_empty() {
                let base = std::env::current_dir().unwrap_or_default().join(WS_DIR);
                for (env_var, subdir) in &config.workspace.isolate_env {
                    cfg.env.set.insert(
                        env_var.clone(),
                        base.join(subdir).to_string_lossy().to_string(),
                    );
                }
            }
            let reg = registry.clone();
            let id_owned = id.clone();
            handles.push(tokio::spawn(async move {
                let result =
                    agent::invoke_agent(AgentRole::Tester, &ctx, &cfg, None, &reg, 0).await;
                (id_owned, result)
            }));
        }

        for handle in handles {
            let (id, result) = handle.await?;
            eprintln!("[ralph] resuming test for {id}...");
            match result {
                Ok(r) => {
                    *cumulative_cost += r.cost_usd.unwrap_or(0.0);
                    if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status {
                        let attempts = state.tasks.get(&id).map_or(0, |e| e.attempts);
                        if let Err(e) = record_nit(&id, "tester", attempts, suggestions).await {
                            eprintln!("[ralph] failed to record nit: {e}");
                        }
                    }
                    // Ingest any new tasks proposed by the tester.
                    if let Err(e) = ingest_new_tasks(&r, tasks_path, &format!("tester/{id}")).await
                    {
                        eprintln!("[ralph] failed to ingest new tasks from tester: {e}");
                    }
                    let exec = state.entry(&id);
                    match r.status {
                        AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                            exec.phase = Phase::Reviewing;
                            exec.last_error = None;
                            progressed = true;
                        }
                        AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                            push_feedback(exec, "Tester", &r.text, &r.stderr_lines);
                            reset_or_fail(exec, config);
                            exec.last_error = Some(reason.clone());
                            progressed = true;
                            eprintln!("[ralph] {id} tests failed: {reason}");
                        }
                    }
                }
                Err(e) => {
                    let exec = state.entry(&id);
                    push_feedback(exec, "Tester", &e.to_string(), &[]);
                    reset_or_fail(exec, config);
                    exec.last_error = Some(e.to_string());
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
    let reviewing: Vec<String> = state
        .tasks
        .iter()
        .filter(|(_, e)| e.phase == Phase::Reviewing)
        .map(|(id, _)| id.clone())
        .collect();

    if !reviewing.is_empty() {
        // Compute diff once for all reviewers.
        let diff = agent::jj_diff_git().await.unwrap_or_default();

        let mut handles = Vec::new();
        for id in &reviewing {
            let t = tasks.iter().find(|t| t.id == *id);
            let (title, desc) = t
                .map(|t| (t.title.as_str(), t.description.as_str()))
                .unwrap_or(("unknown", ""));
            let diff_summary = state
                .tasks
                .get(id)
                .map(|e| {
                    e.files_changed
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let ctx = AgentContext::review(id, title, desc, diff_summary, diff.clone());
            let cfg = config.clone();
            let reg = registry.clone();
            let id_owned = id.clone();
            handles.push(tokio::spawn(async move {
                let result =
                    agent::invoke_agent(AgentRole::Reviewer, &ctx, &cfg, None, &reg, 0).await;
                (id_owned, result)
            }));
        }

        for handle in handles {
            let (id, result) = handle.await?;
            eprintln!("[ralph] resuming review for {id}...");
            match result {
                Ok(r) => {
                    *cumulative_cost += r.cost_usd.unwrap_or(0.0);
                    if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status {
                        let attempts = state.tasks.get(&id).map_or(0, |e| e.attempts);
                        if let Err(e) = record_nit(&id, "reviewer", attempts, suggestions).await {
                            eprintln!("[ralph] failed to record nit: {e}");
                        }
                    }
                    // Ingest any new tasks proposed by the reviewer.
                    if let Err(e) =
                        ingest_new_tasks(&r, tasks_path, &format!("reviewer/{id}")).await
                    {
                        eprintln!("[ralph] failed to ingest new tasks from reviewer: {e}");
                    }
                    let exec = state.entry(&id);
                    match r.status {
                        AgentStatus::Success => {
                            exec.phase = Phase::Done;
                            exec.last_error = None;
                            progressed = true;
                            eprintln!("[ralph] {id} — done!");
                        }
                        AgentStatus::ApprovedWithNits { .. } => {
                            exec.phase = Phase::Done;
                            exec.last_error = None;
                            progressed = true;
                            eprintln!("[ralph] {id} — done (with nits)");
                        }
                        AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                            push_feedback(exec, "Reviewer", &r.text, &r.stderr_lines);
                            reset_or_fail(exec, config);
                            exec.last_error = Some(reason.clone());
                            progressed = true;
                            eprintln!("[ralph] {id} review issues: {reason}");
                        }
                    }
                }
                Err(e) => {
                    let exec = state.entry(&id);
                    push_feedback(exec, "Reviewer", &e.to_string(), &[]);
                    reset_or_fail(exec, config);
                    exec.last_error = Some(e.to_string());
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
fn checkpoint_description(group: &[&Task], state: &ExecutionState) -> String {
    let parts: Vec<String> = group
        .iter()
        .map(|t| {
            let phase = state
                .tasks
                .get(&t.id)
                .map(|e| match e.phase {
                    Phase::Pending => "pending",
                    Phase::Implementing => "implementing",
                    Phase::Testing => "testing",
                    Phase::Reviewing => "reviewing",
                    Phase::Done => "done",
                    Phase::Failed => "failed",
                    Phase::Skipped => "skipped",
                })
                .unwrap_or("unknown");
            format!("{} ({})", t.id, phase)
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
    state: &mut ExecutionState,
    config: &Config,
    registry: &ProcessRegistry,
    cumulative_cost: &mut f64,
    tasks_path: &Path,
) -> Result<()> {
    // Create workspaces and spawn agents
    let mut handles = Vec::new();
    let mut created_ws: Vec<String> = Vec::new();

    for &t in group {
        let ws_path = match create_workspace(&t.id, config).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[ralph] workspace creation failed for {}: {e}", t.id);
                let exec = state.entry(&t.id);
                exec.attempts += 1;
                reset_or_fail(exec, config);
                exec.last_error = Some(format!("workspace creation: {e}"));
                continue;
            }
        };
        created_ws.push(t.id.clone());

        {
            let exec = state.entry(&t.id);
            exec.phase = Phase::Implementing;
            if exec.started_at.is_none() {
                exec.started_at = Some(crate::state::unix_now());
            }
            exec.phase_entered_at = Some(crate::state::unix_now());
        }
        state.save(&PathBuf::from(STATE_PATH)).await?;

        let id = t.id.clone();
        let title = t.title.clone();
        let desc = t.description.clone();
        let (guidance, fb, attempt) = state
            .tasks
            .get(&t.id)
            .map(|e| {
                let g = build_guidance(&e.guidance);
                let g = if g.is_empty() { None } else { Some(g) };
                let fb = build_feedback_history(&e.feedback, FEEDBACK_MAX_LEN);
                let fb = if fb.is_empty() { None } else { Some(fb) };
                (g, fb, e.attempts + 1)
            })
            .unwrap_or((None, None, 1));
        let mut cfg = config.clone();
        for (env_var, subdir) in &config.workspace.isolate_env {
            cfg.env.set.insert(
                env_var.clone(),
                ws_path.join(subdir).to_string_lossy().to_string(),
            );
        }
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
        {
            let exec = state.entry(&id);
            exec.attempts += 1;
        }
        let success = match result {
            Ok(r) => {
                *cumulative_cost += r.cost_usd.unwrap_or(0.0);
                if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status {
                    let attempts = state.tasks.get(&id).map_or(0, |e| e.attempts);
                    if let Err(e) = record_nit(&id, "implementer", attempts, suggestions).await {
                        eprintln!("[ralph] failed to record nit: {e}");
                    }
                }
                // Ingest any new tasks proposed by the implementer.
                if let Err(e) = ingest_new_tasks(&r, tasks_path, &format!("implementer/{id}")).await
                {
                    eprintln!("[ralph] failed to ingest new tasks from implementer: {e}");
                }
                let exec = state.entry(&id);
                match &r.status {
                    AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                        exec.phase = Phase::Testing;
                        exec.last_error = None;
                        true
                    }
                    AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                        reset_or_fail(exec, config);
                        exec.last_error = Some(reason.clone());
                        eprintln!("[ralph] {id} implement failed: {reason}");
                        false
                    }
                }
            }
            Err(e) => {
                let exec = state.entry(&id);
                reset_or_fail(exec, config);
                exec.last_error = Some(e.to_string());
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
        let exec = state.entry(&outcome.id);
        exec.files_changed.extend(files);
        exec.files_changed.sort();
        exec.files_changed.dedup();

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
    state: &mut ExecutionState,
    config: &Config,
    registry: &ProcessRegistry,
    cumulative_cost: &mut f64,
    tasks_path: &Path,
) -> Result<()> {
    let t = group[0];
    let pre_files = agent::jj_changed_files().await.unwrap_or_default();

    {
        let exec = state.entry(&t.id);
        exec.phase = Phase::Implementing;
        if exec.started_at.is_none() {
            exec.started_at = Some(crate::state::unix_now());
        }
        exec.phase_entered_at = Some(crate::state::unix_now());
    }
    state.save(&PathBuf::from(STATE_PATH)).await?;

    let (guidance, feedback_history, attempt) = state
        .tasks
        .get(&t.id)
        .map(|e| {
            let g = build_guidance(&e.guidance);
            let g = if g.is_empty() { None } else { Some(g) };
            let fb = build_feedback_history(&e.feedback, FEEDBACK_MAX_LEN);
            let fb = if fb.is_empty() { None } else { Some(fb) };
            (g, fb, e.attempts + 1)
        })
        .unwrap_or((None, None, 1));
    let ctx = AgentContext::implement(
        &t.id,
        &t.title,
        &t.description,
        guidance.as_deref(),
        feedback_history.as_deref(),
    );
    let mut cfg = config.clone();
    if !config.workspace.isolate_env.is_empty() {
        let base = std::env::current_dir().unwrap_or_default().join(WS_DIR);
        for (env_var, subdir) in &config.workspace.isolate_env {
            cfg.env.set.insert(
                env_var.clone(),
                base.join(subdir).to_string_lossy().to_string(),
            );
        }
    }
    let result =
        agent::invoke_agent(AgentRole::Implementer, &ctx, &cfg, None, registry, attempt).await;

    {
        let exec = state.entry(&t.id);
        exec.attempts += 1;
    }
    match result {
        Ok(r) => {
            *cumulative_cost += r.cost_usd.unwrap_or(0.0);
            if let AgentStatus::ApprovedWithNits { ref suggestions } = r.status {
                let attempts = state.tasks.get(&t.id).map_or(0, |e| e.attempts);
                if let Err(e) = record_nit(&t.id, "implementer", attempts, suggestions).await {
                    eprintln!("[ralph] failed to record nit: {e}");
                }
            }
            // Ingest any new tasks proposed by the implementer.
            if let Err(e) = ingest_new_tasks(&r, tasks_path, &format!("implementer/{}", t.id)).await
            {
                eprintln!("[ralph] failed to ingest new tasks from implementer: {e}");
            }
            let exec = state.entry(&t.id);
            match &r.status {
                AgentStatus::Success | AgentStatus::ApprovedWithNits { .. } => {
                    exec.phase = Phase::Testing;
                    exec.last_error = None;
                }
                AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                    reset_or_fail(exec, config);
                    exec.last_error = Some(reason.clone());
                    eprintln!("[ralph] {} implement failed: {reason}", t.id);
                }
            }
        }
        Err(e) => {
            let exec = state.entry(&t.id);
            reset_or_fail(exec, config);
            exec.last_error = Some(e.to_string());
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
        let exec = state.entry(&t.id);
        exec.files_changed.extend(new_files);
        exec.files_changed.sort();
        exec.files_changed.dedup();
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
}
