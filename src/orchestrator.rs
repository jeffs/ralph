use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command as TokioCommand;

use crate::agent::{
    self, AgentContext, AgentRole, AgentStatus, FEEDBACK_MAX_LEN, truncate_feedback,
};
use crate::config::Config;
use crate::scheduler;
use crate::state::{ExecutionState, Phase};
use crate::task::{self, Task};

use crate::state::TaskExecution;

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

/// Record full agent response text as feedback so the
/// implementer can see what went wrong on the next attempt.
fn push_feedback(exec: &mut TaskExecution, phase_label: &str, full_text: &str) {
    let prefix = format!("[{phase_label} · attempt {}]", exec.attempts);
    let body = truncate_feedback(full_text, FEEDBACK_MAX_LEN);
    exec.feedback.push(format!("{prefix}\n{body}"));
}

/// Main orchestration loop. Iterates until convergence
/// (all tasks done + reviewer approves), stagnation
/// (max attempts exceeded), or iteration cap.
pub async fn run_loop(tasks_path: &Path, max_iterations: usize, config: &Config) -> Result<()> {
    let state_path = PathBuf::from(STATE_PATH);
    isolate_dirty_tree().await;
    cleanup_stale_workspaces().await;

    for iteration in 1..=max_iterations {
        eprintln!("\n[ralph] === iteration {iteration} ===");

        let tasks = task::load_tasks(tasks_path).await?;
        task::validate_deps(&tasks)?;
        let mut state = ExecutionState::load(&state_path).await?;
        let task_ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();

        // Check convergence: all done → final review
        if state.all_done(&task_ids) {
            eprintln!("[ralph] all tasks done, final review...");
            let review = agent::invoke_agent(
                AgentRole::Reviewer,
                &AgentContext::Review {
                    task_id: "final".to_string(),
                    task_title: "Final review".to_string(),
                    task_description: "Review the full project for \
                         correctness."
                        .to_string(),
                },
                config,
                None,
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
                AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
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
        let made_progress = resume_inflight(&tasks, &mut state, config).await?;
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
                run_group_with_workspaces(&group, &mut state, config).await?;
            } else {
                run_group_singleton(&group, &mut state, config).await?;
            }

            state.save(&state_path).await?;

            // Advance any tasks now at Testing/Reviewing
            resume_inflight(&tasks, &mut state, config).await?;
            state.save(&state_path).await?;

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
        match agent::invoke_agent(AgentRole::Tester, &ctx, config, None).await {
            Ok(r) => {
                let exec = state.entry(id);
                match r.status {
                    AgentStatus::Success => {
                        exec.phase = Phase::Reviewing;
                        exec.last_error = None;
                        progressed = true;
                    }
                    AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                        push_feedback(exec, "Tester", &r.text);
                        reset_or_fail(exec, config);
                        exec.last_error = Some(reason.clone());
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
                push_feedback(exec, "Tester", &e.to_string());
                reset_or_fail(exec, config);
                exec.last_error = Some(e.to_string());
                progressed = true;
                eprintln!("[ralph] tester error for {id}: {e}");
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
        match agent::invoke_agent(AgentRole::Reviewer, &ctx, config, None).await {
            Ok(r) => {
                let exec = state.entry(id);
                match r.status {
                    AgentStatus::Success => {
                        exec.phase = Phase::Done;
                        exec.last_error = None;
                        progressed = true;
                        eprintln!("[ralph] {id} — done!");
                    }
                    AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                        push_feedback(exec, "Reviewer", &r.text);
                        reset_or_fail(exec, config);
                        exec.last_error = Some(reason.clone());
                        progressed = true;
                        eprintln!(
                            "[ralph] {id} review issues: \
                             {reason}"
                        );
                    }
                }
            }
            Err(e) => {
                let exec = state.entry(id);
                push_feedback(exec, "Reviewer", &e.to_string());
                reset_or_fail(exec, config);
                exec.last_error = Some(e.to_string());
                progressed = true;
                eprintln!("[ralph] reviewer error for {id}: {e}");
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
                    Phase::Testing => "testing",
                    Phase::Reviewing => "reviewing",
                    Phase::Done => "done",
                    Phase::Failed => "failed",
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

        let id = t.id.clone();
        let title = t.title.clone();
        let desc = t.description.clone();
        let fb = state
            .tasks
            .get(&t.id)
            .and_then(|e| e.feedback.last())
            .cloned();
        let cfg = config.clone();
        handles.push(tokio::spawn(async move {
            let ctx = AgentContext::implement(&id, &title, &desc, fb.as_deref());
            let result =
                agent::invoke_agent(AgentRole::Implementer, &ctx, &cfg, Some(&ws_path)).await;
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
        let exec = state.entry(&id);
        exec.attempts += 1;
        let success = match result {
            Ok(r) => match &r.status {
                AgentStatus::Success => {
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
            },
            Err(e) => {
                reset_or_fail(exec, config);
                exec.last_error = Some(e.to_string());
                eprintln!("[ralph] agent error for {id}: {e}");
                false
            }
        };
        outcomes.push(Outcome { id, success });
    }

    // Merge successful workspaces, abandon failed ones
    for outcome in &outcomes {
        if !created_ws.contains(&outcome.id) {
            continue;
        }
        if outcome.success {
            // Attribute files precisely from workspace
            let rev = format!("ralph-{}@", outcome.id);
            let files = agent::jj_changed_files_for(&rev).await.unwrap_or_default();
            let exec = state.entry(&outcome.id);
            exec.files_changed.extend(files);
            exec.files_changed.sort();
            exec.files_changed.dedup();

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
) -> Result<()> {
    let t = group[0];
    let pre_files = agent::jj_changed_files().await.unwrap_or_default();

    let last_feedback = state
        .tasks
        .get(&t.id)
        .and_then(|e| e.feedback.last())
        .map(|s| s.as_str());
    let ctx = AgentContext::implement(&t.id, &t.title, &t.description, last_feedback);
    let result = agent::invoke_agent(AgentRole::Implementer, &ctx, config, None).await;

    let exec = state.entry(&t.id);
    exec.attempts += 1;
    match result {
        Ok(r) => match &r.status {
            AgentStatus::Success => {
                exec.phase = Phase::Testing;
                exec.last_error = None;
            }
            AgentStatus::Failure { reason } | AgentStatus::NeedsRetry { reason } => {
                reset_or_fail(exec, config);
                exec.last_error = Some(reason.clone());
                eprintln!("[ralph] {} implement failed: {reason}", t.id);
            }
        },
        Err(e) => {
            reset_or_fail(exec, config);
            exec.last_error = Some(e.to_string());
            eprintln!("[ralph] agent error for {}: {e}", t.id);
        }
    }

    // Attribute files via pre/post snapshot
    if exec.phase == Phase::Testing {
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
