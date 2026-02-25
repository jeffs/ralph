use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Per-role model configuration. All fields are required — deserialization
/// fails if any role is missing from the `[models]` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub planner: String,
    pub implementer: String,
    pub tester: String,
    pub reviewer: String,
    pub triager: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            planner: "opus".to_string(),
            implementer: "sonnet".to_string(),
            tester: "haiku".to_string(),
            reviewer: "opus".to_string(),
            triager: "opus".to_string(),
        }
    }
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
    /// Per-role model configuration (all roles required)
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
    /// Automatically triage open nits after final review
    #[serde(default = "default_auto_triage")]
    pub auto_triage: bool,
    /// Maximum number of triage rounds per run
    #[serde(default = "default_max_triage_rounds")]
    pub max_triage_rounds: u32,
    /// Stderr patterns that indicate the agent is stuck (e.g. waiting
    /// for a file lock). When detected, the monitor kills the agent
    /// after a grace period.
    #[serde(default = "default_stuck_patterns")]
    pub stuck_patterns: Vec<String>,
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

fn default_auto_triage() -> bool {
    true
}

fn default_max_triage_rounds() -> u32 {
    3
}

pub(crate) fn default_stuck_patterns() -> Vec<String> {
    vec![
        "Blocking waiting for file lock".to_string(),
        "waiting for lock".to_string(),
    ]
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
            models: ModelConfig::default(),
            max_attempts: default_max_attempts(),
            agent_timeout_secs: default_agent_timeout_secs(),
            agent_idle_timeout_secs: default_agent_idle_timeout_secs(),
            prompts_dir: default_prompts_dir(),
            env: EnvConfig::default(),
            kill_grace_secs: default_kill_grace_secs(),
            max_cost_usd: None,
            escalation_after: default_escalation_after(),
            escalation_model: None,
            auto_triage: default_auto_triage(),
            max_triage_rounds: default_max_triage_rounds(),
            stuck_patterns: default_stuck_patterns(),
        }
    }
}

impl ModelConfig {
    /// Look up the model for a role by its label name.
    /// Panics on unknown role names (programming error).
    fn get(&self, role: &str) -> &str {
        match role {
            "planner" => &self.planner,
            "implementer" => &self.implementer,
            "tester" => &self.tester,
            "reviewer" => &self.reviewer,
            "triager" => &self.triager,
            _ => panic!("unknown agent role: {role}"),
        }
    }
}

impl Config {
    /// Resolve the model string for a given role label.
    pub fn model_for(&self, role: &str) -> &str {
        self.models.get(role)
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
        if role == "tester" && attempt > self.escalation_after {
            if let Some(ref m) = self.escalation_model {
                return m;
            }
            return "sonnet";
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

    /// Minimal `[models]` section for tests that don't care about models.
    const MODELS_TOML: &str = r#"
[models]
planner = "opus"
implementer = "sonnet"
tester = "haiku"
reviewer = "opus"
triager = "opus"
"#;

    #[test]
    fn model_for_returns_configured_models() {
        let config = Config::default();
        assert_eq!(config.model_for("planner"), "opus");
        assert_eq!(config.model_for("implementer"), "sonnet");
        assert_eq!(config.model_for("tester"), "haiku");
        assert_eq!(config.model_for("reviewer"), "opus");
        assert_eq!(config.model_for("triager"), "opus");
    }

    #[test]
    fn deserialize_all_models_explicit() {
        let toml_str = r#"
[models]
planner = "opus"
implementer = "haiku"
tester = "haiku"
reviewer = "opus"
triager = "sonnet"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.model_for("planner"), "opus");
        assert_eq!(config.model_for("implementer"), "haiku");
        assert_eq!(config.model_for("tester"), "haiku");
        assert_eq!(config.model_for("reviewer"), "opus");
        assert_eq!(config.model_for("triager"), "sonnet");
    }

    #[test]
    fn missing_models_section_fails() {
        let result = toml::from_str::<Config>("");
        assert!(result.is_err());
    }

    #[test]
    fn missing_single_role_fails() {
        let toml_str = r#"
[models]
planner = "opus"
implementer = "sonnet"
tester = "sonnet"
reviewer = "opus"
"#;
        let result = toml::from_str::<Config>(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_agent_idle_timeout_secs() {
        let toml_str = format!("agent_idle_timeout_secs = 60\n{MODELS_TOML}");
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.agent_idle_timeout_secs, 60);
    }

    #[test]
    fn deserialize_kill_grace_secs() {
        let toml_str = format!("kill_grace_secs = 10\n{MODELS_TOML}");
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.kill_grace_secs, 10);
    }

    #[test]
    fn kill_grace_secs_defaults_to_5() {
        let config: Config = toml::from_str(MODELS_TOML).unwrap();
        assert_eq!(config.kill_grace_secs, 5);
    }

    #[test]
    fn stuck_patterns_defaults_to_cargo() {
        let config: Config = toml::from_str(MODELS_TOML).unwrap();
        assert_eq!(config.stuck_patterns.len(), 2);
        assert!(
            config
                .stuck_patterns
                .iter()
                .any(|p| p.contains("file lock"))
        );
    }

    #[test]
    fn deserialize_custom_stuck_patterns() {
        let toml_str = format!(
            "stuck_patterns = [\"deadlock detected\", \"acquire lock\"]\n{MODELS_TOML}"
        );
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.stuck_patterns.len(), 2);
        assert_eq!(config.stuck_patterns[0], "deadlock detected");
    }

