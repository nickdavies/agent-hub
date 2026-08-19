use std::path::Path;

use serde::Deserialize;

/// Top-level structure of a triage prompt manifest TOML.
///
/// Wraps the prompt under a `[prompt]` table, mirroring how environments and
/// other config artifacts are structured.
#[derive(Debug, Clone, Deserialize)]
struct TriageManifest {
    prompt: TriagePromptDefinition,
}

/// Triage agent prompt — used by the `__triage__` pseudo-workflow as the
/// universal ingress route into any selectable workflow.
///
/// `system` is optional; `user` is the rendered template body and is required.
/// Templates accept `{{name}}` placeholders that the orchestration layer
/// resolves against the work-item interpolation context (task title, repo
/// catalog hint, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct TriagePromptDefinition {
    #[serde(default)]
    pub system: Option<String>,
    pub user: String,
}

impl TriagePromptDefinition {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "failed to read triage prompt config {}: {e}",
                path.display()
            )
        })?;
        let wrapped: TriageManifest = toml::from_str(&raw).map_err(|e| {
            format!(
                "failed to parse triage prompt config {}: {e}",
                path.display()
            )
        })?;
        Ok(wrapped.prompt)
    }
}
