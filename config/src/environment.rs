use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
struct EnvironmentTomlFile {
    environment: EnvironmentConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnvironmentConfig {
    pub name: String,
    pub description: String,
    pub image: String,
    pub network_policy: String,
    pub approval_mode: String,
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
        Ok(wrapped.environment)
    }
}
