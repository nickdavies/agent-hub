use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{EnvironmentConfig, NetworkPolicyConfig, SystemConfig, flatten_policy};

pub const DURABLE_OPENCODE_MODEL: &str = "openai/gpt-5.6-sol";
pub const DURABLE_OPENCODE_IMAGE: &str = "agent-hub/job-code:2026-09-09";
pub const APPROVAL_BUNDLE_PROTOCOL: &str = protocol::APPROVAL_BUNDLE_PROTOCOL;
pub const APPROVAL_PLUGIN_URI: &str =
    "file:///opencode/config/opencode/approval/agent-hub-opencode-plugin.js";
const APPROVAL_WHEEL: &str = "dippy-0.2.7-py3-none-any.whl";
const APPROVAL_FILES: [&str; 4] = [
    "agent-hub-opencode-plugin.js",
    "agent-hub-permissions.json",
    APPROVAL_WHEEL,
    "dippy.policy",
];
pub const OPENCODE_CONFIG_SENTINEL: &[u8] =
    b"node_modules\npackage.json\npackage-lock.json\nbun.lock\n.gitignore";
pub const OPENCODE_CONFIG_SENTINEL_SHA256: &str =
    "663a068e76d264d0bc6740f5450b6c4193c7b41ecf5e0dc222485b8a17404d95";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentBundleFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentBundle {
    pub files: Vec<EnvironmentBundleFile>,
    pub digest: String,
    pub approval_bundle: Option<ApprovalBundleIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalBundleIdentity {
    pub name: String,
    pub digest: String,
    pub protocol: String,
    pub manifest_sha256: String,
    pub gateway_commit: String,
    pub gateway_archive_sha256: String,
    pub plugin_sha256: String,
    pub dippy_commit: String,
    pub dippy_archive_sha256: String,
    pub dippy_wheel_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalBundleManifest {
    schema: String,
    version: u32,
    protocol: String,
    sources: ApprovalBundleSources,
    build: ApprovalBundleBuild,
    artifacts: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalBundleSources {
    agent_hub: AgentHubApprovalSource,
    dippy: ApprovalSource,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalSource {
    repository: String,
    commit: String,
    tree: String,
    archive_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentHubApprovalSource {
    repository: String,
    commit: String,
    tree: String,
    archive_sha256: String,
    plugin_entrypoint: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalBundleBuild {
    dippy: String,
    plugin: String,
}

impl EnvironmentBundle {
    pub fn from_files(files: Vec<EnvironmentBundleFile>) -> Result<Self, String> {
        let mut ordered = BTreeMap::new();
        for file in files {
            validate_relative_path(&file.path)?;
            if ordered.insert(file.path.clone(), file.bytes).is_some() {
                return Err(format!(
                    "environment bundle contains duplicate path '{}'",
                    file.path
                ));
            }
        }
        let files = ordered
            .into_iter()
            .map(|(path, bytes)| EnvironmentBundleFile { path, bytes })
            .collect::<Vec<_>>();
        let digest = digest_files(&files);
        Ok(Self {
            files,
            digest,
            approval_bundle: None,
        })
    }

    pub fn write_to(&self, destination: &Path) -> Result<(), String> {
        if destination.exists() {
            std::fs::remove_dir_all(destination)
                .map_err(|error| format!("failed to clear {}: {error}", destination.display()))?;
        }
        std::fs::create_dir_all(destination)
            .map_err(|error| format!("failed to create {}: {error}", destination.display()))?;
        for file in &self.files {
            validate_relative_path(&file.path)?;
            let path = destination.join(&file.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
            }
            std::fs::write(&path, &file.bytes)
                .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
        }
        Ok(())
    }

    pub fn verify_digest(&self, authorized_digest: &str) -> Result<(), String> {
        self.verify_opencode_sentinel()?;
        if self.digest == authorized_digest {
            Ok(())
        } else {
            Err("materialized environment bundle digest does not match authorization".to_string())
        }
    }

    pub fn verify_opencode_sentinel(&self) -> Result<(), String> {
        match self.files.iter().find(|file| file.path == ".gitignore") {
            Some(file) if file.bytes == OPENCODE_CONFIG_SENTINEL => Ok(()),
            Some(_) => {
                Err("environment bundle has invalid OpenCode .gitignore sentinel".to_string())
            }
            None => Err("environment bundle is missing OpenCode .gitignore sentinel".to_string()),
        }
    }
}

pub fn materialize_environment_bundle(
    config_root: &Path,
    environment_name: &str,
) -> Result<EnvironmentBundle, String> {
    let system = SystemConfig::load(&config_root.join("system.toml"))?;
    materialize_environment_bundle_with_system(config_root, environment_name, &system)
}

pub fn materialize_environment_bundle_with_system(
    config_root: &Path,
    environment_name: &str,
    system: &SystemConfig,
) -> Result<EnvironmentBundle, String> {
    let policies = load_network_policies(config_root, system)?;
    materialize_environment_bundle_with_snapshot(config_root, environment_name, system, &policies)
}

pub fn materialize_environment_bundle_with_snapshot(
    config_root: &Path,
    environment_name: &str,
    system: &SystemConfig,
    policies: &HashMap<String, NetworkPolicyConfig>,
) -> Result<EnvironmentBundle, String> {
    validate_name(environment_name, "environment")?;
    let source = config_root.join("environments").join(environment_name);
    reject_symlink(&source)?;
    let environment_path = source.join("environment.toml");
    reject_symlink(&environment_path)?;
    let environment_bytes = std::fs::read(&environment_path)
        .map_err(|error| format!("failed to read {}: {error}", environment_path.display()))?;
    let environment = EnvironmentConfig::from_toml(
        std::str::from_utf8(&environment_bytes)
            .map_err(|error| format!("environment config is not UTF-8: {error}"))?,
        &environment_path.display().to_string(),
    )?;
    if environment.name != environment_name {
        return Err(format!(
            "environment directory '{environment_name}' contains manifest for '{}'",
            environment.name
        ));
    }
    if environment.image != DURABLE_OPENCODE_IMAGE {
        return Err(format!(
            "environment '{}' must use pinned OpenCode image '{DURABLE_OPENCODE_IMAGE}'",
            environment.name
        ));
    }

    let flattened = flatten_policy(&environment.network_policy, policies, system)?;
    let network_policy = serde_json::to_vec_pretty(&json!({
        "name": flattened.name,
        "description": flattened.description,
        "unrestricted": flattened.unrestricted,
        "docker_network": flattened.docker_network,
        "hosts": flattened.hosts,
        "env": flattened.env,
    }))
    .map_err(|error| format!("failed to serialize network policy: {error}"))?;

    let mut files = BTreeMap::new();
    insert_file(
        &mut files,
        ".gitignore",
        OPENCODE_CONFIG_SENTINEL.to_vec(),
        false,
    )?;
    insert_file(&mut files, "environment.toml", environment_bytes, false)?;
    {
        let path = source.join("opencode.json");
        reject_symlink(&path)?;
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "required environment file {} is unavailable: {error}",
                path.display()
            )
        })?;
        validate_durable_opencode_config(&bytes)?;
        if environment.approval_mode == "auto" {
            validate_automatic_opencode_plugins(&bytes)?;
            validate_automatic_opencode_permissions(
                &bytes,
                &environment.name,
                &environment.network_policy,
            )?;
        } else if environment.approval_mode == "gateway" {
            validate_supervised_opencode_config(&bytes)?;
        } else {
            return Err(format!(
                "environment '{}' has unsupported approval mode '{}'",
                environment.name, environment.approval_mode
            ));
        }
        insert_file(&mut files, "opencode.json", bytes, false)?;
    }
    let approval_bundle = match environment.approval_mode.as_str() {
        "auto" => {
            if environment.approval_bundle.is_some() {
                return Err("automatic environments must not select an approval bundle".into());
            }
            None
        }
        "gateway" => {
            let name = environment
                .approval_bundle
                .as_deref()
                .ok_or_else(|| "supervised environments require an approval_bundle".to_string())?;
            Some(collect_approval_bundle(
                config_root,
                name,
                system,
                &mut files,
            )?)
        }
        other => {
            return Err(format!(
                "environment '{}' has unsupported approval mode '{other}'",
                environment.name
            ));
        }
    };
    insert_file(&mut files, "network_policy.json", network_policy, false)?;

    for directory in ["prompts", "instructions"] {
        let directory_source = source.join(directory);
        if directory_source.exists() {
            collect_directory(&directory_source, directory, &mut files, false)?;
        }
    }
    for skill in &environment.include_skills {
        validate_name(skill, "skill")?;
        collect_directory(
            &config_root.join("shared").join("skills").join(skill),
            &format!("skills/{skill}"),
            &mut files,
            false,
        )?;
    }
    // Environment-local skills and agents intentionally replace same-path shared files.
    // This preserves the established environment packaging precedence while the final
    // BTreeMap still rejects every other duplicate output path.
    collect_directory(&source.join("skills"), "skills", &mut files, true)?;

    for agent in &environment.include_agents {
        validate_name(agent, "agent")?;
        let source_agent = config_root
            .join("shared")
            .join("agents")
            .join(format!("{agent}.md"));
        reject_symlink(&source_agent)?;
        let bytes = std::fs::read(&source_agent).map_err(|error| {
            format!(
                "required shared agent {} is unavailable: {error}",
                source_agent.display()
            )
        })?;
        validate_agent_model(&bytes, &source_agent)?;
        insert_file(&mut files, &format!("agents/{agent}.md"), bytes, false)?;
    }
    collect_directory(&source.join("agents"), "agents", &mut files, true)?;
    for (path, bytes) in files
        .iter()
        .filter(|(path, _)| path.starts_with("agents/") && path.ends_with(".md"))
    {
        validate_agent_model(bytes, Path::new(path))?;
    }

    let mut bundle = EnvironmentBundle::from_files(
        files
            .into_iter()
            .map(|(path, bytes)| EnvironmentBundleFile { path, bytes })
            .collect(),
    )?;
    bundle.approval_bundle = approval_bundle;
    Ok(bundle)
}

pub fn validate_durable_opencode_config(bytes: &[u8]) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse opencode.json: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "opencode.json must be an object".to_string())?;
    for key in ["model", "small_model"] {
        if object.get(key).and_then(serde_json::Value::as_str) != Some(DURABLE_OPENCODE_MODEL) {
            return Err(format!(
                "opencode.json {key} must be '{DURABLE_OPENCODE_MODEL}'"
            ));
        }
    }
    let providers = object
        .get("enabled_providers")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "opencode.json enabled_providers must be ['openai']".to_string())?;
    if providers.as_slice() != [serde_json::Value::String("openai".to_string())] {
        return Err("opencode.json enabled_providers must contain only 'openai'".to_string());
    }
    reject_copilot_declarations(&value)
}

