use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{
    EnvironmentBundle, NetworkPolicyConfig, RunnerProfilesConfig, SystemConfig,
    TriageRepositoryMount, materialize_environment_bundle_with_snapshot,
    validate_triage_repository_snapshots,
};

#[derive(serde::Deserialize)]
struct SystemFile {
    system: SystemSection,
}

#[derive(serde::Deserialize)]
struct SystemSection {
    #[serde(default)]
    environments: Option<InstalledEnvironments>,
    #[serde(default)]
    network_policies: Option<InstalledNetworkPolicies>,
    #[serde(default)]
    images: Option<InstalledImages>,
    #[serde(default)]
    overlays: Option<OverlayReference>,
    #[serde(default)]
    triage: Option<TriageReference>,
}

#[derive(serde::Deserialize)]
struct InstalledEnvironments {
    #[serde(default)]
    installed: Vec<String>,
}

#[derive(serde::Deserialize)]
struct InstalledNetworkPolicies {
    #[serde(default)]
    installed: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ImageFile {
    image: toml::Value,
}

#[derive(serde::Deserialize)]
struct OverlayFile {
    overlays: Vec<ValidatedOverlay>,
}

#[derive(serde::Deserialize)]
struct TriageFile {
    prompt: ValidatedTriagePromptContent,
}

#[derive(serde::Deserialize)]
struct InstalledImages {
    #[serde(default)]
    installed: Vec<String>,
}

#[derive(serde::Deserialize)]
struct OverlayReference {
    registry: Option<String>,
}

#[derive(serde::Deserialize)]
struct TriageReference {
    config: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CapturedAuthoritativeFile {
    pub relative_path: String,
    pub bytes: Vec<u8>,
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct ValidatedImageManifest {
    pub name: String,
    pub file: CapturedAuthoritativeFile,
    pub image: toml::Value,
    pub relative_files: BTreeMap<String, Vec<u8>>,
    pub file_digests: BTreeMap<String, String>,
    pub directory_digest: String,
}

struct CapturedImageContext {
    file: CapturedAuthoritativeFile,
    relative_files: BTreeMap<String, Vec<u8>>,
    file_digests: BTreeMap<String, String>,
    directory_digest: String,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub struct ValidatedOverlay {
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GithubOverlayConfig {
    pub host: String,
    pub api_base_url: String,
    pub access: GithubOverlayAccess,
    pub network_resource: String,
    pub credential_provider: String,
    pub credential_target: String,
    pub authenticated: bool,
    pub private_repositories: bool,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GithubOverlayAccess {
    Read,
    ReadWrite,
}

impl ValidatedOverlay {
    pub fn github_config(&self) -> Result<Option<GithubOverlayConfig>, String> {
        if self.provider != "github" {
            return Ok(None);
        }
        let config: GithubOverlayConfig = serde_json::from_value(self.config.clone())
            .map_err(|error| format!("invalid GitHub overlay '{}': {error}", self.name))?;
        let host = protocol::canonical_github_host(&config.host)
            .ok_or_else(|| format!("GitHub overlay '{}' has invalid host", self.name))?;
        let api = protocol::canonical_github_api_base_for_host(&host, &config.api_base_url)
            .ok_or_else(|| format!("GitHub overlay '{}' has invalid API base URL", self.name))?;
        let access = match config.access {
            GithubOverlayAccess::Read => {
                return Err(format!(
                    "GitHub overlay '{}' claims read-only access without server-enforced attenuation",
                    self.name
                ));
            }
            GithubOverlayAccess::ReadWrite => "read_write",
        };
        if host != config.host
            || api != config.api_base_url
            || config.network_resource != "github"
            || !config.authenticated
            || !config.private_repositories
            || self.name != format!("github:{host}:{access}")
            || config.credential_provider != self.name
            || config.credential_target != format!("/agent-hub/github/tokens/{host}")
        {
            return Err(format!(
                "GitHub overlay '{}' is not an exact authenticated capability",
                self.name
            ));
        }
        Ok(Some(config))
    }
}

#[derive(Debug, Clone)]
pub struct ValidatedOverlayRegistry {
    pub file: CapturedAuthoritativeFile,
    pub overlays: Vec<ValidatedOverlay>,
}

#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct ValidatedTriagePromptContent {
    pub system: String,
    pub user: String,
}

#[derive(Debug, Clone)]
pub struct ValidatedTriagePrompt {
    pub file: CapturedAuthoritativeFile,
    pub prompt: ValidatedTriagePromptContent,
}

#[derive(Debug, Clone)]
pub struct ValidatedNetworkPolicy {
    pub name: String,
    pub relative_files: BTreeMap<String, Vec<u8>>,
    pub file_digests: BTreeMap<String, String>,
    pub directory_digest: String,
    pub policy: NetworkPolicyConfig,
}

#[derive(Debug, Clone)]
pub struct ValidatedDeployment {
    pub triage_mounts: Vec<TriageRepositoryMount>,
    pub environment_bundle_digests: BTreeMap<String, String>,
    pub runner_profile_digests: BTreeMap<String, String>,
    pub runner_profiles: RunnerProfilesConfig,
    pub local_agent: serde_json::Value,
    pub system_toml: String,
    pub environment_bundles: BTreeMap<String, EnvironmentBundle>,
    pub network_policies: BTreeMap<String, ValidatedNetworkPolicy>,
    pub images: BTreeMap<String, ValidatedImageManifest>,
    pub overlay_registry: Option<ValidatedOverlayRegistry>,
    pub triage_prompt: Option<ValidatedTriagePrompt>,
    pub authoritative_paths: BTreeSet<String>,
}

impl ValidatedDeployment {
    pub fn provenance_digest(&self) -> Result<String, String> {
        let snapshot = serde_json::json!({
            "system_toml": self.system_toml,
            "local_agent": self.local_agent,
            "environment_bundle_digests": self.environment_bundle_digests,
            "runner_profile_digests": self.runner_profile_digests,
            "network_policy_digests": self.network_policies.iter().map(|(name, policy)| (name, &policy.directory_digest)).collect::<BTreeMap<_, _>>(),
            "images": self.images.iter().map(|(name, image)| (name, &image.directory_digest)).collect::<BTreeMap<_, _>>(),
            "authoritative_paths": self.authoritative_paths,
        });
        let bytes = serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    pub fn assert_authoritative_inventory_complete(&self) -> Result<(), String> {
        let mut expected =
            BTreeSet::from(["local-agent.json".to_string(), "system.toml".to_string()]);
        expected.extend(
            self.environment_bundles
                .keys()
                .map(|name| format!("environment_bundle:{name}")),
        );
        for (name, policy) in &self.network_policies {
            expected.extend(
                policy
                    .relative_files
                    .keys()
                    .map(|path| format!("network_policies/{name}/{path}")),
            );
        }
        for image in self.images.values() {
            expected.extend(
                image
                    .relative_files
                    .keys()
                    .map(|path| format!("images/{}/{path}", image.name)),
            );
        }
        if let Some(registry) = &self.overlay_registry {
            expected.insert(registry.file.relative_path.clone());
        }
        if let Some(triage) = &self.triage_prompt {
            expected.insert(triage.file.relative_path.clone());
        }
        if expected != self.authoritative_paths {
            return Err(format!(
                "authoritative snapshot inventory mismatch: expected {expected:?}, captured {:?}",
                self.authoritative_paths
            ));
        }
        Ok(())
    }
}

pub fn validate_authoritative_config_root(root: &Path) -> Result<ValidatedDeployment, String> {
    let system_path = root.join("system.toml");
    let system_toml = std::fs::read_to_string(&system_path)
        .map_err(|error| format!("failed to read {}: {error}", system_path.display()))?;
    let system: SystemFile = toml::from_str(&system_toml)
        .map_err(|error| format!("failed to parse {}: {error}", system_path.display()))?;
    let mut authoritative_paths =
        BTreeSet::from(["local-agent.json".to_string(), "system.toml".to_string()]);
    let mut environment_bundle_digests = BTreeMap::new();
    let mut environment_bundles = BTreeMap::new();
    let mut network_policies = BTreeMap::new();
    if let Some(configured) = &system.system.network_policies {
        for name in &configured.installed {
            let snapshot = capture_network_policy(root, name)?;
            authoritative_paths.extend(
                snapshot
                    .relative_files
                    .keys()
                    .map(|path| format!("network_policies/{name}/{path}")),
            );
            if network_policies.insert(name.clone(), snapshot).is_some() {
                return Err(format!("duplicate installed network policy '{name}'"));
            }
        }
    }
    if let Some(environments) = &system.system.environments {
        let materialization_system = if environments.installed.is_empty() {
            None
        } else {
            Some(SystemConfig::from_toml(
                &system_toml,
                "captured system.toml",
            )?)
        };
        let policy_objects = network_policies
            .iter()
            .map(|(name, snapshot)| (name.clone(), snapshot.policy.clone()))
            .collect();
        for name in &environments.installed {
            let bundle = materialize_environment_bundle_with_snapshot(
                root,
                name,
                materialization_system
                    .as_ref()
                    .expect("installed environments require parsed system config"),
                &policy_objects,
            )?;
            bundle.verify_opencode_sentinel()?;
            environment_bundle_digests.insert(name.clone(), bundle.digest.clone());
            environment_bundles.insert(name.clone(), bundle);
            authoritative_paths.insert(format!("environment_bundle:{name}"));
        }
    }

    let local_path = root.join("local-agent.json");
    let local: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&local_path)
            .map_err(|error| format!("failed to read {}: {error}", local_path.display()))?,
    )
    .map_err(|error| format!("failed to parse {}: {error}", local_path.display()))?;
    crate::root_registry(&local)?;
    let triage_mounts = validate_triage_repository_snapshots(&system_toml, &local)?;
    let runners: RunnerProfilesConfig = serde_json::from_value(local.clone())
        .map_err(|error| format!("failed to parse runner profiles: {error}"))?;
    let runners = runners.validated()?;

    let mut images = BTreeMap::new();
    if let Some(configured) = &system.system.images {
        for name in &configured.installed {
            validate_component(name, "image")?;
            let captured = capture_image(root, name)?;
            let relative_path = captured.file.relative_path.clone();
            let parsed: ImageFile = toml::from_str(
                std::str::from_utf8(&captured.file.bytes)
                    .map_err(|error| format!("image manifest '{name}' is not UTF-8: {error}"))?,
            )
            .map_err(|error| format!("failed to parse {relative_path}: {error}"))?;
            if !parsed.image.is_table() {
                return Err(format!(
                    "image manifest '{relative_path}' must contain an image table"
                ));
            }
            authoritative_paths.extend(
                captured
                    .relative_files
                    .keys()
                    .map(|path| format!("images/{name}/{path}")),
            );
            if images
                .insert(
                    name.clone(),
                    ValidatedImageManifest {
                        name: name.clone(),
                        file: captured.file,
                        image: parsed.image,
                        relative_files: captured.relative_files,
                        file_digests: captured.file_digests,
                        directory_digest: captured.directory_digest,
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate installed image '{name}'"));
            }
        }
    }

    let overlay_registry = system
        .system
        .overlays
        .as_ref()
        .and_then(|overlays| overlays.registry.as_deref())
        .map(|path| {
            let file = capture_file(root, path)?;
            let parsed: OverlayFile = toml::from_str(
                std::str::from_utf8(&file.bytes)
                    .map_err(|error| format!("overlay registry is not UTF-8: {error}"))?,
            )
            .map_err(|error| format!("failed to parse overlay registry '{path}': {error}"))?;
            let mut names = BTreeSet::new();
            for overlay in &parsed.overlays {
                validate_component(&overlay.name, "overlay")?;
                if !names.insert(overlay.name.clone()) {
                    return Err(format!("duplicate overlay '{}'", overlay.name));
                }
                overlay.github_config()?;
            }
            authoritative_paths.insert(file.relative_path.clone());
            Ok(ValidatedOverlayRegistry {
                file,
                overlays: parsed.overlays,
            })
        })
        .transpose()?;

    let triage_prompt = system
        .system
        .triage
        .as_ref()
        .and_then(|triage| triage.config.as_deref())
        .map(|path| {
            let file = capture_file(root, path)?;
            let parsed: TriageFile = toml::from_str(
                std::str::from_utf8(&file.bytes)
                    .map_err(|error| format!("triage prompt is not UTF-8: {error}"))?,
            )
            .map_err(|error| format!("failed to parse triage prompt '{path}': {error}"))?;
            if parsed.prompt.system.is_empty() || parsed.prompt.user.is_empty() {
                return Err("triage prompt system and user content must not be empty".to_string());
            }
            authoritative_paths.insert(file.relative_path.clone());
            Ok(ValidatedTriagePrompt {
                file,
                prompt: parsed.prompt,
            })
        })
        .transpose()?;

    let mut runner_profile_digests = BTreeMap::new();
    let default = runners.resolve_agent_profile(None)?;
    runner_profile_digests.insert(default.profile_name.clone(), default.config_digest);
    let mut names = runners.managed_runners.keys().cloned().collect::<Vec<_>>();
    names.sort();
    for name in names {
        let profile = runners.resolve_agent_profile(Some(&name))?;
        runner_profile_digests.insert(profile.profile_name.clone(), profile.config_digest);
    }

    let deployment = ValidatedDeployment {
        triage_mounts,
        environment_bundle_digests,
        runner_profile_digests,
        runner_profiles: runners,
        local_agent: local,
        system_toml,
        environment_bundles,
        network_policies,
        images,
        overlay_registry,
        triage_prompt,
        authoritative_paths,
    };
    deployment.assert_authoritative_inventory_complete()?;
    Ok(deployment)
}

fn capture_file(root: &Path, configured_path: &str) -> Result<CapturedAuthoritativeFile, String> {
    let relative_path = normalize_relative_path(configured_path)?;
    let path = root.join(&relative_path);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "authoritative input {} is not a regular file",
            path.display()
        ));
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    Ok(CapturedAuthoritativeFile {
        relative_path,
        digest: hex::encode(Sha256::digest(&bytes)),
        bytes,
    })
}

fn capture_image(root: &Path, name: &str) -> Result<CapturedImageContext, String> {
    validate_component(name, "image")?;
    let directory = root.join("images").join(name);
    let mut relative_files = BTreeMap::new();
    capture_directory(&directory, &directory, &mut relative_files)?;
    let manifest_bytes = relative_files
        .get("image.toml")
        .ok_or_else(|| format!("image '{name}' has no image.toml"))?
        .clone();
    let file_digests = relative_files
        .iter()
        .map(|(path, bytes)| (path.clone(), hex::encode(Sha256::digest(bytes))))
        .collect();
    let directory_digest = digest_directory(&relative_files);
    Ok(CapturedImageContext {
        file: CapturedAuthoritativeFile {
            relative_path: format!("images/{name}/image.toml"),
            digest: hex::encode(Sha256::digest(&manifest_bytes)),
            bytes: manifest_bytes,
        },
        relative_files,
        file_digests,
        directory_digest,
    })
}

fn digest_directory(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut digest = Sha256::new();
    for (path, bytes) in files {
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    hex::encode(digest.finalize())
}

fn normalize_relative_path(path: &str) -> Result<String, String> {
    if path.is_empty() {
        return Err("authoritative path must not be empty".to_string());
    }
    let path = PathBuf::from(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "authoritative path '{}' is not normalized and relative",
            path.display()
        ));
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn validate_component(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value == "."
        || value == ".."
    {
        return Err(format!("invalid {label} name '{value}'"));
    }
    Ok(())
}

fn capture_network_policy(root: &Path, name: &str) -> Result<ValidatedNetworkPolicy, String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(format!("invalid network policy name '{name}'"));
    }
    let directory = root.join("network_policies").join(name);
    let mut relative_files = BTreeMap::new();
    capture_directory(&directory, &directory, &mut relative_files)?;
    let policy_bytes = relative_files
        .get("policy.toml")
        .ok_or_else(|| format!("network policy '{name}' has no policy.toml"))?;
    let policy = NetworkPolicyConfig::from_toml(
        std::str::from_utf8(policy_bytes)
            .map_err(|error| format!("network policy '{name}' is not UTF-8: {error}"))?,
        &format!("network_policies/{name}/policy.toml"),
    )?;
    if policy.name != name {
        return Err(format!(
            "network policy directory '{name}' contains policy '{}'",
            policy.name
        ));
    }
    let file_digests = relative_files
        .iter()
        .map(|(path, bytes)| (path.clone(), hex::encode(Sha256::digest(bytes))))
        .collect();
    let directory_digest = digest_directory(&relative_files);
    Ok(ValidatedNetworkPolicy {
        name: name.to_string(),
        relative_files,
        file_digests,
        directory_digest,
        policy,
    })
}

fn capture_directory(
    root: &Path,
    directory: &Path,
    output: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let mut entries = std::fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "network policy snapshot contains symlink {}",
                path.display()
            ));
        }
        if file_type.is_dir() {
            capture_directory(root, &path, output)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if output
                .insert(
                    relative.clone(),
                    std::fs::read(&path)
                        .map_err(|error| format!("failed to read {}: {error}", path.display()))?,
                )
                .is_some()
            {
                return Err(format!("duplicate network policy path '{relative}'"));
            }
        } else {
            return Err(format!(
                "unsupported network policy entry {}",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_overlay_requires_honest_read_write_access() {
        let valid = serde_json::json!({
            "host":"github.com",
            "api_base_url":"https://api.github.com",
            "access":"read_write",
            "network_resource":"github",
            "credential_provider":"github:github.com:read_write",
            "credential_target":"/agent-hub/github/tokens/github.com",
            "authenticated":true,
            "private_repositories":true
        });
        let overlay = |config| ValidatedOverlay {
            name: "github:github.com:read_write".into(),
            provider: "github".into(),
            config,
            description: None,
        };
        overlay(valid.clone()).github_config().unwrap().unwrap();
        let mut read = valid.clone();
        read["access"] = serde_json::json!("read");
        assert!(overlay(read).github_config().is_err());
        for (field, value) in [
            ("host", serde_json::json!("github.snooguts.net")),
            ("credential_target", serde_json::json!("GITHUB_TOKEN")),
            ("authenticated", serde_json::json!(false)),
            ("private_repositories", serde_json::json!(false)),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(overlay(invalid).github_config().is_err());
        }
        let mut unknown = valid;
        unknown["unknown"] = serde_json::json!(true);
        assert!(overlay(unknown).github_config().is_err());
    }

    #[test]
    fn captured_policy_bytes_objects_and_digests_resist_toctou_replacement() {
        let root =
            std::env::temp_dir().join(format!("policy-snapshot-toctou-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("network_policies/none")).unwrap();
        std::fs::create_dir_all(root.join("environments/test")).unwrap();
        std::fs::write(
            root.join("system.toml"),
            "[system]\noperator='test'\nbot_identity='test'\n[system.environments]\ninstalled=['test']\ntriage_environment='test'\n[system.network_policies]\ninstalled=['none']\n",
        )
        .unwrap();
        std::fs::write(
            root.join("local-agent.json"),
            serde_json::to_vec(&serde_json::json!({
                "root_registry":{"roots":[{"identity":"test","path":"/roots/test","roles":["workspace_ro"]}]},
                "default_agent":{"kind":"generic","command":"true"}
            }))
            .unwrap(),
        )
        .unwrap();
        let policy_path = root.join("network_policies/none/policy.toml");
        std::fs::write(
            &policy_path,
            "[policy]\nname='none'\ndescription='original'\ndocker_network='none'\n",
        )
        .unwrap();
        std::fs::write(
            root.join("environments/test/environment.toml"),
            "[environment]\nname='test'\ndescription='test'\nimage='agent-hub/job-code:2026-09-04'\nnetwork_policy='none'\napproval_mode='auto'\n",
        )
        .unwrap();
        std::fs::write(
            root.join("environments/test/agent-hub-permissions.json"),
            "{}",
        )
        .unwrap();
        std::fs::write(root.join("environments/test/dippy.conf"), "allow true\n").unwrap();
        std::fs::write(
            root.join("environments/test/opencode.json"),
            serde_json::to_vec(&serde_json::json!({
                "model":"openai/gpt-5.6-sol",
                "small_model":"openai/gpt-5.6-sol",
                "enabled_providers":["openai"],
                "permission":{"*":"allow"}
            }))
            .unwrap(),
        )
        .unwrap();

        let captured = validate_authoritative_config_root(&root).unwrap();
        let original = captured.network_policies["none"].clone();
        std::fs::write(
            &policy_path,
            "[policy]\nname='none'\ndescription='replacement'\ndocker_network='none'\n[policy.env]\nCHANGED='1'\n",
        )
        .unwrap();
        assert_eq!(captured.network_policies["none"].policy, original.policy);
        assert_eq!(
            captured.network_policies["none"].relative_files,
            original.relative_files
        );
        assert_eq!(
            captured.network_policies["none"].directory_digest,
            original.directory_digest
        );

        let refreshed = validate_authoritative_config_root(&root).unwrap();
        assert_ne!(refreshed.network_policies["none"].policy, original.policy);
        assert_ne!(
            refreshed.network_policies["none"].directory_digest,
            original.directory_digest
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn captured_image_overlay_and_triage_snapshots_resist_toctou_replacement() {
        let root =
            std::env::temp_dir().join(format!("manifest-snapshot-toctou-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("images/job-code")).unwrap();
        std::fs::create_dir_all(root.join("overlays")).unwrap();
        std::fs::write(
            root.join("system.toml"),
            "[system]\noperator='test'\nbot_identity='test'\n[system.environments]\ninstalled=[]\ntriage_environment='none'\n[system.network_policies]\ninstalled=[]\n[system.images]\ninstalled=['job-code']\n[system.overlays]\nregistry='overlays/registry.toml'\n[system.triage]\nconfig='triage.toml'\n",
        )
        .unwrap();
        std::fs::write(
            root.join("local-agent.json"),
            serde_json::to_vec(&serde_json::json!({
                "root_registry":{"roots":[{"identity":"test","path":"/roots/test","roles":["workspace_ro"]}]},
                "default_agent":{"kind":"generic","command":"true"}
            }))
            .unwrap(),
        )
        .unwrap();
        let image_path = root.join("images/job-code/image.toml");
        let dockerfile_path = root.join("images/job-code/Dockerfile");
        let preflight_path = root.join("images/job-code/preflight.sh");
        let overlay_path = root.join("overlays/registry.toml");
        let triage_path = root.join("triage.toml");
        std::fs::write(&image_path, "[image]\nname='original'\n").unwrap();
        std::fs::write(&dockerfile_path, "FROM image@sha256:original\n").unwrap();
        std::fs::write(&preflight_path, "original preflight\n").unwrap();
        std::fs::write(
            &overlay_path,
            "[[overlays]]\nname='original'\nprovider='command'\n[overlays.config]\ncommand='true'\n",
        )
        .unwrap();
        std::fs::write(
            &triage_path,
            "[prompt]\nsystem='original system'\nuser='original user'\n",
        )
        .unwrap();

        let captured = validate_authoritative_config_root(&root).unwrap();
        let image = captured.images["job-code"].clone();
        let overlay = captured.overlay_registry.clone().unwrap();
        let triage = captured.triage_prompt.clone().unwrap();
        std::fs::write(&preflight_path, "replacement preflight\n").unwrap();
        let refreshed_context = validate_authoritative_config_root(&root).unwrap();
        assert_eq!(
            refreshed_context.images["job-code"].file.digest,
            image.file.digest
        );
        assert_ne!(
            refreshed_context.images["job-code"].directory_digest,
            image.directory_digest
        );
        std::fs::write(&image_path, "[image]\nname='replacement'\n").unwrap();
        std::fs::write(
            &overlay_path,
            "[[overlays]]\nname='replacement'\nprovider='command'\n[overlays.config]\ncommand='false'\n",
        )
        .unwrap();
        std::fs::write(
            &triage_path,
            "[prompt]\nsystem='replacement system'\nuser='replacement user'\n",
        )
        .unwrap();
        assert_eq!(captured.images["job-code"].file.bytes, image.file.bytes);
        assert_eq!(captured.images["job-code"].image, image.image);
        assert_eq!(
            captured.overlay_registry.as_ref().unwrap().overlays,
            overlay.overlays
        );
        assert_eq!(
            captured.triage_prompt.as_ref().unwrap().prompt,
            triage.prompt
        );

        let refreshed = validate_authoritative_config_root(&root).unwrap();
        assert_ne!(refreshed.images["job-code"].file.digest, image.file.digest);
        assert_ne!(refreshed.images["job-code"].image, image.image);
        assert_ne!(
            refreshed.overlay_registry.unwrap().overlays,
            overlay.overlays
        );
        assert_ne!(refreshed.triage_prompt.unwrap().prompt, triage.prompt);
        std::fs::remove_dir_all(root).unwrap();
    }
}
