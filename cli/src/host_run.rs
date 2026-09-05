use std::fs::File;
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, ensure};
use chrono::{Duration, Utc};
use protocol::{
    AgentHubRunDecision, AgentHubRunDecisionV1, AgentHubRunIntentV1, AgentHubRunReceiptV1,
    AgentHubRunResolvedV1,
};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::attended::{
    AuthorizationRecord, AuthorizationSelectors, LaunchState, ProposalInput, RelayEndpointOverride,
};
use crate::local::{RequestedContextRoot, RequestedWorkspace};
use crate::state::StateStore;

const HOST_CONFIG_SCHEMA: &str = "agent_hub_run_host_v1";
const MAX_APPROVAL_TIMEOUT_SECS: u64 = 600;
const ONE_STEP_RESULT_DOCUMENT_LIMIT_BYTES: u64 = 128 * 1024;
const GATEWAY_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub(crate) const DECISION_RECEIPT_DOMAIN: &str = "agent_hub_run";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfig {
    schema: String,
    config_root: String,
    config_root_identity: protocol::HostDirectoryIdentity,
    config_artifacts: Vec<HostArtifactPin>,
    self_binary: HostArtifactPin,
    gateway: HostArtifactPin,
    policy: HostArtifactPin,
    opencode_plugin_install: HostArtifactPin,
    image_digests: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostArtifactPin {
    path: String,
    sha256: String,
    source_identity: String,
    device: u64,
    inode: u64,
}

struct VerifiedFile {
    file: File,
}

#[derive(Clone)]
pub(crate) struct HostRunOptions {
    pub host_config: PathBuf,
    pub trusted_host_config: PathBuf,
    pub approvals_server: String,
    pub approvals_token: Option<protocol::Secret>,
    pub approval_timeout_secs: u64,
    pub invocation_id: String,
    pub cancellation: tokio::sync::watch::Receiver<bool>,
}

pub(crate) fn default_host_config() -> anyhow::Result<PathBuf> {
    let home =
        std::env::var_os("HOME").context("HOME is required for trusted host configuration")?;
    Ok(PathBuf::from(home).join(".config/agent-hub/host-run.json"))
}

/// Canonical preparation only. It performs no state mutation or execution.
pub(crate) async fn inspect(
    intent: AgentHubRunIntentV1,
    host_config: &Path,
) -> anyhow::Result<protocol::AttendedAuthorizationArtifact> {
    let (config, _) = load_host_config(host_config)?;
    let pins = verify_host_installation(&config)?;
    let resolved = resolve_intent(intent, &config)?;
    let image = trusted_image_identity(&resolved, &config)?;
    let artifact =
        crate::attended::prepare_canonical_execution(proposal_input(&resolved, &config), image)
            .await?;
    verify_pin_coverage(&config, &artifact)?;
    drop(pins);
    Ok(artifact)
}

/// Policy-only gateway evaluation. It creates no approval, execution, or state.
pub(crate) async fn preview(
    intent: AgentHubRunIntentV1,
    host_config: &Path,
) -> anyhow::Result<serde_json::Value> {
    let (config, _) = load_host_config(host_config)?;
    let pins = verify_host_installation(&config)?;
    let resolved = resolve_intent(intent, &config)?;
    let image = trusted_image_identity(&resolved, &config)?;
    let artifact =
        crate::attended::prepare_canonical_execution(proposal_input(&resolved, &config), image)
            .await?;
    verify_pin_coverage(&config, &artifact)?;
    drop(pins);
    let requested_at = Utc::now();
    let request = AgentHubRunResolvedV1::new(
        uuid::Uuid::new_v4().to_string(),
        artifact,
        requested_at,
        requested_at + Duration::minutes(5),
    )
    .map_err(anyhow::Error::msg)?;
    request_gateway_preview(&request, &config).await
}

/// Prepare, obtain a gateway decision, persist it, and execute. There is no
/// local policy fallback.
pub(crate) async fn run(
    store: &StateStore,
    intent: AgentHubRunIntentV1,
    options: HostRunOptions,
) -> anyhow::Result<protocol::AgentHubRunOutputV1> {
    let (_, trusted_identity) = load_host_config(&options.trusted_host_config)?;
    let (config, selected_identity) = load_host_config(&options.host_config)?;
    ensure!(
        selected_identity == trusted_identity,
        "automatic host rules require the trusted host configuration identity"
    );
    let pins = verify_host_installation(&config)?;
    ensure!(
        (1..=MAX_APPROVAL_TIMEOUT_SECS).contains(&options.approval_timeout_secs),
        "approval timeout is outside the supported range"
    );
    let resolved_intent = resolve_intent(intent, &config)?;
    let image = trusted_image_identity(&resolved_intent, &config)?;
    let artifact = crate::attended::prepare_canonical_execution(
        proposal_input(&resolved_intent, &config),
        image,
    )
    .await?;
    artifact.verify_v3().map_err(anyhow::Error::msg)?;
    verify_pin_coverage(&config, &artifact)?;
    drop(pins);
    drop(verify_host_installation(&config)?);
    let retry = artifact.verify_v3().map_err(anyhow::Error::msg)?.retry;
    let attempts = i64::from(retry.max_attempts);
    let requested_at = Utc::now();
    let invocation_root = PathBuf::from("agent-hub-run/invocations").join(&options.invocation_id);
    let request_path = invocation_root.join("request.json");
    let request = match store.read_json::<AgentHubRunResolvedV1>(&request_path) {
        Ok(existing) => {
            existing.validate(Utc::now()).map_err(anyhow::Error::msg)?;
            ensure!(
                existing.canonical_digest == artifact.digest,
                "invocation key is already bound to a different canonical execution"
            );
            existing
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            let request = AgentHubRunResolvedV1::new(
                options.invocation_id.clone(),
                artifact.clone(),
                requested_at,
                requested_at
                    + Duration::seconds(
                        i64::try_from(options.approval_timeout_secs)?
                            + i64::try_from(resolved_intent.timeout_secs)? * attempts
                            + 600 * attempts,
                    ),
            )
            .map_err(anyhow::Error::msg)?;
            store.create_json(&request_path, &request)?;
            request
        }
        Err(error) => return Err(error),
    };

    let receipt_path = invocation_root.join("receipt.json");
    let receipt = match store.read_json::<AgentHubRunReceiptV1>(&receipt_path) {
        Ok(receipt) => receipt,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            let decision = request_gateway(&request, &config, &options).await?;
            decision
                .validate(&request, Utc::now())
                .map_err(anyhow::Error::msg)?;
            let receipt = AgentHubRunReceiptV1::issue(&request, decision, Utc::now())
                .map_err(anyhow::Error::msg)?;
            store.create_json(&receipt_path, &receipt)?;
            receipt
        }
        Err(error) => return Err(error),
    };
    receipt
        .verify(&request, Utc::now())
        .map_err(anyhow::Error::msg)?;
    if receipt.decision() == AgentHubRunDecision::Deny {
        return Ok(protocol::AgentHubRunOutputV1 {
            schema: protocol::AGENT_HUB_RUN_OUTPUT_SCHEMA_V1.into(),
            invocation_id: request.invocation_id,
            approval_receipt: protocol::AgentHubRunApprovalReceiptV1::issued(receipt),
            local_result_v2: serde_json::json!({
                "schema": "local_result_v2",
                "run_id": null,
                "status": "denied",
                "cleanup_outcome": "not_required"
            }),
            error: None,
        });
    }

    let execution = run_allowed(
        store,
        &resolved_intent,
        &artifact,
        &request,
        &receipt,
        &config,
        &options,
    )
    .await;
    let (local_result_v2, error) = match execution {
        Ok(local_result) => (local_result, None),
        Err(error) => (
            existing_local_result(store, &request.invocation_id).unwrap_or_else(|| {
                serde_json::json!({
                    "schema": "local_result_v2",
                    "run_id": null,
                    "status": "failed",
                    "cleanup_outcome": "unknown"
                })
            }),
            Some(bounded_message(&error)),
        ),
    };
    Ok(protocol::AgentHubRunOutputV1 {
        schema: protocol::AGENT_HUB_RUN_OUTPUT_SCHEMA_V1.into(),
        invocation_id: request.invocation_id,
        approval_receipt: protocol::AgentHubRunApprovalReceiptV1::issued(receipt),
        local_result_v2,
        error,
    })
}

