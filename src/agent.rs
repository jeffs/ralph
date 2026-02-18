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

/// Stderr patterns that indicate the agent is stuck waiting for a file lock.
const STUCK_PATTERNS: &[&str] = &["Blocking waiting for file lock", "waiting for lock"];

/// Tracks active child process group IDs so a centralized signal
/// handler can clean them all up on SIGINT / SIGTERM.
#[derive(Clone, Default)]
pub struct ProcessRegistry {
    pgids: Arc<Mutex<HashSet<u32>>>,
    historical_pgids: Arc<Mutex<HashSet<u32>>>,
    shutdown: Arc<AtomicBool>,
}

impl ProcessRegistry {
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
            kill_process_group(pgid).await;
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
                kill_process_group(pgid).await;
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
}

impl AgentRole {
    fn prompt_filename(self) -> &'static str {
        match self {
            Self::Planner => "planner.md",
            Self::Implementer => "implementer.md",
            Self::Tester => "tester.md",
            Self::Reviewer => "reviewer.md",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Implementer => "implementer",
            Self::Tester => "tester",
            Self::Reviewer => "reviewer",
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
    },
}

impl AgentContext {
    pub fn plan(input: &str) -> Self {
        Self::Plan {
            input: input.to_string(),
        }
    }

    pub fn implement(id: &str, title: &str, description: &str, feedback: Option<&str>) -> Self {
        Self::Implement {
            task_id: id.to_string(),
            task_title: title.to_string(),
            task_description: description.to_string(),
            feedback: feedback.map(String::from),
        }
    }

    pub fn test(id: &str, files: Vec<PathBuf>) -> Self {
        Self::Test {
            task_id: id.to_string(),
            files_changed: files,
        }
    }

