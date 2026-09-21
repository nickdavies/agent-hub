//! Minimal read-only protocol capability discovery.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Maximum encoded capability response accepted from a local or HTTP peer.
pub const AGENT_HUB_CAPABILITIES_MAX_BYTES: usize = 4 * 1024;
pub const AGENT_HUB_CAPABILITIES_SCHEMA_V1: &str = "agent_hub_capabilities_v1";
pub const AGENT_HUB_RUN_CAPABILITY_V1: &str = "agent_hub_run_v1";
pub const AGENT_HUB_CAPABILITIES_MAX_COUNT: usize = 32;
pub const AGENT_HUB_CAPABILITY_MAX_BYTES: usize = 64;

/// Capabilities shared by the host gateway and approval service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentHubCapabilitiesV1 {
    pub schema: String,
    pub capabilities: Vec<String>,
}

impl AgentHubCapabilitiesV1 {
    pub fn current() -> Self {
        Self {
            schema: AGENT_HUB_CAPABILITIES_SCHEMA_V1.into(),
            capabilities: vec![AGENT_HUB_RUN_CAPABILITY_V1.into()],
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != AGENT_HUB_CAPABILITIES_SCHEMA_V1 {
            return Err("unsupported agent hub capabilities schema".into());
        }
        if self.capabilities.len() > AGENT_HUB_CAPABILITIES_MAX_COUNT {
            return Err("agent hub capability count exceeds limit".into());
        }
        for capability in &self.capabilities {
            if capability.is_empty()
                || capability.len() > AGENT_HUB_CAPABILITY_MAX_BYTES
                || !capability
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            {
                return Err("agent hub capability identifier is invalid".into());
            }
        }
        if self.capabilities.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("agent hub capabilities must be sorted and unique".into());
        }
        Ok(())
    }

    pub fn supports(&self, required: &str) -> bool {
        self.capabilities
            .binary_search_by(|capability| capability.as_str().cmp(required))
            .is_ok()
    }

    pub fn require(&self, required: &[&str]) -> Result<(), String> {
        self.validate()?;
        for capability in required {
            validate_capability_identifier(capability)?;
            if !self.supports(capability) {
                return Err(format!(
                    "required agent hub capability is absent: {capability}"
                ));
            }
        }
        Ok(())
    }

    pub fn intersection(&self, other: &Self) -> Result<Self, String> {
        self.validate()?;
        other.validate()?;
        let capabilities = self
            .capabilities
            .iter()
            .filter(|capability| other.supports(capability))
            .cloned()
            .collect();
        Ok(Self {
            schema: AGENT_HUB_CAPABILITIES_SCHEMA_V1.into(),
            capabilities,
        })
    }
}

fn validate_capability_identifier(capability: &str) -> Result<(), String> {
    let manifest = AgentHubCapabilitiesV1 {
        schema: AGENT_HUB_CAPABILITIES_SCHEMA_V1.into(),
        capabilities: vec![capability.into()],
    };
    manifest.validate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_capabilities_are_a_strict_static_manifest() {
        let current = AgentHubCapabilitiesV1::current();
        assert_eq!(
            serde_json::to_value(&current).unwrap(),
            serde_json::json!({
                "schema": "agent_hub_capabilities_v1",
                "capabilities": ["agent_hub_run_v1"]
            })
        );
        assert!(current.validate().is_ok());
        current.require(&[AGENT_HUB_RUN_CAPABILITY_V1]).unwrap();
        assert!(
            serde_json::from_value::<AgentHubCapabilitiesV1>(serde_json::json!({
                "schema": "agent_hub_capabilities_v1",
                "capabilities": ["agent_hub_run_v1"],
                "future": true
            }))
            .is_err()
        );
    }

    #[test]
    fn validation_requires_bounded_sorted_unique_identifiers() {
        for capabilities in [
            vec!["z".into(), "a".into()],
            vec!["same".into(), "same".into()],
            vec!["UPPER".into()],
            vec!["x".repeat(AGENT_HUB_CAPABILITY_MAX_BYTES + 1)],
            (0..=AGENT_HUB_CAPABILITIES_MAX_COUNT)
                .map(|index| format!("capability_{index:02}"))
                .collect(),
        ] {
            assert!(
                AgentHubCapabilitiesV1 {
                    schema: AGENT_HUB_CAPABILITIES_SCHEMA_V1.into(),
                    capabilities,
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            AgentHubCapabilitiesV1 {
                schema: "future_schema".into(),
                capabilities: vec![],
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn intersection_is_deterministic_and_required_capabilities_are_explicit() {
        let left = AgentHubCapabilitiesV1 {
            schema: AGENT_HUB_CAPABILITIES_SCHEMA_V1.into(),
            capabilities: vec!["agent_hub_run_v1".into(), "future_a".into()],
        };
        let right = AgentHubCapabilitiesV1 {
            schema: AGENT_HUB_CAPABILITIES_SCHEMA_V1.into(),
            capabilities: vec!["agent_hub_run_v1".into(), "future_b".into()],
        };
        let intersection = left.intersection(&right).unwrap();
        assert_eq!(intersection, AgentHubCapabilitiesV1::current());
        intersection
            .require(&[AGENT_HUB_RUN_CAPABILITY_V1])
            .unwrap();
        assert!(intersection.require(&["future_a"]).is_err());
    }
}