fn bounded_message(error: &anyhow::Error) -> String {
    let mut message = error.to_string();
    if message.len() > protocol::AGENT_HUB_RUN_MAX_REASON_BYTES {
        let mut end = protocol::AGENT_HUB_RUN_MAX_REASON_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    message
}

fn existing_local_result(store: &StateStore, invocation_id: &str) -> Option<serde_json::Value> {
    let path = PathBuf::from("authorizations/records").join(format!("{invocation_id}.json"));
    let authorization = store.read_json::<AuthorizationRecord>(&path).ok()?;
    let run_id = &authorization.attempts.last()?.run_id;
    serde_json::to_value(
        crate::attended::local_result(store, run_id, ONE_STEP_RESULT_DOCUMENT_LIMIT_BYTES).ok()?,
    )
    .ok()
}

async fn run_allowed(
    store: &StateStore,
    resolved_intent: &AgentHubRunIntentV1,
    artifact: &protocol::AttendedAuthorizationArtifact,
    request: &AgentHubRunResolvedV1,
    receipt: &AgentHubRunReceiptV1,
    config: &HostConfig,
    options: &HostRunOptions,
) -> anyhow::Result<serde_json::Value> {
    revalidate_workspace_paths(resolved_intent)?;
    let canonical_relative =
        PathBuf::from("agent-hub-run/canonical").join(format!("{}.json", artifact.digest));
    store.create_or_verify_json(&canonical_relative, &artifact)?;
    let authorization_path = store.path(&canonical_relative)?;
    let authorized_relative =
        PathBuf::from("authorizations/records").join(format!("{}.json", request.invocation_id));
    let authorization = AuthorizationRecord {
        authorization_id: request.invocation_id.clone(),
        artifact: artifact.clone(),
        authorization_path: path_string(&authorization_path)?,
        authorized_at: Utc::now(),
        receipt_id: receipt.receipt_id().to_string(),
        domain: DECISION_RECEIPT_DOMAIN.into(),
        state: LaunchState::Authorized,
        attempts: Vec::new(),
        active_attempt_id: None,
        decision_receipt: Some(receipt.clone()),
    };
    match store.read_json::<AuthorizationRecord>(&authorized_relative) {
        Ok(existing) => ensure!(
            existing.authorization_id == authorization.authorization_id
                && existing.artifact == authorization.artifact
                && existing.receipt_id == authorization.receipt_id
                && existing.decision_receipt == authorization.decision_receipt,
            "existing invocation authorization binding mismatch"
        ),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            store.create_json(&authorized_relative, &authorization)?;
        }
        Err(error) => return Err(error),
    }

    let relay = RelayEndpointOverride {
        server_url: Some(options.approvals_server.clone()),
        token: options.approvals_token.clone(),
    };
    let mut cancellation = options.cancellation.clone();
    let cancellation_requested = options.cancellation.clone();
    let cancellation_store = store.clone();
    let cancellation_id = request.invocation_id.clone();
    let cancellation_task = tokio::spawn(async move {
        if !*cancellation.borrow() && cancellation.changed().await.is_err() {
            return Ok::<(), anyhow::Error>(());
        }
        cancel_invocation(&cancellation_store, &cancellation_id)
            .await
            .context("cooperative one-step cancellation")
    });
    let execution = async {
        loop {
            let pins = verify_host_installation(config)?;
            verify_pin_coverage(config, artifact)?;
            receipt
                .verify(request, Utc::now())
                .map_err(anyhow::Error::msg)?;
            revalidate_workspace_paths(resolved_intent)?;
            let record = crate::attended::run(
                store,
                &authorization_path,
                Some(&request.invocation_id),
                selectors(resolved_intent, &artifact.digest),
                relay.clone(),
            )
            .await?;
            drop(pins);
            if !record.retry_eligible {
                break Ok::<_, anyhow::Error>(record);
            }
        }
    }
    .await;
    let cancellation_result = if *cancellation_requested.borrow() {
        cancellation_task.await?
    } else {
        cancellation_task.abort();
        cancellation_task.await.ok();
        Ok(())
    };
    let final_record = match (execution, cancellation_result) {
        (Ok(record), Ok(())) => record,
        (Err(execution), Ok(())) => return Err(execution),
        (Ok(_), Err(cancellation)) => return Err(cancellation),
        (Err(execution), Err(cancellation)) => anyhow::bail!(
            "one-step execution failed: {execution:#}; cancellation failed: {cancellation:#}"
        ),
    };
    let local_result = crate::attended::local_result(
        store,
        &final_record.id,
        ONE_STEP_RESULT_DOCUMENT_LIMIT_BYTES,
    )?;
    Ok(serde_json::to_value(local_result)?)
}

