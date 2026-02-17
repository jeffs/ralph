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
pub fn parse_tasks(contents: &str) -> Result<Vec<Task>> {
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str::<Task>(line)
                .with_context(|| format!("parsing task on line {}", i + 1))
        })
        .collect()
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
}
