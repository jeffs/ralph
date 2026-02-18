use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::process::Command as TokioCommand;

use crate::config::Config;

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

/// Invoke a claude agent with the given role and context.
/// When `working_dir` is `Some`, the subprocess runs in that directory.
pub async fn invoke_agent(
    role: AgentRole,
    context: &AgentContext,
    config: &Config,
    working_dir: Option<&Path>,
) -> Result<AgentResult> {
    // Load and interpolate prompt
    let prompt_path = config.prompts_dir.join(role.prompt_filename());
    let template = tokio::fs::read_to_string(&prompt_path)
        .await
        .with_context(|| format!("reading prompt template: {}", prompt_path.display()))?;
    let prompt = context.interpolate(&template);

    eprintln!("[ralph] invoking {} agent...", role.label());

    // Spawn claude — clear CLAUDECODE env var to allow
    // nesting when ralph is invoked from within claude.
    let mut cmd = TokioCommand::new("claude");
    cmd.env_remove("CLAUDECODE")
        .arg("-p")
        .arg(&prompt)
        .arg("--output-format")
        .arg("json")
        .arg("--model")
        .arg(config.model_for(role.label()))
        .arg("--dangerously-skip-permissions")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    let output = cmd
        .spawn()
        .context("spawning claude process")?
        .wait_with_output()
        .await
        .context("waiting for claude process")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        eprintln!("[ralph] {} stderr: {}", role.label(), stderr);
    }

    if !output.status.success() {
        return Ok(AgentResult {
            text: String::new(),
            status: AgentStatus::Failure {
                reason: format!("claude exited with {}", output.status),
            },
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

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
}
