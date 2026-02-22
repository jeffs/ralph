use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command as TokioCommand;

use crate::config::Config;

/// Tracks active child process group IDs so a centralized signal
/// handler can clean them all up on SIGINT / SIGTERM.
#[derive(Clone)]
pub struct ProcessRegistry {
    pgids: Arc<Mutex<HashSet<u32>>>,
    historical_pgids: Arc<Mutex<HashSet<u32>>>,
    shutdown: Arc<AtomicBool>,
    kill_grace_secs: u64,
}

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ProcessRegistry {
    pub fn new(kill_grace_secs: u64) -> Self {
        Self {
            pgids: Arc::default(),
            historical_pgids: Arc::default(),
            shutdown: Arc::default(),
            kill_grace_secs,
        }
    }

    pub fn register(&self, pgid: u32) {
        self.pgids.lock().expect("pgid lock").insert(pgid);
        self.historical_pgids
            .lock()
            .expect("historical pgid lock")
            .insert(pgid);
    }

    pub fn deregister(&self, pgid: u32) {
        self.pgids.lock().expect("pgid lock").remove(&pgid);
    }

    pub async fn kill_all(&self) {
        let pgids: Vec<u32> = self.pgids.lock().expect("pgid lock").drain().collect();
        for pgid in pgids {
            kill_process_group(pgid, self.kill_grace_secs).await;
        }
    }

    /// Check all historically-seen PGIDs. For any that are still alive
    /// but no longer active, send SIGTERM and log a warning.
    pub async fn audit_and_kill_orphans(&self) {
        let historical: Vec<u32> = self
            .historical_pgids
            .lock()
            .expect("historical pgid lock")
            .iter()
            .copied()
            .collect();

        for pgid in historical {
            let is_active = self.pgids.lock().expect("pgid lock").contains(&pgid);
            if is_active {
                continue;
            }

            // Check if the process group is still alive via `kill -0 -<pgid>`
            let alive = TokioCommand::new("kill")
                .args(["-0", &format!("-{pgid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);

            if alive {
                eprintln!(
                    "[ralph] WARNING: orphan process group {pgid} still alive, sending SIGTERM"
                );
                kill_process_group(pgid, self.kill_grace_secs).await;
                // Keep in historical set — next audit will re-check.
            } else {
                self.historical_pgids
                    .lock()
                    .expect("historical pgid lock")
                    .remove(&pgid);
            }
        }
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum AgentRole {
    Planner,
    Implementer,
    Tester,
    Reviewer,
    Triager,
}

impl AgentRole {
    fn prompt_filename(self) -> &'static str {
        match self {
            Self::Planner => "planner.md",
            Self::Implementer => "implementer.md",
            Self::Tester => "tester.md",
            Self::Reviewer => "reviewer.md",
            Self::Triager => "triager.md",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Implementer => "implementer",
            Self::Tester => "tester",
            Self::Reviewer => "reviewer",
            Self::Triager => "triager",
        }
    }
}

/// Context passed to an agent invocation. Each variant
/// carries only the data that role needs.
pub enum AgentContext {
    /// Planner: decompose this input into tasks
    Plan { input: String },
    /// Implementer: execute this task
    Implement {
        task_id: String,
        task_title: String,
        task_description: String,
        guidance: Option<String>,
        feedback: Option<String>,
    },
    /// Tester: validate recent changes
    Test {
        task_id: String,
        files_changed: Vec<PathBuf>,
    },
    /// Reviewer: review implementation against requirements
    Review {
        task_id: String,
        task_title: String,
        task_description: String,
        diff_summary: String,
        diff: String,
    },
    /// Triager: decide which nits to promote or dismiss
    Triage {
        nits_json: String,
        tasks_summary: String,
    },
}

impl AgentContext {
    pub fn plan(input: &str) -> Self {
        Self::Plan {
            input: input.to_string(),
        }
    }

    pub fn implement(
        id: &str,
        title: &str,
        description: &str,
        guidance: Option<&str>,
        feedback: Option<&str>,
    ) -> Self {
        Self::Implement {
            task_id: id.to_string(),
            task_title: title.to_string(),
            task_description: description.to_string(),
            guidance: guidance.map(String::from),
            feedback: feedback.map(String::from),
        }
    }

    pub fn test(id: &str, files: Vec<PathBuf>) -> Self {
        Self::Test {
            task_id: id.to_string(),
            files_changed: files,
        }
    }

    pub fn review(
        id: &str,
        title: &str,
        description: &str,
        diff_summary: String,
        diff: String,
    ) -> Self {
        Self::Review {
            task_id: id.to_string(),
            task_title: title.to_string(),
            task_description: description.to_string(),
            diff_summary,
            diff,
        }
    }

    pub fn triage(nits_json: String, tasks_summary: String) -> Self {
        Self::Triage {
            nits_json,
            tasks_summary,
        }
    }

    /// Return the task ID, if this context is task-scoped.
    fn task_id(&self) -> Option<&str> {
        match self {
            Self::Plan { .. } | Self::Triage { .. } => None,
            Self::Implement { task_id, .. }
            | Self::Test { task_id, .. }
            | Self::Review { task_id, .. } => Some(task_id),
        }
    }

    /// Render the context variables into the prompt template.
    fn interpolate(&self, template: &str) -> String {
        match self {
            Self::Plan { input } => template.replace("{{INPUT}}", input),
            Self::Implement {
                task_id,
                task_title,
                task_description,
                guidance,
                feedback,
            } => {
                let guidance_section = match guidance {
                    Some(g) => format!("\n## Guidance\n\n{g}\n"),
                    None => String::new(),
                };
                let feedback_section = match feedback {
                    Some(fb) => format!(
                        "\n## Previous Attempt Feedback\n\n\
                         The previous implementation attempt failed. \
                         Address the following issues:\n\n{fb}\n"
                    ),
                    None => String::new(),
                };
                template
                    .replace("{{TASK_ID}}", task_id)
                    .replace("{{TASK_TITLE}}", task_title)
                    .replace("{{TASK_DESCRIPTION}}", task_description)
                    .replace("{{GUIDANCE}}", &guidance_section)
                    .replace("{{FEEDBACK}}", &feedback_section)
            }
            Self::Test {
                task_id,
                files_changed,
            } => {
                let files_str = files_changed
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                template
                    .replace("{{TASK_ID}}", task_id)
                    .replace("{{FILES_CHANGED}}", &files_str)
            }
            Self::Review {
                task_id,
                task_title,
                task_description,
                diff_summary,
                diff,
            } => template
                .replace("{{TASK_ID}}", task_id)
                .replace("{{TASK_TITLE}}", task_title)
                .replace("{{TASK_DESCRIPTION}}", task_description)
                .replace("{{DIFF_SUMMARY}}", diff_summary)
                .replace("{{DIFF}}", diff),
            Self::Triage {
                nits_json,
                tasks_summary,
            } => template
                .replace("{{NITS}}", nits_json)
                .replace("{{TASKS_SUMMARY}}", tasks_summary),
        }
    }
}

/// The parsed result from a claude invocation.
#[derive(Debug)]
pub struct AgentResult {
    /// The full text of claude's response
    pub text: String,
    /// Self-reported status from the agent's JSON output
    pub status: AgentStatus,
    /// Cost reported by the claude CLI for this invocation
    pub cost_usd: Option<f64>,
    /// Raw stderr lines from the claude process (for diagnostics)
    pub stderr_lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Success,
    ApprovedWithNits { suggestions: String },
    Failure { reason: String },
    NeedsRetry { reason: String },
}

impl AgentResult {
    /// Extract JSONL content from the response text.
    /// Looks for lines that parse as JSON objects.
    pub fn extract_jsonl(&self) -> Option<String> {
        let lines: Vec<&str> = self
            .text
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                trimmed.starts_with('{')
                    && serde_json::from_str::<serde_json::Value>(trimmed).is_ok()
            })
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n") + "\n")
        }
    }

    /// Extract proposed new tasks from the agent's output.
    /// Looks for a `NEW_TASKS:` marker followed by JSONL lines.
    pub fn parse_new_tasks(&self) -> Vec<ProposedTask> {
        parse_proposed_tasks(&self.text)
    }
}

/// Parse proposed tasks from agent output text.
///
/// Looks for a line starting with `NEW_TASKS:`, then collects
/// subsequent lines that parse as `ProposedTask` JSON objects.
/// Stops at the first blank line or non-JSON line after the marker.
fn parse_proposed_tasks(text: &str) -> Vec<ProposedTask> {
    let mut tasks = Vec::new();
    let mut in_section = false;

    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("NEW_TASKS:") {
            in_section = true;
            // Check for inline JSON after the colon
            let after = trimmed.strip_prefix("NEW_TASKS:").unwrap_or("").trim();
            if after.starts_with('{')
                && let Ok(task) = serde_json::from_str::<ProposedTask>(after)
            {
                tasks.push(task);
            }
            continue;
        }

        if in_section {
            if trimmed.is_empty() {
                break;
            }
            if trimmed.starts_with('{')
                && let Ok(task) = serde_json::from_str::<ProposedTask>(trimmed)
            {
                tasks.push(task);
            }
            // Skip non-JSON lines (markdown fencing, etc.)
        }
    }

    tasks
}

