use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Pending,
    Implementing,
    Testing,
    Reviewing,
    Done,
    Failed,
    Skipped,
}

impl Phase {
    /// Integer phase ID for sorting and machine consumption.
    pub fn phase_ordinal(self) -> u8 {
        match self {
            Phase::Pending => 0,
            Phase::Implementing => 1,
            Phase::Testing => 2,
            Phase::Reviewing => 3,
            Phase::Done => 4,
            Phase::Failed => 5,
            Phase::Skipped => 6,
        }
    }

    /// Whether this phase counts as "dependency satisfied" for
    /// downstream tasks. Both Done and Skipped satisfy deps.
    pub fn satisfies_dep(self) -> bool {
        matches!(self, Phase::Done | Phase::Skipped)
    }
}

/// Current Unix timestamp in seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Lightweight task definition as produced by the planner (JSONL).
/// Execution metadata lives in the unified `Task` struct, persisted
/// via `db.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDef {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub priority: u32,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    /// Human-gated task: scheduler refuses to spawn an agent.
    /// Only emitted in JSONL output when `true`, to keep the
    /// default compact.
    #[serde(default, skip_serializing_if = "is_false")]
    pub manual: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Unified task: definition fields merged with execution state.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub description: String,
    pub priority: u32,
    pub blocked_by: Vec<String>,
    pub phase: Phase,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub files_changed: Vec<PathBuf>,
    pub feedback: Vec<String>,
    pub guidance: Vec<String>,
    pub phase_entered_at: Option<u64>,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
    pub postmortem: Option<String>,
    pub archived: bool,
    /// Human-gated task: the orchestrator must not spawn any
    /// agent. The user drives it to Done via `ralph mark-done`.
    pub manual: bool,
}

impl Task {
    pub fn from_def(def: &TaskDef) -> Self {
        Self {
            id: def.id.clone(),
            title: def.title.clone(),
            description: def.description.clone(),
            priority: def.priority,
            blocked_by: def.blocked_by.clone(),
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
            manual: def.manual,
        }
    }
}

/// Parse JSONL text into tasks. Blank lines are skipped.
/// Provides clear error messages pointing at the offending line.
pub fn parse_tasks(contents: &str) -> Result<Vec<TaskDef>> {
    let tasks: Vec<TaskDef> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str::<TaskDef>(line).with_context(|| {
                let preview = if line.len() > 120 {
                    format!("{}...", &line[..line.floor_char_boundary(120)])
                } else {
                    line.to_string()
                };
                format!("parsing task on line {} \u{2014} {}", i + 1, preview)
            })
        })
        .collect::<Result<Vec<_>>>()?;

    validate_tasks(&tasks)?;
    Ok(tasks)
}

/// Validate task fields beyond what serde enforces.
fn validate_tasks(tasks: &[TaskDef]) -> Result<()> {
    let mut errors = Vec::new();

    for (i, t) in tasks.iter().enumerate() {
        if t.id.trim().is_empty() {
            errors.push(format!("task {} (line {}): `id` is empty", i + 1, i + 1));
        }
        if t.title.trim().is_empty() {
            errors.push(format!("task {} '{}': `title` is empty", i + 1, t.id));
        }
        if t.id.contains(char::is_whitespace) {
            errors.push(format!(
                "task {} '{}': `id` contains whitespace",
                i + 1,
                t.id
            ));
        }
    }

    let mut seen = std::collections::HashSet::new();
    for t in tasks {
        if !seen.insert(&t.id) {
            errors.push(format!("duplicate task ID '{}'", t.id));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("task validation failed:\n  {}", errors.join("\n  "));
    }
}

/// Validate that every `blocked_by` ID references an actual task or a known
/// extra ID (e.g. archived tasks), and that no dependency cycles exist.
/// Returns an error listing all dangling references and/or cycles found.
pub fn validate_deps(tasks: &[TaskDef], extra_ids: &std::collections::HashSet<&str>) -> Result<()> {
    let ids: std::collections::HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let mut bad = Vec::new();
    for t in tasks {
        for dep in &t.blocked_by {
            if !ids.contains(dep.as_str()) && !extra_ids.contains(dep.as_str()) {
                bad.push(format!("{} blocked_by unknown task {}", t.id, dep));
            }
        }
    }

    // DFS-based cycle detection among tasks in this set.
    // Returns the cycle as [a, b, ..., a] (start node repeated at end), or None.
    fn dfs<'a>(
        id: &'a str,
        adj: &std::collections::HashMap<&'a str, Vec<&'a str>>,
        state: &mut std::collections::HashMap<&'a str, u8>,
        path: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        state.insert(id, 1);
        path.push(id);
        if let Some(deps) = adj.get(id) {
            for &dep in deps {
                match *state.get(dep).unwrap_or(&0) {
                    1 => {
                        // dep is on the current DFS stack — cycle found.
                        if let Some(idx) = path.iter().position(|&x| x == dep) {
                            let mut cycle: Vec<String> =
                                path[idx..].iter().map(|s| s.to_string()).collect();
                            cycle.push(dep.to_string());
                            return Some(cycle);
                        }
                    }
                    0 => {
                        if let Some(c) = dfs(dep, adj, state, path) {
                            return Some(c);
                        }
                    }
                    _ => {}
                }
            }
        }
        path.pop();
        state.insert(id, 2);
        None
    }

    let adj: std::collections::HashMap<&str, Vec<&str>> = tasks
        .iter()
        .map(|t| {
            let deps: Vec<&str> = t
                .blocked_by
                .iter()
                .map(|d| d.as_str())
                .filter(|d| ids.contains(d))
                .collect();
            (t.id.as_str(), deps)
        })
        .collect();
    let mut state: std::collections::HashMap<&str, u8> = std::collections::HashMap::new();
    for &id in &ids {
        if *state.get(id).unwrap_or(&0) == 0 {
            let mut path = Vec::new();
            if let Some(cycle) = dfs(id, &adj, &mut state, &mut path) {
                bad.push(format!("dependency cycle: {}", cycle.join(" -> ")));
            }
        }
    }

    if bad.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("dependency validation failed:\n  {}", bad.join("\n  "));
    }
}