async fn cancel_invocation(store: &StateStore, invocation_id: &str) -> anyhow::Result<()> {
    let authorization_path =
        PathBuf::from("authorizations/records").join(format!("{invocation_id}.json"));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "one-step cancellation did not find an owned attempt"
        );
        let authorization = store.read_json::<AuthorizationRecord>(&authorization_path)?;
        let Some(attempt) = authorization.attempts.last() else {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            continue;
        };
        let record = match crate::attended::status(store, &attempt.run_id) {
            Ok(record) => record,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if record.launch_state == LaunchState::Terminal {
            return Ok(());
        }
        return tokio::time::timeout_at(deadline, crate::attended::cancel(store, &attempt.run_id))
            .await
            .context("one-step cancellation exceeded its cleanup deadline")?;
    }
}

fn load_host_config(path: &Path) -> anyhow::Result<(HostConfig, (u64, u64))> {
    ensure!(path.is_absolute(), "host config path must be absolute");
    let verified = open_regular_nofollow(path)?;
    ensure!(
        std::fs::canonicalize(path)? == path,
        "host config path must be canonical"
    );
    let metadata = verified.file.metadata()?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt as _;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let identity = (0, 0);
    let config: HostConfig = serde_json::from_reader(&verified.file)?;
    ensure!(
        config.schema == HOST_CONFIG_SCHEMA,
        "unsupported host run config schema"
    );
    ensure!(
        Path::new(&config.config_root).is_absolute()
            && Path::new(&config.self_binary.path).is_absolute()
            && Path::new(&config.gateway.path).is_absolute()
            && Path::new(&config.policy.path).is_absolute()
            && Path::new(&config.opencode_plugin_install.path).is_absolute(),
        "host run config paths must be absolute"
    );
    ensure!(
        std::fs::canonicalize(&config.config_root)? == Path::new(&config.config_root),
        "host config root must be canonical"
    );
    ensure!(
        config.config_root_identity.canonical_path == config.config_root,
        "host config root identity path differs from config root"
    );
    Ok((config, identity))
}