/// A task proposed by an agent via the `NEW_TASKS:` protocol.
/// The `id` field is optional — the orchestrator generates one
/// if absent or colliding with an existing task.
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedTask {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub priority: Option<u32>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

/// Response shape from `claude -p --output-format json`
#[derive(Deserialize)]
struct ClaudeJsonOutput {
    result: Option<String>,
    total_cost_usd: Option<f64>,
}

/// Classification of failure types for smarter retry decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Agent exceeded wall-clock or idle timeout.
    Timeout,
    /// Compilation or build error in the output.
    BuildError,
    /// Tests failed.
    TestFailure,
    /// Reviewer rejected the implementation.
    ReviewRejection,
    /// Unclassified failure.
    Unknown,
}

/// Classify a failure reason into a FailureKind for retry decisions.
pub fn classify_failure(reason: &str) -> FailureKind {
    let lower = reason.to_lowercase();
    if lower.contains("timed out") || lower.contains("idle for") || lower.contains("stuck on") {
        FailureKind::Timeout
    } else if lower.contains("compile error")
        || lower.contains("build failed")
        || lower.contains("error[e")
        || lower.contains("cannot find")
    {
        FailureKind::BuildError
    } else if lower.contains("test failed")
        || lower.contains("tests failed")
        || lower.contains("assertion failed")
        || lower.contains("test failure")
    {
        FailureKind::TestFailure
    } else if lower.contains("review") || lower.contains("issues found") {
        FailureKind::ReviewRejection
    } else {
        FailureKind::Unknown
    }
}

