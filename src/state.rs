use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

const DIRECTIVES_PATH: &str = ".ralph/directives.json";
const DIRECTIVES_DRAIN_PATH: &str = ".ralph/directives.json.drain";

/// Execution metadata for all tasks. Persisted to
/// `.ralph/state.json`. Separated from the task file so
/// the planner's output stays clean for human review.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ExecutionState {
    pub tasks: HashMap<String, TaskExecution>,
}

/// Per-task execution tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskExecution {
    pub attempts: u32,
    pub phase: Phase,
    pub last_error: Option<String>,
    pub files_changed: Vec<PathBuf>,
    /// Full agent response text from failed tester/reviewer runs,
    /// used to give the implementer actionable feedback on retry.
    #[serde(default)]
    pub feedback: Vec<String>,
    /// Persistent prescriptive guidance injected into the implementer
    /// prompt on every attempt (not just retries). Survives `ralph reset`.
    #[serde(default)]
    pub guidance: Vec<String>,
    /// Unix timestamp (seconds) when current phase was entered.
    #[serde(default)]
    pub phase_entered_at: Option<u64>,
    /// Unix timestamp when the task first started (left Pending).
    #[serde(default)]
    pub started_at: Option<u64>,
    /// Unix timestamp when the task completed (reached Done/Failed).
    #[serde(default)]
    pub completed_at: Option<u64>,
    /// Free-text postmortem note for triaged failures. Distinguishes
    /// investigated failures from fresh ones needing attention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postmortem: Option<String>,
}

pub use crate::task::{Directive, DirectiveAction, Phase, unix_now};

impl Default for TaskExecution {
    fn default() -> Self {
        Self {
            attempts: 0,
            phase: Phase::Pending,
            last_error: None,
            files_changed: Vec::new(),
            feedback: Vec::new(),
            guidance: Vec::new(),
            phase_entered_at: None,
            started_at: None,
            completed_at: None,
            postmortem: None,
        }
    }
}

impl ExecutionState {
    /// Load from disk, returning a default (empty) state if
    /// the file doesn't exist.
    ///
    /// If the main file is missing but a `.tmp` sibling exists
    /// (left behind by a crash mid-save), the `.tmp` is promoted
    /// to the canonical path before loading.
    pub async fn load(path: &Path) -> Result<Self> {
        if path.exists() {
            let contents = tokio::fs::read_to_string(path).await?;
            let state: ExecutionState = serde_json::from_str(&contents)?;
            Ok(state)
        } else {
            let tmp = path.with_extension("json.tmp");
            if tmp.exists() {
                eprintln!("[ralph] recovering state from {}", tmp.display());
                tokio::fs::rename(&tmp, path).await?;
                let contents = tokio::fs::read_to_string(path).await?;
                let state: ExecutionState = serde_json::from_str(&contents)?;
                Ok(state)
            } else {
                Ok(ExecutionState::default())
            }
        }
    }

    /// Persist to disk atomically (write tmp + rename).
    pub async fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }

    /// Get or insert default execution entry for a task.
    pub fn entry(&mut self, task_id: &str) -> &mut TaskExecution {
        self.tasks.entry(task_id.to_string()).or_default()
    }

    /// True when every task_id has reached a terminal satisfying
    /// phase (Done or Skipped).
    pub fn all_done(&self, task_ids: &[String]) -> bool {
        task_ids
            .iter()
            .all(|id| self.tasks.get(id).is_some_and(|e| e.phase.satisfies_dep()))
    }
}

// ── Sideband directives ───────────────────────────────────