fn verify_host_installation(config: &HostConfig) -> anyhow::Result<Vec<VerifiedFile>> {
    verify_directory_identity(&config.config_root_identity)?;
    let current_exe = std::env::current_exe()?.canonicalize()?;
    ensure!(
        current_exe == Path::new(&config.self_binary.path),
        "running executable differs from the pinned host executable"
    );
    let mut verified = vec![
        verify_pin(&config.self_binary, "runtime:agent-hub")?,
        verify_pin(&config.gateway, "runtime:agent-hub-gateway")?,
        verify_pin(&config.policy, "policy:agent-hub-run")?,
        verify_pin(
            &config.opencode_plugin_install,
            &format!("plugin:opencode-{}", config::SUPERVISED_OPENCODE_VERSION),
        )?,
    ];
    ensure!(
        !config.config_artifacts.is_empty() && config.config_artifacts.len() <= 128,
        "host config requires a bounded non-empty config artifact set"
    );
    let mut paths = std::collections::BTreeSet::new();
    let mut relative_paths = std::collections::BTreeSet::new();
    for pin in &config.config_artifacts {
        ensure!(paths.insert(&pin.path), "duplicate pinned config artifact");
        let (relative_path, file) = verify_config_pin(config, pin)?;
        ensure!(
            relative_paths.insert(relative_path),
            "duplicate pinned config artifact relative path"
        );
        verified.push(file);
    }
    let authoritative_paths = config_root_files(Path::new(&config.config_root))?;
    ensure!(
        relative_paths == authoritative_paths,
        "pinned config artifact coverage mismatch: expected {authoritative_paths:?}, found {relative_paths:?}"
    );
    Ok(verified)
}

fn verify_directory_identity(identity: &protocol::HostDirectoryIdentity) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let path = Path::new(&identity.canonical_path);
    ensure!(path.is_absolute(), "pinned directory path must be absolute");
    let metadata = std::fs::metadata(path)?;
    ensure!(metadata.is_dir(), "pinned config root is not a directory");
    ensure!(
        metadata.dev() == identity.device && metadata.ino() == identity.inode,
        "pinned config root identity mismatch"
    );
    Ok(())
}

fn verify_pin(pin: &HostArtifactPin, source_kind: &str) -> anyhow::Result<VerifiedFile> {
    ensure!(
        Path::new(&pin.path).is_absolute()
            && !pin.source_identity.is_empty()
            && pin.source_identity.len() <= 512
            && pin.sha256.len() == 64
            && pin
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "pinned artifact identity is invalid"
    );
    ensure!(
        pin.source_identity == format!("{source_kind}:sha256:{}", pin.sha256),
        "pinned artifact source identity is invalid for {source_kind}"
    );
    let mut verified = open_regular_nofollow(Path::new(&pin.path))?;
    ensure!(
        std::fs::canonicalize(&pin.path)? == Path::new(&pin.path),
        "pinned artifact path is not canonical: {}",
        pin.path
    );
    let mut bytes = Vec::new();
    verified.file.read_to_end(&mut bytes)?;
    ensure!(
        protocol::sha256_hex(&bytes) == pin.sha256,
        "pinned artifact hash mismatch: {}",
        pin.path
    );
    let current = std::fs::metadata(&pin.path)?;
    let opened = verified.file.metadata()?;
    use std::os::unix::fs::MetadataExt as _;
    ensure!(
        current.dev() == opened.dev()
            && current.ino() == opened.ino()
            && opened.dev() == pin.device
            && opened.ino() == pin.inode,
        "pinned artifact path identity changed: {}",
        pin.path
    );
    verified.file.rewind()?;
    Ok(verified)
}

