use std::collections::HashSet;

use crate::config::Config;
use crate::task::{Phase, Task};

/// Return tasks that are ready to execute: not done/failed,
/// all dependencies satisfied, within attempt budget.
pub fn ready_tasks<'a>(tasks: &'a [Task], config: &Config) -> Vec<&'a Task> {
    let done_ids: HashSet<&str> = tasks
        .iter()
        .filter(|t| t.phase.satisfies_dep())
        .map(|t| t.id.as_str())
        .collect();

    let mut ready: Vec<&Task> = tasks
        .iter()
        .filter(|t| {
            // Must be pending (not in-progress, done, or failed)
            let is_pending = t.phase == Phase::Pending;
            // All blockers must be done
            let deps_met = t
                .blocked_by
                .iter()
                .all(|dep| done_ids.contains(dep.as_str()));
            // Haven't exceeded attempt budget
            let within_budget = t.attempts < config.max_attempts;

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
///
/// Not currently used by the orchestrator (implementers run
/// serially), but retained for future parallel strategies
/// that don't require workspace isolation.
#[cfg(test)]
pub fn partition_independent<'a>(ready: &[&'a Task]) -> Vec<Vec<&'a Task>> {
    let mut groups: Vec<Vec<&'a Task>> = Vec::new();

    for &task in ready {
        let files: HashSet<_> = task
            .files_changed
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

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
                .flat_map(|t| {
                    t.files_changed
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

    fn task(id: &str, priority: u32, blocked_by: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: format!("Task {}", id),
            description: String::new(),
            priority,
            blocked_by: blocked_by.into_iter().map(String::from).collect(),
            phase: Phase::Pending,
            attempts: 0,
            last_error: None,
            files_changed: Vec::new(),
            feedback: Vec::new(),
            guidance: Vec::new(),
            phase_entered_at: None,
            started_at: None,
            completed_at: None,
            postmortem: None,
            archived: false,
            manual: false,
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
        let ready = ready_tasks(&tasks, &default_config());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "A");
    }

    #[test]
    fn ready_includes_unblocked_after_dep_done() {
        let tasks = vec![
            Task { phase: Phase::Done, ..task("A", 1, vec![]) },
            task("B", 2, vec!["A"]),
        ];
        let ready = ready_tasks(&tasks, &default_config());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "B");
    }

    #[test]
    fn ready_excludes_done() {
        let tasks = vec![Task { phase: Phase::Done, ..task("A", 1, vec![]) }];
        let ready = ready_tasks(&tasks, &default_config());
        assert!(ready.is_empty());
    }

    #[test]
    fn ready_excludes_over_budget() {
        let tasks = vec![Task { attempts: 3, ..task("A", 1, vec![]) }];
        let ready = ready_tasks(&tasks, &default_config());
        assert!(ready.is_empty());
    }

    #[test]
    fn ready_sorted_by_priority() {
        let tasks = vec![
            task("B", 3, vec![]),
            task("A", 1, vec![]),
            task("C", 2, vec![]),
        ];
        let ready = ready_tasks(&tasks, &default_config());
        let ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["A", "C", "B"]);
    }

    #[test]
    fn ready_includes_manual_task() {
        // A manual task is still "ready" — the orchestrator
        // partitions it out instead of spawning. Returning it
        // here is what causes downstream deps to remain blocked.
        let tasks = vec![
            Task { manual: true, ..task("A", 1, vec![]) },
            task("B", 2, vec!["A"]),
        ];
        let ready = ready_tasks(&tasks, &default_config());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "A");
        assert!(ready[0].manual);
    }

    #[test]
    fn manual_task_blocks_downstream() {
        // Pending manual task A → downstream B should remain
        // blocked. B only becomes ready once A is Done.
        let tasks = vec![
            Task { manual: true, ..task("A", 1, vec![]) },
            task("B", 2, vec!["A"]),
        ];
        let ready = ready_tasks(&tasks, &default_config());
        let ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["A"]);
        assert!(!ids.contains(&"B"));
    }

    #[test]
    fn ready_includes_unblocked_after_dep_skipped() {
        let tasks = vec![
            Task { phase: Phase::Skipped, ..task("A", 1, vec![]) },
            task("B", 2, vec!["A"]),
        ];
        let ready = ready_tasks(&tasks, &default_config());
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "B");
    }

    #[test]
    fn partition_first_attempts_are_singletons() {
        let tasks = vec![task("A", 1, vec![]), task("B", 2, vec![])];
        let refs: Vec<&Task> = tasks.iter().collect();
        let groups = partition_independent(&refs);
        // Each first-attempt task gets its own group
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn partition_disjoint_files_grouped() {
        let tasks = vec![
            Task {
                files_changed: vec![PathBuf::from("src/a.rs")],
                ..task("A", 1, vec![])
            },
            Task {
                files_changed: vec![PathBuf::from("src/b.rs")],
                ..task("B", 2, vec![])
            },
        ];
        let refs: Vec<&Task> = tasks.iter().collect();
        let groups = partition_independent(&refs);
        // Disjoint files → same group
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
    }

    #[test]
    fn partition_overlapping_files_separated() {
        let tasks = vec![
            Task {
                files_changed: vec![PathBuf::from("src/shared.rs")],
                ..task("A", 1, vec![])
            },
            Task {
                files_changed: vec![PathBuf::from("src/shared.rs")],
                ..task("B", 2, vec![])
            },
        ];
        let refs: Vec<&Task> = tasks.iter().collect();
        let groups = partition_independent(&refs);
        assert_eq!(groups.len(), 2);
    }
}
