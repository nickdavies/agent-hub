//! Public, read-only capability discovery contract.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::host_run::{
    AGENT_HUB_RUN_MAX_MOUNTS, AGENT_HUB_RUN_MAX_PROMPT_BYTES, AGENT_HUB_RUN_MAX_SELECTORS,
};
use crate::{
    AgentHubRunImageFamily, AgentHubRunNetworkClass, AgentHubRunToolApproval,
    AttendedOutputContract, NetworkResourceKind, sha256_hex_with_domain,
};

pub const AGENT_HUB_CATALOG_SCHEMA_V1: &str = "agent_hub_catalog_v1";
pub const AGENT_HUB_CATALOG_MAX_BYTES: usize = 256 * 1024;
pub const AGENT_HUB_CATALOG_MAX_ENTRIES: usize = 64;
pub const AGENT_HUB_CATALOG_MAX_NAME_BYTES: usize = 128;
pub const AGENT_HUB_CATALOG_MAX_DESCRIPTION_BYTES: usize = 4096;
pub const AGENT_HUB_CATALOG_MAX_TOOL_FACTS: usize = 128;
pub const AGENT_HUB_CATALOG_MAX_TOOL_NAME_BYTES: usize = 64;
pub const AGENT_HUB_CATALOG_MAX_TOOL_VERSION_BYTES: usize = 256;
const CATALOG_FINGERPRINT_DOMAIN: &[u8] = b"agent-hub:catalog:v1\0";

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryMetadata {
    #[schemars(length(min = 1, max = 6), inner(length(min = 1, max = 160)))]
    pub recommended_for: Vec<String>,
    #[schemars(length(min = 1, max = 6), inner(length(min = 1, max = 160)))]
    pub selection_notes: Vec<String>,
}