    #[test]
    fn deserialize_env_config() {
        let toml_str = format!(
            r#"{MODELS_TOML}
[env]
passthrough = ["MY_TOKEN", "CUSTOM_VAR"]

[env.set]
FOO = "bar"
BAZ = "qux"
"#
        );
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.env.passthrough, vec!["MY_TOKEN", "CUSTOM_VAR"]);
        assert_eq!(config.env.set.get("FOO").map(|s| s.as_str()), Some("bar"));
        assert_eq!(config.env.set.get("BAZ").map(|s| s.as_str()), Some("qux"));
    }

    #[test]
    fn deserialize_max_cost_usd() {
        let toml_str = format!("max_cost_usd = 5.0\n{MODELS_TOML}");
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.max_cost_usd, Some(5.0));
    }

    #[test]
    fn max_cost_usd_defaults_to_none() {
        let config: Config = toml::from_str(MODELS_TOML).unwrap();
        assert_eq!(config.max_cost_usd, None);
    }

    #[test]
    fn escalation_after_defaults_to_2() {
        let config: Config = toml::from_str(MODELS_TOML).unwrap();
        assert_eq!(config.escalation_after, 2);
        assert_eq!(config.escalation_model, None);
    }

    #[test]
    fn deserialize_escalation_config() {
        let toml_str = format!(
            "escalation_after = 3\nescalation_model = \"opus\"\n{MODELS_TOML}"
        );
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.escalation_after, 3);
        assert_eq!(config.escalation_model.as_deref(), Some("opus"));
    }

    #[test]
    fn model_for_attempt_no_escalation() {
        let config = Config::default();
        assert_eq!(config.model_for_attempt("implementer", 1), "sonnet");
        assert_eq!(config.model_for_attempt("implementer", 2), "sonnet");
    }

    #[test]
    fn model_for_attempt_escalates_after_threshold() {
        let config = Config::default();
        // Attempt 3 exceeds threshold → falls back to reviewer model (opus).
        assert_eq!(config.model_for_attempt("implementer", 3), "opus");
    }

    #[test]
    fn model_for_attempt_uses_explicit_escalation_model() {
        let toml_str = format!("escalation_model = \"haiku\"\n{MODELS_TOML}");
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.model_for_attempt("implementer", 3), "haiku");
    }

    #[test]
    fn auto_triage_defaults_to_true() {
        let config: Config = toml::from_str(MODELS_TOML).unwrap();
        assert!(config.auto_triage);
        assert_eq!(config.max_triage_rounds, 3);
    }

    #[test]
    fn deserialize_triage_config() {
        let toml_str = format!(
            "auto_triage = false\nmax_triage_rounds = 5\n{MODELS_TOML}"
        );
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert!(!config.auto_triage);
        assert_eq!(config.max_triage_rounds, 5);
    }

    #[test]
    fn triager_model_defaults_to_opus() {
        let config = Config::default();
        assert_eq!(config.model_for("triager"), "opus");
    }

    #[test]
    fn model_for_attempt_escalates_tester_to_sonnet() {
        let config = Config::default();
        assert_eq!(config.model_for_attempt("tester", 1), "haiku");
        assert_eq!(config.model_for_attempt("tester", 2), "haiku");
        // Attempt 3 exceeds threshold → escalates to sonnet.
        assert_eq!(config.model_for_attempt("tester", 3), "sonnet");
    }

    #[test]
    fn model_for_attempt_tester_uses_explicit_escalation_model() {
        let toml_str = format!("escalation_model = \"opus\"\n{MODELS_TOML}");
        let config: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.model_for_attempt("tester", 2), "haiku");
        assert_eq!(config.model_for_attempt("tester", 3), "opus");
    }

    #[test]
    fn model_for_attempt_ignores_non_implementer_non_tester() {
        let config = Config::default();
        // Reviewer is not affected by escalation.
        assert_eq!(config.model_for_attempt("reviewer", 5), "opus");
    }
}
