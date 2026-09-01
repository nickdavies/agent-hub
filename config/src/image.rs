use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const JOB_CODE_PREFLIGHT_PATH: &str = "/usr/local/bin/agent-hub-job-code-preflight";
pub const JOB_CODE_TOOL_MANIFEST_PATH: &str =
    "/usr/local/share/agent-hub/job-code-tool-manifest.json";
pub const SUPERVISED_OPENCODE_VERSION: &str = "1.18.18";

pub fn job_code_tool_manifest_sha256(contents: &str) -> String {
    hex::encode(Sha256::digest(contents.as_bytes()))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn job_code_preflight_docker_args(
    image: &str,
    runtime_uid: u32,
    runtime_gid: u32,
    pids_limit: u32,
) -> Result<Vec<String>, String> {
    if image.trim().is_empty() {
        return Err("job-code preflight image must not be empty".to_string());
    }
    if runtime_uid == 0 || runtime_gid == 0 {
        return Err("job-code preflight requires a non-root runtime UID and GID".to_string());
    }
    if pids_limit == 0 {
        return Err("job-code preflight PID limit must be positive".to_string());
    }
    let owner = format!("uid={runtime_uid},gid={runtime_gid}");
    Ok(vec![
        "run".to_string(),
        "--rm".to_string(),
        "--network=none".to_string(),
        "--read-only".to_string(),
        format!("--user={runtime_uid}:{runtime_gid}"),
        "--security-opt=no-new-privileges".to_string(),
        "--cap-drop=ALL".to_string(),
        format!("--pids-limit={pids_limit}"),
        format!("--tmpfs=/tmp:rw,exec,nosuid,nodev,size=256m,mode=1777,{owner}"),
        format!("--tmpfs=/home/agent:rw,nosuid,nodev,size=64m,mode=0700,{owner}"),
        format!("--tmpfs=/cache:rw,nosuid,nodev,size=256m,mode=0700,{owner}"),
        "--env=HOME=/home/agent".to_string(),
        "--env=XDG_CACHE_HOME=/cache".to_string(),
        "--env=XDG_CONFIG_HOME=/home/agent/.config".to_string(),
        "--env=XDG_DATA_HOME=/home/agent/.local/share".to_string(),
        "--env=XDG_STATE_HOME=/home/agent/.local/state".to_string(),
        format!("--env=PATH={}", crate::runner::DURABLE_OPENCODE_PATH),
        format!("--env=AGENT_HUB_EXPECTED_UID={runtime_uid}"),
        format!("--env=AGENT_HUB_EXPECTED_GID={runtime_gid}"),
        format!("--env=AGENT_HUB_EXPECTED_IMAGE_IDENTITY={image}"),
        format!("--entrypoint={JOB_CODE_PREFLIGHT_PATH}"),
        image.to_string(),
    ])
}

pub fn validate_job_code_preflight_report(
    bytes: &[u8],
    expected_image: &str,
    expected_uid: u32,
    expected_gid: u32,
) -> Result<serde_json::Value, String> {
    let report: JobCodePreflightReport = serde_json::from_slice(bytes)
        .map_err(|error| format!("preflight returned invalid JSON: {error}"))?;
    if report.schema != "agent-hub.job-code-preflight.v3"
        || report.status != "ok"
        || !is_sha256_identity(expected_image)
        || report.image.resolved != expected_image
        || report.requested.uid != expected_uid
        || report.requested.gid != expected_gid
        || report.runtime != report.requested
        || expected_uid == 0
        || expected_gid == 0
        || report.manifest.path != JOB_CODE_TOOL_MANIFEST_PATH
        || !is_canonical_digest(&report.manifest.sha256, 64)
    {
        return Err("preflight returned an invalid success report".to_string());
    }

    let actual_manifest_sha256 = job_code_tool_manifest_sha256(&report.manifest.contents);
    if report.manifest.sha256 != actual_manifest_sha256 {
        return Err("preflight tool manifest digest does not match its contents".to_string());
    }
    let manifest: JobCodeToolManifest = serde_json::from_str(&report.manifest.contents)
        .map_err(|error| format!("preflight returned an invalid tool manifest: {error}"))?;
    manifest.validate()?;

    serde_json::from_slice(bytes)
        .map_err(|error| format!("preflight returned invalid JSON: {error}"))
}

pub fn validate_supervised_opencode_preflight_report(
    bytes: &[u8],
    expected_image: &str,
    expected_uid: u32,
    expected_gid: u32,
) -> Result<serde_json::Value, String> {
    let report =
        validate_job_code_preflight_report(bytes, expected_image, expected_uid, expected_gid)?;
    let contents = report
        .pointer("/manifest/contents")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "preflight report has no tool manifest contents".to_string())?;
    let manifest: JobCodeToolManifest = serde_json::from_str(contents)
        .map_err(|error| format!("preflight returned an invalid tool manifest: {error}"))?;
    if manifest.tools.opencode != SUPERVISED_OPENCODE_VERSION {
        return Err(format!(
            "supervised OpenCode requires runtime {SUPERVISED_OPENCODE_VERSION} with config and tool.execute.before plugin API support, got {}",
            manifest.tools.opencode
        ));
    }
    Ok(report)
}