/// Scan a slice of ID strings for `PREFIX-N` patterns and return a
/// human-readable summary of taken ID ranges. Used to tell the planner
/// which IDs are already in use so it can avoid collisions.
///
/// Returns an empty string when there are no IDs.
pub fn id_ranges_summary_from_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        return String::new();
    }

    // Collect (prefix, number) pairs from PREFIX-N patterns.
    let mut prefix_numbers: std::collections::BTreeMap<String, Vec<u32>> =
        std::collections::BTreeMap::new();
    for id in ids {
        if let Some((prefix, num_str)) = id.rsplit_once('-')
            && !prefix.is_empty()
            && let Ok(n) = num_str.parse::<u32>()
        {
            prefix_numbers.entry(prefix.to_string()).or_default().push(n);
        }
    }

    if prefix_numbers.is_empty() {
        // No PREFIX-N IDs found — list the raw IDs so the planner
        // still knows they're taken.
        let id_list: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        return format!(
            "The following task IDs are already in use: {}\n\n\
             You MUST NOT reuse any existing ID.\n",
            id_list.join(", ")
        );
    }

    let mut lines = vec!["The following ID prefixes are already in use:".to_string()];
    for (prefix, mut nums) in prefix_numbers {
        nums.sort_unstable();
        let min = nums[0];
        let max = *nums.last().unwrap();
        lines.push(format!(
            "- {prefix}: {min} through {max} (next available: {prefix}-{})",
            max + 1
        ));
    }
    lines.push(String::new());
    lines.push(
        "You MUST NOT reuse any existing ID. Start numbering from the \
         next available number for each prefix."
            .to_string(),
    );
    lines.push(String::new());
    lines.join("\n")
}

