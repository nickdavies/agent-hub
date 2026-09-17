use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, ensure};
use protocol::{
    AgentHubCatalogCredentialPrerequisite, AgentHubCatalogEnvironmentV1, AgentHubCatalogImageV1,
    AgentHubCatalogOverlayPrerequisitesV1, AgentHubCatalogOverlayV1,
    AgentHubCatalogRepositoryPrerequisite, AgentHubCatalogToolFactV1, AgentHubCatalogToolFactsV1,
    AgentHubCatalogV1, AgentHubRunImageFamily, NetworkResourceKind,
};
use serde::Deserialize;

const TOOL_FACTS_SCHEMA: &str = "agent_hub_image_tool_facts_v1";
pub(crate) const INTERNAL_CATALOG_MAX_BYTES: usize = 512 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolFactsManifest {
    schema: String,
    image_identity: String,
    tools: Vec<ToolFact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolFact {
    name: String,
    version: String,
}

use crate::attended_compat::{compatible_network_options, compatible_tool_approvals, image_family};

pub(crate) fn build(host_config: &Path) -> anyhow::Result<AgentHubCatalogV1> {
    let snapshot = crate::host_run::load_catalog_config(host_config, false)?;
    build_from_snapshot(&snapshot.files, &snapshot.image_digests)
}

#[derive(serde::Serialize)]
pub(crate) struct InternalCatalogEnvelope {
    schema: &'static str,
    catalog: AgentHubCatalogV1,
    cache_identity: crate::host_run::CatalogCacheIdentity,
}

pub(crate) fn build_internal(host_config: &Path) -> anyhow::Result<InternalCatalogEnvelope> {
    let snapshot = crate::host_run::load_catalog_config(host_config, true)?;
    Ok(InternalCatalogEnvelope {
        schema: "agent_hub_catalog_internal_v1",
        catalog: build_from_snapshot(&snapshot.files, &snapshot.image_digests)?,
        cache_identity: snapshot
            .cache_identity
            .context("catalog cache identity was not captured")?,
    })
}

fn build_from_snapshot(
    files: &BTreeMap<String, Vec<u8>>,
    image_digests: &BTreeMap<String, String>,
) -> anyhow::Result<AgentHubCatalogV1> {
    let system_bytes = files
        .get("system.toml")
        .context("pinned config has no system.toml")?;
    let system = config::SystemConfig::from_toml(
        std::str::from_utf8(system_bytes).context("system.toml is not UTF-8")?,
        "captured system.toml",
    )
    .map_err(anyhow::Error::msg)?;

    let mut image_configs = BTreeMap::new();
    if let Some(images) = &system.images {
        ensure_unique(&images.installed, "installed image")?;
        for name in &images.installed {
            let path = format!("images/{name}/image.toml");
            let bytes = files
                .get(&path)
                .with_context(|| format!("pinned config has no {path}"))?;
            let image = config::ImageConfig::from_toml(
                std::str::from_utf8(bytes)
                    .with_context(|| format!("captured {path} is not UTF-8"))?,
                &format!("captured {path}"),
            )
            .map_err(anyhow::Error::msg)?;
            ensure!(
                image.name == *name,
                "image directory and manifest names differ"
            );
            image_configs.insert(name.clone(), image);
        }
    }

    let mut overlays_by_name = BTreeMap::new();
    if let Some(overlays) = &system.overlays {
        let bytes = files
            .get(&overlays.registry)
            .with_context(|| format!("pinned config has no {}", overlays.registry))?;
        for overlay in config::parse_overlay_registry(
            std::str::from_utf8(bytes).context("captured overlay registry is not UTF-8")?,
            &overlays.registry,
        )
        .map_err(anyhow::Error::msg)?
        {
            overlays_by_name.insert(overlay.name.clone(), overlay);
        }
    }

    let installed_environments = system
        .environments
        .as_ref()
        .map(|configured| configured.installed.as_slice())
        .unwrap_or_default();
    ensure_unique(installed_environments, "installed environment")?;
    let expected_image_digests = installed_environments
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let actual_image_digests = image_digests.keys().cloned().collect::<BTreeSet<_>>();
    ensure!(
        actual_image_digests == expected_image_digests,
        "trusted host image identity inventory mismatch: expected {expected_image_digests:?}, found {actual_image_digests:?}"
    );
    for identity in image_digests.values() {
        ensure!(
            valid_image_identity(identity),
            "trusted host image identity is not sha256"
        );
    }
    let mut environments = Vec::new();
    let mut referenced_overlays = BTreeSet::new();
    let mut job_code_image = None;
    let mut job_code_image_identities = BTreeSet::new();
    for name in installed_environments {
        let path = format!("environments/{name}/environment.toml");
        let bytes = files
            .get(&path)
            .with_context(|| format!("pinned config has no {path}"))?;
        let environment = config::EnvironmentConfig::from_toml(
            std::str::from_utf8(bytes).with_context(|| format!("captured {path} is not UTF-8"))?,
            &format!("captured {path}"),
        )
        .map_err(anyhow::Error::msg)?;
        ensure!(
            environment.name == *name,
            "environment directory and manifest names differ"
        );

        // Validate every installed environment before applying the public V1
        // job-code projection. A non-selectable environment must not hide
        // invalid runtime configuration.
        let approval_bundle_available = match environment.approval_mode.as_str() {
            "auto" => {
                ensure!(
                    environment.approval_bundle.is_none(),
                    "automatic environments must not select an approval bundle"
                );
                false
            }
            "gateway" => {
                let bundle = environment
                    .approval_bundle
                    .as_deref()
                    .context("supervised environments require an approval_bundle")?;
                config::validate_captured_approval_bundle(files, bundle, &system)
                    .map_err(anyhow::Error::msg)?;
                true
            }
            other => anyhow::bail!("unsupported attended tool approval mode '{other}'"),
        };
        let network_options = compatible_network_options(&environment.network_policy)?;
        let tool_approvals =
            compatible_tool_approvals(&environment.approval_mode, approval_bundle_available)?;
        for overlay in &environment.allowed_overlays {
            ensure!(
                overlays_by_name.contains_key(overlay),
                "environment '{name}' references unknown overlay '{overlay}'"
            );
        }

        let family = image_family(&environment.image)
            .rsplit('/')
            .next()
            .context("configured image has no family")?
            .to_string();
        let image = image_configs
            .get(&family)
            .with_context(|| format!("image family '{family}' is not installed"))?;
        ensure!(
            image.tag == environment.image,
            "installed image manifest does not resolve family '{family}' to '{}'",
            environment.image
        );
        if family != AgentHubRunImageFamily::JobCode.as_str() {
            continue;
        }
        job_code_image_identities.insert(&image_digests[name]);
        for overlay in &environment.allowed_overlays {
            referenced_overlays.insert(overlay.clone());
        }
        job_code_image = Some(image);
        environments.push(AgentHubCatalogEnvironmentV1 {
            name: environment.name,
            description: environment.description,
            discovery: environment.discovery,
            image_family: AgentHubRunImageFamily::JobCode,
            network_options,
            tool_approvals,
            overlays: environment.allowed_overlays,
            max_timeout_secs: environment.max_timeout_secs,
        });
    }

    let images = job_code_image
        .map(|image| {
            let tool_facts = project_tool_facts(files, image, &job_code_image_identities)?;
            Ok(AgentHubCatalogImageV1 {
                family: AgentHubRunImageFamily::JobCode,
                description: image.description.clone(),
                discovery: image.discovery.clone(),
                tool_facts,
            })
        })
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;
    let overlays = referenced_overlays
        .into_iter()
        .map(|name| {
            let overlay = &overlays_by_name[&name];
            let (scopes, prerequisites) = overlay_projection(overlay)?;
            Ok(AgentHubCatalogOverlayV1 {
                name,
                provider: overlay.provider.clone(),
                description: overlay.description.clone(),
                discovery: overlay.discovery.clone(),
                scopes,
                prerequisites,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    AgentHubCatalogV1::new(environments, images, overlays).map_err(anyhow::Error::msg)
}

fn project_tool_facts(
    files: &BTreeMap<String, Vec<u8>>,
    image: &config::ImageConfig,
    image_identities: &BTreeSet<&String>,
) -> anyhow::Result<AgentHubCatalogToolFactsV1> {
    let Some(metadata) = &image.tool_facts else {
        return Ok(AgentHubCatalogToolFactsV1::Unavailable);
    };
    ensure!(
        image_identities.len() == 1,
        "declared image tool facts require one exact pinned image identity"
    );
    let bytes = files.get(&metadata.path).with_context(|| {
        format!(
            "declared image tool facts manifest '{}' is missing",
            metadata.path
        )
    })?;
    ensure!(
        protocol::sha256_hex(bytes) == metadata.sha256,
        "declared image tool facts manifest digest mismatch"
    );
    let manifest: ToolFactsManifest =
        serde_json::from_slice(bytes).context("declared image tool facts manifest is malformed")?;
    ensure!(
        manifest.schema == TOOL_FACTS_SCHEMA,
        "declared image tool facts manifest schema is unsupported"
    );
    ensure!(
        image_identities.contains(&manifest.image_identity),
        "declared image tool facts manifest image identity mismatch"
    );
    let mut tools = manifest
        .tools
        .into_iter()
        .map(|tool| AgentHubCatalogToolFactV1 {
            name: tool.name,
            version: tool.version,
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(AgentHubCatalogToolFactsV1::Available { tools })
}

fn overlay_projection(
    overlay: &config::ValidatedOverlay,
) -> anyhow::Result<(Vec<String>, AgentHubCatalogOverlayPrerequisitesV1)> {
    if let Some(github) = overlay.github_config().map_err(anyhow::Error::msg)? {
        return Ok((
            vec![
                "private_repositories".into(),
                "repositories:read_write".into(),
            ],
            AgentHubCatalogOverlayPrerequisitesV1 {
                network_resources: vec![NetworkResourceKind::Github],
                credential: Some(AgentHubCatalogCredentialPrerequisite::HostConfiguration),
                repository: Some(AgentHubCatalogRepositoryPrerequisite::MatchingRemoteHost),
                notes: vec![format!(
                    "Requires a requested repository whose remote host exactly matches {}",
                    github.host
                )],
            },
        ));
    }
    let (credential, note) = match overlay.provider.as_str() {
        "command" => (
            AgentHubCatalogCredentialPrerequisite::HostCommand,
            "Requires the host command credential provider to resolve successfully before launch",
        ),
        "mcp-bridge" => (
            AgentHubCatalogCredentialPrerequisite::ConfiguredMcpBridge,
            "Requires a separately configured host MCP bridge",
        ),
        _ => (
            AgentHubCatalogCredentialPrerequisite::HostConfiguration,
            "Requires matching host credential configuration before launch",
        ),
    };
    Ok((
        vec![overlay.name.clone()],
        AgentHubCatalogOverlayPrerequisitesV1 {
            network_resources: Vec::new(),
            credential: Some(credential),
            repository: None,
            notes: vec![note.into()],
        },
    ))
}

fn valid_image_identity(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn ensure_unique(values: &[String], label: &str) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        ensure!(!value.is_empty(), "{label} name must not be empty");
        ensure!(seen.insert(value), "duplicate {label} '{value}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            (
                "system.toml".into(),
                br#"[system]
operator='test'
bot_identity='test'
[system.environments]
installed=['code']
triage_environment='code'
[system.images]
installed=['job-code']
"#
                .to_vec(),
            ),
            (
                "environments/code/environment.toml".into(),
                br#"[environment]
name='code'
description='Code'
image='agent-hub/job-code:2026-09-09'
network_policy='build'
approval_mode='auto'
[environment.discovery]
recommended_for=['Routine implementation, refactoring, and test changes with automatic tool use']
selection_notes=['Choose a GitHub overlay only when the task needs authenticated repository access']
"#
                .to_vec(),
            ),
            (
                "images/job-code/image.toml".into(),
                br#"[image]
name='job-code'
tag='agent-hub/job-code:2026-09-09'
description='Image'
"#
                .to_vec(),
            ),
        ])
    }

    fn image_identities() -> BTreeMap<String, String> {
        BTreeMap::from([("code".into(), format!("sha256:{}", "a".repeat(64)))])
    }

    fn declare_tool_facts(files: &mut BTreeMap<String, Vec<u8>>, manifest: &[u8]) {
        files.insert("images/job-code/tool-facts.json".into(), manifest.to_vec());
        files.insert(
            "images/job-code/image.toml".into(),
            format!(
                "[image]\nname='job-code'\ntag='agent-hub/job-code:2026-09-09'\ndescription='Image'\n[image.tool_facts]\npath='images/job-code/tool-facts.json'\nsha256='{}'\n",
                protocol::sha256_hex(manifest)
            )
            .into_bytes(),
        );
    }

    #[test]
    fn catalog_projects_only_public_launchable_metadata() {
        let catalog = build_from_snapshot(
            &files(),
            &BTreeMap::from([("code".into(), format!("sha256:{}", "a".repeat(64)))]),
        )
        .unwrap();
        assert_eq!(catalog.environments.len(), 1);
        assert_eq!(catalog.images.len(), 1);
        assert_eq!(catalog.environments[0].network_options.len(), 67);
        assert!(matches!(
            catalog.images[0].tool_facts,
            AgentHubCatalogToolFactsV1::Unavailable
        ));
        let json = serde_json::to_string(&catalog).unwrap();
        for private in [
            "config_root",
            "command",
            "credential_target",
            "network_policy",
            "/workspace",
            "cache_identity",
            "modified_ns",
            "changed_ns",
        ] {
            assert!(!json.contains(private), "leaked private field {private}");
        }
    }

    #[test]
    fn catalog_rejects_missing_and_extra_trusted_image_identities() {
        assert!(build_from_snapshot(&files(), &BTreeMap::new()).is_err());
        let identities = BTreeMap::from([
            ("code".into(), format!("sha256:{}", "a".repeat(64))),
            ("extra".into(), format!("sha256:{}", "b".repeat(64))),
        ]);
        assert!(build_from_snapshot(&files(), &identities).is_err());
    }

    #[test]
    fn declared_tool_facts_are_exactly_bound_and_project_only_public_pairs() {
        let identity = format!("sha256:{}", "a".repeat(64));
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": TOOL_FACTS_SCHEMA,
            "image_identity": identity,
            "tools": [
                {"name":"rustc", "version":"1.91.0 (public build)"},
                {"name":"cargo", "version":"1.91.0"}
            ]
        }))
        .unwrap();
        let mut files = files();
        declare_tool_facts(&mut files, &manifest);
        let catalog = build_from_snapshot(&files, &image_identities()).unwrap();
        let AgentHubCatalogToolFactsV1::Available { tools } = &catalog.images[0].tool_facts else {
            panic!("expected available tool facts");
        };
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["cargo", "rustc"]
        );
        let public = serde_json::to_string(&catalog).unwrap();
        for private in [
            "images/job-code/tool-facts.json",
            &identity,
            &protocol::sha256_hex(&manifest),
            TOOL_FACTS_SCHEMA,
        ] {
            assert!(
                !public.contains(private),
                "leaked private tool metadata {private}"
            );
        }
    }

    #[test]
    fn declared_tool_facts_fail_closed_for_missing_malformed_digest_and_image() {
        let identity = format!("sha256:{}", "a".repeat(64));
        let valid = serde_json::to_vec(&serde_json::json!({
            "schema": TOOL_FACTS_SCHEMA,
            "image_identity": identity,
            "tools": [{"name":"cargo", "version":"1.91.0"}]
        }))
        .unwrap();

        let mut missing = files();
        declare_tool_facts(&mut missing, &valid);
        missing.remove("images/job-code/tool-facts.json");
        assert!(build_from_snapshot(&missing, &image_identities()).is_err());

        let mut malformed = files();
        declare_tool_facts(&mut malformed, b"not-json");
        assert!(build_from_snapshot(&malformed, &image_identities()).is_err());

        let mut wrong_digest = files();
        declare_tool_facts(&mut wrong_digest, &valid);
        let image = String::from_utf8(wrong_digest["images/job-code/image.toml"].clone())
            .unwrap()
            .replace(&protocol::sha256_hex(&valid), &"b".repeat(64));
        wrong_digest.insert("images/job-code/image.toml".into(), image.into_bytes());
        assert!(build_from_snapshot(&wrong_digest, &image_identities()).is_err());

        let wrong_image = serde_json::to_vec(&serde_json::json!({
            "schema": TOOL_FACTS_SCHEMA,
            "image_identity": format!("sha256:{}", "b".repeat(64)),
            "tools": [{"name":"cargo", "version":"1.91.0"}]
        }))
        .unwrap();
        let mut files = files();
        declare_tool_facts(&mut files, &wrong_image);
        assert!(build_from_snapshot(&files, &image_identities()).is_err());
    }

    #[test]
    fn declared_tool_facts_reject_private_or_unbounded_manifest_data() {
        for manifest in [
            serde_json::json!({
                "schema": TOOL_FACTS_SCHEMA,
                "image_identity": format!("sha256:{}", "a".repeat(64)),
                "tools": [{"name":"cargo", "version":"1", "private_path":"/secret"}]
            }),
            serde_json::json!({
                "schema": TOOL_FACTS_SCHEMA,
                "image_identity": format!("sha256:{}", "a".repeat(64)),
                "tools": [{"name":"private/path", "version":"1"}]
            }),
            serde_json::json!({
                "schema": TOOL_FACTS_SCHEMA,
                "image_identity": format!("sha256:{}", "a".repeat(64)),
                "tools": [{"name":"cargo", "version":"x".repeat(257)}]
            }),
        ] {
            let manifest = serde_json::to_vec(&manifest).unwrap();
            let mut files = files();
            declare_tool_facts(&mut files, &manifest);
            assert!(build_from_snapshot(&files, &image_identities()).is_err());
        }
    }

    #[test]
    fn supervised_catalog_entry_requires_a_materialized_approval_bundle() {
        let mut files = files();
        files.insert(
            "environments/code/environment.toml".into(),
            br#"[environment]
name='code'
description='Code'
image='agent-hub/job-code:2026-09-09'
network_policy='build'
approval_mode='gateway'
approval_bundle='missing'
"#
            .to_vec(),
        );
        let identities = BTreeMap::from([("code".into(), format!("sha256:{}", "a".repeat(64)))]);
        assert!(
            build_from_snapshot(&files, &identities)
                .unwrap_err()
                .to_string()
                .contains("approval bundle file inventory")
        );
    }

    fn add_nonselectable_environment(files: &mut BTreeMap<String, Vec<u8>>, body: &[u8]) {
        let system = String::from_utf8(files["system.toml"].clone())
            .unwrap()
            .replace("installed=['code']", "installed=['code','infra']");
        files.insert("system.toml".into(), system.into_bytes());
        files.insert("environments/infra/environment.toml".into(), body.to_vec());
    }

    fn install_infra_image(files: &mut BTreeMap<String, Vec<u8>>, tag: &str) {
        let system = String::from_utf8(files["system.toml"].clone())
            .unwrap()
            .replace(
                "installed=['job-code']",
                "installed=['job-code','job-infra']",
            );
        files.insert("system.toml".into(), system.into_bytes());
        files.insert(
            "images/job-infra/image.toml".into(),
            format!("[image]\nname='job-infra'\ntag='{tag}'\ndescription='Infrastructure image'\n")
                .into_bytes(),
        );
    }

    #[test]
    fn invalid_nonselectable_environment_fails_closed() {
        let mut files = files();
        add_nonselectable_environment(
            &mut files,
            br#"[environment]
name='infra'
description='Infrastructure'
image='agent-hub/job-infra:2026-09-09'
network_policy='unknown-policy'
approval_mode='auto'
"#,
        );
        let mut identities = image_identities();
        identities.insert("infra".into(), format!("sha256:{}", "b".repeat(64)));
        let error = build_from_snapshot(&files, &identities).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported attended network selector"),
            "{error:#}"
        );
    }

    #[test]
    fn valid_nonselectable_environment_is_validated_then_excluded() {
        let mut files = files();
        add_nonselectable_environment(
            &mut files,
            br#"[environment]
name='infra'
description='Infrastructure'
image='agent-hub/job-infra:2026-09-09'
network_policy='none'
approval_mode='auto'
"#,
        );
        install_infra_image(&mut files, "agent-hub/job-infra:2026-09-09");
        let mut identities = image_identities();
        identities.insert("infra".into(), format!("sha256:{}", "b".repeat(64)));
        let catalog = build_from_snapshot(&files, &identities).unwrap();
        assert_eq!(
            catalog
                .environments
                .iter()
                .map(|value| value.name.as_str())
                .collect::<Vec<_>>(),
            ["code"]
        );
    }

    #[test]
    fn nonselectable_environment_requires_an_installed_exact_image_manifest() {
        let environment = br#"[environment]
name='infra'
description='Infrastructure'
image='agent-hub/job-infra:2026-09-09'
network_policy='none'
approval_mode='auto'
"#;

        let mut missing = files();
        add_nonselectable_environment(&mut missing, environment);
        let mut identities = image_identities();
        identities.insert("infra".into(), format!("sha256:{}", "b".repeat(64)));
        let error = build_from_snapshot(&missing, &identities).unwrap_err();
        assert!(error.to_string().contains("not installed"), "{error:#}");

        let mut wrong_tag = missing;
        install_infra_image(&mut wrong_tag, "agent-hub/job-infra:typo");
        let error = build_from_snapshot(&wrong_tag, &identities).unwrap_err();
        assert!(
            error.to_string().contains("does not resolve family"),
            "{error:#}"
        );
    }

    #[test]
    fn github_projection_requires_network_credential_and_matching_repository() {
        let overlay = config::ValidatedOverlay {
            name: "github:github.com:read_write".into(),
            provider: "github".into(),
            description: Some("GitHub".into()),
            discovery: None,
            config: serde_json::json!({
                "host":"github.com",
                "api_base_url":"https://api.github.com",
                "access":"read_write",
                "network_resource":"github",
                "credential_provider":"github:github.com:read_write",
                "credential_target":"/agent-hub/github/tokens/github.com",
                "authenticated":true,
                "private_repositories":true
            }),
        };
        let (scopes, prerequisites) = overlay_projection(&overlay).unwrap();
        assert!(scopes.contains(&"repositories:read_write".to_string()));
        assert_eq!(
            prerequisites.network_resources,
            [NetworkResourceKind::Github]
        );
        assert_eq!(
            prerequisites.credential,
            Some(AgentHubCatalogCredentialPrerequisite::HostConfiguration)
        );
        assert_eq!(
            prerequisites.repository,
            Some(AgentHubCatalogRepositoryPrerequisite::MatchingRemoteHost)
        );
        let public = serde_json::to_string(&(scopes, prerequisites)).unwrap();
        for private in [
            "gh auth token",
            "/agent-hub/github/tokens/github.com",
            "credential_target",
            "credential_provider",
        ] {
            assert!(!public.contains(private));
        }
    }

    #[test]
    fn command_overlay_projection_excludes_provider_command_and_secret_details() {
        let overlay = config::ValidatedOverlay {
            name: "aws:readonly".into(),
            provider: "command".into(),
            description: Some("AWS read-only".into()),
            discovery: None,
            config: serde_json::json!({
                "command":"/private/provider-command",
                "args":["--secret-path", "/private/token"],
                "capture_env":"AWS_SESSION_TOKEN",
                "secret":"super-secret"
            }),
        };
        let projection = overlay_projection(&overlay).unwrap();
        let public_value = serde_json::to_value(&projection).unwrap();
        assert_eq!(
            public_value,
            serde_json::json!([
                ["aws:readonly"],
                {
                    "network_resources": [],
                    "credential": "host_command",
                    "notes": ["Requires the host command credential provider to resolve successfully before launch"]
                }
            ])
        );
        let public = serde_json::to_string(&public_value).unwrap();
        for private in [
            "/private/provider-command",
            "--secret-path",
            "/private/token",
            "AWS_SESSION_TOKEN",
            "super-secret",
        ] {
            assert!(!public.contains(private));
        }
    }
}
