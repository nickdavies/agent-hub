use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::expand_tilde;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct TriageRepositoryMount {
    pub host_path: PathBuf,
    pub container_path: String,
    pub read_only: bool,
}

#[derive(Deserialize)]
struct LocalTriageConfig {
    #[serde(default)]
    triage_repos: Vec<String>,
    #[serde(default)]
    root_registry: crate::RootRegistryConfig,
    #[serde(default)]
    triage_mounts: Vec<LocalTriageMount>,
}

#[derive(Deserialize)]
struct LocalTriageMount {
    host_path: String,
    container_path: String,
    read_only: bool,
}

#[derive(Deserialize)]
struct SystemFile {
    system: SystemDiscoveryConfig,
}

#[derive(Deserialize)]
struct SystemDiscoveryConfig {
    #[serde(default)]
    repo_discovery: Option<RepoDiscoveryConfig>,
}

#[derive(Deserialize)]
struct RepoDiscoveryConfig {
    enabled: bool,
    #[serde(default)]
    strategies: Vec<RepoDiscoveryStrategy>,
}

#[derive(Deserialize)]
struct RepoDiscoveryStrategy {
    #[serde(rename = "type")]
    strategy_type: String,
    path: String,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

pub fn checked_in_triage_repository_mounts(
    config_root: &Path,
) -> Result<Vec<TriageRepositoryMount>, String> {
    let system_path = config_root.join("system.toml");
    let system = std::fs::read_to_string(&system_path)
        .map_err(|error| format!("failed to read {}: {error}", system_path.display()))?;
    let local_path = config_root.join("local-agent.json");
    let local: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&local_path)
            .map_err(|error| format!("failed to read {}: {error}", local_path.display()))?,
    )
    .map_err(|error| format!("failed to parse {}: {error}", local_path.display()))?;
    validate_triage_repository_snapshots(&system, &local)
}

