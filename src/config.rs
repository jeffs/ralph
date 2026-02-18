use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspaceConfig {
    /// Paths to symlink from project root into each workspace
    #[serde(default)]
    pub shared: Vec<String>,
}

/// Per-role model overrides. Any omitted role falls back to the
/// top-level `model` field.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implementer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tester: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Default model for all agent roles
    #[serde(default = "default_model")]
    pub model: String,
    /// Per-role model overrides
    #[serde(default, skip_serializing_if = "ModelConfig::all_default")]
    pub models: ModelConfig,
    /// Max attempts per task before marking failed
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Directory containing prompt templates
    #[serde(default = "default_prompts_dir")]
    pub prompts_dir: PathBuf,
    /// Workspace isolation settings
    #[serde(default)]
    pub workspace: WorkspaceConfig,
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
            models: ModelConfig::default(),
            max_attempts: default_max_attempts(),
            prompts_dir: default_prompts_dir(),
            workspace: WorkspaceConfig::default(),
        }
    }
}

impl ModelConfig {
    fn all_default(&self) -> bool {
        self.planner.is_none()
            && self.implementer.is_none()
            && self.tester.is_none()
            && self.reviewer.is_none()
    }

    /// Look up the override for a role by its label name.
    fn get(&self, role: &str) -> Option<&str> {
        let field = match role {
            "planner" => &self.planner,
            "implementer" => &self.implementer,
            "tester" => &self.tester,
            "reviewer" => &self.reviewer,
            _ => return None,
        };
        field.as_deref()
    }
}

impl Config {
    /// Resolve the model string for a given role label.
    /// Checks per-role overrides first, falls back to the default `model`.
    pub fn model_for(&self, role: &str) -> &str {
        self.models.get(role).unwrap_or(&self.model)
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_for_falls_back_to_default() {
        let config = Config::default();
        assert_eq!(config.model_for("planner"), "sonnet");
        assert_eq!(config.model_for("implementer"), "sonnet");
        assert_eq!(config.model_for("tester"), "sonnet");
        assert_eq!(config.model_for("reviewer"), "sonnet");
    }

    #[test]
    fn model_for_respects_per_role_override() {
        let config = Config {
            models: ModelConfig {
                planner: Some("opus".to_string()),
                reviewer: Some("opus".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(config.model_for("planner"), "opus");
        assert_eq!(config.model_for("implementer"), "sonnet");
        assert_eq!(config.model_for("tester"), "sonnet");
        assert_eq!(config.model_for("reviewer"), "opus");
    }

    #[test]
    fn deserialize_simple_model_only() {
        let toml_str = r#"model = "haiku""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.model_for("planner"), "haiku");
        assert_eq!(config.model_for("implementer"), "haiku");
    }

    #[test]
    fn deserialize_with_per_role_overrides() {
        let toml_str = r#"
model = "sonnet"

[models]
planner = "opus"
reviewer = "opus"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.model_for("planner"), "opus");
        assert_eq!(config.model_for("implementer"), "sonnet");
        assert_eq!(config.model_for("tester"), "sonnet");
        assert_eq!(config.model_for("reviewer"), "opus");
    }
}