fn is_sha256_identity(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|digest| is_canonical_digest(digest, 64))
}

fn is_canonical_digest(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobCodePreflightReport {
    schema: String,
    status: String,
    image: ResolvedImageIdentity,
    requested: RuntimeIdentity,
    runtime: RuntimeIdentity,
    manifest: ReportedToolManifest,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedImageIdentity {
    resolved: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RuntimeIdentity {
    uid: u32,
    gid: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportedToolManifest {
    path: String,
    sha256: String,
    contents: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobCodeToolManifest {
    schema: String,
    image: String,
    debian_snapshot: String,
    runtime: ToolRuntime,
    tools: RequiredTools,
    provenance: ToolProvenance,
}

impl JobCodeToolManifest {
    fn validate(&self) -> Result<(), String> {
        if self.schema != "agent-hub.job-code-tools.v3"
            || self.image != "agent-hub/job-code:2026-08-27"
            || !is_debian_snapshot(&self.debian_snapshot)
            || self.runtime.rust_home != "/opt/rust"
            || self.runtime.rust_bin != "/opt/rust/bin"
            || self.runtime.cargo_home != "$HOME/.cargo"
        {
            return Err("preflight tool manifest identity is invalid".to_string());
        }
        self.tools.validate()?;
        self.provenance.validate()
    }
}

fn is_debian_snapshot(value: &str) -> bool {
    value.len() == 16
        && value.as_bytes().get(8) == Some(&b'T')
        && value.ends_with('Z')
        && value[..8].bytes().all(|byte| byte.is_ascii_digit())
        && value[9..15].bytes().all(|byte| byte.is_ascii_digit())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolRuntime {
    rust_home: String,
    rust_bin: String,
    cargo_home: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequiredTools {
    agent_hub_gateway: String,
    cargo: String,
    cc: String,
    cmake: String,
    corepack: String,
    curl: String,
    cxx: String,
    gh: String,
    git: String,
    go: String,
    jq: String,
    make: String,
    ninja: String,
    node: String,
    npm: String,
    opencode: String,
    pip: String,
    pkg_config: String,
    pnpm: String,
    python: String,
    rg: String,
    rustc: String,
    unbound: String,
    unbound_checkconf: String,
    uv: String,
    yarn: String,
}

impl RequiredTools {
    fn validate(&self) -> Result<(), String> {
        let versions = [
            &self.agent_hub_gateway,
            &self.cargo,
            &self.cc,
            &self.cmake,
            &self.corepack,
            &self.curl,
            &self.cxx,
            &self.gh,
            &self.git,
            &self.go,
            &self.jq,
            &self.make,
            &self.ninja,
            &self.node,
            &self.npm,
            &self.opencode,
            &self.pip,
            &self.pkg_config,
            &self.pnpm,
            &self.python,
            &self.rg,
            &self.rustc,
            &self.unbound,
            &self.unbound_checkconf,
            &self.uv,
            &self.yarn,
        ];
        if versions.iter().any(|version| version.is_empty()) {
            return Err("preflight tool manifest contains an empty version".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolProvenance {
    packages: ToolPackages,
    sources: ToolSources,
}

impl ToolProvenance {
    fn validate(&self) -> Result<(), String> {
        self.packages.validate()?;
        self.sources.validate()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolPackages {
    unbound: DebianPackage,
}

impl ToolPackages {
    fn validate(&self) -> Result<(), String> {
        if self.unbound.version != "1.17.1-2+deb12u4"
            || self.unbound.snapshot != "20260824T000000Z"
            || self.unbound.uid != 100
            || self.unbound.gid != 101
        {
            return Err("Unbound package provenance is not pinned".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DebianPackage {
    version: String,
    snapshot: String,
    uid: u32,
    gid: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolSources {
    agent_hub: DownloadSource,
    gh: DownloadSource,
    go: DownloadSource,
    node: DownloadSource,
    opencode: DownloadSource,
    pnpm: DownloadSource,
    rust_cargo: DownloadSource,
    rust_channel_manifest: DownloadSource,
    rust_std: DownloadSource,
    rustc: DownloadSource,
    uv: DownloadSource,
    yarn: DownloadSource,
}

impl ToolSources {
    fn validate(&self) -> Result<(), String> {
        for source in [
            &self.agent_hub,
            &self.gh,
            &self.go,
            &self.node,
            &self.opencode,
            &self.pnpm,
            &self.rust_cargo,
            &self.rust_channel_manifest,
            &self.rust_std,
            &self.rustc,
            &self.uv,
            &self.yarn,
        ] {
            source.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadSource {
    url: String,
    digest: SourceDigest,
}

impl DownloadSource {
    fn validate(&self) -> Result<(), String> {
        if !self.url.starts_with("https://") {
            return Err("tool source URL must use HTTPS".to_string());
        }
        self.digest.validate()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceDigest {
    algorithm: String,
    value: String,
}

impl SourceDigest {
    fn validate(&self) -> Result<(), String> {
        let length = match self.algorithm.as_str() {
            "sha256" => 64,
            "sha512" => 128,
            _ => return Err("tool source digest algorithm is unsupported".to_string()),
        };
        if !is_canonical_digest(&self.value, length) {
            return Err("tool source digest is not canonical lowercase hexadecimal".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageTomlFile {
    image: ImageConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    pub name: String,
    pub tag: String,
    pub description: String,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub build_contexts: BTreeMap<String, ImageBuildContext>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildContext {
    #[serde(rename = "type")]
    pub context_type: String,
    pub url: String,
    pub commit: String,
    pub archive_sha256: String,
}

impl ImageBuildContext {
    pub fn validate(&self) -> Result<(), String> {
        if self.context_type != "git"
            || !self.url.starts_with("https://")
            || !is_canonical_digest(&self.commit, 40)
            || !is_canonical_digest(&self.archive_sha256, 64)
        {
            return Err("image source build context identity is invalid".into());
        }
        Ok(())
    }
}

impl ImageConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read image config {}: {e}", path.display()))?;
        let wrapped: ImageTomlFile = toml::from_str(&raw)
            .map_err(|e| format!("failed to parse image config {}: {e}", path.display()))?;
        Ok(wrapped.image)
    }
}

pub fn resolve_build_order(configs: &[ImageConfig]) -> Result<Vec<String>, String> {
    let mut by_name = HashMap::with_capacity(configs.len());
    for cfg in configs {
        if by_name.contains_key(&cfg.name) {
            return Err(format!("duplicate image config '{}'", cfg.name));
        }
        by_name.insert(cfg.name.clone(), cfg);
    }

    let mut order = Vec::with_capacity(configs.len());
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();

    for cfg in configs {
        visit_image(&cfg.name, &by_name, &mut visiting, &mut visited, &mut order)?;
    }

    Ok(order)
}

fn visit_image(
    name: &str,
    by_name: &HashMap<String, &ImageConfig>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
    order: &mut Vec<String>,
) -> Result<(), String> {
    if visited.contains(name) {
        return Ok(());
    }
    if visiting.contains(name) {
        return Err(format!("cyclic image extends chain detected at '{name}'"));
    }

    let cfg = by_name
        .get(name)
        .ok_or_else(|| format!("image '{name}' is not defined"))?;

    visiting.insert(name.to_string());
    if let Some(parent) = &cfg.extends {
        if !by_name.contains_key(parent) {
            return Err(format!(
                "image '{}' extends unknown image '{}'",
                cfg.name, parent
            ));
        }
        visit_image(parent, by_name, visiting, visited, order)?;
    }
    visiting.remove(name);

    visited.insert(name.to_string());
    order.push(name.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_source_context_requires_exact_archive_identity_and_rejects_unknown_fields() {
        let valid = r#"
[image]
name = "job-code"
tag = "agent-hub/job-code:2026-08-27"
description = "test"
[image.build_contexts.agent-hub-source]
type = "git"
url = "https://github.com/nickdavies/agent-hub.git"
commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
archive_sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
"#;
        let parsed: ImageTomlFile = toml::from_str(valid).unwrap();
        parsed.image.build_contexts["agent-hub-source"]
            .validate()
            .unwrap();
        assert!(toml::from_str::<ImageTomlFile>(&format!("{valid}\nunknown = true\n")).is_err());
        let mut mismatched = parsed.image.build_contexts["agent-hub-source"].clone();
        mismatched.archive_sha256 = "not-a-digest".into();
        assert!(mismatched.validate().is_err());
    }

    #[test]
    fn job_code_preflight_is_non_root_isolated_and_mount_free() {
        let args = job_code_preflight_docker_args(
            "image@sha256:abc",
            1000,
            1001,
            protocol::APPROVED_EXECUTION_PIDS_LIMIT,
        )
        .unwrap();
        assert_eq!(args.first().map(String::as_str), Some("run"));
        assert!(args.contains(&"--network=none".to_string()));
        assert!(args.contains(&"--read-only".to_string()));
        assert!(args.contains(&"--user=1000:1001".to_string()));
        assert!(args.contains(&"--cap-drop=ALL".to_string()));
        assert!(args.contains(&"--security-opt=no-new-privileges".to_string()));
        assert!(args.iter().any(|arg| arg.starts_with("--tmpfs=/tmp:rw,")));
        assert!(
            args.iter()
                .any(|arg| arg.starts_with("--tmpfs=/home/agent:rw,"))
        );
        assert!(args.iter().any(|arg| arg.starts_with("--tmpfs=/cache:rw,")));
        assert!(args.contains(&format!("--entrypoint={JOB_CODE_PREFLIGHT_PATH}")));
        assert_eq!(args.last().map(String::as_str), Some("image@sha256:abc"));
        assert!(
            !args
                .iter()
                .any(|arg| matches!(arg.as_str(), "-v" | "--volume" | "--mount"))
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("TOKEN") || arg.contains("CREDENTIAL"))
        );
    }

    #[test]
    fn job_code_preflight_rejects_root_and_empty_images() {
        assert!(job_code_preflight_docker_args("image", 0, 1000, 512).is_err());
        assert!(job_code_preflight_docker_args("image", 1000, 0, 512).is_err());
        assert!(job_code_preflight_docker_args(" ", 1000, 1000, 512).is_err());
        assert!(job_code_preflight_docker_args("image", 1000, 1000, 0).is_err());
    }

    #[test]
    fn job_code_preflight_report_requires_strict_success_identity() {
        let valid = valid_preflight_report();
        let bytes = serde_json::to_vec(&valid).unwrap();
        validate_job_code_preflight_report(
            &bytes,
            &format!("sha256:{}", "a".repeat(64)),
            1000,
            1001,
        )
        .unwrap();

        for pointer in [
            "/extra",
            "/image/extra",
            "/requested/extra",
            "/runtime/extra",
            "/manifest/extra",
        ] {
            let mut invalid = valid.clone();
            invalid
                .pointer_mut(pointer.rsplit_once('/').unwrap().0)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("extra".to_string(), serde_json::Value::Bool(true));
            assert!(
                validate_job_code_preflight_report(
                    &serde_json::to_vec(&invalid).unwrap(),
                    &format!("sha256:{}", "a".repeat(64)),
                    1000,
                    1001,
                )
                .is_err()
            );
        }

        let mut mismatched = valid.clone();
        mismatched["runtime"]["uid"] = serde_json::json!(1002);
        assert!(
            validate_job_code_preflight_report(
                &serde_json::to_vec(&mismatched).unwrap(),
                &format!("sha256:{}", "a".repeat(64)),
                1000,
                1001,
            )
            .is_err()
        );

        let mut uppercase = valid;
        uppercase["manifest"]["sha256"] = serde_json::Value::String("A".repeat(64));
        assert!(
            validate_job_code_preflight_report(
                &serde_json::to_vec(&uppercase).unwrap(),
                &format!("sha256:{}", "a".repeat(64)),
                1000,
                1001,
            )
            .is_err()
        );
    }

    #[test]
    fn job_code_preflight_report_rejects_unknown_or_missing_manifest_fields() {
        for mutate in [
            "top",
            "runtime",
            "tools",
            "provenance",
            "sources",
            "source",
            "digest",
        ] {
            let mut report = valid_preflight_report();
            let mut manifest: serde_json::Value =
                serde_json::from_str(report["manifest"]["contents"].as_str().unwrap()).unwrap();
            let target = match mutate {
                "top" => &mut manifest,
                "runtime" => &mut manifest["runtime"],
                "tools" => &mut manifest["tools"],
                "provenance" => &mut manifest["provenance"],
                "sources" => &mut manifest["provenance"]["sources"],
                "source" => &mut manifest["provenance"]["sources"]["gh"],
                "digest" => &mut manifest["provenance"]["sources"]["gh"]["digest"],
                _ => unreachable!(),
            };
            target
                .as_object_mut()
                .unwrap()
                .insert("extra".to_string(), serde_json::Value::Bool(true));
            set_manifest(&mut report, &manifest);
            assert!(
                validate_job_code_preflight_report(
                    &serde_json::to_vec(&report).unwrap(),
                    &format!("sha256:{}", "a".repeat(64)),
                    1000,
                    1001,
                )
                .is_err(),
                "accepted unknown field at {mutate}"
            );
        }

        let mut report = valid_preflight_report();
        let mut manifest: serde_json::Value =
            serde_json::from_str(report["manifest"]["contents"].as_str().unwrap()).unwrap();
        manifest["tools"].as_object_mut().unwrap().remove("cargo");
        set_manifest(&mut report, &manifest);
        assert!(
            validate_job_code_preflight_report(
                &serde_json::to_vec(&report).unwrap(),
                &format!("sha256:{}", "a".repeat(64)),
                1000,
                1001,
            )
            .is_err()
        );
    }

    #[test]
    fn supervised_opencode_preflight_requires_compatible_runtime() {
        let valid = valid_preflight_report();
        validate_supervised_opencode_preflight_report(
            &serde_json::to_vec(&valid).unwrap(),
            &format!("sha256:{}", "a".repeat(64)),
            1000,
            1001,
        )
        .unwrap();

        let mut incompatible = valid;
        let mut manifest: serde_json::Value =
            serde_json::from_str(incompatible["manifest"]["contents"].as_str().unwrap()).unwrap();
        manifest["tools"]["opencode"] = serde_json::json!("1.18.17");
        set_manifest(&mut incompatible, &manifest);
        assert!(
            validate_supervised_opencode_preflight_report(
                &serde_json::to_vec(&incompatible).unwrap(),
                &format!("sha256:{}", "a".repeat(64)),
                1000,
                1001,
            )
            .unwrap_err()
            .contains("tool.execute.before")
        );
    }

    fn valid_preflight_report() -> serde_json::Value {
        let tool_names = [
            "agent_hub_gateway",
            "cargo",
            "cc",
            "cmake",
            "corepack",
            "curl",
            "cxx",
            "gh",
            "git",
            "go",
            "jq",
            "make",
            "ninja",
            "node",
            "npm",
            "opencode",
            "pip",
            "pkg_config",
            "pnpm",
            "python",
            "rg",
            "rustc",
            "unbound",
            "unbound_checkconf",
            "uv",
            "yarn",
        ];
        let tools = tool_names
            .into_iter()
            .map(|name| {
                let version = if name == "opencode" {
                    SUPERVISED_OPENCODE_VERSION.to_string()
                } else {
                    format!("{name} 1.0")
                };
                (name.to_string(), serde_json::json!(version))
            })
            .collect::<serde_json::Map<_, _>>();
        let source_names = [
            "agent_hub",
            "gh",
            "go",
            "node",
            "opencode",
            "pnpm",
            "rust_cargo",
            "rust_channel_manifest",
            "rust_std",
            "rustc",
            "uv",
            "yarn",
        ];
        let sources = source_names
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    serde_json::json!({
                        "url":format!("https://example.com/{name}-1.0.tar.xz"),
                        "digest":{"algorithm":"sha256","value":"b".repeat(64)}
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let manifest = serde_json::json!({
            "schema":"agent-hub.job-code-tools.v3",
            "image":"agent-hub/job-code:2026-08-27",
            "debian_snapshot":"20260824T000000Z",
            "runtime":{"rust_home":"/opt/rust","rust_bin":"/opt/rust/bin","cargo_home":"$HOME/.cargo"},
            "tools":tools,
            "provenance":{"packages":{"unbound":{"version":"1.17.1-2+deb12u4","snapshot":"20260824T000000Z","uid":100,"gid":101}},"sources":sources},
        });
        let contents = serde_json::to_string(&manifest).unwrap();
        let sha256 = hex::encode(Sha256::digest(contents.as_bytes()));
        serde_json::json!({
            "schema":"agent-hub.job-code-preflight.v3",
            "status":"ok",
            "image":{"resolved":format!("sha256:{}", "a".repeat(64))},
            "requested":{"uid":1000,"gid":1001},
            "runtime":{"uid":1000,"gid":1001},
            "manifest":{"path":JOB_CODE_TOOL_MANIFEST_PATH,"sha256":sha256,"contents":contents},
        })
    }

    fn set_manifest(report: &mut serde_json::Value, manifest: &serde_json::Value) {
        let contents = serde_json::to_string(manifest).unwrap();
        report["manifest"]["sha256"] =
            serde_json::Value::String(hex::encode(Sha256::digest(contents.as_bytes())));
        report["manifest"]["contents"] = serde_json::Value::String(contents);
    }
}
