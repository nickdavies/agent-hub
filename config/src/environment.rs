use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentTomlFile {
    environment: EnvironmentConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
    pub name: String,
    pub description: String,
    pub image: String,
    pub network_policy: String,
    pub approval_mode: String,
    #[serde(default)]
    pub approval_bundle: Option<String>,
    #[serde(default)]
    pub allowed_overlays: Vec<String>,
    #[serde(default)]
    pub include_skills: Vec<String>,
    #[serde(default)]
    pub include_agents: Vec<String>,
    /// Optional runtime preference; mirrors the field in
    /// agent-hub-configs/environments/<env>/environment.toml.
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub discovery: Option<protocol::DiscoveryMetadata>,
    #[serde(default = "default_max_timeout_secs")]
    pub max_timeout_secs: u64,
}

fn default_max_timeout_secs() -> u64 {
    86_400
}

impl EnvironmentConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read environment config {}: {e}", path.display()))?;
        Self::from_toml(&raw, &path.display().to_string())
    }

    pub fn from_toml(raw: &str, label: &str) -> Result<Self, String> {
        let wrapped: EnvironmentTomlFile = toml::from_str(raw)
            .map_err(|e| format!("failed to parse environment config {label}: {e}"))?;
        if let Some(discovery) = &wrapped.environment.discovery {
            discovery.validate()?;
        }
        Ok(wrapped.environment)
    }
}

#[cfg(test)]
mod tests {
    use super::EnvironmentConfig;

    const ENVIRONMENT: &str = r#"
[environment]
name = "development"
description = "Development"
image = "agent-hub/job-code:locked"
network_policy = "build"
approval_mode = "gateway"
"#;

    #[test]
    fn environment_toml_rejects_unknown_fields_at_every_level() {
        assert_eq!(
            EnvironmentConfig::from_toml(ENVIRONMENT, "test")
                .unwrap()
                .max_timeout_secs,
            86_400
        );
        assert!(
            EnvironmentConfig::from_toml(ENVIRONMENT, "test")
                .unwrap()
                .discovery
                .is_none()
        );
        assert_eq!(
            EnvironmentConfig::from_toml(
                &format!("{ENVIRONMENT}\nmax_timeout_secs = 900\n"),
                "test",
            )
            .unwrap()
            .max_timeout_secs,
            900
        );
        assert!(
            EnvironmentConfig::from_toml(&format!("{ENVIRONMENT}\nunknown = true\n"), "test")
                .is_err()
        );
        assert!(
            EnvironmentConfig::from_toml(&format!("unknown = true\n{ENVIRONMENT}"), "test")
                .is_err()
        );
        let discovered = EnvironmentConfig::from_toml(
            &format!(
                "{ENVIRONMENT}\n[environment.discovery]\nrecommended_for = ['Routine implementation, refactoring, and test changes with automatic tool use']\nselection_notes = ['Choose a GitHub overlay only when the task needs authenticated repository access', 'Use a supervised environment when tool calls require human review']\n"
            ),
            "test",
        )
        .unwrap();
        assert_eq!(
            discovered.discovery.unwrap().recommended_for,
            ["Routine implementation, refactoring, and test changes with automatic tool use"]
        );
        assert!(EnvironmentConfig::from_toml(
            &format!("{ENVIRONMENT}\n[environment.discovery]\nrecommended_for = []\nselection_notes = ['note']\n"),
            "test",
        )
        .is_err());
    }
}