fn validate_automatic_opencode_plugins(bytes: &[u8]) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse opencode.json: {error}"))?;
    if value.get("plugin").is_some() {
        return Err("automatic OpenCode environments must not load attended plugins".to_string());
    }
    Ok(())
}

fn validate_automatic_opencode_permissions(
    bytes: &[u8],
    environment: &str,
    network_policy: &str,
) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse opencode.json: {error}"))?;
    let permissions = value
        .get("permission")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            "automatic OpenCode environments require explicit permissions".to_string()
        })?;
    if permissions.get("*").and_then(serde_json::Value::as_str) != Some("allow") {
        return Err("automatic OpenCode permissions must explicitly allow by default".to_string());
    }
    if contains_action(&serde_json::Value::Object(permissions.clone()), "ask") {
        return Err("automatic OpenCode environments must not contain ask permissions".to_string());
    }
    let webfetch = permissions
        .get("webfetch")
        .and_then(serde_json::Value::as_str);
    if environment == "research-auto" && webfetch != Some("deny") {
        return Err("research-auto must explicitly deny WebFetch".to_string());
    }
    if environment == "research-web-auto"
        && (webfetch != Some("allow") || network_policy != "guarded-web")
    {
        return Err(
            "research-web-auto may explicitly allow WebFetch only with guarded-web".to_string(),
        );
    }
    if webfetch == Some("allow") && network_policy != "guarded-web" {
        return Err("explicit WebFetch allow requires guarded-web".to_string());
    }
    Ok(())
}

