use std::path::Path;

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

/// A sideband override written by `ralph skip/fail/reset` and
/// drained atomically by the orchestrator each iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Directive {
    pub task_id: String,
    pub action: DirectiveAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectiveAction {
    Skip,
    Fail,
    Reset,
}

/// Ralph's canonical task format. One JSON object per line
/// in a JSONL file. The planner produces these; `run` consumes
/// them. Execution metadata (attempts, phase) lives separately
/// in state.rs so this file stays clean for human review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub priority: u32,
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

/// Read tasks from a JSONL file: one JSON object per line.
pub async fn load_tasks(path: &Path) -> Result<Vec<Task>> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    parse_tasks(&contents)
}

/// Parse JSONL text into tasks. Blank lines are skipped.
/// Provides clear error messages pointing at the offending line.
pub fn parse_tasks(contents: &str) -> Result<Vec<Task>> {
    let tasks: Vec<Task> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str::<Task>(line).with_context(|| {
                let preview = if line.len() > 120 {
                    format!("{}...", &line[..120])
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
fn validate_tasks(tasks: &[Task]) -> Result<()> {
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

/// Validate that every `blocked_by` ID references an
/// actual task or a known extra ID (e.g. archived tasks).
/// Returns an error listing the dangling references,
/// preventing silent deadlocks.
pub fn validate_deps(tasks: &[Task], extra_ids: &std::collections::HashSet<&str>) -> Result<()> {
    let ids: std::collections::HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let mut bad = Vec::new();
    for t in tasks {
        for dep in &t.blocked_by {
            if !ids.contains(dep.as_str()) && !extra_ids.contains(dep.as_str()) {
                bad.push(format!("{} blocked_by unknown task {}", t.id, dep));
            }
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("dangling dependency references:\n  {}", bad.join("\n  "));
    }
}

/// Write tasks to a JSONL file.
#[allow(dead_code)]
pub async fn write_tasks(path: &Path, tasks: &[Task]) -> Result<()> {
    let mut buf = String::new();
    for t in tasks {
        let line = serde_json::to_string(t)?;
        buf.push_str(&line);
        buf.push('\n');
    }
    tokio::fs::write(path, buf).await?;
    Ok(())
}

/// Load tasks from the archive file, returning an empty vec if missing.
pub async fn load_archive(path: &Path) -> Result<Vec<Task>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    load_tasks(path).await
}

/// Move tasks from `tasks_path` to `archive_path`.
/// The caller must ensure all IDs exist and are eligible.
pub async fn archive_tasks(
    tasks_path: &Path,
    archive_path: &Path,
    ids: &std::collections::HashSet<&str>,
) -> Result<()> {
    let all = load_tasks(tasks_path).await?;
    let (to_archive, remaining): (Vec<Task>, Vec<Task>) =
        all.into_iter().partition(|t| ids.contains(t.id.as_str()));
    append_tasks(archive_path, &to_archive).await?;
    write_tasks(tasks_path, &remaining).await?;
    Ok(())
}

/// Move a task from `archive_path` back to `tasks_path`.
pub async fn restore_task(tasks_path: &Path, archive_path: &Path, task_id: &str) -> Result<()> {
    let archived = load_archive(archive_path).await?;
    let (to_restore, remaining): (Vec<Task>, Vec<Task>) =
        archived.into_iter().partition(|t| t.id == task_id);
    if to_restore.is_empty() {
        anyhow::bail!("task '{}' not found in archive", task_id);
    }
    append_tasks(tasks_path, &to_restore).await?;
    if remaining.is_empty() {
        tokio::fs::remove_file(archive_path).await?;
    } else {
        write_tasks(archive_path, &remaining).await?;
    }
    Ok(())
}

/// Append tasks to a JSONL file without rewriting existing content.
pub async fn append_tasks(path: &Path, tasks: &[Task]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut buf = String::new();
    for t in tasks {
        buf.push_str(&serde_json::to_string(t)?);
        buf.push('\n');
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(buf.as_bytes()).await?;
    Ok(())
}

/// Scan tasks for `PREFIX-N` ID patterns and return a human-readable
/// summary of taken ID ranges. Used to tell the planner which IDs
/// are already in use so it can avoid collisions.
///
/// Returns an empty string when there are no tasks.
pub fn id_ranges_summary(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return String::new();
    }

    // Collect (prefix, number) pairs from PREFIX-N patterns.
    let mut prefix_numbers: std::collections::BTreeMap<&str, Vec<u32>> =
        std::collections::BTreeMap::new();
    for t in tasks {
        if let Some((prefix, num_str)) = t.id.rsplit_once('-')
            && !prefix.is_empty()
            && let Ok(n) = num_str.parse::<u32>()
        {
            prefix_numbers.entry(prefix).or_default().push(n);
        }
    }

    if prefix_numbers.is_empty() {
        // No PREFIX-N IDs found — list the raw IDs so the planner
        // still knows they're taken.
        let ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        return format!(
            "The following task IDs are already in use: {}\n\n\
             You MUST NOT reuse any existing ID.\n",
            ids.join(", ")
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
    tasks: &mut [Task],
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

/// Generate the next auto-ID for dynamically discovered tasks.
/// Scans existing IDs for the `GEN-N` pattern and increments.
#[allow(dead_code)]
pub fn next_generated_id(existing: &[Task]) -> String {
    let max = existing
        .iter()
        .filter_map(|t| t.id.strip_prefix("GEN-")?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("GEN-{}", max + 1)
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
            Task {
                id: "A".into(),
                title: "Alpha".into(),
                description: "desc".into(),
                priority: 1,
                blocked_by: vec![],
            },
            Task {
                id: "B".into(),
                title: "Beta".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec!["A".into()],
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
            Task {
                id: "A".into(),
                title: "A".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec![],
            },
            Task {
                id: "B".into(),
                title: "B".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec!["A".into()],
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
    fn next_generated_id_empty() {
        assert_eq!(next_generated_id(&[]), "GEN-1");
    }

    #[test]
    fn next_generated_id_increments() {
        let tasks = vec![
            Task {
                id: "GEN-3".into(),
                title: "A".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec![],
            },
            Task {
                id: "T1".into(),
                title: "B".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec![],
            },
        ];
        assert_eq!(next_generated_id(&tasks), "GEN-4");
    }

    #[test]
    fn next_generated_id_ignores_non_gen() {
        let tasks = vec![Task {
            id: "FIX-10".into(),
            title: "A".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec![],
        }];
        assert_eq!(next_generated_id(&tasks), "GEN-1");
    }

    #[test]
    fn renumber_no_collisions() {
        let taken: std::collections::HashSet<String> =
            ["REPL-1", "REPL-2"].iter().map(|s| s.to_string()).collect();
        let mut tasks = vec![Task {
            id: "REPL-3".into(),
            title: "A".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec![],
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
            Task {
                id: "GEN-1".into(),
                title: "A".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec![],
            },
            Task {
                id: "GEN-2".into(),
                title: "B".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec!["GEN-1".into()],
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
        let mut tasks = vec![Task {
            id: "T1".into(),
            title: "A".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec![],
        }];
        let renames = renumber_collisions(&mut tasks, &taken);
        assert_eq!(renames.len(), 1);
        assert_eq!(tasks[0].id, "T1-2");
    }

    #[test]
    fn validate_deps_catches_dangling() {
        let tasks = vec![Task {
            id: "A".into(),
            title: "A".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec!["NONEXISTENT".into()],
        }];
        let empty = std::collections::HashSet::new();
        let err = validate_deps(&tasks, &empty).unwrap_err();
        assert!(
            err.to_string().contains("NONEXISTENT"),
            "error should name the bad ref: {err}"
        );
    }

    #[test]
    fn id_ranges_summary_empty() {
        assert_eq!(id_ranges_summary(&[]), "");
    }

    #[test]
    fn id_ranges_summary_mixed_prefixes() {
        let tasks = vec![
            Task {
                id: "REPL-1".into(),
                title: "A".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec![],
            },
            Task {
                id: "REPL-3".into(),
                title: "B".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec![],
            },
            Task {
                id: "REPL-5".into(),
                title: "C".into(),
                description: String::new(),
                priority: 3,
                blocked_by: vec![],
            },
            Task {
                id: "GETVAR-1".into(),
                title: "D".into(),
                description: String::new(),
                priority: 4,
                blocked_by: vec![],
            },
            Task {
                id: "GETVAR-3".into(),
                title: "E".into(),
                description: String::new(),
                priority: 5,
                blocked_by: vec![],
            },
        ];
        let summary = id_ranges_summary(&tasks);
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
        let tasks = vec![
            Task {
                id: "T1".into(),
                title: "A".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec![],
            },
            Task {
                id: "T2".into(),
                title: "B".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec![],
            },
        ];
        let summary = id_ranges_summary(&tasks);
        // No PREFIX-N pattern, so falls back to listing raw IDs.
        assert!(summary.contains("T1"), "got: {summary}");
        assert!(summary.contains("T2"), "got: {summary}");
        assert!(summary.contains("MUST NOT reuse"));
    }

    #[test]
    fn id_ranges_summary_mix_of_prefixed_and_plain() {
        let tasks = vec![
            Task {
                id: "AUTH-1".into(),
                title: "A".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec![],
            },
            Task {
                id: "AUTH-2".into(),
                title: "B".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec![],
            },
            Task {
                id: "PLAIN".into(),
                title: "C".into(),
                description: String::new(),
                priority: 3,
                blocked_by: vec![],
            },
        ];
        let summary = id_ranges_summary(&tasks);
        // The PREFIX-N ones are recognized; PLAIN is not PREFIX-N so won't appear as a range.
        assert!(
            summary.contains("AUTH: 1 through 2 (next available: AUTH-3)"),
            "got: {summary}"
        );
    }

    #[test]
    fn validate_deps_accepts_archived_id() {
        let tasks = vec![Task {
            id: "B".into(),
            title: "B".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec!["ARCHIVED-1".into()],
        }];
        let mut extra = std::collections::HashSet::new();
        extra.insert("ARCHIVED-1");
        assert!(validate_deps(&tasks, &extra).is_ok());
    }

    #[tokio::test]
    async fn archive_restore_roundtrip() {
        let dir = std::env::temp_dir()
            .join("ralph_task_tests")
            .join("archive_roundtrip")
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let tasks_path = dir.join("tasks.jsonl");
        let archive_path = dir.join("archive.jsonl");

        let tasks = vec![
            Task {
                id: "T1".into(),
                title: "Done task".into(),
                description: String::new(),
                priority: 1,
                blocked_by: vec![],
            },
            Task {
                id: "T2".into(),
                title: "Active task".into(),
                description: String::new(),
                priority: 2,
                blocked_by: vec![],
            },
        ];
        write_tasks(&tasks_path, &tasks).await.unwrap();

        // Archive T1
        let mut ids = std::collections::HashSet::new();
        ids.insert("T1");
        archive_tasks(&tasks_path, &archive_path, &ids)
            .await
            .unwrap();

        let remaining = load_tasks(&tasks_path).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "T2");

        let archived = load_archive(&archive_path).await.unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, "T1");

        // Restore T1
        restore_task(&tasks_path, &archive_path, "T1")
            .await
            .unwrap();

        let restored = load_tasks(&tasks_path).await.unwrap();
        assert_eq!(restored.len(), 2);
        assert!(restored.iter().any(|t| t.id == "T1"));
        assert!(!archive_path.exists()); // archive removed when empty

        std::fs::remove_dir_all(&dir).ok();
    }
}
