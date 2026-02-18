use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

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
    /// Unix timestamp (seconds) when current phase was entered.
    #[serde(default)]
    pub phase_entered_at: Option<u64>,
    /// Unix timestamp when the task first started (left Pending).
    #[serde(default)]
    pub started_at: Option<u64>,
    /// Unix timestamp when the task completed (reached Done/Failed).
    #[serde(default)]
    pub completed_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Pending,
    Testing,
    Reviewing,
    Done,
    Failed,
}

impl Default for TaskExecution {
    fn default() -> Self {
        Self {
            attempts: 0,
            phase: Phase::Pending,
            last_error: None,
            files_changed: Vec::new(),
            feedback: Vec::new(),
            phase_entered_at: None,
            started_at: None,
            completed_at: None,
        }
    }
}

/// Current Unix timestamp in seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

    /// True when every task_id has reached Done.
    pub fn all_done(&self, task_ids: &[String]) -> bool {
        task_ids
            .iter()
            .all(|id| self.tasks.get(id).is_some_and(|e| e.phase == Phase::Done))
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
        assert_eq!(loaded.tasks["T1"].files_changed, vec![PathBuf::from("lib.rs")]);
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
}