/// Append a single directive as one JSONL line. Safe for
/// concurrent appenders (writes < PIPE_BUF are atomic on POSIX).
pub async fn append_directive(directive: &Directive) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let path = PathBuf::from(DIRECTIVES_PATH);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut line = serde_json::to_string(directive)?;
    line.push('\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Read directives without consuming them. Used by `ralph status`
/// to preview pending overrides.
pub async fn load_directives() -> Result<Vec<Directive>> {
    parse_directives_file(Path::new(DIRECTIVES_PATH)).await
}

/// Atomically consume all pending directives:
/// rename → parse → delete. Handles interrupted drains
/// (leftover `.drain` file).
pub async fn drain_directives() -> Result<Vec<Directive>> {
    let path = PathBuf::from(DIRECTIVES_PATH);
    let drain = PathBuf::from(DIRECTIVES_DRAIN_PATH);

    // If a previous drain was interrupted, process the leftover.
    if drain.exists() && !path.exists() {
        let directives = parse_directives_file(&drain).await?;
        let _ = tokio::fs::remove_file(&drain).await;
        return Ok(directives);
    }

    if !path.exists() {
        return Ok(Vec::new());
    }

    // Atomic rename prevents new appends from mixing with our read.
    tokio::fs::rename(&path, &drain).await?;
    let directives = parse_directives_file(&drain).await?;
    let _ = tokio::fs::remove_file(&drain).await;
    Ok(directives)
}

async fn parse_directives_file(path: &Path) -> Result<Vec<Directive>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = tokio::fs::read_to_string(path).await?;
    let mut directives = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Directive>(trimmed) {
            Ok(d) => directives.push(d),
            Err(e) => eprintln!("[ralph] ignoring malformed directive: {e}"),
        }
    }
    Ok(directives)
}

impl ExecutionState {
    /// Apply sideband directives to the in-memory state.
    /// Unknown task IDs are logged and skipped.
    /// Multiple directives for the same task are applied in order
    /// (last wins).
    pub fn apply_directives(&mut self, directives: &[Directive], known_ids: &[String]) {
        let known: std::collections::HashSet<&str> = known_ids.iter().map(|s| s.as_str()).collect();
        for d in directives {
            if !known.contains(d.task_id.as_str()) {
                eprintln!(
                    "[ralph] directive for unknown task '{}', skipping",
                    d.task_id
                );
                continue;
            }
            let exec = self.entry(&d.task_id);
            match d.action {
                DirectiveAction::Skip => {
                    exec.phase = Phase::Skipped;
                    exec.completed_at = Some(unix_now());
                }
                DirectiveAction::Fail => {
                    exec.phase = Phase::Failed;
                    exec.last_error = Some("manually failed via `ralph fail`".to_string());
                    exec.completed_at = Some(unix_now());
                }
                DirectiveAction::Reset => {
                    exec.phase = Phase::Pending;
                    exec.attempts = 0;
                    exec.last_error = None;
                    exec.feedback.clear();
                    // guidance is intentionally preserved
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_empty() {
        let state = ExecutionState::default();
        assert!(state.tasks.is_empty());
    }

    #[test]
    fn entry_creates_default() {
        let mut state = ExecutionState::default();
        let exec = state.entry("T1");
        assert_eq!(exec.phase, Phase::Pending);
        assert_eq!(exec.attempts, 0);
    }

    #[test]
    fn all_done_checks_all_ids() {
        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Done;
        state.entry("T2").phase = Phase::Testing;

        let ids = vec!["T1".into(), "T2".into()];
        assert!(!state.all_done(&ids));

        state.entry("T2").phase = Phase::Done;
        assert!(state.all_done(&ids));
    }

    #[test]
    fn all_done_false_for_missing() {
        let state = ExecutionState::default();
        assert!(!state.all_done(&["T1".into()]));
    }

    #[test]
    fn roundtrip_json() {
        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Done;
        state.entry("T1").attempts = 2;
        state.entry("T1").files_changed = vec![PathBuf::from("src/main.rs")];
        state.entry("T2").last_error = Some("compile error".into());

        let json = serde_json::to_string(&state).unwrap();
        let loaded: ExecutionState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tasks["T1"].phase, Phase::Done);
        assert_eq!(loaded.tasks["T1"].attempts, 2);
        assert_eq!(
            loaded.tasks["T2"].last_error.as_deref(),
            Some("compile error")
        );
    }

    #[test]
    fn backward_compat_deserialize_without_feedback() {
        // Old state.json without the feedback field must still load.
        let json = r#"{
            "tasks": {
                "T1": {
                    "attempts": 1,
                    "phase": "Pending",
                    "last_error": null,
                    "files_changed": []
                }
            }
        }"#;
        let state: ExecutionState = serde_json::from_str(json).unwrap();
        assert!(state.tasks["T1"].feedback.is_empty());
    }

    #[test]
    fn backward_compat_deserialize_without_guidance() {
        let json = r#"{
            "tasks": {
                "T1": {
                    "attempts": 1,
                    "phase": "Pending",
                    "last_error": null,
                    "files_changed": [],
                    "feedback": []
                }
            }
        }"#;
        let state: ExecutionState = serde_json::from_str(json).unwrap();
        assert!(state.tasks["T1"].guidance.is_empty());
    }

    #[test]
    fn roundtrip_json_with_guidance() {
        let mut state = ExecutionState::default();
        let exec = state.entry("T1");
        exec.guidance = vec![
            "Root cause is uuid's js feature".into(),
            "Rebuild fixtures after changing features".into(),
        ];

        let json = serde_json::to_string(&state).unwrap();
        let loaded: ExecutionState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tasks["T1"].guidance.len(), 2);
        assert!(loaded.tasks["T1"].guidance[0].contains("uuid"));
    }