fn verify_config_pin(
    config: &HostConfig,
    pin: &HostArtifactPin,
) -> anyhow::Result<(String, VerifiedFile)> {
    let relative_path = pin
        .source_identity
        .strip_prefix("config:")
        .and_then(|value| value.strip_suffix(&format!(":sha256:{}", pin.sha256)))
        .filter(|value| !value.is_empty())
        .context("pinned config artifact source identity is invalid")?;
    let relative = Path::new(relative_path);
    ensure!(
        relative_path.len() <= 1024
            && !relative.is_absolute()
            && relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
            && relative.to_string_lossy().replace('\\', "/") == relative_path,
        "pinned config artifact source identity is invalid"
    );
    let expected_path = Path::new(&config.config_root).join(relative);
    ensure!(
        Path::new(&pin.path) == expected_path,
        "pinned config artifact path does not match its canonical relative path"
    );
    Ok((
        relative_path.to_string(),
        verify_pin(pin, &format!("config:{relative_path}"))?,
    ))
}

fn config_root_files(root: &Path) -> anyhow::Result<std::collections::BTreeSet<String>> {
    fn collect(
        root: &Path,
        directory: &Path,
        files: &mut std::collections::BTreeSet<String>,
    ) -> anyhow::Result<()> {
        let mut entries = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type()?;
            ensure!(
                !file_type.is_symlink(),
                "authoritative config root contains a path alias: {}",
                path.display()
            );
            if file_type.is_dir() {
                collect(root, &path, files)?;
            } else {
                ensure!(
                    file_type.is_file(),
                    "authoritative config root contains a non-file entry: {}",
                    path.display()
                );
                let relative = path
                    .strip_prefix(root)?
                    .to_str()
                    .context("authoritative config root contains a non-UTF-8 relative path")?;
                ensure!(
                    files.insert(relative.to_string()),
                    "duplicate authoritative config relative path"
                );
                ensure!(
                    files.len() <= 128,
                    "authoritative config root exceeds file limit"
                );
            }
        }
        Ok(())
    }

    let mut files = std::collections::BTreeSet::new();
    collect(root, root, &mut files)?;
    Ok(files)
}

fn verify_pin_coverage(
    config: &HostConfig,
    artifact: &protocol::AttendedAuthorizationArtifact,
) -> anyhow::Result<()> {
    let request = artifact.verify_v3().map_err(anyhow::Error::msg)?;
    ensure!(
        request.execution.config_root == config.config_root,
        "canonical execution config root differs from pinned config root"
    );
    Ok(())
}

fn open_regular_nofollow(path: &Path) -> anyhow::Result<VerifiedFile> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| {
            format!(
                "open pinned file without following links {}",
                path.display()
            )
        })?;
    ensure!(
        file.metadata()?.is_file(),
        "pinned path is not a regular file"
    );
    Ok(VerifiedFile { file })
}

fn resolve_intent(
    mut intent: AgentHubRunIntentV1,
    _config: &HostConfig,
) -> anyhow::Result<AgentHubRunIntentV1> {
    intent.validate_shape().map_err(anyhow::Error::msg)?;
    let export_workspace = intent.workspace_export_root.as_ref().and_then(|root| {
        intent
            .workspaces
            .iter()
            .find(|workspace| Path::new(root) == Path::new(&workspace.path).join(".agent-hub/runs"))
            .map(|workspace| workspace.alias.clone())
    });
    for item in &mut intent.workspaces {
        item.path = canonical_directory(&item.path)?;
    }
    for item in &mut intent.context_roots {
        item.path = canonical_directory(&item.path)?;
    }
    if let Some(alias) = export_workspace {
        let workspace = intent
            .workspaces
            .iter()
            .find(|workspace| workspace.alias == alias)
            .context("workspace export workspace disappeared during resolution")?;
        intent.workspace_export_root = Some(path_string(
            &Path::new(&workspace.path).join(".agent-hub/runs"),
        )?);
    }
    intent.validate_shape().map_err(anyhow::Error::msg)?;
    Ok(intent)
}

