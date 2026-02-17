use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command as TokioCommand;

use crate::agent::{
    self, AgentContext, AgentRole, AgentStatus,
};
use crate::config::Config;
use crate::scheduler;
use crate::state::{ExecutionState, Phase};
use crate::task::{self, Task};

const STATE_PATH: &str = ".ralph/state.json";

/// Main orchestration loop. Iterates until convergence
/// (all tasks done + reviewer approves), stagnation
/// (max attempts exceeded), or iteration cap.
pub async fn run_loop(
    tasks_path: &Path,
    max_iterations: usize,
    config: &Config,
) -> Result<()> {
    let state_path = PathBuf::from(STATE_PATH);

    for iteration in 1..=max_iterations {
        eprintln!("\n[ralph] === iteration {iteration} ===");

        let tasks = task::load_tasks(tasks_path).await?;
        let mut state =
            ExecutionState::load(&state_path).await?;
        let task_ids: Vec<String> =
            tasks.iter().map(|t| t.id.clone()).collect();

        // Check convergence: all done → final review
        if state.all_done(&task_ids) {
            eprintln!(
                "[ralph] all tasks done, final review..."
            );
            let review = agent::invoke_agent(
                AgentRole::Reviewer,
                &AgentContext::Review {
                    task_id: "final".to_string(),
                    task_title: "Final review".to_string(),
                    task_description:
                        "Review the full project for \
                         correctness."
                            .to_string(),
                },
                config,
            )
            .await?;

            match review.status {
                AgentStatus::Success => {
                    eprintln!(
                        "[ralph] final review passed — \
                         converged!"
                    );
                    return Ok(());
                }
                AgentStatus::Failure { reason }
                | AgentStatus::NeedsRetry { reason } => {
                    eprintln!(
                        "[ralph] reviewer found issues: \
                         {reason}"
                    );
                    eprintln!(
                        "[ralph] issues should be added as \
                         new tasks. Stopping."
                    );
                    return Ok(());
                }
            }
        }

        // Resume interrupted in-flight tasks before
        // scheduling new work.
        let made_progress =
            resume_inflight(&tasks, &mut state, config)
                .await?;
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
                state.tasks.get(&t.id).map_or(false, |e| {
                    e.attempts >= config.max_attempts
                        && e.phase != Phase::Done
                })
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
        let ready =
            scheduler::ready_tasks(&tasks, &state, config);
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
        let groups =
            scheduler::partition_independent(&ready, &state);

        for group in groups {
            // Fan out: parallel implementers
            let implement_handles: Vec<_> = group
                .iter()
                .map(|t| {
                    let id = t.id.clone();
                    let title = t.title.clone();
                    let desc = t.description.clone();
                    let cfg = config.clone();
                    tokio::spawn(async move {
                        let ctx = AgentContext::implement(
                            &id, &title, &desc,
                        );
                        let result = agent::invoke_agent(
                            AgentRole::Implementer,
                            &ctx,
                            &cfg,
                        )
                        .await;
                        (id, result)
                    })
                })
                .collect();

            // Collect results
            for handle in implement_handles {
                let (id, result) = handle.await?;
                let exec = state.entry(&id);
                exec.attempts += 1;
                match result {
                    Ok(r) => {
                        exec.files_changed.extend(
                            r.files_changed.iter().cloned(),
                        );
                        exec.files_changed.sort();
                        exec.files_changed.dedup();
                        match &r.status {
                            AgentStatus::Success => {
                                exec.phase = Phase::Testing;
                                exec.last_error = None;
                            }
                            AgentStatus::Failure { reason }
                            | AgentStatus::NeedsRetry {
                                reason,
                            } => {
                                exec.phase = Phase::Pending;
                                exec.last_error =
                                    Some(reason.clone());
                                eprintln!(
                                    "[ralph] {id} implement \
                                     failed: {reason}"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        exec.last_error =
                            Some(e.to_string());
                        eprintln!(
                            "[ralph] agent error for \
                             {id}: {e}"
                        );
                    }
                }
            }

            state.save(&state_path).await?;

            // Advance any tasks now at Testing/Reviewing
            resume_inflight(&tasks, &mut state, config)
                .await?;
            state.save(&state_path).await?;

            // Commit progress if working tree is dirty
            if let Err(e) = git_commit_progress().await {
                eprintln!(
                    "[ralph] git commit skipped: {e}"
                );
            }
        }
    }

    eprintln!("[ralph] loop finished.");
    Ok(())
}

/// Advance all tasks stuck at Testing or Reviewing.
/// Returns true if any task moved forward.
async fn resume_inflight(
    tasks: &[Task],
    state: &mut ExecutionState,
    config: &Config,
) -> Result<bool> {
    let mut progressed = false;

    // Testing phase → run tester
    let testing: Vec<String> = state
        .tasks
        .iter()
        .filter(|(_, e)| e.phase == Phase::Testing)
        .map(|(id, _)| id.clone())
        .collect();

    for id in &testing {
        eprintln!("[ralph] resuming test for {id}...");
        let files = state
            .tasks
            .get(id)
            .map(|e| e.files_changed.clone())
            .unwrap_or_default();

        let ctx = AgentContext::test(id, files);
        match agent::invoke_agent(
            AgentRole::Tester,
            &ctx,
            config,
        )
        .await
        {
            Ok(r) => {
                let exec = state.entry(id);
                match r.status {
                    AgentStatus::Success => {
                        exec.phase = Phase::Reviewing;
                        exec.last_error = None;
                        progressed = true;
                    }
                    AgentStatus::Failure { reason }
                    | AgentStatus::NeedsRetry { reason } => {
                        exec.phase = Phase::Pending;
                        exec.last_error =
                            Some(reason.clone());
                        progressed = true;
                        eprintln!(
                            "[ralph] {id} tests failed: \
                             {reason}"
                        );
                    }
                }
            }
            Err(e) => {
                let exec = state.entry(id);
                exec.phase = Phase::Pending;
                exec.last_error = Some(e.to_string());
                progressed = true;
                eprintln!(
                    "[ralph] tester error for {id}: {e}"
                );
            }
        }
    }

    // Reviewing phase → run reviewer
    let reviewing: Vec<String> = state
        .tasks
        .iter()
        .filter(|(_, e)| e.phase == Phase::Reviewing)
        .map(|(id, _)| id.clone())
        .collect();

    for id in &reviewing {
        eprintln!("[ralph] resuming review for {id}...");
        let t = tasks.iter().find(|t| t.id == *id);
        let (title, desc) = t
            .map(|t| (t.title.as_str(), t.description.as_str()))
            .unwrap_or(("unknown", ""));

        let ctx = AgentContext::review(id, title, desc);
        match agent::invoke_agent(
            AgentRole::Reviewer,
            &ctx,
            config,
        )
        .await
        {
            Ok(r) => {
                let exec = state.entry(id);
                match r.status {
                    AgentStatus::Success => {
                        exec.phase = Phase::Done;
                        exec.last_error = None;
                        progressed = true;
                        eprintln!(
                            "[ralph] {id} — done!"
                        );
                    }
                    AgentStatus::Failure { reason }
                    | AgentStatus::NeedsRetry { reason } => {
                        exec.phase = Phase::Pending;
                        exec.last_error =
                            Some(reason.clone());
                        progressed = true;
                        eprintln!(
                            "[ralph] {id} review issues: \
                             {reason}"
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[ralph] reviewer error for {id}: {e}"
                );
            }
        }
    }

    Ok(progressed)
}

/// Stage and commit any changes made during this iteration.
async fn git_commit_progress() -> Result<()> {
    let status = TokioCommand::new("git")
        .args(["status", "--porcelain"])
        .stdout(Stdio::piped())
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&status.stdout);
    if stdout.trim().is_empty() {
        return Ok(());
    }

    TokioCommand::new("git")
        .args(["add", "-A"])
        .status()
        .await
        .context("git add")?;

    TokioCommand::new("git")
        .args([
            "commit", "-m",
            "ralph: checkpoint progress",
        ])
        .status()
        .await
        .context("git commit")?;

    Ok(())
}
