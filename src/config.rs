use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Paths to symlink from project root into each workspace
    #[serde(default)]
    pub shared: Vec<String>,
    /// When true, each workspace gets its own CARGO_TARGET_DIR
    /// to avoid cargo lock contention. Increases disk usage.
    #[serde(default = "default_isolate_target_dir")]
    pub isolate_target_dir: bool,
}

fn default_isolate_target_dir() -> bool {
    true
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            shared: Vec::new(),
            isolate_target_dir: default_isolate_target_dir(),
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EnvConfig {
    /// Additional env var names to forward beyond the hardcoded essentials
    #[serde(default)]
    pub passthrough: Vec<String>,
    /// Explicit key=value env var overrides
    #[serde(default)]
    pub set: HashMap<String, String>,
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
    /// Wall-clock timeout per agent invocation in seconds
    #[serde(default = "default_agent_timeout_secs")]
    pub agent_timeout_secs: u64,
    /// Idle timeout per agent invocation in seconds
    #[serde(default = "default_agent_idle_timeout_secs")]
    pub agent_idle_timeout_secs: u64,
    /// Directory containing prompt templates
    #[serde(default = "default_prompts_dir")]
    pub prompts_dir: PathBuf,
    /// Workspace isolation settings
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    /// Environment variable forwarding and override configuration
    #[serde(default)]
    pub env: EnvConfig,
    /// Grace period between SIGTERM and SIGKILL in seconds
    #[serde(default = "default_kill_grace_secs")]
    pub kill_grace_secs: u64,
    /// Maximum cumulative cost (USD) before stopping the run
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Number of failed attempts before escalating to a stronger model
    #[serde(default = "default_escalation_after")]
    pub escalation_after: u32,
    /// Model to escalate to (defaults to models.reviewer if unset)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_model: Option<String>,
}

fn default_model() -> String {
    "sonnet".to_string()
}

fn default_max_attempts() -> u32 {
    3
}

fn default_agent_timeout_secs() -> u64 {
    1800 // 30 minutes
}

fn default_agent_idle_timeout_secs() -> u64 {
    180
}

fn default_kill_grace_secs() -> u64 {
    5
}

fn default_escalation_after() -> u32 {
    2
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
            agent_timeout_secs: default_agent_timeout_secs(),
            agent_idle_timeout_secs: default_agent_idle_timeout_secs(),
            prompts_dir: default_prompts_dir(),
            workspace: WorkspaceConfig::default(),
            env: EnvConfig::default(),
            kill_grace_secs: default_kill_grace_secs(),
            max_cost_usd: None,
            escalation_after: default_escalation_after(),
            escalation_model: None,
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

    /// Resolve the model for a given role and attempt number.
    /// When the implementer exceeds `escalation_after` attempts, returns
    /// `escalation_model` (or the reviewer model as fallback).
    pub fn model_for_attempt(&self, role: &str, attempt: u32) -> &str {
        if role == "implementer" && attempt > self.escalation_after {
            if let Some(ref m) = self.escalation_model {
                return m;
            }
            return self.model_for("reviewer");
        }
        self.model_for(role)
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

    #[test]
    fn deserialize_agent_idle_timeout_secs() {
        let toml_str = r#"agent_idle_timeout_secs = 60"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent_idle_timeout_secs, 60);
    }

    #[test]
    fn deserialize_kill_grace_secs() {
        let toml_str = r#"kill_grace_secs = 10"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.kill_grace_secs, 10);
    }

    #[test]
    fn kill_grace_secs_defaults_to_5() {
        let toml_str = r#"model = "sonnet""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.kill_grace_secs, 5);
    }

    #[test]
    fn isolate_target_dir_defaults_to_true() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.workspace.isolate_target_dir);
    }

    #[test]
    fn isolate_target_dir_can_be_disabled() {
        let toml_str = r#"
[workspace]
isolate_target_dir = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.workspace.isolate_target_dir);
    }

    #[test]
    fn deserialize_env_config() {
        let toml_str = r#"
[env]
passthrough = ["MY_TOKEN", "CUSTOM_VAR"]

[env.set]
FOO = "bar"
BAZ = "qux"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.env.passthrough, vec!["MY_TOKEN", "CUSTOM_VAR"]);
        assert_eq!(config.env.set.get("FOO").map(|s| s.as_str()), Some("bar"));
        assert_eq!(config.env.set.get("BAZ").map(|s| s.as_str()), Some("qux"));
    }

    #[test]
    fn deserialize_max_cost_usd() {
        let toml_str = r#"max_cost_usd = 5.0"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_cost_usd, Some(5.0));
    }

    #[test]
    fn max_cost_usd_defaults_to_none() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.max_cost_usd, None);
    }

    #[test]
    fn escalation_after_defaults_to_2() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.escalation_after, 2);
        assert_eq!(config.escalation_model, None);
    }

    #[test]
    fn deserialize_escalation_config() {
        let toml_str = r#"
escalation_after = 3
escalation_model = "opus"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.escalation_after, 3);
        assert_eq!(config.escalation_model.as_deref(), Some("opus"));
    }

    #[test]
    fn model_for_attempt_no_escalation() {
        let config = Config::default();
        // Attempts 1 and 2 use the normal implementer model.
        assert_eq!(config.model_for_attempt("implementer", 1), "sonnet");
        assert_eq!(config.model_for_attempt("implementer", 2), "sonnet");
    }

    #[test]
    fn model_for_attempt_escalates_after_threshold() {
        let config = Config::default(); // escalation_after=2, no escalation_model
        // Attempt 3 exceeds threshold → falls back to reviewer model (sonnet by default).
        assert_eq!(config.model_for_attempt("implementer", 3), "sonnet");

        // With a reviewer override, escalation uses the reviewer model.
        let config = Config {
            models: ModelConfig {
                reviewer: Some("opus".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(config.model_for_attempt("implementer", 3), "opus");
    }

    #[test]
    fn model_for_attempt_uses_explicit_escalation_model() {
        let config: Config = toml::from_str(r#"escalation_model = "haiku""#).unwrap();
        assert_eq!(config.model_for_attempt("implementer", 3), "haiku");
    }

    #[test]
    fn model_for_attempt_ignores_non_implementer() {
        let config = Config {
            models: ModelConfig {
                reviewer: Some("opus".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        // Non-implementer roles are not affected by escalation.
        assert_eq!(config.model_for_attempt("tester", 5), "sonnet");
        assert_eq!(config.model_for_attempt("reviewer", 5), "opus");
    }
}
