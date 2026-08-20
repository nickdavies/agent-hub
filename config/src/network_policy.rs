use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::system::SystemConfig;

#[derive(Debug, Clone, Deserialize)]
struct NetworkPolicyTomlFile {
    policy: NetworkPolicyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct NetworkPolicyConfig {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub docker_network: Option<String>,
    #[serde(default)]
    pub unrestricted: bool,
    #[serde(default)]
    pub allow: Vec<PolicyAllowRule>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl NetworkPolicyConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "failed to read network policy config {}: {e}",
                path.display()
            )
        })?;
        Self::from_toml(&raw, &path.display().to_string())
    }

    pub fn from_toml(raw: &str, label: &str) -> Result<Self, String> {
        let wrapped: NetworkPolicyTomlFile = toml::from_str(raw)
            .map_err(|e| format!("failed to parse network policy config {label}: {e}"))?;
        Ok(wrapped.policy)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PolicyAllowRule {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub hosts_from: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FlattenedPolicy {
    pub name: String,
    pub description: String,
    pub unrestricted: bool,
    #[serde(default)]
    pub docker_network: Option<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

pub fn flatten_policy(
    name: &str,
    policies: &HashMap<String, NetworkPolicyConfig>,
    system: &SystemConfig,
) -> Result<FlattenedPolicy, String> {
    let mut stack = Vec::new();
    let mut visiting = HashSet::new();
    gather_policy_chain(name, policies, &mut visiting, &mut stack)?;

    let mut description = String::new();
    let mut unrestricted = false;
    let mut docker_network = None;
    let mut hosts = Vec::new();
    let mut seen_hosts = HashSet::new();
    let mut env = HashMap::new();

    for policy in stack {
        description = policy.description.clone();
        unrestricted = unrestricted || policy.unrestricted;
        if let Some(net) = &policy.docker_network {
            docker_network = Some(net.clone());
        }

        for allow in &policy.allow {
            for host in &allow.hosts {
                if seen_hosts.insert(host.clone()) {
                    hosts.push(host.clone());
                }
            }
            if let Some(source) = &allow.hosts_from {
                for host in resolve_hosts_from(source, system)? {
                    if seen_hosts.insert(host.clone()) {
                        hosts.push(host);
                    }
                }
            }
        }

        for (key, value) in &policy.env {
            env.insert(key.clone(), value.clone());
        }
    }

    Ok(FlattenedPolicy {
        name: name.to_string(),
        description,
        unrestricted,
        docker_network,
        hosts,
        env,
    })
}

fn gather_policy_chain<'a>(
    name: &str,
    policies: &'a HashMap<String, NetworkPolicyConfig>,
    visiting: &mut HashSet<String>,
    stack: &mut Vec<&'a NetworkPolicyConfig>,
) -> Result<(), String> {
    if visiting.contains(name) {
        return Err(format!("cyclic policy extends chain detected at '{name}'"));
    }

    let policy = policies
        .get(name)
        .ok_or_else(|| format!("network policy '{name}' is not defined"))?;

    visiting.insert(name.to_string());
    if let Some(parent) = &policy.extends {
        gather_policy_chain(parent, policies, visiting, stack)?;
    }
    visiting.remove(name);

    stack.push(policy);
    Ok(())
}

fn resolve_hosts_from(reference: &str, system: &SystemConfig) -> Result<Vec<String>, String> {
    match reference {
        "system.llm_providers" => Ok(system
            .llm_providers
            .iter()
            .flat_map(|provider| provider.hosts.clone())
            .collect()),
        _ => Err(format!("unsupported hosts_from reference '{reference}'")),
    }
}