fn validate_supervised_opencode_config(bytes: &[u8]) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse opencode.json: {error}"))?;
    let plugins = value
        .get("plugin")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "supervised OpenCode environments require the bundled plugin".to_string())?;
    if plugins.as_slice() != [serde_json::Value::String(APPROVAL_PLUGIN_URI.to_string())] {
        return Err(
            "supervised OpenCode plugin URI must select the immutable bundled plugin".into(),
        );
    }
    let permissions = value
        .get("permission")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            "supervised OpenCode environments require explicit permissions".to_string()
        })?;
    validate_supervised_permission_tree(
        &serde_json::Value::Object(permissions.clone()),
        "permission",
    )?;
    if let Some(agents) = value.get("agent") {
        let agents = agents
            .as_object()
            .ok_or_else(|| "supervised OpenCode agent overrides must be an object".to_string())?;
        for (name, agent) in agents {
            let agent = agent.as_object().ok_or_else(|| {
                format!("supervised OpenCode agent override '{name}' must be an object")
            })?;
            if let Some(permission) = agent.get("permission") {
                validate_supervised_permission_tree(
                    permission,
                    &format!("agent.{name}.permission"),
                )?;
            }
        }
    }
    Ok(())
}

fn validate_supervised_permission_tree(
    value: &serde_json::Value,
    location: &str,
) -> Result<(), String> {
    match value.as_object() {
        Some(entries)
            if entries.len() == 1
                && entries.get("*").and_then(serde_json::Value::as_str) == Some("deny") =>
        {
            Ok(())
        }
        _ => Err(format!(
            "supervised OpenCode {location} must be the exact fail-closed {{\"*\":\"deny\"}} startup fallback"
        )),
    }
}