fn trusted_image_identity(
    intent: &AgentHubRunIntentV1,
    config: &HostConfig,
) -> anyhow::Result<String> {
    let identity = config
        .image_digests
        .get(&intent.environment)
        .with_context(|| {
            format!(
                "trusted host config has no image identity for environment '{}'",
                intent.environment
            )
        })?;
    ensure!(
        identity
            .strip_prefix("sha256:")
            .is_some_and(|digest| digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
        "trusted host image identity is not sha256"
    );
    Ok(identity.clone())
}

fn revalidate_workspace_paths(intent: &AgentHubRunIntentV1) -> anyhow::Result<()> {
    for item in &intent.workspaces {
        ensure!(
            canonical_directory(&item.path)? == item.path,
            "workspace path identity changed before execution"
        );
    }
    for item in &intent.context_roots {
        ensure!(
            canonical_directory(&item.path)? == item.path,
            "context root identity changed before execution"
        );
    }
    Ok(())
}

fn canonical_directory(path: &str) -> anyhow::Result<String> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize requested directory {path}"))?;
    ensure!(
        canonical.is_dir(),
        "requested path is not a directory: {path}"
    );
    path_string(&canonical)
}

fn proposal_input(intent: &AgentHubRunIntentV1, config: &HostConfig) -> ProposalInput {
    ProposalInput {
        config_root: PathBuf::from(&config.config_root),
        environment: intent.environment.clone(),
        image_family: intent.image_family.as_str().into(),
        network_class: intent.network_class.as_str().into(),
        network_resources: intent.network_resources.clone(),
        tool_approval: intent.tool_approval.as_str().into(),
        overlays: intent.overlays.clone(),
        timeout_secs: intent.timeout_secs,
        prompt: intent.prompt.clone(),
        workspaces: intent
            .workspaces
            .iter()
            .map(|w| RequestedWorkspace {
                alias: w.alias.clone(),
                path: PathBuf::from(&w.path),
                access: w.access,
            })
            .collect(),
        context_roots: intent
            .context_roots
            .iter()
            .map(|r| RequestedContextRoot {
                alias: r.alias.clone(),
                path: PathBuf::from(&r.path),
                access: protocol::WorkspaceAccess::ReadOnly,
            })
            .collect(),
        primary_workspace: intent.primary_workspace.clone(),
        workspace_export_root: intent.workspace_export_root.as_deref().map(PathBuf::from),
    }
}

fn selectors<'a>(intent: &'a AgentHubRunIntentV1, digest: &'a str) -> AuthorizationSelectors<'a> {
    AuthorizationSelectors {
        digest,
        environment: &intent.environment,
        image_family: intent.image_family.as_str(),
        network_class: intent.network_class.as_str(),
        network_resources: &intent.network_resources,
        tool_approval: intent.tool_approval.as_str(),
        overlays: &intent.overlays,
        timeout_secs: intent.timeout_secs,
    }
}