impl DiscoveryMetadata {
    pub fn validate(&self) -> Result<(), String> {
        validate_discovery(Some(self))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogV1 {
    pub schema: AgentHubCatalogSchema,
    #[schemars(length(min = 64, max = 64))]
    pub fingerprint: String,
    #[schemars(length(max = 64))]
    pub environments: Vec<AgentHubCatalogEnvironmentV1>,
    #[schemars(length(max = 64))]
    pub images: Vec<AgentHubCatalogImageV1>,
    #[schemars(length(max = 64))]
    pub overlays: Vec<AgentHubCatalogOverlayV1>,
    pub guidance: AgentHubCatalogGuidanceV1,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum AgentHubCatalogSchema {
    #[serde(rename = "agent_hub_catalog_v1")]
    V1,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogEnvironmentV1 {
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    #[schemars(length(max = 4096))]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<DiscoveryMetadata>,
    pub image_family: AgentHubRunImageFamily,
    #[schemars(length(min = 1, max = 128))]
    pub network_options: Vec<AgentHubCatalogNetworkOptionV1>,
    #[schemars(length(min = 1, max = 2))]
    pub tool_approvals: Vec<AgentHubRunToolApproval>,
    #[schemars(length(max = 16), inner(length(min = 1, max = 128)))]
    pub overlays: Vec<String>,
    #[schemars(range(min = 1, max = 86400))]
    pub max_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogImageV1 {
    pub family: AgentHubRunImageFamily,
    #[schemars(length(max = 4096))]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<DiscoveryMetadata>,
    pub tool_facts: AgentHubCatalogToolFactsV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentHubCatalogToolFactsV1 {
    Unavailable,
    Available {
        #[schemars(length(min = 1, max = 128))]
        tools: Vec<AgentHubCatalogToolFactV1>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogToolFactV1 {
    #[schemars(length(min = 1, max = 64))]
    pub name: String,
    #[schemars(length(min = 1, max = 256))]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogOverlayV1 {
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    #[schemars(length(min = 1, max = 128))]
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 4096))]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<DiscoveryMetadata>,
    #[schemars(length(min = 1, max = 8), inner(length(min = 1, max = 128)))]
    pub scopes: Vec<String>,
    pub prerequisites: AgentHubCatalogOverlayPrerequisitesV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogNetworkOptionV1 {
    pub network_class: AgentHubRunNetworkClass,
    #[schemars(length(max = 7))]
    pub resources: Vec<NetworkResourceKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogOverlayPrerequisitesV1 {
    #[schemars(length(max = 2))]
    pub network_resources: Vec<NetworkResourceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<AgentHubCatalogCredentialPrerequisite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<AgentHubCatalogRepositoryPrerequisite>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 4), inner(length(min = 1, max = 256)))]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentHubCatalogCredentialPrerequisite {
    HostCommand,
    HostConfiguration,
    ConfiguredMcpBridge,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentHubCatalogRepositoryPrerequisite {
    MatchingRemoteHost,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogGuidanceV1 {
    pub limits: AgentHubCatalogLimitsV1,
    pub outputs: AgentHubCatalogOutputsV1,
    pub workspace: AgentHubCatalogWorkspaceGuidanceV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogLimitsV1 {
    pub max_prompt_bytes: u64,
    pub max_workspaces: u32,
    pub max_context_roots: u32,
    pub max_overlays: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogOutputsV1 {
    pub structured_result: bool,
    pub artifact_manifest: bool,
    pub repository_result: bool,
    pub cleanup_result: bool,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub transcript_bytes: u64,
    pub artifact_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCatalogWorkspaceGuidanceV1 {
    pub workspace_required: bool,
    pub primary_workspace_required: bool,
    pub context_roots_read_only: bool,
    pub workspace_export_supported: bool,
    pub workspace_export_is_untrusted: bool,
}

impl Default for AgentHubCatalogGuidanceV1 {
    fn default() -> Self {
        let output = AttendedOutputContract::default().limits;
        Self {
            limits: AgentHubCatalogLimitsV1 {
                max_prompt_bytes: AGENT_HUB_RUN_MAX_PROMPT_BYTES as u64,
                max_workspaces: AGENT_HUB_RUN_MAX_MOUNTS as u32,
                max_context_roots: AGENT_HUB_RUN_MAX_MOUNTS as u32,
                max_overlays: AGENT_HUB_RUN_MAX_SELECTORS as u32,
            },
            outputs: AgentHubCatalogOutputsV1 {
                structured_result: true,
                artifact_manifest: true,
                repository_result: true,
                cleanup_result: true,
                stdout_bytes: output.stdout_bytes,
                stderr_bytes: output.stderr_bytes,
                transcript_bytes: output.transcript_bytes,
                artifact_bytes: output.artifact_bytes,
            },
            workspace: AgentHubCatalogWorkspaceGuidanceV1 {
                workspace_required: true,
                primary_workspace_required: true,
                context_roots_read_only: true,
                workspace_export_supported: true,
                workspace_export_is_untrusted: true,
            },
        }
    }
}

impl AgentHubCatalogV1 {
    pub fn new(
        mut environments: Vec<AgentHubCatalogEnvironmentV1>,
        mut images: Vec<AgentHubCatalogImageV1>,
        mut overlays: Vec<AgentHubCatalogOverlayV1>,
    ) -> Result<Self, String> {
        environments.sort_by(|left, right| left.name.cmp(&right.name));
        for environment in &mut environments {
            for option in &mut environment.network_options {
                option.resources.sort_by_key(ToString::to_string);
            }
            environment
                .network_options
                .sort_by_key(network_option_sort_key);
            environment
                .tool_approvals
                .sort_by_key(|value| value.as_str());
            environment.overlays.sort();
        }
        for image in &mut images {
            if let AgentHubCatalogToolFactsV1::Available { tools } = &mut image.tool_facts {
                tools.sort_by(|left, right| left.name.cmp(&right.name));
            }
        }
        images.sort_by_key(|image| image.family.as_str());
        overlays.sort_by(|left, right| left.name.cmp(&right.name));
        for overlay in &mut overlays {
            overlay.scopes.sort();
            overlay
                .prerequisites
                .network_resources
                .sort_by_key(ToString::to_string);
        }
        let mut catalog = Self {
            schema: AgentHubCatalogSchema::V1,
            fingerprint: String::new(),
            environments,
            images,
            overlays,
            guidance: AgentHubCatalogGuidanceV1::default(),
        };
        catalog.fingerprint = catalog.expected_fingerprint()?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.environments.len() > AGENT_HUB_CATALOG_MAX_ENTRIES
            || self.images.len() > AGENT_HUB_CATALOG_MAX_ENTRIES
            || self.overlays.len() > AGENT_HUB_CATALOG_MAX_ENTRIES
        {
            return Err("catalog entry count exceeds limit".to_string());
        }
        require_sorted_unique(
            self.environments.iter().map(|item| item.name.as_str()),
            "environment",
        )?;
        require_sorted_unique(self.images.iter().map(|item| item.family.as_str()), "image")?;
        require_sorted_unique(
            self.overlays.iter().map(|item| item.name.as_str()),
            "overlay",
        )?;

        let image_families = self
            .images
            .iter()
            .map(|image| image.family.as_str())
            .collect::<BTreeSet<_>>();
        let overlay_names = self
            .overlays
            .iter()
            .map(|overlay| overlay.name.as_str())
            .collect::<BTreeSet<_>>();
        for environment in &self.environments {
            validate_text(
                &environment.name,
                1,
                AGENT_HUB_CATALOG_MAX_NAME_BYTES,
                "environment name",
            )?;
            validate_text(
                &environment.description,
                0,
                AGENT_HUB_CATALOG_MAX_DESCRIPTION_BYTES,
                "environment description",
            )?;
            validate_discovery(environment.discovery.as_ref())?;
            if !image_families.contains(environment.image_family.as_str()) {
                return Err(format!(
                    "environment '{}' references an unavailable image family",
                    environment.name
                ));
            }
            require_sorted_unique_owned(
                environment
                    .network_options
                    .iter()
                    .map(network_option_sort_key),
                "network option",
            )?;
            require_sorted_unique(
                environment.tool_approvals.iter().map(|item| item.as_str()),
                "tool approval",
            )?;
            require_sorted_unique(
                environment.overlays.iter().map(String::as_str),
                "environment overlay",
            )?;
            if environment.network_options.is_empty()
                || environment.network_options.len() > 128
                || environment.tool_approvals.is_empty()
                || environment.tool_approvals.len() > 2
                || environment.overlays.len() > 16
                || !(1..=86_400).contains(&environment.max_timeout_secs)
            {
                return Err(format!(
                    "environment '{}' capability bounds are invalid",
                    environment.name
                ));
            }
            for option in &environment.network_options {
                if option.resources.len() > 7 {
                    return Err(format!(
                        "environment '{}' network option exceeds resource limit",
                        environment.name
                    ));
                }
                require_sorted_unique_owned(
                    option.resources.iter().map(ToString::to_string),
                    "network resource",
                )?;
                if option.resources.contains(&NetworkResourceKind::GuardedWeb)
                    && option.resources.len() != 1
                {
                    return Err("guarded_web must be the sole network resource".to_string());
                }
                if !network_option_is_valid(option) {
                    return Err("catalog network option exceeds its class".to_string());
                }
            }
            if let Some(name) = environment
                .overlays
                .iter()
                .find(|name| !overlay_names.contains(name.as_str()))
            {
                return Err(format!(
                    "environment '{}' references unknown overlay '{name}'",
                    environment.name
                ));
            }
        }
        for image in &self.images {
            validate_text(
                &image.description,
                0,
                AGENT_HUB_CATALOG_MAX_DESCRIPTION_BYTES,
                "image description",
            )?;
            validate_discovery(image.discovery.as_ref())?;
            if let AgentHubCatalogToolFactsV1::Available { tools } = &image.tool_facts {
                if tools.is_empty() || tools.len() > AGENT_HUB_CATALOG_MAX_TOOL_FACTS {
                    return Err("catalog image tool fact count is invalid".to_string());
                }
                require_sorted_unique(tools.iter().map(|tool| tool.name.as_str()), "tool fact")?;
                for tool in tools {
                    validate_tool_name(&tool.name)?;
                    validate_text(
                        &tool.version,
                        1,
                        AGENT_HUB_CATALOG_MAX_TOOL_VERSION_BYTES,
                        "tool version",
                    )?;
                }
            }
        }
        for overlay in &self.overlays {
            validate_text(
                &overlay.name,
                1,
                AGENT_HUB_CATALOG_MAX_NAME_BYTES,
                "overlay name",
            )?;
            validate_text(
                &overlay.provider,
                1,
                AGENT_HUB_CATALOG_MAX_NAME_BYTES,
                "overlay provider",
            )?;
            if let Some(description) = &overlay.description {
                validate_text(
                    description,
                    0,
                    AGENT_HUB_CATALOG_MAX_DESCRIPTION_BYTES,
                    "overlay description",
                )?;
            }
            validate_discovery(overlay.discovery.as_ref())?;
            if overlay.scopes.is_empty() || overlay.scopes.len() > 8 {
                return Err("overlay scope count is invalid".to_string());
            }
            require_sorted_unique(overlay.scopes.iter().map(String::as_str), "overlay scope")?;
            for scope in &overlay.scopes {
                validate_text(scope, 1, AGENT_HUB_CATALOG_MAX_NAME_BYTES, "overlay scope")?;
            }
            require_sorted_unique_owned(
                overlay
                    .prerequisites
                    .network_resources
                    .iter()
                    .map(ToString::to_string),
                "overlay prerequisite network resource",
            )?;
            if overlay.prerequisites.network_resources.len() > 2
                || overlay.prerequisites.notes.len() > 4
            {
                return Err("overlay prerequisite count exceeds limit".to_string());
            }
            require_unique(
                overlay.prerequisites.notes.iter().map(String::as_str),
                "overlay prerequisite note",
            )?;
            for note in &overlay.prerequisites.notes {
                validate_text(note, 1, 256, "overlay prerequisite note")?;
            }
        }
        if self.guidance != AgentHubCatalogGuidanceV1::default() {
            return Err("catalog guidance is not canonical".to_string());
        }
        if self.fingerprint != self.expected_fingerprint()? {
            return Err("catalog fingerprint mismatch".to_string());
        }
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len()
            > AGENT_HUB_CATALOG_MAX_BYTES
        {
            return Err("catalog payload exceeds limit".to_string());
        }
        Ok(())
    }

    fn expected_fingerprint(&self) -> Result<String, String> {
        let mut projection = self.clone();
        projection.fingerprint.clear();
        let bytes = serde_json::to_vec(&projection).map_err(|error| error.to_string())?;
        Ok(sha256_hex_with_domain(CATALOG_FINGERPRINT_DOMAIN, &bytes))
    }
}

fn validate_discovery(metadata: Option<&DiscoveryMetadata>) -> Result<(), String> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    for (label, values) in [
        ("recommended_for", &metadata.recommended_for),
        ("selection_notes", &metadata.selection_notes),
    ] {
        if values.is_empty() || values.len() > 6 {
            return Err(format!("discovery {label} count is invalid"));
        }
        require_unique(values.iter().map(String::as_str), label)?;
        for value in values {
            if value.is_empty()
                || value.chars().count() > 160
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                return Err(format!(
                    "discovery {label} entry is outside its canonical bounds"
                ));
            }
        }
    }
    Ok(())
}

fn network_option_sort_key(option: &AgentHubCatalogNetworkOptionV1) -> String {
    format!(
        "{}:{}",
        option.network_class.as_str(),
        option
            .resources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn network_option_is_valid(option: &AgentHubCatalogNetworkOptionV1) -> bool {
    let allowed = match option.network_class {
        AgentHubRunNetworkClass::None => Vec::new(),
        AgentHubRunNetworkClass::LlmOnly => vec![NetworkResourceKind::Llm],
        AgentHubRunNetworkClass::Build => vec![
            NetworkResourceKind::Llm,
            NetworkResourceKind::Github,
            NetworkResourceKind::GoRegistry,
            NetworkResourceKind::CratesRegistry,
            NetworkResourceKind::PypiRegistry,
            NetworkResourceKind::NpmRegistry,
        ],
        AgentHubRunNetworkClass::GuardedWeb => vec![NetworkResourceKind::GuardedWeb],
    };
    option
        .resources
        .iter()
        .all(|resource| allowed.contains(resource))
}

fn validate_text(value: &str, min: usize, max: usize, label: &str) -> Result<(), String> {
    if value.len() < min
        || value.len() > max
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(format!("{label} is outside its canonical bounds"));
    }
    Ok(())
}

fn validate_tool_name(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > AGENT_HUB_CATALOG_MAX_TOOL_NAME_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.' | b'_'))
    {
        return Err("tool name is outside its canonical bounds".to_string());
    }
    Ok(())
}

fn require_sorted_unique<'a>(
    values: impl Iterator<Item = &'a str>,
    label: &str,
) -> Result<(), String> {
    let values = values.collect::<Vec<_>>();
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(format!("catalog {label} values are not sorted and unique"));
    }
    Ok(())
}

fn require_unique<'a>(values: impl Iterator<Item = &'a str>, label: &str) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(format!("catalog {label} values are not unique"));
        }
    }
    Ok(())
}

fn require_sorted_unique_owned(
    values: impl Iterator<Item = String>,
    label: &str,
) -> Result<(), String> {
    let values = values.collect::<Vec<_>>();
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(format!("catalog {label} values are not sorted and unique"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> AgentHubCatalogV1 {
        AgentHubCatalogV1::new(
            vec![AgentHubCatalogEnvironmentV1 {
                name: "code".into(),
                description: "Code safely".into(),
                discovery: Some(DiscoveryMetadata {
                    recommended_for: vec!["Code changes".into()],
                    selection_notes: vec!["Choose the least privilege".into()],
                }),
                image_family: AgentHubRunImageFamily::JobCode,
                network_options: vec![AgentHubCatalogNetworkOptionV1 {
                    network_class: AgentHubRunNetworkClass::None,
                    resources: Vec::new(),
                }],
                tool_approvals: vec![AgentHubRunToolApproval::Dippy],
                overlays: vec!["github:github.com:read_write".into()],
                max_timeout_secs: 600,
            }],
            vec![AgentHubCatalogImageV1 {
                family: AgentHubRunImageFamily::JobCode,
                description: "Job image".into(),
                discovery: None,
                tool_facts: AgentHubCatalogToolFactsV1::Unavailable,
            }],
            vec![AgentHubCatalogOverlayV1 {
                name: "github:github.com:read_write".into(),
                provider: "github".into(),
                description: None,
                discovery: None,
                scopes: vec!["repositories:read_write".into()],
                prerequisites: AgentHubCatalogOverlayPrerequisitesV1 {
                    network_resources: vec![NetworkResourceKind::Github],
                    credential: Some(AgentHubCatalogCredentialPrerequisite::HostConfiguration),
                    repository: Some(AgentHubCatalogRepositoryPrerequisite::MatchingRemoteHost),
                    notes: vec!["Requires a matching repository remote".into()],
                },
            }],
        )
        .unwrap()
    }

    #[test]
    fn catalog_is_fingerprinted_and_rejects_tampering() {
        let catalog = catalog();
        assert_eq!(catalog.schema, AgentHubCatalogSchema::V1);
        assert_eq!(catalog.fingerprint.len(), 64);
        assert_eq!(catalog.guidance.limits.max_prompt_bytes, 128 * 1024);
        assert!(catalog.guidance.outputs.structured_result);
        assert!(catalog.guidance.workspace.context_roots_read_only);
        catalog.validate().unwrap();

        let mut tampered = catalog;
        tampered.environments[0].max_timeout_secs += 1;
        assert_eq!(
            tampered.validate().unwrap_err(),
            "catalog fingerprint mismatch"
        );
    }

    #[test]
    fn catalog_rejects_unknown_fields_and_dangling_private_projection_names() {
        let mut value = serde_json::to_value(catalog()).unwrap();
        value["private_path"] = serde_json::json!("/secret");
        assert!(serde_json::from_value::<AgentHubCatalogV1>(value).is_err());

        let mut dangling = catalog();
        dangling.environments[0].overlays = vec!["missing".into()];
        assert!(dangling.validate().unwrap_err().contains("unknown overlay"));
    }

    #[test]
    fn catalog_enforces_the_serialized_payload_limit() {
        let environments = (0..AGENT_HUB_CATALOG_MAX_ENTRIES)
            .map(|index| AgentHubCatalogEnvironmentV1 {
                name: format!("environment-{index:02}"),
                description: "x".repeat(AGENT_HUB_CATALOG_MAX_DESCRIPTION_BYTES),
                discovery: None,
                image_family: AgentHubRunImageFamily::JobCode,
                network_options: vec![AgentHubCatalogNetworkOptionV1 {
                    network_class: AgentHubRunNetworkClass::None,
                    resources: Vec::new(),
                }],
                tool_approvals: vec![AgentHubRunToolApproval::Auto],
                overlays: Vec::new(),
                max_timeout_secs: 600,
            })
            .collect();
        let error = AgentHubCatalogV1::new(
            environments,
            vec![AgentHubCatalogImageV1 {
                family: AgentHubRunImageFamily::JobCode,
                description: String::new(),
                discovery: None,
                tool_facts: AgentHubCatalogToolFactsV1::Unavailable,
            }],
            Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error, "catalog payload exceeds limit");
    }

    #[test]
    fn discovery_metadata_matches_the_bounded_selection_hint_contract() {
        DiscoveryMetadata {
            recommended_for: vec!["Focused implementation work".into()],
            selection_notes: vec!["Choose the least-privileged option".into()],
        }
        .validate()
        .unwrap();
        for invalid in [
            DiscoveryMetadata {
                recommended_for: Vec::new(),
                selection_notes: vec!["note".into()],
            },
            DiscoveryMetadata {
                recommended_for: vec!["duplicate".into(), "duplicate".into()],
                selection_notes: vec!["note".into()],
            },
            DiscoveryMetadata {
                recommended_for: vec!["x".repeat(161)],
                selection_notes: vec!["note".into()],
            },
            DiscoveryMetadata {
                recommended_for: vec!["recommended".into()],
                selection_notes: vec![" leading whitespace".into()],
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn available_tool_facts_are_strict_bounded_public_pairs() {
        let mut catalog = catalog();
        catalog.images[0].tool_facts = AgentHubCatalogToolFactsV1::Available {
            tools: vec![AgentHubCatalogToolFactV1 {
                name: "rustc".into(),
                version: "1.91.0".into(),
            }],
        };
        catalog.fingerprint = catalog.expected_fingerprint().unwrap();
        catalog.validate().unwrap();

        let mut invalid = catalog.clone();
        invalid.images[0].tool_facts = AgentHubCatalogToolFactsV1::Available {
            tools: vec![AgentHubCatalogToolFactV1 {
                name: "private/path".into(),
                version: "1".into(),
            }],
        };
        invalid.fingerprint = invalid.expected_fingerprint().unwrap();
        assert!(invalid.validate().is_err());

        let mut value = serde_json::to_value(catalog).unwrap();
        value["images"][0]["tool_facts"]["tools"][0]["private"] = serde_json::json!("secret");
        assert!(serde_json::from_value::<AgentHubCatalogV1>(value).is_err());
    }
}
