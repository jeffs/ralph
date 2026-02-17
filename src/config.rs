use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Model to use for agent invocations
    #[serde(default = "default_model")]
    pub model: String,
    /// Max attempts per task before marking failed
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Directory containing prompt templates
    #[serde(default = "default_prompts_dir")]
    pub prompts_dir: PathBuf,
}

fn default_model() -> String {
    "sonnet".to_string()
}

fn default_max_attempts() -> u32 {
    3
}

fn default_prompts_dir() -> PathBuf {
    // Resolve relative to the ralph binary's location,
    // falling back to a compile-time default.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("prompts");
        if candidate.is_dir() {
            return candidate;
        }
        // Check sibling of the target dir (dev layout)
        let candidate = dir.ancestors().find_map(|a| {
            let p = a.join("prompts");
            p.is_dir().then_some(p)
        });
        if let Some(p) = candidate {
            return p;
        }
    }
    PathBuf::from("prompts")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: default_model(),
            max_attempts: default_max_attempts(),
            prompts_dir: default_prompts_dir(),
        }
    }
}

impl Config {
    pub async fn load() -> Result<Self> {
        let path = PathBuf::from(".ralph/config.toml");
        if path.exists() {
            let contents = tokio::fs::read_to_string(&path).await?;
            let config: Config = toml::from_str(&contents)?;
            Ok(config)
        } else {
            Ok(Config::default())
        }
    }
}