async fn request_gateway(
    request: &AgentHubRunResolvedV1,
    config: &HostConfig,
    options: &HostRunOptions,
) -> anyhow::Result<AgentHubRunDecisionV1> {
    let input = request.gateway_event().map_err(anyhow::Error::msg)?;
    let gateway = open_verified_pin(&config.gateway)?;
    let policy = open_verified_pin(&config.policy)?;
    let gateway_path = inherited_fd_path(&gateway.file)?;
    let policy_path = inherited_fd_path(&policy.file)?;
    let mut command = tokio::process::Command::new(gateway_path);
    command
        .kill_on_drop(true)
        .env_clear()
        .arg("agent-hub-run-resolved")
        .arg("--server")
        .arg(&options.approvals_server)
        .arg("--config")
        .arg(policy_path)
        .arg("--timeout")
        .arg(options.approval_timeout_secs.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = &options.approvals_token {
        command.env("AGENT_HUB_TOKEN", token.expose());
    }
    let child = command
        .spawn()
        .context("start trusted agent_hub_run permission gateway")?;
    let output = collect_gateway_output(
        child,
        &serde_json::to_vec(&input)?,
        Some(options.cancellation.clone()),
    )
    .await?;
    ensure!(
        output.status.success(),
        "agent_hub_run permission gateway failed closed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("parse agent_hub_run gateway decision")
}

async fn request_gateway_preview(
    request: &AgentHubRunResolvedV1,
    config: &HostConfig,
) -> anyhow::Result<serde_json::Value> {
    let input = request.gateway_event().map_err(anyhow::Error::msg)?;
    let gateway = open_verified_pin(&config.gateway)?;
    let policy = open_verified_pin(&config.policy)?;
    let mut command = tokio::process::Command::new(inherited_fd_path(&gateway.file)?);
    command
        .kill_on_drop(true)
        .env_clear()
        .arg("agent-hub-run-resolved")
        .arg("--server")
        .arg("http://127.0.0.1:1")
        .arg("--config")
        .arg(inherited_fd_path(&policy.file)?)
        .arg("--timeout")
        .arg("1")
        .arg("--preview")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().context("start trusted policy preview")?;
    let output = collect_gateway_output(child, &serde_json::to_vec(&input)?, None).await?;
    ensure!(
        output.status.success(),
        "agent_hub_run policy preview failed closed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        value["schema"] == "agent_hub_run_policy_preview_v1"
            && value["projection"] == serde_json::to_value(&request.policy_projection)?
            && matches!(
                value["matched_action"].as_str(),
                Some("allow" | "ask" | "deny" | "forbidden_delegate")
            ),
        "agent_hub_run policy preview binding mismatch"
    );
    Ok(value)
}

enum GatewayStreamEvent {
    Complete(bool, Vec<u8>),
    Overflow(&'static str),
    Failed(&'static str, std::io::Error),
}

async fn read_gateway_stream(
    mut stream: impl AsyncRead + Unpin,
    stdout: bool,
    label: &'static str,
    sender: tokio::sync::mpsc::UnboundedSender<GatewayStreamEvent>,
) {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => {
                let _ = sender.send(GatewayStreamEvent::Complete(stdout, output));
                return;
            }
            Ok(count)
                if output.len().saturating_add(count)
                    > protocol::AGENT_HUB_RUN_MAX_GATEWAY_OUTPUT_BYTES =>
            {
                let _ = sender.send(GatewayStreamEvent::Overflow(label));
                return;
            }
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error) => {
                let _ = sender.send(GatewayStreamEvent::Failed(label, error));
                return;
            }
        }
    }
}

async fn terminate_gateway(child: &mut tokio::process::Child) -> anyhow::Result<()> {
    #[cfg(unix)]
    if child.try_wait()?.is_none() {
        let result = unsafe {
            libc::kill(
                child.id().context("permission gateway has no PID")? as i32,
                libc::SIGTERM,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("signal agent_hub_run permission gateway");
        }
    }
    #[cfg(not(unix))]
    if child.try_wait()?.is_none() {
        child
            .start_kill()
            .context("terminate agent_hub_run permission gateway")?;
    }
    if tokio::time::timeout(GATEWAY_CLEANUP_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        kill_gateway(child).await?;
    }
    Ok(())
}

async fn kill_gateway(child: &mut tokio::process::Child) -> anyhow::Result<()> {
    if child.try_wait()?.is_none() {
        child
            .start_kill()
            .context("kill agent_hub_run permission gateway")?;
    }
    tokio::time::timeout(GATEWAY_CLEANUP_TIMEOUT, child.wait())
        .await
        .context("permission gateway did not exit within cleanup deadline")??;
    Ok(())
}

async fn collect_gateway_output(
    mut child: tokio::process::Child,
    input: &[u8],
    mut cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> anyhow::Result<std::process::Output> {
    let stdout = child
        .stdout
        .take()
        .context("permission gateway stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("permission gateway stderr unavailable")?;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(read_gateway_stream(stdout, true, "stdout", sender.clone()));
    let stderr_task = tokio::spawn(read_gateway_stream(stderr, false, "stderr", sender));

    let mut stdin = child
        .stdin
        .take()
        .context("permission gateway stdin unavailable")?;
    let write = async {
        stdin.write_all(input).await?;
        stdin.shutdown().await
    };
    if let Some(cancel) = cancellation.as_mut() {
        tokio::select! {
            result = write => result.context("write permission gateway request")?,
            _ = wait_for_cancellation(cancel) => {
                terminate_gateway(&mut child).await?;
                stdout_task.abort();
                stderr_task.abort();
                anyhow::bail!("agent_hub_run cancelled while awaiting permission");
            }
        }
    } else {
        write.await.context("write permission gateway request")?;
    }
    drop(stdin);

    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    while status.is_none() || stdout.is_none() || stderr.is_none() {
        tokio::select! {
            result = child.wait(), if status.is_none() => {
                status = Some(result.context("wait for permission gateway")?);
            }
            event = receiver.recv(), if stdout.is_none() || stderr.is_none() => {
                match event.context("permission gateway stream readers stopped")? {
                    GatewayStreamEvent::Complete(true, bytes) => stdout = Some(bytes),
                    GatewayStreamEvent::Complete(false, bytes) => stderr = Some(bytes),
                    GatewayStreamEvent::Overflow(label) => {
                        kill_gateway(&mut child).await?;
                        stdout_task.abort();
                        stderr_task.abort();
                        anyhow::bail!("agent_hub_run gateway {label} exceeds limit");
                    }
                    GatewayStreamEvent::Failed(label, error) => {
                        terminate_gateway(&mut child).await?;
                        stdout_task.abort();
                        stderr_task.abort();
                        return Err(error).with_context(|| format!("read permission gateway {label}"));
                    }
                }
            }
            _ = async {
                if let Some(cancel) = cancellation.as_mut() {
                    wait_for_cancellation(cancel).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                terminate_gateway(&mut child).await?;
                stdout_task.abort();
                stderr_task.abort();
                anyhow::bail!("agent_hub_run cancelled while awaiting permission");
            }
        }
    }
    stdout_task
        .await
        .context("join permission gateway stdout reader")?;
    stderr_task
        .await
        .context("join permission gateway stderr reader")?;
    Ok(std::process::Output {
        status: status.expect("collected above"),
        stdout: stdout.expect("collected above"),
        stderr: stderr.expect("collected above"),
    })
}

async fn wait_for_cancellation(cancellation: &mut tokio::sync::watch::Receiver<bool>) {
    if *cancellation.borrow() {
        return;
    }
    while cancellation.changed().await.is_ok() {
        if *cancellation.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

fn open_verified_pin(pin: &HostArtifactPin) -> anyhow::Result<VerifiedFile> {
    let source_kind = pin
        .source_identity
        .rsplit_once(":sha256:")
        .map(|(source, _)| source)
        .context("pinned artifact source identity is invalid")?;
    verify_pin(pin, source_kind)
}

#[cfg(unix)]
fn inherited_fd_path(file: &File) -> anyhow::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;
    let fd = file.as_raw_fd();
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("make pinned descriptor inheritable");
    }
    Ok(PathBuf::from(format!("/proc/self/fd/{fd}")))
}

#[cfg(not(unix))]
fn inherited_fd_path(_file: &File) -> anyhow::Result<PathBuf> {
    anyhow::bail!("pinned host execution requires Unix file descriptors")
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .context("agent_hub_run path is not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/test-tmp");
        std::fs::create_dir_all(&root).unwrap();
        root.join(format!("{name}-{}", uuid::Uuid::new_v4()))
    }

    fn python_child(script: &str, pid_path: &Path) -> tokio::process::Child {
        let mut command = tokio::process::Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(script)
            .arg(pid_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.spawn().unwrap()
    }

    fn assert_reaped(pid_path: &Path) {
        let pid = std::fs::read_to_string(pid_path)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        std::fs::remove_file(pid_path).unwrap();
    }

    #[tokio::test]
    async fn gateway_streams_are_concurrent_hard_capped_and_overflow_is_reaped() {
        let pid_path = test_path("gateway-overflow-pid");
        let child = python_child(
            "import os,sys,threading; open(sys.argv[1],'w').write(str(os.getpid())); f=lambda s: (s.write(b'x'*131072),s.flush()); a=threading.Thread(target=f,args=(sys.stdout.buffer,)); b=threading.Thread(target=f,args=(sys.stderr.buffer,)); a.start(); b.start(); a.join(); b.join()",
            &pid_path,
        );
        let error = collect_gateway_output(child, b"", None).await.unwrap_err();
        assert!(error.to_string().contains("exceeds limit"), "{error:#}");
        assert_reaped(&pid_path);
    }

    #[tokio::test]
    async fn cancellation_terminates_and_reaps_a_pending_gateway() {
        let pid_path = test_path("gateway-cancel-pid");
        let child = python_child(
            "import os,sys,time; open(sys.argv[1],'w').write(str(os.getpid())); time.sleep(60)",
            &pid_path,
        );
        let (cancel, cancellation) = tokio::sync::watch::channel(false);
        let task =
            tokio::spawn(
                async move { collect_gateway_output(child, b"", Some(cancellation)).await },
            );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pid_path.is_file() {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancel.send(true).unwrap();
        let error = task.await.unwrap().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cancelled while awaiting permission")
        );
        assert_reaped(&pid_path);
    }
}
