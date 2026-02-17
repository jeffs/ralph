use std::collections::HashSet;

use crate::config::Config;
use crate::state::{ExecutionState, Phase};
use crate::task::Task;

/// Return tasks that are ready to execute: not done/failed,
/// all dependencies satisfied, within attempt budget.
pub fn ready_tasks<'a>(
    tasks: &'a [Task],
    state: &ExecutionState,
    config: &Config,
) -> Vec<&'a Task> {
    let done_ids: HashSet<&str> = state
        .tasks
        .iter()
        .filter(|(_, exec)| exec.phase == Phase::Done)
        .map(|(id, _)| id.as_str())
        .collect();

    let mut ready: Vec<&Task> = tasks
        .iter()
        .filter(|t| {
            let exec = state.tasks.get(&t.id);
            let phase = exec.map_or(Phase::Pending, |e| e.phase);
            let attempts = exec.map_or(0, |e| e.attempts);

            // Must be pending (not in-progress, done, or failed)
            let is_pending = phase == Phase::Pending;
            // All blockers must be done
            let deps_met = t
                .blocked_by
                .iter()
                .all(|dep| done_ids.contains(dep.as_str()));
            // Haven't exceeded attempt budget
            let within_budget = attempts < config.max_attempts;

            is_pending && deps_met && within_budget
        })
        .collect();

    // Sort by priority (lower number = higher priority)
    ready.sort_by_key(|t| t.priority);
    ready
}

/// Partition ready tasks into groups that can run in
/// parallel. Tasks whose previously-changed file sets
/// overlap must be serialized. Tasks with no file history
/// (first attempt) are placed in their own singleton group
/// to establish their footprint.
pub fn partition_independent<'a>(ready: &[&'a Task], state: &ExecutionState) -> Vec<Vec<&'a Task>> {
    let mut groups: Vec<Vec<&'a Task>> = Vec::new();

    for &task in ready {
        let exec = state.tasks.get(&task.id);
        let files: HashSet<_> = exec
            .map(|e| {
                e.files_changed
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();

        // First-attempt tasks have no file history — run
        // them alone to establish their footprint.
        if files.is_empty() {
            groups.push(vec![task]);
            continue;
        }

        // Try to add to an existing group with no overlap
        let mut placed = false;
        for group in &mut groups {
            let group_files: HashSet<String> = group
                .iter()
                .filter_map(|t| state.tasks.get(&t.id))
                .flat_map(|e| {
                    e.files_changed
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                })
                .collect();

            if files.is_disjoint(&group_files) {
                group.push(task);
                placed = true;
                break;
            }
        }
        if !placed {
            groups.push(vec![task]);
        }
    }

    groups
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::state::TaskExecution;

    fn task(id: &str, priority: u32, blocked_by: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: format!("Task {}", id),
            description: String::new(),
            priority,
            blocked_by: blocked_by.into_iter().map(String::from).collect(),
        }
    }

    fn default_config() -> Config {
        Config {
            max_attempts: 3,
            ..Config::default()
        }
    }

    #[test]
    fn ready_excludes_blocked() {
        let tasks = vec![task("A", 1, vec![]), task("B", 2, vec!["A"])];
        let state = ExecutionState::default();
        let ready = ready_tasks(&tasks, &state, &default_config());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "A");
    }

    #[test]
    fn ready_includes_unblocked_after_dep_done() {
        let tasks = vec![task("A", 1, vec![]), task("B", 2, vec!["A"])];
        let mut state = ExecutionState::default();
        state.entry("A").phase = Phase::Done;
        let ready = ready_tasks(&tasks, &state, &default_config());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "B");
    }

    #[test]
    fn ready_excludes_done() {
        let tasks = vec![task("A", 1, vec![])];
        let mut state = ExecutionState::default();
        state.entry("A").phase = Phase::Done;
        let ready = ready_tasks(&tasks, &state, &default_config());
        assert!(ready.is_empty());
    }

    #[test]
    fn ready_excludes_over_budget() {
        let tasks = vec![task("A", 1, vec![])];
        let mut state = ExecutionState::default();
        state.entry("A").attempts = 3;
        let ready = ready_tasks(&tasks, &state, &default_config());
        assert!(ready.is_empty());
    }

    #[test]
    fn ready_sorted_by_priority() {
        let tasks = vec![
            task("B", 3, vec![]),
            task("A", 1, vec![]),
            task("C", 2, vec![]),
        ];
        let state = ExecutionState::default();
        let ready = ready_tasks(&tasks, &state, &default_config());
        let ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["A", "C", "B"]);
    }

    #[test]
    fn partition_first_attempts_are_singletons() {
        let tasks = vec![task("A", 1, vec![]), task("B", 2, vec![])];
        let refs: Vec<&Task> = tasks.iter().collect();
        let state = ExecutionState::default();
        let groups = partition_independent(&refs, &state);
        // Each first-attempt task gets its own group
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn partition_disjoint_files_grouped() {
        let tasks = vec![task("A", 1, vec![]), task("B", 2, vec![])];
        let refs: Vec<&Task> = tasks.iter().collect();
        let mut state = ExecutionState::default();
        state.tasks.insert(
            "A".into(),
            TaskExecution {
                files_changed: vec![PathBuf::from("src/a.rs")],
                ..Default::default()
            },
        );
        state.tasks.insert(
            "B".into(),
            TaskExecution {
                files_changed: vec![PathBuf::from("src/b.rs")],
                ..Default::default()
            },
        );
        let groups = partition_independent(&refs, &state);
        // Disjoint files → same group
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
    }

    #[test]
    fn partition_overlapping_files_separated() {
        let tasks = vec![task("A", 1, vec![]), task("B", 2, vec![])];
        let refs: Vec<&Task> = tasks.iter().collect();
        let mut state = ExecutionState::default();
        state.tasks.insert(
            "A".into(),
            TaskExecution {
                files_changed: vec![PathBuf::from("src/shared.rs")],
                ..Default::default()
            },
        );
        state.tasks.insert(
            "B".into(),
            TaskExecution {
                files_changed: vec![PathBuf::from("src/shared.rs")],
                ..Default::default()
            },
        );
        let groups = partition_independent(&refs, &state);
        assert_eq!(groups.len(), 2);
    }
}