    pub fn review(id: &str, title: &str, description: &str) -> Self {
        Self::Review {
            task_id: id.to_string(),
            task_title: title.to_string(),
            task_description: description.to_string(),
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
                feedback,
            } => {
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
            } => template
                .replace("{{TASK_ID}}", task_id)
                .replace("{{TASK_TITLE}}", task_title)
                .replace("{{TASK_DESCRIPTION}}", task_description),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Success,
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
}

/// Response shape from `claude -p --output-format json`
#[derive(Deserialize)]
struct ClaudeJsonOutput {
    result: Option<String>,
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

/// Send SIGTERM to all processes in the given process group.
/// Ignores errors (the group may have already exited).
pub(crate) async fn kill_process_group(pgid: u32) {
    let _ = TokioCommand::new("kill")
        .args(["-TERM", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Poll interval for the idle-monitoring loop.
const POLL_INTERVAL_SECS: u64 = 30;
/// CPU% threshold below which a process group is considered idle.
const IDLE_CPU_THRESHOLD: f64 = 1.0;

/// Grace period after a stuck-pattern is detected before killing the agent.
const STUCK_GRACE_SECS: u64 = 60;

/// Monitor a running child process for idleness, stuck patterns, and hard timeout.
///
/// Polls `child.wait()` in 30-second intervals. On each timeout (child still
/// running), samples the process group's CPU usage. If cumulative idle time
/// exceeds `agent_idle_timeout_secs`, kills the group and returns a Failure
/// result. Enforces `agent_timeout_secs` as the hard ceiling.
///
/// `stuck_flag` is set by the stderr reader task when a stuck pattern is
/// detected. Once set, the agent is killed after `STUCK_GRACE_SECS`.
async fn monitor_agent(
    mut child: tokio::process::Child,
    pgid: u32,
    agent_timeout_secs: u64,
    agent_idle_timeout_secs: u64,
    stuck_flag: Arc<AtomicBool>,
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
            kill_process_group(pgid).await;
            return Err(AgentStatus::Failure {
                reason: format!("agent timed out after {agent_timeout_secs}s"),
            });
        }

        // Check stuck flag.
        if stuck_flag.load(Ordering::Acquire) {
            let since = stuck_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= stuck_grace {
                kill_process_group(pgid).await;
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
                kill_process_group(pgid).await;
                return Err(AgentStatus::Failure {
                    reason: format!("error waiting for agent process: {e}"),
                });
            }
            Err(_poll_timeout_elapsed) => {
                // Child still running — sample CPU.
                let cpu = sample_pgid_cpu(pgid).await;
                if cpu < IDLE_CPU_THRESHOLD {
                    idle_duration += poll_timeout;
                    if idle_duration >= idle_limit {
                        kill_process_group(pgid).await;
                        return Err(AgentStatus::Failure {
                            reason: format!(
                                "agent idle for {}s (possible deadlock)",
                                idle_duration.as_secs()
                            ),
                        });
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
) -> Result<AgentResult> {
    // Load and interpolate prompt
    let prompt_path = config.prompts_dir.join(role.prompt_filename());
    let template = tokio::fs::read_to_string(&prompt_path)
        .await
        .with_context(|| format!("reading prompt template: {}", prompt_path.display()))?;
    let prompt = context.interpolate(&template);

    eprintln!("[ralph] invoking {} agent...", role.label());

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
    cmd.arg("-p")
        .arg(&prompt)
        .arg("--output-format")
        .arg("json")
        .arg("--model")
        .arg(config.model_for(role.label()))
        .arg("--dangerously-skip-permissions")
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
    let stuck_flag = Arc::new(AtomicBool::new(false));
    let stuck_flag_clone = Arc::clone(&stuck_flag);
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr_handle).lines();
        let mut collected: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[ralph] {role_label}: {line}");
            if STUCK_PATTERNS.iter().any(|pat| line.contains(pat)) {
                stuck_flag_clone.store(true, Ordering::Release);
            }
            collected.push(line);
        }
        collected
    });

    registry.register(child_pid);
    let exit_status = match monitor_agent(
        child,
        child_pid,
        config.agent_timeout_secs,
        config.agent_idle_timeout_secs,
        stuck_flag,
    )
    .await
    {
        Ok(status) => status,
        Err(status) => {
            eprintln!("[ralph] {} agent stopped: {:?}", role.label(), status,);
            registry.deregister(child_pid);
            return Ok(AgentResult {
                text: String::new(),
                status,
            });
        }
    };
    kill_process_group(child_pid).await;
    registry.deregister(child_pid);

    // Join the concurrent stdout drain task.
    let stdout_bytes = stdout_task.await.unwrap_or_default();

    let stderr_lines = stderr_task.await.unwrap_or_default();
    if !stderr_lines.is_empty() {
        eprintln!(
            "[ralph] {} stderr: {}",
            role.label(),
            stderr_lines.join("\n")
        );
    }

    if !exit_status.success() {
        return Ok(AgentResult {
            text: String::new(),
            status: AgentStatus::Failure {
                reason: format!("claude exited with {}", exit_status),
            },
        });
    }

    let stdout = String::from_utf8_lossy(&stdout_bytes);

    // Parse the JSON envelope
    let text = match serde_json::from_str::<ClaudeJsonOutput>(&stdout) {
        Ok(parsed) => parsed.result.unwrap_or_default(),
        Err(_) => {
            // Fall back to raw stdout if JSON parsing fails
            stdout.to_string()
        }
    };

    // Parse agent's self-reported status from the text
    let status = parse_agent_status(&text);

    Ok(AgentResult { text, status })
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

        // Try to match FAILURE: or NEEDS_RETRY: and collect the reason.
        let (prefix, make_status): (&str, fn(String) -> AgentStatus) =
            if let Some(r) = rest.strip_prefix("FAILURE:") {
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
pub(crate) const FEEDBACK_MAX_LEN: usize = 8000;

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
        let ctx = AgentContext::implement("T1", "Fix bug", "desc", None);
        let result =
            ctx.interpolate("Task {{TASK_ID}}: {{TASK_TITLE}} — {{TASK_DESCRIPTION}}{{FEEDBACK}}");
        assert_eq!(result, "Task T1: Fix bug — desc");
    }

    #[test]
    fn interpolate_implement_with_feedback() {
        let ctx = AgentContext::implement("T1", "Fix bug", "desc", Some("compile error on line 5"));
        let result = ctx.interpolate("{{TASK_ID}}{{FEEDBACK}}## Instructions");
        assert!(result.contains("## Previous Attempt Feedback"));
        assert!(result.contains("compile error on line 5"));
        assert!(result.contains("## Instructions"));
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
        let matching = [
            "Blocking waiting for file lock on package cache",
            "warning: waiting for lock on build directory",
            "  waiting for lock...",
        ];
        for line in &matching {
            assert!(
                STUCK_PATTERNS.iter().any(|pat| line.contains(pat)),
                "expected pattern match for: {line}"
            );
        }
    }

    #[test]
    fn stuck_patterns_do_not_match_unrelated_lines() {
        let non_matching = [
            "Compiling foo v0.1.0",
            "error[E0308]: mismatched types",
            "warning: unused variable `x`",
            "Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s",
        ];
        for line in &non_matching {
            assert!(
                !STUCK_PATTERNS.iter().any(|pat| line.contains(pat)),
                "unexpected pattern match for: {line}"
            );
        }
    }
}