/// Sample total CPU% for all processes in a process group via `ps`.
/// Returns 0.0 if the process group no longer exists or ps fails.
///
/// NOTE: `ps -g` selects by PGID on macOS/BSD but by session ID on
/// Linux (procps). If porting to Linux, use `ps -o pcpu= --pgroup <pgid>`
/// or read from /proc directly.
pub async fn sample_pgid_cpu(pgid: u32) -> f64 {
    let output = TokioCommand::new("ps")
        .args(["-o", "pcpu=", "-g", &pgid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;

    let output = match output {
        Ok(o) => o,
        Err(_) => return 0.0,
    };

    parse_pcpu_output(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the output of `ps -o pcpu= -g <pgid>` into a summed CPU%.
fn parse_pcpu_output(output: &str) -> f64 {
    output
        .lines()
        .filter_map(|line| line.trim().parse::<f64>().ok())
        .sum()
}

/// Send SIGTERM to a process group, wait up to `grace_secs`,
/// then escalate to SIGKILL if still alive.
pub(crate) async fn kill_process_group(pgid: u32, grace_secs: u64) {
    // Send SIGTERM first.
    let _ = TokioCommand::new("kill")
        .args(["-TERM", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    if grace_secs == 0 {
        // Immediate SIGKILL.
        let _ = TokioCommand::new("kill")
            .args(["-KILL", &format!("-{pgid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        return;
    }

    // Wait for grace period, then SIGKILL if still alive.
    tokio::time::sleep(Duration::from_secs(grace_secs)).await;
    let alive = TokioCommand::new("kill")
        .args(["-0", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);

    if alive {
        eprintln!(
            "[ralph] process group {pgid} still alive after \
             {grace_secs}s grace, sending SIGKILL"
        );
        let _ = TokioCommand::new("kill")
            .args(["-KILL", &format!("-{pgid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

/// Poll interval for the idle-monitoring loop.
const POLL_INTERVAL_SECS: u64 = 30;
/// CPU% threshold below which a process group is considered idle.
const IDLE_CPU_THRESHOLD: f64 = 1.0;

/// Grace period after a stuck-pattern is detected before killing the agent.
const STUCK_GRACE_SECS: u64 = 60;

/// Shared liveness state between the stderr reader and the monitor loop.
/// Carries the stuck-on-lock flag and a timestamp of the last stderr line,
/// so the monitor can distinguish "idle waiting for API" from "truly stuck".
struct AgentLiveness {
    stuck: AtomicBool,
    last_stderr_at: Mutex<Instant>,
}

/// Monitor a running child process for idleness, stuck patterns, and hard timeout.
///
/// Polls `child.wait()` in 30-second intervals. On each timeout (child still
/// running), samples the process group's CPU usage. If cumulative idle time
/// exceeds `agent_idle_timeout_secs`, kills the group and returns a Failure
/// result. Enforces `agent_timeout_secs` as the hard ceiling.
///
/// Stderr activity (tracked via `liveness.last_stderr_at`) resets the idle
/// counter, preventing false kills when the CLI is waiting for an API response.
async fn monitor_agent(
    mut child: tokio::process::Child,
    pgid: u32,
    agent_timeout_secs: u64,
    agent_idle_timeout_secs: u64,
    liveness: Arc<AgentLiveness>,
) -> Result<std::process::ExitStatus, AgentStatus> {
    let started = Instant::now();
    let hard_limit = Duration::from_secs(agent_timeout_secs);
    let idle_limit = Duration::from_secs(agent_idle_timeout_secs);
    let poll_interval = Duration::from_secs(POLL_INTERVAL_SECS);
    let stuck_grace = Duration::from_secs(STUCK_GRACE_SECS);

    let mut idle_duration = Duration::ZERO;
    let mut stuck_since: Option<Instant> = None;

    loop {
        let elapsed = started.elapsed();
        if elapsed >= hard_limit {
            kill_process_group(pgid, 0).await;
            return Err(AgentStatus::Failure {
                reason: format!("agent timed out after {agent_timeout_secs}s"),
            });
        }

        // Check stuck flag.
        if liveness.stuck.load(Ordering::Acquire) {
            let since = stuck_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= stuck_grace {
                kill_process_group(pgid, 0).await;
                return Err(AgentStatus::Failure {
                    reason: format!(
                        "agent stuck on file lock for {}s",
                        since.elapsed().as_secs()
                    ),
                });
            }
        }

        // Remaining time before the hard limit.
        let remaining = hard_limit - elapsed;
        let poll_timeout = poll_interval.min(remaining);

        match tokio::time::timeout(poll_timeout, child.wait()).await {
            Ok(Ok(status)) => return Ok(status),
            Ok(Err(e)) => {
                // IO error waiting for child
                kill_process_group(pgid, 0).await;
                return Err(AgentStatus::Failure {
                    reason: format!("error waiting for agent process: {e}"),
                });
            }
            Err(_poll_timeout_elapsed) => {
                // Child still running — sample CPU.
                let cpu = sample_pgid_cpu(pgid).await;
                if cpu < IDLE_CPU_THRESHOLD {
                    // CPU is low, but check if stderr was active recently.
                    // The CLI writes progress to stderr while waiting for
                    // the API, so recent stderr activity means the agent
                    // is alive — just server-side.
                    let stderr_recent = {
                        let last = liveness.last_stderr_at.lock().expect("liveness lock");
                        last.elapsed() < poll_timeout
                    };
                    if stderr_recent {
                        idle_duration = Duration::ZERO;
                    } else {
                        idle_duration += poll_timeout;
                        if idle_duration >= idle_limit {
                            kill_process_group(pgid, 0).await;
                            return Err(AgentStatus::Failure {
                                reason: format!(
                                    "agent idle for {}s (possible deadlock)",
                                    idle_duration.as_secs()
                                ),
                            });
                        }
                    }
                } else {
                    // Active — reset idle counter and stuck grace timer.
                    idle_duration = Duration::ZERO;
                    stuck_since = None;
                }
            }
        }
    }
}

/// Invoke a claude agent with the given role and context.
/// When `working_dir` is `Some`, the subprocess runs in that directory.
///
/// The caller must install a centralized signal handler via
/// [`ProcessRegistry`]; this function registers/deregisters the child
/// process group but does not handle signals itself.
pub async fn invoke_agent(
    role: AgentRole,
    context: &AgentContext,
    config: &Config,
    working_dir: Option<&Path>,
    registry: &ProcessRegistry,
    attempt: u32,
) -> Result<AgentResult> {
    // Load and interpolate prompt
    let prompt_path = config.prompts_dir.join(role.prompt_filename());
    let template = tokio::fs::read_to_string(&prompt_path)
        .await
        .with_context(|| format!("reading prompt template: {}", prompt_path.display()))?;
    let prompt = context.interpolate(&template);

    let model = config.model_for_attempt(role.label(), attempt);
    match context.task_id() {
        Some(id) => eprintln!(
            "[ralph] {id} — invoking {} agent (model: {model})...",
            role.label()
        ),
        None => eprintln!(
            "[ralph] invoking {} agent (model: {model})...",
            role.label()
        ),
    }

    // Spawn claude in its own process group so that child
    // processes (rust-analyzer, LSP servers, cargo) are
    // cleaned up when the agent finishes, rather than being
    // reparented to PID 1 as orphans.
    let mut cmd = TokioCommand::new("claude");
    cmd.env_clear();
    // Forward essential vars if present in the current environment.
    for key in &[
        "HOME",
        "USER",
        "SHELL",
        "PATH",
        "LANG",
        "TERM",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "SSH_AUTH_SOCK",
    ] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    // Forward any additional vars from config.env.passthrough.
    for key in &config.env.passthrough {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    // Apply explicit overrides from config.env.set.
    for (key, val) in &config.env.set {
        cmd.env(key, val);
    }
    // Apply workspace environment isolation: compute absolute paths
    // from the agent's working directory so each workspace gets its
    // own isolated directories (e.g. CARGO_TARGET_DIR).
    if let Some(dir) = working_dir {
        for (var_name, subdir) in &config.workspace.isolate_env {
            let path = dir.join(subdir);
            cmd.env(var_name, &path);
        }
    }
    cmd.arg("-p")
        .arg(&prompt)
        .arg("--output-format")
        .arg("json")
        .arg("--model")
        .arg(model)
        .arg("--dangerously-skip-permissions")
        .arg("--no-session-persistence")
        .arg("--strict-mcp-config")
        .arg("--mcp-config")
        .arg(r#"{"mcpServers":{}}"#)
        .arg("--settings")
        .arg(r#"{"enabledPlugins":{"rust-analyzer-lsp@claude-plugins-official":false,"typescript-lsp@claude-plugins-official":false,"pyright-lsp@claude-plugins-official":false}}"#)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    cmd.process_group(0);
    let mut child = cmd.spawn().context("spawning claude process")?;
    let child_pid = child.id().expect("child has pid immediately after spawn");

    // Take stderr and stdout handles before passing the child to the monitor.
    let stderr_handle = child.stderr.take().expect("stderr is piped");
    let stdout_handle = child.stdout.take().expect("stdout is piped");

    // Spawn a task to drain stdout concurrently, avoiding pipe-buffer deadlock.
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut reader = stdout_handle;
        let _ = reader.read_to_end(&mut buf).await;
        buf
    });

    // Spawn a task to stream stderr in real-time.
    let role_label = role.label();
    let stuck_patterns = config.stuck_patterns.clone();
    let liveness = Arc::new(AgentLiveness {
        stuck: AtomicBool::new(false),
        last_stderr_at: Mutex::new(Instant::now()),
    });
    let liveness_clone = Arc::clone(&liveness);
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr_handle).lines();
        let mut collected: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[ralph] {role_label}: {line}");
            // Update liveness timestamp on every line — proves the
            // agent is actively communicating, even if CPU is low.
            *liveness_clone.last_stderr_at.lock().expect("liveness lock") = Instant::now();
            if stuck_patterns.iter().any(|pat| line.contains(pat)) {
                liveness_clone.stuck.store(true, Ordering::Release);
            }
            collected.push(line);
        }
        collected
    });

    registry.register(child_pid);
    let monitor_result = monitor_agent(
        child,
        child_pid,
        config.agent_timeout_secs,
        config.agent_idle_timeout_secs,
        liveness,
    )
    .await;

    // Always join both IO tasks, regardless of monitor outcome.
    // This ensures we capture partial output even on kill/timeout.
    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_lines = stderr_task.await.unwrap_or_default();

    /// Parse stdout bytes into (text, cost_usd) via the JSON envelope,
    /// falling back to raw text if JSON parsing fails.
    fn parse_stdout(bytes: &[u8]) -> (String, Option<f64>) {
        let raw = String::from_utf8_lossy(bytes);
        match serde_json::from_str::<ClaudeJsonOutput>(&raw) {
            Ok(parsed) => (parsed.result.unwrap_or_default(), parsed.total_cost_usd),
            Err(_) => (raw.into_owned(), None),
        }
    }

    match monitor_result {
        Ok(exit_status) => {
            kill_process_group(child_pid, 0).await;
            registry.deregister(child_pid);

            if !exit_status.success() {
                let (text, cost_usd) = parse_stdout(&stdout_bytes);
                return Ok(AgentResult {
                    text,
                    status: AgentStatus::Failure {
                        reason: format!("claude exited with {}", exit_status),
                    },
                    cost_usd,
                    stderr_lines,
                });
            }

            let (text, cost_usd) = parse_stdout(&stdout_bytes);
            if let Some(cost) = cost_usd {
                eprintln!("[ralph] {} agent cost: ${cost:.4}", role.label());
            }

            let status = parse_agent_status(&text);
            Ok(AgentResult {
                text,
                status,
                cost_usd,
                stderr_lines,
            })
        }
        Err(status) => {
            // kill already happened in monitor_agent
            eprintln!("[ralph] {} agent stopped: {:?}", role.label(), status);
            registry.deregister(child_pid);

            let (text, cost_usd) = parse_stdout(&stdout_bytes);
            Ok(AgentResult {
                text,
                status,
                cost_usd,
                stderr_lines,
            })
        }
    }
}

/// Parse a numbered-list failure reason into individual proposed tasks.
///
/// Recognizes patterns like "1. description" or "2) description" and
/// creates one [`ProposedTask`] per item. Used as a fallback when an
/// agent returns a failure reason without structured `NEW_TASKS:` output.
pub fn tasks_from_numbered_list(reason: &str) -> Vec<ProposedTask> {
    let mut tasks = Vec::new();
    for line in reason.lines() {
        let trimmed = line.trim();
        if let Some(text) = strip_numbered_prefix(trimmed)
            && !text.is_empty()
        {
            tasks.push(ProposedTask {
                id: None,
                title: text.to_string(),
                description: String::new(),
                priority: None,
                blocked_by: vec![],
            });
        }
    }
    tasks
}

/// Strip a leading numbered prefix like "1. " or "2) " from a line.
fn strip_numbered_prefix(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
    if rest.len() == line.trim().len() {
        return None; // No leading digits
    }
    rest.strip_prefix('.')
        .or_else(|| rest.strip_prefix(')'))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Maximum length for captured reason text.
const REASON_MAX_LEN: usize = 2000;

/// Parse the agent's self-reported status. Agents are
/// instructed to include a status line like:
///   STATUS: SUCCESS
///   STATUS: FAILURE: reason
///   STATUS: NEEDS_RETRY: reason
///
/// When the reason after FAILURE:/NEEDS_RETRY: is empty,
/// subsequent non-empty lines are captured as the reason.
fn parse_agent_status(text: &str) -> AgentStatus {
    let lines: Vec<&str> = text.lines().collect();

    // Scan from the end to find the last STATUS line.
    for (idx, line) in lines.iter().enumerate().rev() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("STATUS:") else {
            continue;
        };
        let rest = rest.trim();

        if rest.starts_with("SUCCESS") {
            return AgentStatus::Success;
        }

        // Try to match APPROVED_WITH_NITS:, FAILURE:, or NEEDS_RETRY: and collect the reason.
        let (prefix, make_status): (&str, fn(String) -> AgentStatus) =
            if let Some(r) = rest.strip_prefix("APPROVED_WITH_NITS:") {
                (r, |suggestions| AgentStatus::ApprovedWithNits {
                    suggestions,
                })
            } else if let Some(r) = rest.strip_prefix("FAILURE:") {
                (r, |reason| AgentStatus::Failure { reason })
            } else if let Some(r) = rest.strip_prefix("NEEDS_RETRY:") {
                (r, |reason| AgentStatus::NeedsRetry { reason })
            } else {
                continue;
            };

        let reason = collect_reason(prefix, &lines[idx + 1..]);
        return make_status(reason);
    }

    // No status line found — don't assume success.
    AgentStatus::NeedsRetry {
        reason: "no STATUS line in agent output".to_string(),
    }
}

/// Build a reason string from the inline text after the status prefix
/// and, if that's empty, from subsequent non-empty lines.
fn collect_reason(inline: &str, trailing_lines: &[&str]) -> String {
    let inline = inline.trim();

    if !inline.is_empty() {
        // Inline reason present — append any trailing lines too.
        let mut reason = inline.to_string();
        for line in trailing_lines {
            let t = line.trim();
            if t.is_empty() {
                break;
            }
            reason.push('\n');
            reason.push_str(t);
            if reason.len() >= REASON_MAX_LEN {
                break;
            }
        }
        truncate_reason(reason)
    } else {
        // No inline reason — gather trailing lines.
        let mut parts: Vec<&str> = Vec::new();
        let mut len = 0;
        for line in trailing_lines {
            let t = line.trim();
            if t.is_empty() {
                break;
            }
            parts.push(t);
            len += t.len() + 1; // +1 for newline
            if len >= REASON_MAX_LEN {
                break;
            }
        }
        if parts.is_empty() {
            "no reason provided".to_string()
        } else {
            truncate_reason(parts.join("\n"))
        }
    }
}

fn truncate_reason(s: String) -> String {
    if s.len() <= REASON_MAX_LEN {
        s
    } else {
        // Truncate at a char boundary.
        let mut end = REASON_MAX_LEN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut truncated = s[..end].to_string();
        truncated.push_str("...");
        truncated
    }
}

/// Maximum length for feedback text forwarded to the implementer.
pub(crate) const FEEDBACK_MAX_LEN: usize = 16_000;

/// Truncate feedback text at a char boundary, appending "..." if needed.
pub(crate) fn truncate_feedback(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.to_string();
    }
    let mut end = max_len;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = text[..end].to_string();
    truncated.push_str("...");
    truncated
}

/// Build a combined feedback string from all entries, with budget-based
/// truncation. Keeps the last 2 entries in full; older entries share the
/// remaining budget equally and are truncated if necessary.
pub(crate) fn build_feedback_history(entries: &[String], max_len: usize) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let sep = "\n\n";

    // Fast path: everything fits.
    let total: usize = entries.iter().map(|e| e.len()).sum::<usize>()
        + sep.len() * entries.len().saturating_sub(1);
    if total <= max_len {
        return entries.join(sep);
    }

    // Preserve the last 2 entries in full; truncate older entries to fit.
    let keep = entries.len().min(2);
    let (older, recent) = entries.split_at(entries.len() - keep);
    let recent_block = recent.join(sep);

    if older.is_empty() {
        return recent_block;
    }

    // Budget for older entries (one separator between older and recent blocks).
    let fixed = recent_block.len() + sep.len();
    if fixed >= max_len {
        return recent_block;
    }
    let budget = max_len - fixed;

    // Separators within the older block.
    let older_seps = sep.len() * older.len().saturating_sub(1);
    if budget <= older_seps {
        return recent_block;
    }
    let text_budget = (budget - older_seps) / older.len();
    // truncate_feedback appends "..." (3 chars) when it truncates, so
    // reserve space for the suffix to stay within budget.
    let per_entry = text_budget.saturating_sub(3);

    let older_block: String = older
        .iter()
        .map(|e| truncate_feedback(e, per_entry))
        .collect::<Vec<_>>()
        .join(sep);

    format!("{older_block}{sep}{recent_block}")
}

/// Parse `jj diff --summary` output into a sorted, deduped
/// list of file paths.
fn parse_diff_summary(output: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = output
        .lines()
        .filter_map(|l| {
            l.split_once(' ')
                .map(|(_, path)| PathBuf::from(path.trim()))
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Get all files changed in the working-copy commit relative
/// to its parent. Covers modifications, additions, and
/// deletions in a single `jj diff --summary` call.
pub async fn jj_changed_files() -> Result<Vec<PathBuf>> {
    let output = TokioCommand::new("jj")
        .args(["diff", "--summary"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    Ok(parse_diff_summary(&String::from_utf8_lossy(&output.stdout)))
}

/// Get the full git-format diff of the working-copy commit.
pub async fn jj_diff_git() -> Result<String> {
    let output = TokioCommand::new("jj")
        .args(["diff", "--git"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Get files changed in a specific revision (e.g. a workspace's
/// working-copy commit viewed from the default workspace via
/// `ralph-{id}@`).
pub async fn jj_changed_files_for(revision: &str) -> Result<Vec<PathBuf>> {
    let output = TokioCommand::new("jj")
        .args(["diff", "--summary", "-r", revision])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    Ok(parse_diff_summary(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    #[test]
    fn parse_pcpu_output_sums_values() {
        // Normal case: multiple processes with CPU usage
        let output = "  1.5\n  2.3\n  0.0\n";
        let total = parse_pcpu_output(output);
        assert!((total - 3.8).abs() < 0.001, "expected 3.8, got {total}");
    }

    #[test]
    fn parse_pcpu_output_empty() {
        // Process group gone / no output
        let total = parse_pcpu_output("");
        assert_eq!(total, 0.0);
    }

    #[test]
    fn parse_pcpu_output_ignores_non_numeric_lines() {
        // Some ps implementations emit headers despite -o pcpu=; skip them.
        let output = "%CPU\n 0.5\n 1.2\n";
        let total = parse_pcpu_output(output);
        assert!((total - 1.7).abs() < 0.001, "expected 1.7, got {total}");
    }

    #[test]
    fn parse_pcpu_output_single_process_idle() {
        let output = " 0.0\n";
        let total = parse_pcpu_output(output);
        assert!(total < IDLE_CPU_THRESHOLD);
    }

    #[test]
    fn parse_status_success() {
        let text = "Did some work.\nSTATUS: SUCCESS\n";
        assert_eq!(parse_agent_status(text), AgentStatus::Success);
    }

    #[test]
    fn parse_status_failure() {
        let text = "STATUS: FAILURE: compile error";
        assert_eq!(
            parse_agent_status(text),
            AgentStatus::Failure {
                reason: "compile error".to_string()
            }
        );
    }

    #[test]
    fn parse_status_retry() {
        let text = "STATUS: NEEDS_RETRY: test flake";
        assert_eq!(
            parse_agent_status(text),
            AgentStatus::NeedsRetry {
                reason: "test flake".to_string()
            }
        );
    }

    #[test]
    fn parse_status_missing_defaults_needs_retry() {
        let text = "Just some output with no status line.";
        match parse_agent_status(text) {
            AgentStatus::NeedsRetry { reason } => {
                assert_eq!(reason, "no STATUS line in agent output");
            }
            other => panic!("expected NeedsRetry, got {:?}", other),
        }
    }

    #[test]
    fn parse_status_failure_multiline() {
        let text = "Review complete.\nSTATUS: FAILURE:\n1. Missing error handling\n2. No tests\n3. Unused import\n";
        match parse_agent_status(text) {
            AgentStatus::Failure { reason } => {
                assert_eq!(
                    reason,
                    "1. Missing error handling\n2. No tests\n3. Unused import"
                );
            }
            other => panic!("expected Failure, got {:?}", other),
        }
    }

    #[test]
    fn parse_status_failure_multiline_with_inline_preamble() {
        let text = "STATUS: FAILURE: issues found\n1. Thing one\n2. Thing two\n";
        match parse_agent_status(text) {
            AgentStatus::Failure { reason } => {
                assert_eq!(reason, "issues found\n1. Thing one\n2. Thing two");
            }
            other => panic!("expected Failure, got {:?}", other),
        }
    }

    #[test]
    fn parse_status_failure_empty_no_trailing_lines() {
        let text = "STATUS: FAILURE:";
        match parse_agent_status(text) {
            AgentStatus::Failure { reason } => {
                assert_eq!(reason, "no reason provided");
            }
            other => panic!("expected Failure, got {:?}", other),
        }
    }

    #[test]
    fn parse_status_failure_empty_only_blank_trailing() {
        let text = "STATUS: FAILURE:\n\n\n";
        match parse_agent_status(text) {
            AgentStatus::Failure { reason } => {
                assert_eq!(reason, "no reason provided");
            }
            other => panic!("expected Failure, got {:?}", other),
        }
    }

    #[test]
    fn parse_status_reason_truncated() {
        // Build a reason that exceeds REASON_MAX_LEN
        let long_line = "x".repeat(REASON_MAX_LEN + 500);
        let text = format!("STATUS: FAILURE: {}", long_line);
        match parse_agent_status(&text) {
            AgentStatus::Failure { reason } => {
                assert!(reason.len() <= REASON_MAX_LEN + 3); // +3 for "..."
                assert!(reason.ends_with("..."));
            }
            other => panic!("expected Failure, got {:?}", other),
        }
    }

    #[test]
    fn parse_status_needs_retry_multiline() {
        let text = "STATUS: NEEDS_RETRY:\nflaky network\ntimeout on port 443\n";
        match parse_agent_status(text) {
            AgentStatus::NeedsRetry { reason } => {
                assert_eq!(reason, "flaky network\ntimeout on port 443");
            }
            other => panic!("expected NeedsRetry, got {:?}", other),
        }
    }

    #[test]
    fn extract_jsonl_from_text() {
        let result = AgentResult {
            text: r#"Here are the tasks:
{"id":"T1","title":"Do thing","priority":1}
{"id":"T2","title":"Other","priority":2,"blocked_by":["T1"]}
Done!"#
                .to_string(),
            status: AgentStatus::Success,
            cost_usd: None,
            stderr_lines: vec![],
        };
        let jsonl = result.extract_jsonl().unwrap();
        assert!(jsonl.contains("T1"));
        assert!(jsonl.contains("T2"));
    }

    #[test]
    fn interpolate_plan() {
        let ctx = AgentContext::plan("add auth");
        let result = ctx.interpolate("Do: {{INPUT}}");
        assert_eq!(result, "Do: add auth");
    }

    #[test]
    fn interpolate_implement() {
        let ctx = AgentContext::implement("T1", "Fix bug", "desc", None, None);
        let result = ctx.interpolate(
            "Task {{TASK_ID}}: {{TASK_TITLE}} — {{TASK_DESCRIPTION}}{{GUIDANCE}}{{FEEDBACK}}",
        );
        assert_eq!(result, "Task T1: Fix bug — desc");
    }

    #[test]
    fn interpolate_implement_with_feedback() {
        let ctx = AgentContext::implement(
            "T1",
            "Fix bug",
            "desc",
            None,
            Some("compile error on line 5"),
        );
        let result = ctx.interpolate("{{TASK_ID}}{{GUIDANCE}}{{FEEDBACK}}## Instructions");
        assert!(result.contains("## Previous Attempt Feedback"));
        assert!(result.contains("compile error on line 5"));
        assert!(result.contains("## Instructions"));
    }

    #[test]
    fn interpolate_implement_with_guidance() {
        let ctx = AgentContext::implement(
            "T1",
            "Fix bug",
            "desc",
            Some("- Check the uuid feature flags"),
            None,
        );
        let result = ctx.interpolate("{{TASK_ID}}{{GUIDANCE}}{{FEEDBACK}}## Instructions");
        assert!(result.contains("## Guidance"));
        assert!(result.contains("Check the uuid feature flags"));
        assert!(result.contains("## Instructions"));
    }

    #[test]
    fn interpolate_implement_with_guidance_and_feedback() {
        let ctx = AgentContext::implement(
            "T1",
            "Fix bug",
            "desc",
            Some("- Fix the root cause"),
            Some("compile error on line 5"),
        );
        let result = ctx.interpolate("{{TASK_ID}}{{GUIDANCE}}{{FEEDBACK}}## Instructions");
        // Guidance comes before feedback
        let guidance_pos = result.find("## Guidance").unwrap();
        let feedback_pos = result.find("## Previous Attempt Feedback").unwrap();
        assert!(guidance_pos < feedback_pos);
    }

    #[test]
    fn interpolate_triage() {
        let ctx = AgentContext::triage(
            r#"{"id":"NIT-1","content":"rename foo"}"#.to_string(),
            "[T1] Do thing (Done)".to_string(),
        );
        let result = ctx.interpolate("Nits:\n{{NITS}}\n\nTasks:\n{{TASKS_SUMMARY}}");
        assert!(result.contains("NIT-1"));
        assert!(result.contains("[T1] Do thing (Done)"));
    }

    #[test]
    fn truncate_feedback_within_limit() {
        let text = "short";
        assert_eq!(truncate_feedback(text, 100), "short");
    }

    #[test]
    fn truncate_feedback_at_boundary() {
        let text = "a".repeat(100);
        let result = truncate_feedback(&text, 50);
        assert_eq!(result.len(), 53); // 50 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_feedback_respects_char_boundary() {
        // 'é' is 2 bytes in UTF-8
        let text = "é".repeat(100); // 200 bytes
        let result = truncate_feedback(&text, 51); // would split 'é' at byte 51
        assert!(result.ends_with("..."));
        // Must not panic or produce invalid UTF-8
        assert!(result.len() <= 54); // 50 (rounded down) + "..."
    }

    #[test]
    fn stuck_patterns_match_known_lines() {
        let patterns = config::default_stuck_patterns();
        let matching = [
            "Blocking waiting for file lock on package cache",
            "warning: waiting for lock on build directory",
            "  waiting for lock...",
        ];
        for line in &matching {
            assert!(
                patterns.iter().any(|pat| line.contains(pat.as_str())),
                "expected pattern match for: {line}"
            );
        }
    }

    #[test]
    fn stuck_patterns_do_not_match_unrelated_lines() {
        let patterns = config::default_stuck_patterns();
        let non_matching = [
            "Compiling foo v0.1.0",
            "error[E0308]: mismatched types",
            "warning: unused variable `x`",
            "Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s",
        ];
        for line in &non_matching {
            assert!(
                !patterns.iter().any(|pat| line.contains(pat.as_str())),
                "unexpected pattern match for: {line}"
            );
        }
    }

    #[test]
    fn classify_timeout() {
        assert_eq!(
            classify_failure("agent timed out after 1800s"),
            FailureKind::Timeout
        );
        assert_eq!(
            classify_failure("agent idle for 180s (possible deadlock)"),
            FailureKind::Timeout
        );
        assert_eq!(
            classify_failure("agent stuck on file lock for 60s"),
            FailureKind::Timeout
        );
    }

    #[test]
    fn classify_build_error() {
        assert_eq!(
            classify_failure("compile error in src/main.rs"),
            FailureKind::BuildError
        );
        assert_eq!(
            classify_failure("error[E0308]: mismatched types"),
            FailureKind::BuildError
        );
    }

    #[test]
    fn classify_test_failure() {
        assert_eq!(
            classify_failure("tests failed: 2 passed, 1 failed"),
            FailureKind::TestFailure
        );
        assert_eq!(
            classify_failure("assertion failed: expected 3, got 4"),
            FailureKind::TestFailure
        );
    }

    #[test]
    fn classify_review_rejection() {
        assert_eq!(
            classify_failure("review: issues found in implementation"),
            FailureKind::ReviewRejection
        );
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(
            classify_failure("something unexpected happened"),
            FailureKind::Unknown
        );
    }

    #[test]
    fn parse_new_tasks_basic() {
        let text = r#"Here are the issues.
NEW_TASKS:
{"title":"Fix widget config","description":"Move to TOML","priority":1}
{"title":"Add NumberInput","priority":2}

STATUS: FAILURE: issues found"#;
        let tasks = parse_proposed_tasks(text);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title, "Fix widget config");
        assert_eq!(tasks[0].priority, Some(1));
        assert_eq!(tasks[1].title, "Add NumberInput");
        assert!(tasks[0].id.is_none());
    }

    #[test]
    fn parse_new_tasks_with_id() {
        let text = "NEW_TASKS:\n{\"id\":\"FIX-1\",\"title\":\"Fix it\"}\n";
        let tasks = parse_proposed_tasks(text);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id.as_deref(), Some("FIX-1"));
    }

    #[test]
    fn parse_new_tasks_empty_when_absent() {
        let text = "Did some work.\nSTATUS: SUCCESS\n";
        let tasks = parse_proposed_tasks(text);
        assert!(tasks.is_empty());
    }

    #[test]
    fn parse_new_tasks_stops_at_blank_line() {
        let text = "NEW_TASKS:\n{\"title\":\"A\"}\n\n{\"title\":\"B\"}\n";
        let tasks = parse_proposed_tasks(text);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "A");
    }

    #[test]
    fn parse_new_tasks_skips_non_json() {
        let text = "NEW_TASKS:\n```json\n{\"title\":\"A\"}\n```\n";
        let tasks = parse_proposed_tasks(text);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "A");
    }

    #[test]
    fn tasks_from_numbered_list_basic() {
        let reason =
            "1. Widget config in wrong place\n2. Missing NumberInput\n3. Wrong field names";
        let tasks = tasks_from_numbered_list(reason);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].title, "Widget config in wrong place");
        assert_eq!(tasks[1].title, "Missing NumberInput");
        assert_eq!(tasks[2].title, "Wrong field names");
    }

    #[test]
    fn tasks_from_numbered_list_handles_parens() {
        let reason = "1) First issue\n2) Second issue";
        let tasks = tasks_from_numbered_list(reason);
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn tasks_from_numbered_list_ignores_non_numbered() {
        let reason = "Some preamble\n1. Actual issue\nMore text";
        let tasks = tasks_from_numbered_list(reason);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Actual issue");
    }

    #[test]
    fn strip_numbered_prefix_basic() {
        assert_eq!(strip_numbered_prefix("1. hello"), Some("hello"));
        assert_eq!(
            strip_numbered_prefix("12. multi-digit"),
            Some("multi-digit")
        );
        assert_eq!(strip_numbered_prefix("3) paren style"), Some("paren style"));
        assert_eq!(strip_numbered_prefix("no number"), None);
        assert_eq!(strip_numbered_prefix("1."), None); // empty after strip
    }

    #[test]
    fn parse_claude_json_with_cost() {
        let json = r#"{"result":"hello","total_cost_usd":0.0042}"#;
        let parsed: ClaudeJsonOutput = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.result.as_deref(), Some("hello"));
        assert!((parsed.total_cost_usd.unwrap() - 0.0042).abs() < 1e-9);
    }

    #[test]
    fn parse_claude_json_without_cost() {
        let json = r#"{"result":"hello"}"#;
        let parsed: ClaudeJsonOutput = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.result.as_deref(), Some("hello"));
        assert!(parsed.total_cost_usd.is_none());
    }

    #[test]
    fn parse_status_approved_with_nits() {
        let text = "Looks good overall.\nSTATUS: APPROVED_WITH_NITS: consider renaming foo to bar";
        assert_eq!(
            parse_agent_status(text),
            AgentStatus::ApprovedWithNits {
                suggestions: "consider renaming foo to bar".to_string()
            }
        );
    }

    #[test]
    fn parse_status_approved_with_nits_multiline() {
        let text = "STATUS: APPROVED_WITH_NITS:\n1. rename foo\n2. add docstring\n";
        match parse_agent_status(text) {
            AgentStatus::ApprovedWithNits { suggestions } => {
                assert_eq!(suggestions, "1. rename foo\n2. add docstring");
            }
            other => panic!("expected ApprovedWithNits, got {:?}", other),
        }
    }

    #[test]
    fn build_feedback_history_empty() {
        assert_eq!(build_feedback_history(&[], 1000), "");
    }

    #[test]
    fn build_feedback_history_single_entry() {
        let entries = vec!["[Tester · attempt 1] error".to_string()];
        assert_eq!(
            build_feedback_history(&entries, 1000),
            "[Tester · attempt 1] error"
        );
    }

    #[test]
    fn build_feedback_history_all_fit() {
        let entries = vec![
            "[Tester · attempt 1] error A".to_string(),
            "[Reviewer · attempt 2] error B".to_string(),
        ];
        let result = build_feedback_history(&entries, 10000);
        assert!(result.contains("error A"));
        assert!(result.contains("error B"));
        assert!(result.contains("\n\n"));
    }

    #[test]
    fn build_feedback_history_truncates_older_entries() {
        let old = format!("[Tester · attempt 1] {}", "x".repeat(5000));
        let recent1 = "[Tester · attempt 2] recent1".to_string();
        let recent2 = "[Reviewer · attempt 2] recent2".to_string();
        let entries = vec![old.clone(), recent1.clone(), recent2.clone()];

        // Budget large enough for the recent entries but not the old one in full.
        let budget = recent1.len() + recent2.len() + 200;
        let result = build_feedback_history(&entries, budget);
        // Last 2 entries preserved in full.
        assert!(result.contains(&recent1));
        assert!(result.contains(&recent2));
        // Older entry was truncated (not present in full).
        assert!(!result.contains(&old));
        assert!(result.len() <= budget);
    }

    #[test]
    fn build_feedback_history_recent_exceeds_budget() {
        let old = "old entry".to_string();
        let recent1 = format!("[Tester · attempt 2] {}", "x".repeat(500));
        let recent2 = format!("[Reviewer · attempt 2] {}", "y".repeat(500));
        let entries = vec![old.clone(), recent1.clone(), recent2.clone()];
        // Budget smaller than recent block — older entry is dropped.
        let result = build_feedback_history(&entries, 100);
        assert!(!result.contains(&old));
        // Recent entries are still returned even though they exceed budget.
        assert!(result.contains(&recent1));
        assert!(result.contains(&recent2));
    }
}