fn collect_approval_bundle(
    config_root: &Path,
    name: &str,
    system: &SystemConfig,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<ApprovalBundleIdentity, String> {
    validate_name(name, "approval bundle")?;
    let source = config_root
        .join("shared")
        .join("approval-bundles")
        .join(name);
    reject_symlink(&source)?;
    let mut entries = std::fs::read_dir(&source)
        .map_err(|error| format!("failed to read {}: {error}", source.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read {}: {error}", source.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let expected = BTreeMap::from_iter(
        std::iter::once("manifest.json")
            .chain(APPROVAL_FILES)
            .map(|name| (name.to_string(), ())),
    );
    let mut captured = BTreeMap::new();
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("approval bundle path in {} is not UTF-8", source.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("failed to inspect {}: {error}", entry.path().display()))?;
        if !file_type.is_file() || file_type.is_symlink() {
            return Err(format!(
                "approval bundle entry {} is not a regular file",
                entry.path().display()
            ));
        }
        if captured
            .insert(
                name.clone(),
                std::fs::read(entry.path()).map_err(|error| error.to_string())?,
            )
            .is_some()
        {
            return Err(format!("approval bundle contains duplicate file '{name}'"));
        }
    }
    if captured
        .keys()
        .map(|name| (name.clone(), ()))
        .collect::<BTreeMap<_, _>>()
        != expected
    {
        return Err("approval bundle file inventory is not exact".into());
    }
    let manifest_bytes = captured.get("manifest.json").expect("inventory checked");
    let manifest_sha256 = hex::encode(Sha256::digest(manifest_bytes));
    let manifest: ApprovalBundleManifest = serde_json::from_slice(manifest_bytes)
        .map_err(|error| format!("failed to parse approval bundle manifest: {error}"))?;
    validate_approval_manifest(&manifest)?;
    for artifact in APPROVAL_FILES {
        let expected = &manifest.artifacts[artifact];
        let actual = hex::encode(Sha256::digest(&captured[artifact]));
        if &actual != expected {
            return Err(format!(
                "approval bundle artifact '{artifact}' digest mismatch"
            ));
        }
    }
    validate_approval_permissions(&captured["agent-hub-permissions.json"])?;
    let policy = std::str::from_utf8(&captured["dippy.policy"])
        .map_err(|error| format!("Dippy policy is not UTF-8: {error}"))?;
    if !policy.starts_with("dippy-policy 1\n") {
        return Err("approval bundle Dippy policy version is unsupported".into());
    }
    for (path, bytes) in &captured {
        insert_file(files, &format!("approval/{path}"), bytes.clone(), false)?;
    }
    let digest = digest_named_files(&captured);
    let identity = ApprovalBundleIdentity {
        name: name.to_string(),
        digest,
        protocol: manifest.protocol,
        manifest_sha256,
        gateway_commit: manifest.sources.agent_hub.commit,
        gateway_archive_sha256: manifest.sources.agent_hub.archive_sha256,
        plugin_sha256: manifest.artifacts["agent-hub-opencode-plugin.js"].clone(),
        dippy_commit: manifest.sources.dippy.commit,
        dippy_archive_sha256: manifest.sources.dippy.archive_sha256,
        dippy_wheel_sha256: manifest.artifacts[APPROVAL_WHEEL].clone(),
    };
    let trust = system.approval_bundle_trust.get(name).ok_or_else(|| {
        format!("approval bundle '{name}' has no authoritative system trust root")
    })?;
    if trust.manifest_sha256 != identity.manifest_sha256
        || trust.gateway_commit != identity.gateway_commit
        || trust.gateway_archive_sha256 != identity.gateway_archive_sha256
        || trust.plugin_sha256 != identity.plugin_sha256
        || trust.dippy_commit != identity.dippy_commit
        || trust.dippy_archive_sha256 != identity.dippy_archive_sha256
        || trust.dippy_wheel_sha256 != identity.dippy_wheel_sha256
    {
        return Err(format!(
            "approval bundle '{name}' does not exactly match its authoritative system trust root"
        ));
    }
    Ok(identity)
}

fn validate_approval_manifest(manifest: &ApprovalBundleManifest) -> Result<(), String> {
    if manifest.schema != "agent-hub.approval-bundle"
        || manifest.version != 1
        || manifest.protocol != APPROVAL_BUNDLE_PROTOCOL
        || manifest.sources.agent_hub.plugin_entrypoint != "provider-plugins/opencode/index.ts"
        || manifest
            .artifacts
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            != APPROVAL_FILES
    {
        return Err(
            "approval bundle manifest identity or artifact inventory is unsupported".into(),
        );
    }
    for (repository, commit, tree, archive) in [
        (
            &manifest.sources.agent_hub.repository,
            &manifest.sources.agent_hub.commit,
            &manifest.sources.agent_hub.tree,
            &manifest.sources.agent_hub.archive_sha256,
        ),
        (
            &manifest.sources.dippy.repository,
            &manifest.sources.dippy.commit,
            &manifest.sources.dippy.tree,
            &manifest.sources.dippy.archive_sha256,
        ),
    ] {
        if !repository.starts_with("https://")
            || !is_lower_hex(commit, 40)
            || !is_lower_hex(tree, 40)
            || !is_lower_hex(archive, 64)
        {
            return Err("approval bundle source identity is invalid".into());
        }
    }
    if manifest.build.dippy.trim().is_empty() || manifest.build.plugin.trim().is_empty() {
        return Err("approval bundle build identity is empty".into());
    }
    if manifest
        .artifacts
        .values()
        .any(|digest| !is_lower_hex(digest, 64))
    {
        return Err("approval bundle artifact digest is invalid".into());
    }
    Ok(())
}

fn validate_approval_permissions(bytes: &[u8]) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse approval permissions: {error}"))?;
    let command = format!(
        "/usr/bin/env PYTHONPATH=/opencode/config/opencode/approval/{APPROVAL_WHEEL} /usr/bin/python3 -m dippy --immutable-policy /opencode/config/opencode/approval/dippy.policy"
    );
    let expected = json!({
        "version": 1,
        "default": "ask",
        "rules": [
            {"tools": ["@shell"], "action": "delegate", "command": command}
        ]
    });
    if value != expected {
        return Err(
            "approval bundle permissions do not use the immutable bundled Dippy policy".into(),
        );
    }
    Ok(())
}

