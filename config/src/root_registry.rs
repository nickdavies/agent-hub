use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct RootRegistryConfig {
    pub roots: Vec<RootConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub identity: String,
    pub path: String,
    pub roles: Vec<RootRole>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RootRole {
    WorkspaceRo,
    WorkspaceRw,
    ContextRo,
    GitMetadata,
}

pub fn root_registry(local_agent: &serde_json::Value) -> Result<RootRegistryConfig, String> {
    let value = local_agent
        .get("root_registry")
        .cloned()
        .ok_or_else(|| "local-agent.json has no root_registry".to_string())?;
    let registry: RootRegistryConfig = serde_json::from_value(value)
        .map_err(|error| format!("invalid local-agent root_registry: {error}"))?;
    if registry.roots.is_empty() {
        return Err("local-agent root_registry must not be empty".to_string());
    }
    let mut identities = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for root in &registry.roots {
        if root.identity.is_empty()
            || root.identity.len() > 63
            || !root
                .identity
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || !root.identity.as_bytes()[0].is_ascii_alphanumeric()
            || !root.identity.as_bytes()[root.identity.len() - 1].is_ascii_alphanumeric()
        {
            return Err(format!(
                "root identity '{}' is not canonical",
                root.identity
            ));
        }
        if !identities.insert(root.identity.clone()) {
            return Err(format!("duplicate root identity '{}'", root.identity));
        }
        let path = std::path::Path::new(&root.path);
        if !path.is_absolute()
            || root.path == "/"
            || root.path.ends_with('/')
            || root.path.contains([':', ','])
            || root.path.chars().any(char::is_control)
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
        {
            return Err(format!("root path '{}' is not canonical", root.path));
        }
        if !paths.insert(root.path.clone()) {
            return Err(format!("duplicate root path '{}'", root.path));
        }
        if root.roles.is_empty() {
            return Err(format!("root '{}' has no roles", root.identity));
        }
        let mut roles = root.roles.clone();
        roles.sort();
        if roles.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(format!("root '{}' has duplicate roles", root.identity));
        }
        if roles.contains(&RootRole::WorkspaceRw) && !roles.contains(&RootRole::WorkspaceRo) {
            return Err(format!(
                "root '{}' grants workspace_rw without workspace_ro",
                root.identity
            ));
        }
    }
    let paths = registry
        .roots
        .iter()
        .map(|root| std::path::Path::new(&root.path))
        .collect::<Vec<_>>();
    for (index, left) in paths.iter().enumerate() {
        for right in paths.iter().skip(index + 1) {
            if left.starts_with(right) || right.starts_with(left) {
                return Err(format!(
                    "root registry paths overlap: '{}' and '{}'",
                    left.display(),
                    right.display()
                ));
            }
        }
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_rejects_unknown_fields_overlap_and_role_escalation() {
        for value in [
            serde_json::json!({"root_registry":{"roots":[{"identity":"code","path":"/srv/code","roles":["context_ro"],"extra":true}]}}),
            serde_json::json!({"root_registry":{"roots":[{"identity":"all","path":"/srv","roles":["workspace_ro"]},{"identity":"code","path":"/srv/code","roles":["context_ro"]}]}}),
            serde_json::json!({"root_registry":{"roots":[{"identity":"code","path":"/srv/code","roles":["workspace_rw"]}]}}),
        ] {
            assert!(root_registry(&value).is_err());
        }
    }

    #[test]
    fn registry_accepts_strict_disjoint_roots() {
        let value = serde_json::json!({"root_registry":{"roots":[
            {"identity":"code","path":"/srv/code","roles":["workspace_ro","workspace_rw","context_ro","git_metadata"]},
            {"identity":"oss","path":"/srv/oss","roles":["workspace_ro","context_ro","git_metadata"]}
        ]}});
        assert_eq!(root_registry(&value).unwrap().roots.len(), 2);
    }
}
