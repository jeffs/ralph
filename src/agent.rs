use std::path::PathBuf;
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

    pub fn implement(id: &str, title: &str, description: &str) -> Self {
        Self::Implement {
            task_id: id.to_string(),
            task_title: title.to_string(),
            task_description: description.to_string(),
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
            } => template
                .replace("{{TASK_ID}}", task_id)
                .replace("{{TASK_TITLE}}", task_title)
                .replace("{{TASK_DESCRIPTION}}", task_description),
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
pub async fn invoke_agent(
    role: AgentRole,
    context: &AgentContext,
    config: &Config,
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
    let output = TokioCommand::new("claude")
        .env_remove("CLAUDECODE")
        .arg("-p")
        .arg(&prompt)
        .arg("--output-format")
        .arg("json")
        .arg("--model")
        .arg(&config.model)
        .arg("--dangerously-skip-permissions")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

/// Parse the agent's self-reported status. Agents are
/// instructed to include a status line like:
///   STATUS: SUCCESS
///   STATUS: FAILURE: reason
///   STATUS: NEEDS_RETRY: reason
fn parse_agent_status(text: &str) -> AgentStatus {
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("STATUS:") {
            let rest = rest.trim();
            if rest.starts_with("SUCCESS") {
                return AgentStatus::Success;
            } else if let Some(reason) = rest.strip_prefix("FAILURE:") {
                return AgentStatus::Failure {
                    reason: reason.trim().to_string(),
                };
            } else if let Some(reason) = rest.strip_prefix("NEEDS_RETRY:") {
                return AgentStatus::NeedsRetry {
                    reason: reason.trim().to_string(),
                };
            }
        }
    }
    // No status line found — don't assume success.
    AgentStatus::NeedsRetry {
        reason: "no STATUS line in agent output".to_string(),
    }
}

/// Get all dirty files: tracked files that differ from HEAD
/// plus untracked (new) files. This is the complete picture
/// of what has changed in the working tree.
pub async fn git_changed_files() -> Result<Vec<PathBuf>> {
    // Modified/deleted tracked files
    let diff = TokioCommand::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;
    // New untracked files (respects .gitignore)
    let untracked = TokioCommand::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    let mut files: Vec<PathBuf> = [&diff.stdout, &untracked.stdout]
        .into_iter()
        .flat_map(|out| {
            String::from_utf8_lossy(out)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
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
        assert!(matches!(
            parse_agent_status(text),
            AgentStatus::NeedsRetry { .. }
        ));
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
        let ctx = AgentContext::implement("T1", "Fix bug", "desc");
        let result = ctx.interpolate("Task {{TASK_ID}}: {{TASK_TITLE}} — {{TASK_DESCRIPTION}}");
        assert_eq!(result, "Task T1: Fix bug — desc");
    }
}