fn digest_named_files(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut digest = Sha256::new();
    for (path, bytes) in files {
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    hex::encode(digest.finalize())
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn contains_action(value: &serde_json::Value, action: &str) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object.values().any(|value| contains_action(value, action))
        }
        serde_json::Value::Array(values) => {
            values.iter().any(|value| contains_action(value, action))
        }
        serde_json::Value::String(value) => value == action,
        _ => false,
    }
}

fn validate_agent_model(bytes: &[u8], path: &Path) -> Result<(), String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| format!("agent {} is not UTF-8: {error}", path.display()))?;
    if let Some(model) = text
        .lines()
        .find_map(|line| line.strip_prefix("model:").map(str::trim))
        && model != DURABLE_OPENCODE_MODEL
    {
        return Err(format!(
            "agent {} model must be '{DURABLE_OPENCODE_MODEL}'",
            path.display()
        ));
    }
    Ok(())
}

fn reject_copilot_declarations(value: &serde_json::Value) -> Result<(), String> {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if key.to_ascii_lowercase().contains("copilot") {
                    return Err("opencode.json must not declare GitHub Copilot".to_string());
                }
                reject_copilot_declarations(value)?;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                reject_copilot_declarations(value)?;
            }
        }
        serde_json::Value::String(value) if value.to_ascii_lowercase().contains("copilot") => {
            return Err("opencode.json must not declare GitHub Copilot".to_string());
        }
        _ => {}
    }
    Ok(())
}

fn load_network_policies(
    config_root: &Path,
    system: &SystemConfig,
) -> Result<HashMap<String, NetworkPolicyConfig>, String> {
    let mut policies = HashMap::new();
    if let Some(configured) = &system.network_policies {
        for name in &configured.installed {
            validate_name(name, "network policy")?;
            let policy = NetworkPolicyConfig::load(
                &config_root
                    .join("network_policies")
                    .join(name)
                    .join("policy.toml"),
            )?;
            if policies.insert(policy.name.clone(), policy).is_some() {
                return Err(format!("duplicate network policy '{name}'"));
            }
        }
    }
    Ok(policies)
}

fn collect_directory(
    source: &Path,
    destination: &str,
    files: &mut BTreeMap<String, Vec<u8>>,
    replace: bool,
) -> Result<(), String> {
    if !source.exists() {
        return if replace {
            Ok(())
        } else {
            Err(format!(
                "required directory {} is unavailable",
                source.display()
            ))
        };
    }
    reject_symlink(source)?;
    let mut entries = std::fs::read_dir(source)
        .map_err(|error| format!("failed to read {}: {error}", source.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read {}: {error}", source.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry
            .file_type()
            .map_err(|error| format!("failed to inspect {}: {error}", entry.path().display()))?;
        if file_type.is_symlink() {
            return Err(format!(
                "environment bundle source contains symlink {}",
                entry.path().display()
            ));
        }
        let name = entry.file_name().into_string().map_err(|_| {
            format!(
                "environment bundle path in {} is not UTF-8",
                source.display()
            )
        })?;
        let relative = format!("{destination}/{name}");
        if file_type.is_dir() {
            collect_directory(&entry.path(), &relative, files, replace)?;
        } else if file_type.is_file() {
            insert_file(
                files,
                &relative,
                std::fs::read(entry.path()).map_err(|error| error.to_string())?,
                replace,
            )?;
        } else {
            return Err(format!(
                "unsupported environment bundle entry {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "environment bundle source contains symlink {}",
            path.display()
        ));
    }
    Ok(())
}

fn insert_file(
    files: &mut BTreeMap<String, Vec<u8>>,
    path: &str,
    bytes: Vec<u8>,
    replace: bool,
) -> Result<(), String> {
    validate_relative_path(path)?;
    if !replace && files.contains_key(path) {
        return Err(format!(
            "environment bundle contains duplicate path '{path}'"
        ));
    }
    files.insert(path.to_string(), bytes);
    Ok(())
}

fn validate_name(name: &str, label: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(format!("invalid {label} name '{name}'"));
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    let candidate = PathBuf::from(path);
    if path.is_empty()
        || candidate.is_absolute()
        || candidate
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("invalid environment bundle path '{path}'"));
    }
    Ok(())
}

