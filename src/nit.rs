use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NitStatus {
    Open,
    Promoted,
    Dismissed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nit {
    pub id: String,
    pub source_task: String,
    pub source_role: String,
    pub attempt: u32,
    pub content: String,
    pub status: NitStatus,
    pub promoted_to: Option<String>,
    pub created_at: u64,
}

/// Load all nits from a JSONL file. Returns empty vec if file doesn't exist.
pub async fn load_nits(path: &Path) -> Result<Vec<Nit>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = tokio::fs::read_to_string(path).await?;
    parse_nits(&contents)
}

fn parse_nits(contents: &str) -> Result<Vec<Nit>> {
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

/// Append a single nit to the JSONL file (no rewrite).
pub async fn append_nit(path: &Path, nit: &Nit) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_string(nit)?;
    line.push('\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    Ok(())
}

/// Full rewrite of nits file (for status updates).
pub async fn save_nits(path: &Path, nits: &[Nit]) -> Result<()> {
    let mut buf = String::new();
    for nit in nits {
        buf.push_str(&serde_json::to_string(nit)?);
        buf.push('\n');
    }
    tokio::fs::write(path, buf).await?;
    Ok(())
}

/// Generate the next nit ID based on existing nits.
pub fn next_nit_id(nits: &[Nit]) -> String {
    let max = nits
        .iter()
        .filter_map(|n| n.id.strip_prefix("NIT-")?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("NIT-{}", max + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_nit(id: &str, status: NitStatus) -> Nit {
        Nit {
            id: id.into(),
            source_task: "T1".into(),
            source_role: "reviewer".into(),
            attempt: 1,
            content: "fix this".into(),
            status,
            promoted_to: None,
            created_at: 1000,
        }
    }

    #[test]
    fn next_nit_id_empty() {
        assert_eq!(next_nit_id(&[]), "NIT-1");
    }

    #[test]
    fn next_nit_id_increments() {
        let nits = vec![
            make_nit("NIT-1", NitStatus::Open),
            make_nit("NIT-3", NitStatus::Dismissed),
        ];
        assert_eq!(next_nit_id(&nits), "NIT-4");
    }

    #[test]
    fn roundtrip_jsonl() {
        let nit = Nit {
            id: "NIT-1".into(),
            source_task: "BUILD-4".into(),
            source_role: "reviewer".into(),
            attempt: 1,
            content: "manifest.json not updated".into(),
            status: NitStatus::Open,
            promoted_to: None,
            created_at: 1708300000,
        };
        let json = serde_json::to_string(&nit).unwrap();
        let parsed: Nit = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "NIT-1");
        assert_eq!(parsed.status, NitStatus::Open);
        assert!(parsed.promoted_to.is_none());
    }

    #[test]
    fn parse_nits_skips_blank_lines() {
        let input = concat!(
            r#"{"id":"NIT-1","source_task":"T1","source_role":"reviewer","attempt":1,"content":"fix","status":"open","promoted_to":null,"created_at":1000}"#,
            "\n\n",
            r#"{"id":"NIT-2","source_task":"T2","source_role":"tester","attempt":1,"content":"also","status":"open","promoted_to":null,"created_at":2000}"#,
            "\n"
        );
        let nits = parse_nits(input).unwrap();
        assert_eq!(nits.len(), 2);
    }

    #[test]
    fn status_update_roundtrip() {
        let mut nit = make_nit("NIT-1", NitStatus::Open);
        nit.status = NitStatus::Promoted;
        nit.promoted_to = Some("NIT1".into());
        let json = serde_json::to_string(&nit).unwrap();
        let parsed: Nit = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, NitStatus::Promoted);
        assert_eq!(parsed.promoted_to.as_deref(), Some("NIT1"));
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("ralph_nit_tests")
            .join(name)
            .join(format!("{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn append_and_load_roundtrip() {
        let dir = test_dir("append_load");
        let path = dir.join("nits.jsonl");

        append_nit(&path, &make_nit("NIT-1", NitStatus::Open))
            .await
            .unwrap();
        append_nit(&path, &make_nit("NIT-2", NitStatus::Open))
            .await
            .unwrap();

        let loaded = load_nits(&path).await.unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, "NIT-1");
        assert_eq!(loaded[1].id, "NIT-2");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn save_overwrites() {
        let dir = test_dir("save_overwrite");
        let path = dir.join("nits.jsonl");

        let nits = vec![make_nit("NIT-1", NitStatus::Open)];
        save_nits(&path, &nits).await.unwrap();

        let mut updated = load_nits(&path).await.unwrap();
        updated[0].status = NitStatus::Dismissed;
        save_nits(&path, &updated).await.unwrap();

        let reloaded = load_nits(&path).await.unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].status, NitStatus::Dismissed);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn load_nits_missing_file() {
        let path = std::env::temp_dir().join("ralph_nit_missing_file.jsonl");
        let _ = std::fs::remove_file(&path);
        let nits = load_nits(&path).await.unwrap();
        assert!(nits.is_empty());
    }
}