/// Renumber any tasks whose IDs collide with `taken_ids`.
///
/// For `PREFIX-N` style IDs, increments N past the highest taken
/// number for that prefix. For non-prefixed IDs, appends `-2`, `-3`, etc.
/// Updates `blocked_by` references within `tasks` to match.
/// Returns the list of `(old_id, new_id)` renames performed.
pub fn renumber_collisions(
    tasks: &mut [TaskDef],
    taken_ids: &std::collections::HashSet<String>,
) -> Vec<(String, String)> {
    let mut prefix_max: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    for id in taken_ids {
        if let Some((prefix, num_str)) = id.rsplit_once('-')
            && !prefix.is_empty()
            && let Ok(n) = num_str.parse::<u32>()
        {
            let entry = prefix_max.entry(prefix.to_string()).or_insert(0);
            *entry = (*entry).max(n);
        }
    }

    let mut all_ids = taken_ids.clone();
    let mut renames: Vec<(String, String)> = Vec::new();

    for task in tasks.iter_mut() {
        if !all_ids.contains(&task.id) {
            all_ids.insert(task.id.clone());
            continue;
        }

        let old_id = task.id.clone();
        let new_id = if let Some((prefix, num_str)) = old_id.rsplit_once('-')
            && !prefix.is_empty()
            && num_str.parse::<u32>().is_ok()
        {
            let next = prefix_max.entry(prefix.to_string()).or_insert(0);
            *next += 1;
            format!("{prefix}-{next}")
        } else {
            let mut candidate = format!("{old_id}-2");
            let mut n = 3u32;
            while all_ids.contains(&candidate) {
                candidate = format!("{old_id}-{n}");
                n += 1;
            }
            candidate
        };

        all_ids.insert(new_id.clone());
        task.id = new_id.clone();
        renames.push((old_id, new_id));
    }

    if !renames.is_empty() {
        let rename_map: std::collections::HashMap<&str, &str> = renames
            .iter()
            .map(|(old, new)| (old.as_str(), new.as_str()))
            .collect();
        for task in tasks.iter_mut() {
            for dep in &mut task.blocked_by {
                if let Some(new) = rename_map.get(dep.as_str()) {
                    *dep = new.to_string();
                }
            }
        }
    }

    renames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_jsonl() {
        let input = r#"{"id":"T1","title":"Do thing","priority":1}
{"id":"T2","title":"Other","priority":2,"blocked_by":["T1"]}"#;
        let tasks = parse_tasks(input).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "T1");
        assert_eq!(tasks[1].blocked_by, vec!["T1"]);
    }

    #[test]
    fn parse_skips_blank_lines() {
        let input = r#"{"id":"T1","title":"A","priority":1}

{"id":"T2","title":"B","priority":2}"#;
        let tasks = parse_tasks(input).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn parse_rejects_bad_json() {
        let input = "not json";
        assert!(parse_tasks(input).is_err());
    }

    #[test]
    fn roundtrip() {
        let tasks = vec![
            TaskDef {
                id: "A".into(),
                title: "Alpha".into(),
                description: "desc".into(),
                priority: 1,
                blocked_by: vec![],
                            manual: false,
            },
            TaskDef {
                id: "B".into(),
                title: "Beta".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec!["A".into()],
                            manual: false,
            },
        ];
        let mut buf = String::new();
        for t in &tasks {
            buf.push_str(&serde_json::to_string(t).unwrap());
            buf.push('\n');
        }
        let loaded = parse_tasks(&buf).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "A");
        assert_eq!(loaded[1].blocked_by, vec!["A"]);
    }

    #[test]
    fn validate_deps_ok() {
        let tasks = vec![
            TaskDef {
                id: "A".into(),
                title: "A".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec![],
                            manual: false,
            },
            TaskDef {
                id: "B".into(),
                title: "B".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec!["A".into()],
                            manual: false,
            },
        ];
        let empty = std::collections::HashSet::new();
        assert!(validate_deps(&tasks, &empty).is_ok());
    }

    #[test]
    fn validate_rejects_empty_id() {
        let input = r#"{"id":"","title":"A","priority":1}"#;
        assert!(parse_tasks(input).is_err());
    }

    #[test]
    fn validate_rejects_whitespace_in_id() {
        let input = r#"{"id":"T 1","title":"A","priority":1}"#;
        assert!(parse_tasks(input).is_err());
    }

    #[test]
    fn validate_rejects_empty_title() {
        let input = r#"{"id":"T1","title":"","priority":1}"#;
        assert!(parse_tasks(input).is_err());
    }

    #[test]
    fn validate_rejects_duplicate_ids() {
        let input = r#"{"id":"T1","title":"A","priority":1}
{"id":"T1","title":"B","priority":2}"#;
        assert!(parse_tasks(input).is_err());
    }

    #[test]
    fn renumber_no_collisions() {
        let taken: std::collections::HashSet<String> =
            ["REPL-1", "REPL-2"].iter().map(|s| s.to_string()).collect();
        let mut tasks = vec![TaskDef {
            id: "REPL-3".into(),
            title: "A".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec![],
                    manual: false,
        }];
        let renames = renumber_collisions(&mut tasks, &taken);
        assert!(renames.is_empty());
        assert_eq!(tasks[0].id, "REPL-3");
    }

    #[test]
    fn renumber_prefixed_collisions() {
        let taken: std::collections::HashSet<String> =
            ["GEN-1", "GEN-2", "GEN-3"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let mut tasks = vec![
            TaskDef {
                id: "GEN-1".into(),
                title: "A".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec![],
                            manual: false,
            },
            TaskDef {
                id: "GEN-2".into(),
                title: "B".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec!["GEN-1".into()],
                            manual: false,
            },
        ];
        let renames = renumber_collisions(&mut tasks, &taken);
        assert_eq!(renames.len(), 2);
        assert_eq!(tasks[0].id, "GEN-4");
        assert_eq!(tasks[1].id, "GEN-5");
        // blocked_by references updated
        assert_eq!(tasks[1].blocked_by, vec!["GEN-4"]);
    }

    #[test]
    fn renumber_non_prefixed_collisions() {
        let taken: std::collections::HashSet<String> =
            ["T1"].iter().map(|s| s.to_string()).collect();
        let mut tasks = vec![TaskDef {
            id: "T1".into(),
            title: "A".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec![],
                    manual: false,
        }];
        let renames = renumber_collisions(&mut tasks, &taken);
        assert_eq!(renames.len(), 1);
        assert_eq!(tasks[0].id, "T1-2");
    }

    #[test]
    fn validate_deps_catches_dangling() {
        let tasks = vec![TaskDef {
            id: "A".into(),
            title: "A".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec!["NONEXISTENT".into()],
                    manual: false,
        }];
        let empty = std::collections::HashSet::new();
        let err = validate_deps(&tasks, &empty).unwrap_err();
        assert!(
            err.to_string().contains("NONEXISTENT"),
            "error should name the bad ref: {err}"
        );
    }

    #[test]
    fn validate_deps_catches_cycle() {
        let tasks = vec![
            TaskDef {
                id: "A".into(),
                title: "A".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec!["B".into()],
                            manual: false,
            },
            TaskDef {
                id: "B".into(),
                title: "B".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec!["A".into()],
                            manual: false,
            },
        ];
        let empty = std::collections::HashSet::new();
        let err = validate_deps(&tasks, &empty).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("A") && msg.contains("B"),
            "error should name the cycle participants: {err}"
        );
        assert!(msg.contains("cycle"), "error should mention 'cycle': {err}");
    }

    #[test]
    fn validate_deps_catches_three_way_cycle() {
        let tasks = vec![
            TaskDef {
                id: "X".into(),
                title: "X".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec!["Z".into()],
                            manual: false,
            },
            TaskDef {
                id: "Y".into(),
                title: "Y".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec!["X".into()],
                            manual: false,
            },
            TaskDef {
                id: "Z".into(),
                title: "Z".into(),
                description: String::new(),
                priority: 3,
                blocked_by: vec!["Y".into()],
                            manual: false,
            },
        ];
        let empty = std::collections::HashSet::new();
        let err = validate_deps(&tasks, &empty).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("X") && msg.contains("Y") && msg.contains("Z"),
            "error should name all cycle participants: {err}"
        );
        assert!(msg.contains("cycle"), "error should mention 'cycle': {err}");
    }

    #[test]
    fn id_ranges_summary_empty() {
        assert_eq!(id_ranges_summary_from_ids(&[]), "");
    }

    #[test]
    fn id_ranges_summary_mixed_prefixes() {
        let ids: Vec<String> = ["REPL-1", "REPL-3", "REPL-5", "GETVAR-1", "GETVAR-3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let summary = id_ranges_summary_from_ids(&ids);
        assert!(
            summary.contains("REPL: 1 through 5 (next available: REPL-6)"),
            "got: {summary}"
        );
        assert!(
            summary.contains("GETVAR: 1 through 3 (next available: GETVAR-4)"),
            "got: {summary}"
        );
        assert!(summary.contains("MUST NOT reuse"));
    }

    #[test]
    fn id_ranges_summary_non_prefix_ids() {
        let ids: Vec<String> = ["T1", "T2"].iter().map(|s| s.to_string()).collect();
        let summary = id_ranges_summary_from_ids(&ids);
        // No PREFIX-N pattern, so falls back to listing raw IDs.
        assert!(summary.contains("T1"), "got: {summary}");
        assert!(summary.contains("T2"), "got: {summary}");
        assert!(summary.contains("MUST NOT reuse"));
    }

    #[test]
    fn id_ranges_summary_mix_of_prefixed_and_plain() {
        let ids: Vec<String> = ["AUTH-1", "AUTH-2", "PLAIN"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let summary = id_ranges_summary_from_ids(&ids);
        // The PREFIX-N ones are recognized; PLAIN is not PREFIX-N so won't appear as a range.
        assert!(
            summary.contains("AUTH: 1 through 2 (next available: AUTH-3)"),
            "got: {summary}"
        );
    }

    #[test]
    fn validate_deps_accepts_archived_id() {
        let tasks = vec![TaskDef {
            id: "B".into(),
            title: "B".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec!["ARCHIVED-1".into()],
                    manual: false,
        }];
        let mut extra = std::collections::HashSet::new();
        extra.insert("ARCHIVED-1");
        assert!(validate_deps(&tasks, &extra).is_ok());
    }
}