fn digest_files(files: &[EnvironmentBundleFile]) -> String {
    let mut digest = Sha256::new();
    for file in files {
        digest.update((file.path.len() as u64).to_be_bytes());
        digest.update(file.path.as_bytes());
        digest.update((file.bytes.len() as u64).to_be_bytes());
        digest.update(&file.bytes);
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_approval_bundle(
        root: &Path,
        manifest_extra: Option<(&str, serde_json::Value)>,
    ) -> SystemConfig {
        let bundle = root.join("shared/approval-bundles/test");
        std::fs::create_dir_all(&bundle).unwrap();
        let command = format!(
            "/usr/bin/env PYTHONPATH=/opencode/config/opencode/approval/{APPROVAL_WHEEL} /usr/bin/python3 -m dippy --immutable-policy /opencode/config/opencode/approval/dippy.policy"
        );
        let artifacts = BTreeMap::from([
            ("agent-hub-opencode-plugin.js", b"plugin".to_vec()),
            (
                "agent-hub-permissions.json",
                serde_json::to_vec(&json!({
                    "version":1,"default":"ask","rules":[
                        {"tools":["@shell"],"action":"delegate","command":command}
                    ]
                }))
                .unwrap(),
            ),
            (APPROVAL_WHEEL, b"wheel".to_vec()),
            ("dippy.policy", b"dippy-policy 1\ndeny rm\n".to_vec()),
        ]);
        for (name, bytes) in &artifacts {
            std::fs::write(bundle.join(name), bytes).unwrap();
        }
        let mut manifest = json!({
            "schema":"agent-hub.approval-bundle",
            "version":1,
            "protocol":APPROVAL_BUNDLE_PROTOCOL,
            "sources":{
                "agent_hub":{"repository":"https://example.com/agent-hub.git","commit":"a".repeat(40),"tree":"b".repeat(40),"archive_sha256":"c".repeat(64),"plugin_entrypoint":"provider-plugins/opencode/index.ts"},
                "dippy":{"repository":"https://example.com/dippy.git","commit":"d".repeat(40),"tree":"e".repeat(40),"archive_sha256":"f".repeat(64)}
            },
            "build":{"dippy":"uv build","plugin":"bun build"},
            "artifacts":artifacts.iter().map(|(name, bytes)| ((*name).to_string(), hex::encode(Sha256::digest(bytes)))).collect::<BTreeMap<_, _>>()
        });
        if let Some((name, value)) = manifest_extra {
            manifest.as_object_mut().unwrap().insert(name.into(), value);
        }
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        std::fs::write(bundle.join("manifest.json"), &manifest_bytes).unwrap();
        SystemConfig::from_toml(
            &format!(
                r#"[system]
operator = "test"
bot_identity = "test"

[system.approval_bundle_trust.test]
manifest_sha256 = "{}"
gateway_commit = "{}"
gateway_archive_sha256 = "{}"
plugin_sha256 = "{}"
dippy_commit = "{}"
dippy_archive_sha256 = "{}"
dippy_wheel_sha256 = "{}"
"#,
                hex::encode(Sha256::digest(&manifest_bytes)),
                "a".repeat(40),
                "c".repeat(64),
                manifest["artifacts"]["agent-hub-opencode-plugin.js"]
                    .as_str()
                    .unwrap(),
                "d".repeat(40),
                "f".repeat(64),
                manifest["artifacts"][APPROVAL_WHEEL].as_str().unwrap(),
            ),
            "test system",
        )
        .unwrap()
    }

    #[test]
    fn approval_bundle_materialization_is_strict_and_tamper_evident() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-approval-bundle-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let system = write_test_approval_bundle(&root, None);
        let mut files = BTreeMap::new();
        let identity = collect_approval_bundle(&root, "test", &system, &mut files).unwrap();
        assert_eq!(identity.protocol, APPROVAL_BUNDLE_PROTOCOL);
        assert_eq!(files.len(), 5);
        assert!(files.contains_key("approval/agent-hub-opencode-plugin.js"));

        let mut tampered_trust = system.clone();
        tampered_trust
            .approval_bundle_trust
            .get_mut("test")
            .unwrap()
            .plugin_sha256 = "0".repeat(64);
        assert!(
            collect_approval_bundle(&root, "test", &tampered_trust, &mut BTreeMap::new())
                .unwrap_err()
                .contains("trust root")
        );

        std::fs::write(
            root.join("shared/approval-bundles/test/agent-hub-opencode-plugin.js"),
            "tampered",
        )
        .unwrap();
        assert!(collect_approval_bundle(&root, "test", &system, &mut BTreeMap::new()).is_err());

        std::fs::remove_dir_all(root.join("shared/approval-bundles/test")).unwrap();
        let system = write_test_approval_bundle(&root, Some(("unknown", json!(true))));
        assert!(collect_approval_bundle(&root, "test", &system, &mut BTreeMap::new()).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bundle_is_order_independent_and_binds_paths_and_bytes() {
        let first = EnvironmentBundle::from_files(vec![
            EnvironmentBundleFile {
                path: "b".into(),
                bytes: vec![2],
            },
            EnvironmentBundleFile {
                path: "a".into(),
                bytes: vec![1],
            },
        ])
        .unwrap();
        let second = EnvironmentBundle::from_files(vec![
            EnvironmentBundleFile {
                path: "a".into(),
                bytes: vec![1],
            },
            EnvironmentBundleFile {
                path: "b".into(),
                bytes: vec![2],
            },
        ])
        .unwrap();
        assert_eq!(first, second);
        assert_ne!(
            first.digest,
            EnvironmentBundle::from_files(vec![
                EnvironmentBundleFile {
                    path: "a".into(),
                    bytes: vec![1]
                },
                EnvironmentBundleFile {
                    path: "b".into(),
                    bytes: vec![3]
                },
            ])
            .unwrap()
            .digest
        );
        assert_ne!(
            first.digest,
            EnvironmentBundle::from_files(vec![
                EnvironmentBundleFile {
                    path: "a".into(),
                    bytes: vec![1]
                },
                EnvironmentBundleFile {
                    path: "c".into(),
                    bytes: vec![2]
                },
            ])
            .unwrap()
            .digest
        );
    }

    #[test]
    fn bundle_rejects_duplicates_and_traversal() {
        assert!(
            EnvironmentBundle::from_files(vec![
                EnvironmentBundleFile {
                    path: "a".into(),
                    bytes: vec![]
                },
                EnvironmentBundleFile {
                    path: "a".into(),
                    bytes: vec![]
                },
            ])
            .is_err()
        );
        for path in ["", "/absolute", "../escape", "a/../escape"] {
            assert!(
                EnvironmentBundle::from_files(vec![EnvironmentBundleFile {
                    path: path.into(),
                    bytes: vec![]
                }])
                .is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn durable_config_requires_openai_pins_and_rejects_copilot() {
        let valid = serde_json::to_vec(&json!({
            "model": DURABLE_OPENCODE_MODEL,
            "small_model": DURABLE_OPENCODE_MODEL,
            "enabled_providers": ["openai"]
        }))
        .unwrap();
        validate_durable_opencode_config(&valid).unwrap();
        for invalid in [
            json!({"model":"github-copilot/gpt", "small_model":DURABLE_OPENCODE_MODEL, "enabled_providers":["openai"]}),
            json!({"model":DURABLE_OPENCODE_MODEL, "small_model":DURABLE_OPENCODE_MODEL, "enabled_providers":["openai", "github-copilot"]}),
            json!({"model":DURABLE_OPENCODE_MODEL, "small_model":DURABLE_OPENCODE_MODEL, "enabled_providers":["openai"], "plugin":["opencode-copilot-auth"]}),
        ] {
            assert!(
                validate_durable_opencode_config(&serde_json::to_vec(&invalid).unwrap()).is_err()
            );
        }
        let attended = serde_json::to_vec(&json!({
            "model": DURABLE_OPENCODE_MODEL,
            "small_model": DURABLE_OPENCODE_MODEL,
            "enabled_providers": ["openai"],
            "plugin": ["agent-hub-attended"]
        }))
        .unwrap();
        assert!(validate_automatic_opencode_plugins(&attended).is_err());
    }

    #[test]
    fn approval_modes_require_fail_closed_supervision_or_non_interactive_auto_rules() {
        let supervised = serde_json::to_vec(&json!({
            "permission":{"*":"deny"},
            "plugin":[APPROVAL_PLUGIN_URI],
            "agent":{"review":{"permission":{"*":"deny"}}}
        }))
        .unwrap();
        validate_supervised_opencode_config(&supervised).unwrap();
        for invalid in [
            json!({"permission":{"*":"allow"}}),
            json!({"permission":{"*":"deny"},"plugin":["unexpected"]}),
            json!({"permission":{"*":"deny"},"plugin":[APPROVAL_PLUGIN_URI],"agent":{"review":{"permission":{"bash":"allow"}}}}),
            json!({"permission":{"*":"deny","bash":{"git *":"allow"}},"plugin":[APPROVAL_PLUGIN_URI]}),
            json!({"permission":{"*":"ask"},"plugin":[APPROVAL_PLUGIN_URI]}),
        ] {
            assert!(
                validate_supervised_opencode_config(&serde_json::to_vec(&invalid).unwrap())
                    .is_err()
            );
        }

        validate_automatic_opencode_permissions(
            &serde_json::to_vec(&json!({"permission":{"*":"allow","webfetch":"deny"}})).unwrap(),
            "research-auto",
            "none",
        )
        .unwrap();
        validate_automatic_opencode_permissions(
            &serde_json::to_vec(&json!({"permission":{"*":"allow","webfetch":"allow"}})).unwrap(),
            "research-web-auto",
            "guarded-web",
        )
        .unwrap();
        assert!(
            validate_automatic_opencode_permissions(
                &serde_json::to_vec(&json!({"permission":{"*":"allow","bash":"ask"}})).unwrap(),
                "code-auto",
                "build",
            )
            .is_err()
        );
        assert!(
            validate_automatic_opencode_permissions(
                &serde_json::to_vec(&json!({"permission":{"*":"allow","webfetch":"allow"}}))
                    .unwrap(),
                "research-web-auto",
                "build",
            )
            .is_err()
        );
    }

    #[test]
    fn materialization_copies_shared_files_and_changes_digest_with_content() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-bundle-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for directory in [
            "environments/test",
            "network_policies/none",
            "shared/skills/example",
            "shared/agents",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(
            root.join("system.toml"),
            r#"[system]
operator = "test"
bot_identity = "test"
[system.network_policies]
installed = ["none"]
[system.environments]
installed = ["test"]
triage_environment = "test"
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("network_policies/none/policy.toml"),
            "[policy]\nname = \"none\"\ndescription = \"none\"\ndocker_network = \"none\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("environments/test/environment.toml"),
            r#"[environment]
name = "test"
description = "test"
image = "agent-hub/job-code:2026-09-09"
network_policy = "none"
approval_mode = "auto"
include_skills = ["example"]
include_agents = ["example"]
"#,
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
            serde_json::to_vec(&json!({
                "model": DURABLE_OPENCODE_MODEL,
                "small_model": DURABLE_OPENCODE_MODEL,
                "enabled_providers": ["openai"],
                "permission": {"*":"allow"}
            }))
            .unwrap(),
        )
        .unwrap();
        let skill = root.join("shared/skills/example/SKILL.md");
        std::fs::write(&skill, "first").unwrap();
        std::fs::write(
            root.join("shared/agents/example.md"),
            format!("---\nmodel: {DURABLE_OPENCODE_MODEL}\n---\n"),
        )
        .unwrap();

        let local_skill = root.join("environments/test/skills/example/SKILL.md");
        let local_agent = root.join("environments/test/agents/example.md");
        std::fs::create_dir_all(local_skill.parent().unwrap()).unwrap();
        std::fs::create_dir_all(local_agent.parent().unwrap()).unwrap();
        std::fs::write(&local_skill, "local skill").unwrap();
        std::fs::write(
            &local_agent,
            format!("---\nmodel: {DURABLE_OPENCODE_MODEL}\n---\nlocal agent\n"),
        )
        .unwrap();

        let first = materialize_environment_bundle(&root, "test").unwrap();
        assert!(
            first
                .files
                .iter()
                .all(|file| file.path != "agent-hub-permissions.json" && file.path != "dippy.conf")
        );
        assert!(first.approval_bundle.is_none());
        assert!(
            first
                .files
                .iter()
                .all(|file| !file.path.starts_with("approval/"))
        );
        let server_authorized_digest = first.digest.clone();
        first.verify_digest(&server_authorized_digest).unwrap();
        let sentinel = first
            .files
            .iter()
            .find(|file| file.path == ".gitignore")
            .expect("OpenCode bundle sentinel");
        assert_eq!(sentinel.bytes, OPENCODE_CONFIG_SENTINEL);
        assert_eq!(
            hex::encode(Sha256::digest(OPENCODE_CONFIG_SENTINEL)),
            OPENCODE_CONFIG_SENTINEL_SHA256
        );
        assert!(
            first
                .files
                .iter()
                .any(|file| file.path == "skills/example/SKILL.md")
        );
        assert_eq!(
            first
                .files
                .iter()
                .find(|file| file.path == "skills/example/SKILL.md")
                .unwrap()
                .bytes,
            b"local skill"
        );
        assert!(
            String::from_utf8(
                first
                    .files
                    .iter()
                    .find(|file| file.path == "agents/example.md")
                    .unwrap()
                    .bytes
                    .clone()
            )
            .unwrap()
            .contains("local agent")
        );
        assert!(
            first
                .files
                .iter()
                .any(|file| file.path == "agents/example.md")
        );
        assert_eq!(
            first
                .files
                .iter()
                .find(|file| file.path == "opencode.json")
                .unwrap()
                .bytes,
            std::fs::read(root.join("environments/test/opencode.json")).unwrap()
        );
        std::fs::write(&local_skill, "second local skill").unwrap();
        let second = materialize_environment_bundle(&root, "test").unwrap();
        assert_ne!(first.digest, second.digest);
        assert!(second.verify_digest(&server_authorized_digest).is_err());

        let without_sentinel = EnvironmentBundle::from_files(
            first
                .files
                .iter()
                .filter(|file| file.path != ".gitignore")
                .cloned()
                .collect(),
        )
        .unwrap();
        assert_ne!(first.digest, without_sentinel.digest);
        assert!(without_sentinel.verify_opencode_sentinel().is_err());

        let mut changed_sentinel = first.files.clone();
        changed_sentinel
            .iter_mut()
            .find(|file| file.path == ".gitignore")
            .unwrap()
            .bytes
            .push(b' ');
        assert_ne!(
            first.digest,
            EnvironmentBundle::from_files(changed_sentinel)
                .unwrap()
                .digest
        );
        assert!(
            EnvironmentBundle::from_files(
                first
                    .files
                    .iter()
                    .map(|file| EnvironmentBundleFile {
                        path: file.path.clone(),
                        bytes: if file.path == ".gitignore" {
                            b"wrong\n".to_vec()
                        } else {
                            file.bytes.clone()
                        },
                    })
                    .collect(),
            )
            .unwrap()
            .verify_opencode_sentinel()
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