pub fn validate_triage_repository_snapshots(
    system_toml: &str,
    local_agent: &serde_json::Value,
) -> Result<Vec<TriageRepositoryMount>, String> {
    let system: SystemFile = toml::from_str(system_toml)
        .map_err(|error| format!("failed to parse system.toml: {error}"))?;
    let local: LocalTriageConfig = serde_json::from_value(local_agent.clone())
        .map_err(|error| format!("failed to parse local-agent triage config: {error}"))?;
    let discovery = system
        .system
        .repo_discovery
        .as_ref()
        .filter(|discovery| discovery.enabled);
    let Some(_discovery) = discovery else {
        if local.triage_repos.is_empty() && local.triage_mounts.is_empty() {
            return Ok(Vec::new());
        }
        return Err(
            "configured triage repositories require active server repo discovery".to_string(),
        );
    };
    if local.triage_repos.is_empty()
        || local.root_registry.roots.is_empty()
        || local.triage_mounts.is_empty()
    {
        return Err(
            "active repo discovery requires non-empty explicit triage_repos and root_registry"
                .to_string(),
        );
    }

    let approved_roots = local
        .root_registry
        .roots
        .iter()
        .filter(|root| {
            root.roles.contains(&crate::RootRole::ContextRo)
                && root.roles.contains(&crate::RootRole::GitMetadata)
        })
        .map(|root| canonicalize_configured_path(&root.path, "approved repository root"))
        .collect::<Result<Vec<_>, _>>()?;
    if approved_roots.is_empty() {
        return Err("triage requires a root with context_ro and git_metadata roles".to_string());
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(std::fs::canonicalize)
        .transpose()
        .map_err(|error| format!("failed to canonicalize HOME: {error}"))?;
    for root in &approved_roots {
        if root == Path::new("/") || home.as_ref() == Some(root) {
            return Err(format!(
                "approved repository root '{}' is too broad",
                root.display()
            ));
        }
    }

    let triage_repos = local
        .triage_repos
        .iter()
        .map(|repo| canonical_repository(repo))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let discovered = project_server_repository_discovery_snapshot(&system)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    if triage_repos
        != discovered
            .iter()
            .map(|mount| mount.host_path.clone())
            .collect()
    {
        return Err(
            "server repo discovery and worker triage_repos must resolve to the same exact repositories"
                .to_string(),
        );
    }

    let mut mounts = local
        .triage_mounts
        .iter()
        .map(|mount| {
            Ok(TriageRepositoryMount {
                host_path: canonical_repository(&mount.host_path)?,
                container_path: mount.container_path.clone(),
                read_only: mount.read_only,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    mounts.sort();
    let mut aliases = BTreeSet::new();
    for mount in &mounts {
        let repo = &mount.host_path;
        if !approved_roots
            .iter()
            .any(|root| repo.starts_with(root) && *repo != *root)
        {
            return Err(format!(
                "triage repository '{}' is outside its approved parent roots",
                repo.display()
            ));
        }
        if !aliases.insert(mount.container_path.clone()) {
            return Err(format!(
                "duplicate triage mount target '{}'",
                mount.container_path
            ));
        }
    }
    let mut discovered = discovered.into_iter().collect::<Vec<_>>();
    discovered.sort();
    if mounts != discovered {
        return Err(
            "server repository discovery projection does not exactly match worker triage mount contract"
                .to_string(),
        );
    }
    Ok(mounts)
}

pub fn project_server_repository_discovery(
    config_root: &Path,
) -> Result<Vec<TriageRepositoryMount>, String> {
    let system_path = config_root.join("system.toml");
    let system: SystemFile = toml::from_str(
        &std::fs::read_to_string(&system_path)
            .map_err(|error| format!("failed to read {}: {error}", system_path.display()))?,
    )
    .map_err(|error| format!("failed to parse {}: {error}", system_path.display()))?;
    project_server_repository_discovery_snapshot(&system)
}

fn project_server_repository_discovery_snapshot(
    system: &SystemFile,
) -> Result<Vec<TriageRepositoryMount>, String> {
    let Some(discovery) = system
        .system
        .repo_discovery
        .as_ref()
        .filter(|discovery| discovery.enabled)
    else {
        return Ok(Vec::new());
    };
    let mut mounts = Vec::new();
    for strategy in &discovery.strategies {
        if strategy.strategy_type != "directory"
            || !strategy.include.is_empty()
            || !strategy.exclude.is_empty()
        {
            return Err("server repository projection requires exact static roots".to_string());
        }
        let repo = canonical_repository(&strategy.path)?;
        let alias = repo
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("repository '{}' has no UTF-8 alias", repo.display()))?
            .to_string();
        mounts.push(TriageRepositoryMount {
            host_path: repo,
            container_path: format!("/repos/{alias}"),
            read_only: true,
        });
    }
    mounts.sort();
    Ok(mounts)
}

fn canonical_repository(configured: &str) -> Result<PathBuf, String> {
    let path = canonicalize_configured_path(configured, "triage repository")?;
    if !path.join(".git").exists() {
        return Err(format!(
            "triage repository '{}' is not a git root",
            path.display()
        ));
    }
    Ok(path)
}

fn canonicalize_configured_path(configured: &str, label: &str) -> Result<PathBuf, String> {
    let expanded = PathBuf::from(expand_tilde(configured));
    if !expanded.is_absolute() {
        return Err(format!(
            "{label} '{configured}' must resolve to an absolute path"
        ));
    }
    std::fs::canonicalize(&expanded).map_err(|error| {
        format!(
            "failed to canonicalize {label} '{}': {error}",
            expanded.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_server_and_worker_triage_repositories_produce_identical_mounts() {
        let root =
            std::env::temp_dir().join(format!("agent-hub-triage-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repos = root.join("repos");
        let first = repos.join("agent-hub");
        let second = repos.join("agent-hub-configs");
        for repo in [&first, &second] {
            std::fs::create_dir_all(repo.join(".git")).unwrap();
        }
        std::fs::create_dir_all(root.join("config")).unwrap();
        let config = root.join("config");
        std::fs::write(
            config.join("system.toml"),
            format!(
                "[system]\noperator='test'\nbot_identity='test'\n[system.repo_discovery]\nenabled=true\n[[system.repo_discovery.strategies]]\ntype='directory'\npath='{}'\ninclude=[]\nexclude=[]\n[[system.repo_discovery.strategies]]\ntype='directory'\npath='{}'\ninclude=[]\nexclude=[]\n",
                first.display(),
                second.display()
            ),
        )
        .unwrap();
        std::fs::write(
            config.join("local-agent.json"),
            serde_json::to_vec(&serde_json::json!({
                "triage_repos":[first.clone(), second.clone()],
                "root_registry":{"roots":[{"identity":"repos","path":repos,"roles":["context_ro","git_metadata"]}]},
                "default_agent":{"kind":"generic","command":"true"},
                "triage_mounts":[
                    {"host_path":first.clone(),"container_path":"/repos/agent-hub","read_only":true},
                    {"host_path":second.clone(),"container_path":"/repos/agent-hub-configs","read_only":true}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let mounts = checked_in_triage_repository_mounts(&config).unwrap();
        let captured = crate::validate_authoritative_config_root(&config).unwrap();
        let captured_profile = captured
            .runner_profiles
            .resolve_agent_profile(None)
            .unwrap();
        assert_eq!(mounts.len(), 2);
        assert!(
            mounts
                .iter()
                .all(|mount| mount.container_path.starts_with("/repos/"))
        );

        let mut local: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config.join("local-agent.json")).unwrap())
                .unwrap();
        let valid = local.clone();
        let mut replaced = valid.clone();
        replaced["default_agent"]["command"] = serde_json::json!("false");
        std::fs::write(
            config.join("local-agent.json"),
            serde_json::to_vec(&replaced).unwrap(),
        )
        .unwrap();
        assert_eq!(
            captured
                .runner_profiles
                .resolve_agent_profile(None)
                .unwrap(),
            captured_profile
        );
        assert_ne!(
            crate::validate_authoritative_config_root(&config)
                .unwrap()
                .runner_profiles
                .resolve_agent_profile(None)
                .unwrap(),
            captured_profile
        );
        std::fs::write(
            config.join("local-agent.json"),
            serde_json::to_vec(&valid).unwrap(),
        )
        .unwrap();
        local["triage_repos"] = serde_json::json!([first]);
        std::fs::write(
            config.join("local-agent.json"),
            serde_json::to_vec(&local).unwrap(),
        )
        .unwrap();
        assert!(crate::validate_authoritative_config_root(&config).is_err());

        let third = repos.join("substituted");
        std::fs::create_dir_all(third.join(".git")).unwrap();
        for mismatch in ["source", "alias", "read_only", "cardinality"] {
            let mut local = valid.clone();
            match mismatch {
                "source" => local["triage_mounts"][0]["host_path"] = serde_json::json!(third),
                "alias" => {
                    local["triage_mounts"][0]["container_path"] = serde_json::json!("/repos/wrong")
                }
                "read_only" => local["triage_mounts"][0]["read_only"] = serde_json::json!(false),
                "cardinality" => {
                    local["triage_mounts"].as_array_mut().unwrap().pop();
                }
                _ => unreachable!(),
            }
            std::fs::write(
                config.join("local-agent.json"),
                serde_json::to_vec(&local).unwrap(),
            )
            .unwrap();
            assert!(
                crate::validate_authoritative_config_root(&config).is_err(),
                "{mismatch}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
