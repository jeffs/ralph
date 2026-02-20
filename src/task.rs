use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
/// actual task. Returns an error listing the dangling
/// references, preventing silent deadlocks.
pub fn validate_deps(tasks: &[Task]) -> Result<()> {
    let ids: std::collections::HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let mut bad = Vec::new();
    for t in tasks {
        for dep in &t.blocked_by {
            if !ids.contains(dep.as_str()) {
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
        assert!(validate_deps(&tasks).is_ok());
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
    fn validate_deps_catches_dangling() {
        let tasks = vec![Task {
            id: "A".into(),
            title: "A".into(),
            description: String::new(),
            priority: 1,
            blocked_by: vec!["NONEXISTENT".into()],
        }];
        let err = validate_deps(&tasks).unwrap_err();
        assert!(
            err.to_string().contains("NONEXISTENT"),
            "error should name the bad ref: {err}"
        );
    }
}
