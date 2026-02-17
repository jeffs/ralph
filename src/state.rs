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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Pending,
    Implementing,
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
        }
    }
}

impl ExecutionState {
    /// Load from disk, returning a default (empty) state if
    /// the file doesn't exist.
    pub async fn load(path: &Path) -> Result<Self> {
        if path.exists() {
            let contents = tokio::fs::read_to_string(path).await?;
            let state: ExecutionState = serde_json::from_str(&contents)?;
            Ok(state)
        } else {
            Ok(ExecutionState::default())
        }
    }

    /// Persist to disk.
    pub async fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        tokio::fs::write(path, json).await?;
        Ok(())
    }

    /// Get or insert default execution entry for a task.
    pub fn entry(&mut self, task_id: &str) -> &mut TaskExecution {
        self.tasks
            .entry(task_id.to_string())
            .or_default()
    }

    /// True when every task_id has reached Done.
    pub fn all_done(&self, task_ids: &[String]) -> bool {
        task_ids.iter().all(|id| {
            self.tasks
                .get(id)
                .is_some_and(|e| e.phase == Phase::Done)
        })
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
        state.entry("T2").phase = Phase::Implementing;

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
        state.entry("T1").files_changed =
            vec![PathBuf::from("src/main.rs")];
        state.entry("T2").last_error =
            Some("compile error".into());

        let json = serde_json::to_string(&state).unwrap();
        let loaded: ExecutionState =
            serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tasks["T1"].phase, Phase::Done);
        assert_eq!(loaded.tasks["T1"].attempts, 2);
        assert_eq!(
            loaded.tasks["T2"].last_error.as_deref(),
            Some("compile error")
        );
    }
}
