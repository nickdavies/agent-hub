use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::system::SystemConfig;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkPolicyTomlFile {
    policy: NetworkPolicyConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
        for rule in &wrapped.policy.allow {
            canonical_policy_hosts(&rule.hosts)?;
        }
        Ok(wrapped.policy)
    }
}

pub fn canonical_policy_hosts(hosts: &[String]) -> Result<Vec<String>, String> {
    let mut canonical = Vec::with_capacity(hosts.len());
    for host in hosts {
        canonical.push(canonical_policy_host(host)?);
    }
    canonical.sort();
    canonical.dedup();
    Ok(canonical)
}

fn canonical_policy_host(host: &str) -> Result<String, String> {
    if host.is_empty() || host.trim() != host {
        return Err(format!(
            "network policy host '{host}' must be non-empty without surrounding whitespace"
        ));
    }
    if host.contains("//") || host.contains('/') || host.contains(':') || host.ends_with('.') {
        return Err(format!(
            "network policy host '{host}' is not a bare DNS host"
        ));
    }

    let (wildcard, name) = host
        .strip_prefix("*.")
        .map_or((false, host), |name| (true, name));
    if name.parse::<IpAddr>().is_ok() {
        return Err(format!(
            "network policy host '{host}' must not be an IP literal"
        ));
    }
    if name.split('.').all(|label| {
        label.bytes().all(|byte| byte.is_ascii_digit())
            || label.strip_prefix("0x").is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    }) {
        return Err(format!(
            "network policy host '{host}' must not be a numeric IP form"
        ));
    }
    if name.len() > 253 || !name.contains('.') {
        return Err(format!(
            "network policy host '{host}' must be a fully-qualified DNS name"
        ));
    }
    for label in name.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(format!(
                "network policy host '{host}' is not a valid DNS name"
            ));
        }
    }
    let canonical = name.to_ascii_lowercase();
    Ok(if wildcard {
        format!("*.{canonical}")
    } else {
        canonical
    })
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyAllowRule {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub hosts_from: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
            for host in canonical_policy_hosts(&allow.hosts)? {
                if seen_hosts.insert(host.clone()) {
                    hosts.push(host);
                }
            }
            if let Some(source) = &allow.hosts_from {
                for host in canonical_policy_hosts(&resolve_hosts_from(source, system)?)? {
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

#[cfg(test)]
mod tests {
    use super::{NetworkPolicyConfig, canonical_policy_hosts};

    #[test]
    fn policy_dtos_reject_unknown_fields_at_every_level() {
        for raw in [
            "[policy]\nname='none'\ndescription='none'\nunknown=true\n",
            "[policy]\nname='none'\ndescription='none'\n[[policy.allow]]\nhosts=['example.com']\nunknown=true\n",
            "[policy]\nname='none'\ndescription='none'\n[unknown]\nvalue=true\n",
        ] {
            assert!(NetworkPolicyConfig::from_toml(raw, "test policy").is_err());
        }
    }

    #[test]
    fn policy_hosts_are_canonical_and_deterministically_ordered() {
        assert_eq!(
            canonical_policy_hosts(&[
                "Z.Example.com".to_string(),
                "a.example.com".to_string(),
                "a.example.com".to_string(),
            ])
            .unwrap(),
            ["a.example.com", "z.example.com"]
        );
    }

    #[test]
    fn policy_hosts_reject_urls_ip_literals_and_malformed_names_without_dropping() {
        for host in [
            "",
            " example.com",
            "https://example.com",
            "127.0.0.1",
            "0177.0.0.1",
            "0x7f.0x0.0x0.0x1",
            "::1",
            "localhost",
            "example.com:443",
            "example.com.",
            "bad_label.example.com",
        ] {
            assert!(
                canonical_policy_hosts(&[host.to_string()]).is_err(),
                "accepted {host}"
            );
        }
    }
}