    #[test]
    fn backward_compat_deserialize_without_timestamps() {
        let json = r#"{
            "tasks": {
                "T1": {
                    "attempts": 1,
                    "phase": "Pending",
                    "last_error": null,
                    "files_changed": [],
                    "feedback": []
                }
            }
        }"#;
        let state: ExecutionState = serde_json::from_str(json).unwrap();
        assert!(state.tasks["T1"].phase_entered_at.is_none());
        assert!(state.tasks["T1"].started_at.is_none());
        assert!(state.tasks["T1"].completed_at.is_none());
    }

    #[test]
    fn roundtrip_json_with_timestamps() {
        let mut state = ExecutionState::default();
        let exec = state.entry("T1");
        exec.phase = Phase::Testing;
        exec.started_at = Some(1700000000);
        exec.phase_entered_at = Some(1700000100);

        let json = serde_json::to_string(&state).unwrap();
        let loaded: ExecutionState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tasks["T1"].started_at, Some(1700000000));
        assert_eq!(loaded.tasks["T1"].phase_entered_at, Some(1700000100));
    }

    #[test]
    fn unix_now_returns_nonzero() {
        assert!(unix_now() > 0);
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("ralph_state_tests")
            .join(name)
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn save_and_load_roundtrip_on_disk() {
        let dir = test_dir("roundtrip");
        let path = dir.join("state.json");

        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Done;
        state.entry("T1").attempts = 3;
        state.entry("T1").files_changed = vec![PathBuf::from("lib.rs")];

        state.save(&path).await.unwrap();
        let loaded = ExecutionState::load(&path).await.unwrap();

        assert_eq!(loaded.tasks["T1"].phase, Phase::Done);
        assert_eq!(loaded.tasks["T1"].attempts, 3);
        assert_eq!(
            loaded.tasks["T1"].files_changed,
            vec![PathBuf::from("lib.rs")]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_recovers_from_tmp_file() {
        let dir = test_dir("recover_tmp");
        let path = dir.join("state.json");
        let tmp = path.with_extension("json.tmp");

        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Testing;
        let json = serde_json::to_string_pretty(&state).unwrap();
        tokio::fs::write(&tmp, &json).await.unwrap();

        assert!(!path.exists());
        assert!(tmp.exists());

        let loaded = ExecutionState::load(&path).await.unwrap();
        assert_eq!(loaded.tasks["T1"].phase, Phase::Testing);
        assert!(path.exists());
        assert!(!tmp.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tmp_file_cleaned_up_after_save() {
        let dir = test_dir("tmp_cleanup");
        let path = dir.join("state.json");
        let tmp = path.with_extension("json.tmp");

        let state = ExecutionState::default();
        state.save(&path).await.unwrap();

        assert!(path.exists());
        assert!(!tmp.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrip_json_with_feedback() {
        let mut state = ExecutionState::default();
        let exec = state.entry("T1");
        exec.phase = Phase::Pending;
        exec.attempts = 2;
        exec.feedback = vec![
            "[Tester · attempt 1] compile error in main.rs".into(),
            "[Reviewer · attempt 2] missing error handling".into(),
        ];

        let json = serde_json::to_string(&state).unwrap();
        let loaded: ExecutionState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tasks["T1"].feedback.len(), 2);
        assert!(loaded.tasks["T1"].feedback[0].contains("Tester"));
        assert!(loaded.tasks["T1"].feedback[1].contains("Reviewer"));
    }

    #[test]
    fn phase_ordinal_returns_correct_integers() {
        assert_eq!(Phase::Pending.phase_ordinal(), 0);
        assert_eq!(Phase::Implementing.phase_ordinal(), 1);
        assert_eq!(Phase::Testing.phase_ordinal(), 2);
        assert_eq!(Phase::Reviewing.phase_ordinal(), 3);
        assert_eq!(Phase::Done.phase_ordinal(), 4);
        assert_eq!(Phase::Failed.phase_ordinal(), 5);
        assert_eq!(Phase::Skipped.phase_ordinal(), 6);
    }

    #[test]
    fn roundtrip_implementing_phase() {
        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Implementing;

        let json = serde_json::to_string(&state).unwrap();
        let loaded: ExecutionState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tasks["T1"].phase, Phase::Implementing);
    }

    #[test]
    fn roundtrip_skipped_phase() {
        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Skipped;

        let json = serde_json::to_string(&state).unwrap();
        let loaded: ExecutionState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tasks["T1"].phase, Phase::Skipped);
    }

    #[test]
    fn satisfies_dep() {
        assert!(Phase::Done.satisfies_dep());
        assert!(Phase::Skipped.satisfies_dep());
        assert!(!Phase::Pending.satisfies_dep());
        assert!(!Phase::Implementing.satisfies_dep());
        assert!(!Phase::Testing.satisfies_dep());
        assert!(!Phase::Reviewing.satisfies_dep());
        assert!(!Phase::Failed.satisfies_dep());
    }

    #[test]
    fn all_done_accepts_skipped() {
        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Done;
        state.entry("T2").phase = Phase::Skipped;

        let ids = vec!["T1".into(), "T2".into()];
        assert!(state.all_done(&ids));
    }

    #[test]
    fn apply_directives_skip() {
        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Pending;
        let known = vec!["T1".into()];
        let directives = vec![Directive {
            task_id: "T1".into(),
            action: DirectiveAction::Skip,
        }];
        state.apply_directives(&directives, &known);
        assert_eq!(state.tasks["T1"].phase, Phase::Skipped);
        assert!(state.tasks["T1"].completed_at.is_some());
    }

    #[test]
    fn apply_directives_fail() {
        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Pending;
        let known = vec!["T1".into()];
        let directives = vec![Directive {
            task_id: "T1".into(),
            action: DirectiveAction::Fail,
        }];
        state.apply_directives(&directives, &known);
        assert_eq!(state.tasks["T1"].phase, Phase::Failed);
        assert!(state.tasks["T1"].last_error.is_some());
        assert!(state.tasks["T1"].completed_at.is_some());
    }

    #[test]
    fn apply_directives_reset() {
        let mut state = ExecutionState::default();
        let exec = state.entry("T1");
        exec.phase = Phase::Failed;
        exec.attempts = 3;
        exec.last_error = Some("error".into());
        exec.feedback = vec!["fb".into()];
        exec.guidance = vec!["keep this".into()];

        let known = vec!["T1".into()];
        let directives = vec![Directive {
            task_id: "T1".into(),
            action: DirectiveAction::Reset,
        }];
        state.apply_directives(&directives, &known);
        assert_eq!(state.tasks["T1"].phase, Phase::Pending);
        assert_eq!(state.tasks["T1"].attempts, 0);
        assert!(state.tasks["T1"].last_error.is_none());
        assert!(state.tasks["T1"].feedback.is_empty());
        assert_eq!(state.tasks["T1"].guidance, vec!["keep this".to_string()]);
    }

    #[test]
    fn apply_directives_unknown_id_skipped() {
        let mut state = ExecutionState::default();
        let known: Vec<String> = vec!["T1".into()];
        let directives = vec![Directive {
            task_id: "BOGUS".into(),
            action: DirectiveAction::Skip,
        }];
        state.apply_directives(&directives, &known);
        assert!(!state.tasks.contains_key("BOGUS"));
    }

    #[test]
    fn apply_directives_multi_last_wins() {
        let mut state = ExecutionState::default();
        state.entry("T1").phase = Phase::Pending;
        let known = vec!["T1".into()];
        let directives = vec![
            Directive {
                task_id: "T1".into(),
                action: DirectiveAction::Skip,
            },
            Directive {
                task_id: "T1".into(),
                action: DirectiveAction::Reset,
            },
        ];
        state.apply_directives(&directives, &known);
        assert_eq!(state.tasks["T1"].phase, Phase::Pending);
    }

    #[tokio::test]
    async fn directive_append_and_load_roundtrip() {
        let dir = test_dir("directives_roundtrip");
        // Temporarily override the path by writing directly
        let path = dir.join("directives.json");
        let d1 = Directive {
            task_id: "T1".into(),
            action: DirectiveAction::Skip,
        };
        let d2 = Directive {
            task_id: "T2".into(),
            action: DirectiveAction::Fail,
        };

        // Write JSONL manually to the test path
        {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .unwrap();
            for d in [&d1, &d2] {
                let mut line = serde_json::to_string(d).unwrap();
                line.push('\n');
                file.write_all(line.as_bytes()).await.unwrap();
            }
        }

        let loaded = parse_directives_file(&path).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].task_id, "T1");
        assert_eq!(loaded[0].action, DirectiveAction::Skip);
        assert_eq!(loaded[1].task_id, "T2");
        assert_eq!(loaded[1].action, DirectiveAction::Fail);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn drain_directives_removes_file() {
        let dir = test_dir("directives_drain");
        let path = dir.join("directives.json");
        let drain = dir.join("directives.json.drain");

        // Write a directive
        {
            let d = Directive {
                task_id: "T1".into(),
                action: DirectiveAction::Reset,
            };
            let mut line = serde_json::to_string(&d).unwrap();
            line.push('\n');
            tokio::fs::write(&path, line.as_bytes()).await.unwrap();
        }

        // Simulate drain: rename, parse, delete
        tokio::fs::rename(&path, &drain).await.unwrap();
        let loaded = parse_directives_file(&drain).await.unwrap();
        tokio::fs::remove_file(&drain).await.unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].action, DirectiveAction::Reset);
        assert!(!path.exists());
        assert!(!drain.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directive_action_roundtrip_json() {
        for action in [
            DirectiveAction::Skip,
            DirectiveAction::Fail,
            DirectiveAction::Reset,
        ] {
            let d = Directive {
                task_id: "X".into(),
                action,
            };
            let json = serde_json::to_string(&d).unwrap();
            let loaded: Directive = serde_json::from_str(&json).unwrap();
            assert_eq!(loaded.action, action);
        }
    }
}
