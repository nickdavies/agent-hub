use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, ensure};
use chrono::{Duration, Utc};
use protocol::{
    AgentHubRunDecision, AgentHubRunDecisionV1, AgentHubRunIntentV1, AgentHubRunReceiptV1,
    AgentHubRunResolvedV1,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::attended::{
    AuthorizationRecord, AuthorizationSelectors, LaunchState, ProposalInput, RelayEndpointOverride,
};
use crate::local::{RequestedContextRoot, RequestedWorkspace};
use crate::state::StateStore;

const HOST_CONFIG_SCHEMA: &str = "agent_hub_run_host_v2";
const MAX_CONFIG_FILES: usize = 128;
const MAX_CONFIG_DIRECTORIES: usize = 128;
const MAX_CONFIG_TRAVERSAL_ENTRIES: usize = MAX_CONFIG_FILES + MAX_CONFIG_DIRECTORIES;
const MAX_CATALOG_CACHE_WATCH_ENTRIES: usize = MAX_CONFIG_FILES + MAX_CONFIG_DIRECTORIES + 2;
const MAX_HOST_CONFIG_BYTES: usize = 256 * 1024;
const MAX_CONFIG_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CONFIG_AGGREGATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_APPROVAL_TIMEOUT_SECS: u64 = 600;
const ONE_STEP_RESULT_DOCUMENT_LIMIT_BYTES: u64 = 128 * 1024;
const GATEWAY_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const CAPABILITIES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub(crate) const DECISION_RECEIPT_DOMAIN: &str = "agent_hub_run";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfig {
    schema: String,
    config_root: String,
    gateway: HostPath,
    policy: HostPath,
    managed_images: std::collections::BTreeMap<String, String>,
    backend_metadata_revision: String,
    #[serde(skip)]
    host_config_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostPath {
    path: String,
}

struct VerifiedFile {
    file: File,
}

type VerifiedConfigInventory = (
    Vec<(String, VerifiedFile)>,
    std::collections::BTreeMap<String, std::fs::Metadata>,
);

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostConfigIdentity {
    device: u64,
    inode: u64,
    snapshot_sha256: String,
}

pub(crate) struct CatalogConfigSnapshot {
    pub files: std::collections::BTreeMap<String, Vec<u8>>,
    pub managed_images: std::collections::BTreeMap<String, String>,
    pub backend_metadata_revision: String,
    pub cache_identity: Option<CatalogCacheIdentity>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CatalogCacheIdentity {
    pub schema: &'static str,
    pub backend_metadata_revision: String,
    pub watch: Vec<CatalogWatchStamp>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CatalogWatchStamp {
    pub path: String,
    pub device: String,
    pub inode: String,
    pub mode: u32,
    #[serde(rename = "type")]
    pub file_type: &'static str,
    pub size: String,
    pub modified_ns: String,
    pub changed_ns: String,
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
    Ok(trusted_host_home()?.join(".config/agent-hub/host-run.json"))
}

/// Query both configured control-plane peers without preparing or mutating a run.
pub(crate) async fn capabilities(
    host_config: &Path,
    approvals_server: &str,
    approvals_token: Option<protocol::Secret>,
) -> anyhow::Result<protocol::AgentHubCapabilitiesV1> {
    let (config, _) = load_host_config(host_config)?;
    let gateway = open_backend_file(Path::new(&config.gateway.path), "gateway executable")?;
    let mut command = tokio::process::Command::new(inherited_fd_path(&gateway.file)?);
    command
        .kill_on_drop(true)
        .env_clear()
        .env("HOME", trusted_host_home()?)
        .arg("capabilities")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command
        .spawn()
        .context("start pinned gateway capability query")?;
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let mut collect = Box::pin(collect_gateway_output_bounded(
        child,
        b"",
        Some(cancel_rx),
        protocol::AGENT_HUB_CAPABILITIES_MAX_BYTES,
    ));
    let output = tokio::select! {
        output = &mut collect => output?,
        _ = tokio::time::sleep(CAPABILITIES_TIMEOUT) => {
            let _ = cancel_tx.send(true);
            collect.await.context("pinned gateway capability query timed out")?
        }
    };
    ensure!(
        output.status.success(),
        "pinned gateway capability query failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let gateway_capabilities: protocol::AgentHubCapabilitiesV1 =
        serde_json::from_slice(&output.stdout).context("parse pinned gateway capabilities")?;
    gateway_capabilities
        .validate()
        .map_err(anyhow::Error::msg)?;

    let client = agent_hub_approval_client::ApprovalClient::with_options(
        approvals_server,
        approvals_token,
        agent_hub_approval_client::ClientOptions {
            operation_timeout: CAPABILITIES_TIMEOUT,
            response_limit: protocol::AGENT_HUB_CAPABILITIES_MAX_BYTES,
        },
    )?;
    let service_capabilities = client.capabilities().await?.value;
    let intersection = gateway_capabilities
        .intersection(&service_capabilities)
        .map_err(anyhow::Error::msg)?;
    intersection
        .require(&[protocol::AGENT_HUB_RUN_CAPABILITY_V1])
        .map_err(anyhow::Error::msg)?;
    Ok(intersection)
}

fn trusted_host_home() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME")
        .context("HOME is required and must be UTF-8 for trusted host configuration")?;
    let home = PathBuf::from(home);
    ensure!(home.is_absolute(), "trusted host HOME must be absolute");
    ensure!(
        home.is_dir(),
        "trusted host HOME must be an existing directory"
    );
    Ok(home)
}

/// Canonical preparation only. It performs no state mutation or execution.
pub(crate) async fn inspect(
    intent: AgentHubRunIntentV1,
    host_config: &Path,
) -> anyhow::Result<protocol::AttendedAuthorizationArtifact> {
    let (config, _) = load_host_config(host_config)?;
    let pins = verify_host_installation(&config)?;
    let deployment = config::validate_authoritative_config_root(Path::new(&config.config_root))
        .map_err(anyhow::Error::msg)?;
    let resolved = resolve_intent(intent, &config)?;
    let image = resolve_managed_image(&resolved, &config, &deployment).await?;
    let artifact = crate::attended::prepare_canonical_execution_from_deployment(
        proposal_input(&resolved, &config, &[])?,
        image,
        &deployment,
    )
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
    let deployment = config::validate_authoritative_config_root(Path::new(&config.config_root))
        .map_err(anyhow::Error::msg)?;
    let resolved = resolve_intent(intent, &config)?;
    let image = resolve_managed_image(&resolved, &config, &deployment).await?;
    let artifact = crate::attended::prepare_canonical_execution_from_deployment(
        proposal_input(&resolved, &config, &[])?,
        image,
        &deployment,
    )
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
    let deployment = config::validate_authoritative_config_root(Path::new(&config.config_root))
        .map_err(anyhow::Error::msg)?;
    let image = resolve_managed_image(&resolved_intent, &config, &deployment).await?;
    let artifact = crate::attended::prepare_canonical_execution_from_deployment(
        proposal_input(&resolved_intent, &config, &[store.root().to_path_buf()])?,
        image,
        &deployment,
    )
    .await?;
    artifact.verify_v4().map_err(anyhow::Error::msg)?;
    verify_pin_coverage(&config, &artifact)?;
    drop(pins);
    drop(verify_host_installation(&config)?);
    let retry = artifact.verify_v4().map_err(anyhow::Error::msg)?.retry;
    let attempts = i64::from(retry.max_attempts);
    let requested_at = Utc::now();
    let invocation_root = PathBuf::from("agent-hub-run/invocations").join(&options.invocation_id);
    let request_path = invocation_root.join("request.json");
    let request = match store.read_json::<AgentHubRunResolvedV1>(&request_path) {
        Ok(existing) => {
            existing.validate(Utc::now()).map_err(anyhow::Error::msg)?;
            validate_persisted_request_binding(
                &existing.invocation_id,
                &existing.canonical_digest,
                &options.invocation_id,
                &artifact.digest,
            )?;
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

fn validate_persisted_request_binding(
    embedded_invocation_id: &str,
    embedded_digest: &str,
    trusted_invocation_id: &str,
    expected_digest: &str,
) -> anyhow::Result<()> {
    ensure!(
        embedded_invocation_id == trusted_invocation_id,
        "persisted invocation ID does not match the trusted invocation key"
    );
    ensure!(
        embedded_digest == expected_digest,
        "invocation key is already bound to a different canonical execution"
    );
    Ok(())
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
            verify_protected_paths(config, store, resolved_intent, artifact)?;
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

fn load_host_config(path: &Path) -> anyhow::Result<(HostConfig, HostConfigIdentity)> {
    let (config, identity, _) = load_host_config_file(path)?;
    Ok((config, identity))
}

fn load_host_config_file(
    path: &Path,
) -> anyhow::Result<(HostConfig, HostConfigIdentity, VerifiedFile)> {
    ensure!(path.is_absolute(), "host config path must be absolute");
    let verified = open_regular_nofollow(path)?;
    ensure!(
        std::fs::canonicalize(path)? == path,
        "host config path must be canonical"
    );
    let metadata = verified.file.metadata()?;
    reject_protected_metadata_aliases(path, &metadata, false)?;
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt as _;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let (device, inode) = (0, 0);
    let mut reader = (&verified.file).take((MAX_HOST_CONFIG_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_HOST_CONFIG_BYTES,
        "host config exceeds byte limit"
    );
    let identity = HostConfigIdentity {
        device,
        inode,
        snapshot_sha256: protocol::sha256_hex(&bytes),
    };
    let after = verified.file.metadata()?;
    ensure!(
        same_catalog_metadata(&metadata, &after) && after.len() == bytes.len() as u64,
        "host config changed while being captured"
    );
    ensure!(
        same_catalog_metadata(&after, &std::fs::symlink_metadata(path)?),
        "host config was replaced while being captured"
    );
    let mut config: HostConfig = serde_json::from_slice(&bytes)?;
    config.host_config_path = path.to_path_buf();
    ensure!(
        config.schema == HOST_CONFIG_SCHEMA,
        "unsupported host run config schema"
    );
    ensure!(
        Path::new(&config.config_root).is_absolute()
            && Path::new(&config.gateway.path).is_absolute()
            && Path::new(&config.policy.path).is_absolute(),
        "host run config paths must be absolute"
    );
    ensure!(
        std::fs::canonicalize(&config.config_root)? == Path::new(&config.config_root),
        "host config root must be canonical"
    );
    ensure!(
        !config.backend_metadata_revision.is_empty()
            && config.backend_metadata_revision.len() <= 128
            && !config
                .backend_metadata_revision
                .chars()
                .any(char::is_control),
        "backend metadata revision is invalid"
    );
    ensure!(
        !config.managed_images.is_empty() && config.managed_images.len() <= 16,
        "managed image aliases must be bounded and non-empty"
    );
    for (family, reference) in &config.managed_images {
        ensure!(
            !family.is_empty()
                && family.len() <= 64
                && !reference.is_empty()
                && reference.len() <= 512
                && !reference.chars().any(char::is_control),
            "managed image alias is invalid"
        );
    }
    validate_backend_path(Path::new(&config.gateway.path), "gateway executable")?;
    validate_backend_path(Path::new(&config.policy.path), "gateway policy")?;
    Ok((config, identity, verified))
}

fn verify_host_installation(config: &HostConfig) -> anyhow::Result<Vec<VerifiedFile>> {
    ensure!(
        Path::new(&config.config_root).is_dir(),
        "config root is not a directory"
    );
    let mut files = vec![
        open_backend_file(Path::new(&config.gateway.path), "gateway executable")?,
        open_backend_file(Path::new(&config.policy.path), "gateway policy")?,
    ];
    files.extend(
        verify_config_inventory(config)?
            .0
            .into_iter()
            .map(|(_, file)| file),
    );
    Ok(files)
}

fn verify_config_inventory(config: &HostConfig) -> anyhow::Result<VerifiedConfigInventory> {
    let mut verified = Vec::new();
    let (authoritative_paths, directories) = config_root_inventory(Path::new(&config.config_root))?;
    for relative in authoritative_paths {
        let path = Path::new(&config.config_root).join(&relative);
        verified.push((relative, open_backend_file(&path, "config artifact")?));
    }
    Ok((verified, directories))
}

/// Capture only the fully pinned authoritative config inventory. The optional
/// cache identity additionally verifies the running executable; neither path
/// performs execution preparation.
pub(crate) fn load_catalog_config(
    host_config: &Path,
    include_cache_identity: bool,
) -> anyhow::Result<CatalogConfigSnapshot> {
    let (config, _, host_config_file) = load_host_config_file(host_config)?;
    let (inventory, directories) = verify_config_inventory(&config)?;
    let mut files = std::collections::BTreeMap::new();
    let mut watch = std::collections::BTreeMap::new();
    if include_cache_identity {
        watch.insert(
            path_string(host_config)?,
            catalog_watch_stamp(host_config, &host_config_file.file.metadata()?, "file")?,
        );
        for (path, metadata) in &directories {
            watch.insert(
                path.clone(),
                catalog_watch_stamp(Path::new(path), metadata, "directory")?,
            );
        }
    }
    let mut aggregate_bytes = 0usize;
    for (relative, mut verified) in inventory {
        let before = verified.file.metadata()?;
        let bytes = read_limited_config_file(&mut verified.file, MAX_CONFIG_FILE_BYTES, &relative)?;
        aggregate_bytes = aggregate_bytes
            .checked_add(bytes.len())
            .context("config snapshot byte count overflow")?;
        ensure!(
            aggregate_bytes <= MAX_CONFIG_AGGREGATE_BYTES,
            "authoritative config snapshot exceeds aggregate byte limit"
        );
        let after = verified.file.metadata()?;
        ensure!(
            same_catalog_metadata(&before, &after) && after.len() == bytes.len() as u64,
            "config artifact '{relative}' changed while being captured"
        );
        let path = Path::new(&config.config_root).join(&relative);
        let current = std::fs::symlink_metadata(&path)?;
        ensure!(
            same_catalog_metadata(&after, &current),
            "config artifact '{relative}' was replaced while being captured"
        );
        ensure!(
            files.insert(relative.clone(), bytes).is_none(),
            "duplicate captured config artifact"
        );
        if include_cache_identity {
            watch.insert(
                path_string(&path)?,
                catalog_watch_stamp(&path, &verified.file.metadata()?, "file")?,
            );
        }
    }
    let (paths_after, directories_after) = config_root_inventory(Path::new(&config.config_root))?;
    ensure!(
        paths_after == files.keys().cloned().collect()
            && directories.keys().eq(directories_after.keys())
            && directories.iter().all(|(path, before)| directories_after
                .get(path)
                .is_some_and(|after| same_catalog_metadata(before, after))),
        "authoritative config snapshot changed while being captured"
    );
    ensure!(
        same_catalog_metadata(
            &host_config_file.file.metadata()?,
            &std::fs::symlink_metadata(host_config)?,
        ),
        "host config changed while catalog inputs were being captured"
    );
    if include_cache_identity {
        let current_exe = std::env::current_exe()?.canonicalize()?;
        let executable = open_backend_file(&current_exe, "current executable")?;
        watch.insert(
            path_string(&current_exe)?,
            catalog_watch_stamp(&current_exe, &executable.file.metadata()?, "file")?,
        );
        ensure!(
            watch.len() <= MAX_CATALOG_CACHE_WATCH_ENTRIES,
            "catalog cache watch inventory exceeds limit"
        );
    }
    let backend_metadata_revision = config.backend_metadata_revision.clone();
    Ok(CatalogConfigSnapshot {
        files,
        managed_images: config.managed_images,
        backend_metadata_revision,
        cache_identity: include_cache_identity.then(|| CatalogCacheIdentity {
            schema: "agent_hub_catalog_cache_identity_v2",
            backend_metadata_revision: config.backend_metadata_revision,
            watch: watch.into_values().collect(),
        }),
    })
}

fn read_limited_config_file(file: &mut File, limit: usize, label: &str) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.by_ref()
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "config artifact '{label}' exceeds per-file byte limit"
    );
    Ok(bytes)
}

fn same_catalog_metadata(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.mode() == right.mode()
        && left.size() == right.size()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn catalog_watch_stamp(
    path: &Path,
    metadata: &std::fs::Metadata,
    file_type: &'static str,
) -> anyhow::Result<CatalogWatchStamp> {
    use std::os::unix::fs::MetadataExt as _;
    let path = path_string(path)?;
    ensure!(
        Path::new(&path).is_absolute() && path.len() <= 4096 && !path.chars().any(char::is_control),
        "catalog cache watch path is outside its bounds"
    );
    let nanoseconds = |seconds: i64, nanos: i64| {
        (i128::from(seconds) * 1_000_000_000 + i128::from(nanos)).to_string()
    };
    Ok(CatalogWatchStamp {
        path,
        device: metadata.dev().to_string(),
        inode: metadata.ino().to_string(),
        mode: metadata.mode(),
        file_type,
        size: metadata.size().to_string(),
        modified_ns: nanoseconds(metadata.mtime(), metadata.mtime_nsec()),
        changed_ns: nanoseconds(metadata.ctime(), metadata.ctime_nsec()),
    })
}

fn validate_backend_path(path: &Path, role: &str) -> anyhow::Result<()> {
    ensure!(
        path.is_absolute()
            && path
                .to_str()
                .is_some_and(|value| value.len() <= 4096 && !value.chars().any(char::is_control)),
        "{role} path is invalid"
    );
    ensure!(
        std::fs::canonicalize(path)? == path,
        "{role} path must be canonical"
    );
    Ok(())
}

fn open_backend_file(path: &Path, role: &str) -> anyhow::Result<VerifiedFile> {
    let verified = open_regular_nofollow(path)?;
    validate_backend_path(path, role)?;
    let metadata = verified.file.metadata()?;
    ensure!(
        same_catalog_metadata(&metadata, &std::fs::symlink_metadata(path)?),
        "{role} changed while being opened"
    );
    reject_protected_metadata_aliases(path, &metadata, role.contains("executable"))?;
    Ok(verified)
}

fn config_root_inventory(
    root: &Path,
) -> anyhow::Result<(
    std::collections::BTreeSet<String>,
    std::collections::BTreeMap<String, std::fs::Metadata>,
)> {
    const MAX_DEPTH: usize = 16;

    fn collect(
        root: &Path,
        directory: &Path,
        depth: usize,
        entries_seen: &mut usize,
        files: &mut std::collections::BTreeSet<String>,
        directories: &mut std::collections::BTreeMap<String, std::fs::Metadata>,
    ) -> anyhow::Result<()> {
        ensure!(
            depth <= MAX_DEPTH,
            "authoritative config root exceeds depth limit"
        );
        let metadata = std::fs::symlink_metadata(directory)?;
        ensure!(
            metadata.is_dir(),
            "authoritative config directory is invalid"
        );
        directories.insert(path_string(directory)?, metadata);
        ensure!(
            directories.len() <= MAX_CONFIG_DIRECTORIES,
            "authoritative config root exceeds directory limit"
        );
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(directory)? {
            *entries_seen += 1;
            ensure!(
                *entries_seen <= MAX_CONFIG_TRAVERSAL_ENTRIES,
                "authoritative config root exceeds entry limit"
            );
            entries.push(entry?);
        }
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
                collect(root, &path, depth + 1, entries_seen, files, directories)?;
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
                    files.len() <= MAX_CONFIG_FILES,
                    "authoritative config root exceeds file limit"
                );
            }
        }
        Ok(())
    }

    let mut files = std::collections::BTreeSet::new();
    let mut directories = std::collections::BTreeMap::new();
    let mut entries_seen = 0;
    collect(
        root,
        root,
        0,
        &mut entries_seen,
        &mut files,
        &mut directories,
    )?;
    Ok((files, directories))
}

fn verify_pin_coverage(
    config: &HostConfig,
    artifact: &protocol::AttendedAuthorizationArtifact,
) -> anyhow::Result<()> {
    let request = artifact.verify_v4().map_err(anyhow::Error::msg)?;
    ensure!(
        request.execution.config_root == config.config_root,
        "canonical execution config root differs from trusted config root"
    );
    ensure!(
        config.managed_images.get(&request.proposal.image_family)
            == Some(&request.proposal.image_tag),
        "canonical execution image differs from the managed family reference"
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

async fn resolve_managed_image(
    intent: &AgentHubRunIntentV1,
    config: &HostConfig,
    deployment: &config::ValidatedDeployment,
) -> anyhow::Result<String> {
    ensure!(
        config
            .managed_images
            .contains_key(intent.image_family.as_str()),
        "trusted host config has no managed image for family '{}'",
        intent.image_family.as_str()
    );
    let bundle = deployment
        .environment_bundles
        .get(&intent.environment)
        .context("selected environment is not installed")?;
    let environment_file = bundle
        .files
        .iter()
        .find(|file| file.path == "environment.toml")
        .context("selected environment bundle has no environment manifest")?;
    let environment = config::EnvironmentConfig::from_toml(
        std::str::from_utf8(&environment_file.bytes)
            .context("selected environment manifest is not UTF-8")?,
        "captured environment.toml",
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        environment.name == intent.environment,
        "selected environment name does not match its captured manifest"
    );
    let resolved = config::resolve_managed_image_reference(
        &environment.image,
        Some(intent.image_family.as_str()),
        deployment.images.iter().map(|(family, manifest)| {
            (
                family.as_str(),
                manifest.image.get("name").and_then(|value| value.as_str()),
                manifest.image.get("tag").and_then(|value| value.as_str()),
            )
        }),
        &config.managed_images,
    )
    .map_err(anyhow::Error::msg)?;
    let docker =
        which::which("docker").context("Docker CLI is required to resolve managed images")?;
    let identity = agent_hub_executor::inspect_image_identity(
        &docker,
        &resolved.reference,
        &agent_hub_executor::Deadline::new(Some(15))?,
        &agent_hub_executor::Cancellation::never(),
    )
    .await?;
    if let Some(publication) = deployment
        .images
        .get(intent.image_family.as_str())
        .and_then(|manifest| manifest.image.get("tool_facts"))
    {
        ensure!(
            publication
                .get("image_reference")
                .and_then(|value| value.as_str())
                == Some(resolved.reference.as_str())
                && publication
                    .get("backend_metadata_revision")
                    .and_then(|value| value.as_str())
                    == Some(config.backend_metadata_revision.as_str())
                && publication
                    .get("image_identity")
                    .and_then(|value| value.as_str())
                    == Some(identity.as_str()),
            "published tool facts do not match the selected managed image generation"
        );
    }
    Ok(identity)
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

fn verify_protected_paths(
    config: &HostConfig,
    store: &StateStore,
    intent: &AgentHubRunIntentV1,
    artifact: &protocol::AttendedAuthorizationArtifact,
) -> anyhow::Result<()> {
    let execution = artifact.verify_v4().map_err(anyhow::Error::msg)?;
    let mut protected = vec![
        std::fs::canonicalize(&config.host_config_path)?,
        std::fs::canonicalize(&config.config_root)?,
        std::fs::canonicalize(&config.gateway.path)?,
        std::fs::canonicalize(&config.policy.path)?,
        std::env::current_exe()?.canonicalize()?,
        crate::local::canonicalize_existing_ancestor(store.root())?,
    ];
    for provider in &execution.execution.credential_providers {
        for value in [Some(provider.source.as_str()), provider.command.as_deref()]
            .into_iter()
            .flatten()
        {
            let path = Path::new(value);
            if path.is_absolute() && path.exists() {
                let canonical = std::fs::canonicalize(path)?;
                reject_protected_file_aliases(&canonical, false)?;
                protected.push(canonical);
            } else if provider.command.as_deref() == Some(value)
                && let Ok(resolved) = which::which(value)
            {
                let canonical = resolved.canonicalize()?;
                reject_protected_file_aliases(&canonical, true)?;
                protected.push(canonical);
            }
        }
    }
    crate::local::reject_protected_mounts(
        intent
            .workspaces
            .iter()
            .map(|item| Path::new(&item.path))
            .chain(
                intent
                    .context_roots
                    .iter()
                    .map(|item| Path::new(&item.path)),
            ),
        protected.iter().map(PathBuf::as_path),
    )
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

fn proposal_input(
    intent: &AgentHubRunIntentV1,
    config: &HostConfig,
    extra_protected: &[PathBuf],
) -> anyhow::Result<ProposalInput> {
    let mut protected_paths = vec![
        std::fs::canonicalize(&config.host_config_path)?,
        std::fs::canonicalize(&config.config_root)?,
        std::fs::canonicalize(&config.gateway.path)?,
        std::fs::canonicalize(&config.policy.path)?,
        std::env::current_exe()?.canonicalize()?,
    ];
    for path in extra_protected {
        protected_paths.push(crate::local::canonicalize_existing_ancestor(path)?);
    }
    Ok(ProposalInput {
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
        protected_paths,
    })
}

fn reject_protected_file_aliases(path: &Path, readonly_executable: bool) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(path)?;
    reject_protected_metadata_aliases(path, &metadata, readonly_executable)
}

fn reject_protected_metadata_aliases(
    path: &Path,
    metadata: &std::fs::Metadata,
    readonly_executable: bool,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    if metadata.is_file() && metadata.nlink() > 1 {
        let safe_system_executable = readonly_executable
            && metadata.uid() == 0
            && metadata.permissions().mode() & 0o022 == 0;
        ensure!(
            safe_system_executable,
            "protected file '{}' has multiple filesystem aliases",
            path.display()
        );
    }
    Ok(())
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
    let gateway = open_backend_file(Path::new(&config.gateway.path), "gateway executable")?;
    let policy = open_backend_file(Path::new(&config.policy.path), "gateway policy")?;
    let gateway_path = inherited_fd_path(&gateway.file)?;
    let policy_path = inherited_fd_path(&policy.file)?;
    let mut command = tokio::process::Command::new(gateway_path);
    command
        .kill_on_drop(true)
        .env_clear()
        .env("HOME", trusted_host_home()?)
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
    let gateway = open_backend_file(Path::new(&config.gateway.path), "gateway executable")?;
    let policy = open_backend_file(Path::new(&config.policy.path), "gateway policy")?;
    let mut command = tokio::process::Command::new(inherited_fd_path(&gateway.file)?);
    command
        .kill_on_drop(true)
        .env_clear()
        .env("HOME", trusted_host_home()?)
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
    output_limit: usize,
) {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => {
                let _ = sender.send(GatewayStreamEvent::Complete(stdout, output));
                return;
            }
            Ok(count) if output.len().saturating_add(count) > output_limit => {
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
    child: tokio::process::Child,
    input: &[u8],
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> anyhow::Result<std::process::Output> {
    collect_gateway_output_bounded(
        child,
        input,
        cancellation,
        protocol::AGENT_HUB_RUN_MAX_GATEWAY_OUTPUT_BYTES,
    )
    .await
}

async fn collect_gateway_output_bounded(
    mut child: tokio::process::Child,
    input: &[u8],
    mut cancellation: Option<tokio::sync::watch::Receiver<bool>>,
    output_limit: usize,
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
    let stdout_task = tokio::spawn(read_gateway_stream(
        stdout,
        true,
        "stdout",
        sender.clone(),
        output_limit,
    ));
    let stderr_task = tokio::spawn(read_gateway_stream(
        stderr,
        false,
        "stderr",
        sender,
        output_limit,
    ));

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

    #[test]
    fn persisted_request_must_match_trusted_invocation_and_digest() {
        assert!(validate_persisted_request_binding("a", "digest", "a", "digest").is_ok());
        let replay = validate_persisted_request_binding("b", "digest", "a", "digest").unwrap_err();
        assert!(replay.to_string().contains("trusted invocation"));
        assert!(validate_persisted_request_binding("a", "other", "a", "digest").is_err());
    }

    #[test]
    fn config_file_reader_enforces_byte_limit_before_allocation_can_grow_unbounded() {
        let path = test_path("catalog-byte-limit");
        std::fs::write(&path, vec![b'x'; 33]).unwrap();
        let mut exact = File::open(&path).unwrap();
        assert_eq!(
            read_limited_config_file(&mut exact, 33, "fixture")
                .unwrap()
                .len(),
            33
        );
        let mut oversized = File::open(&path).unwrap();
        assert!(read_limited_config_file(&mut oversized, 32, "fixture").is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn protected_backend_file_rejects_hardlink_alias() {
        let root = test_path("protected-hardlink");
        std::fs::create_dir_all(&root).unwrap();
        let protected = root.join("policy.json");
        let alias = root.join("workspace-alias.json");
        std::fs::write(&protected, b"{}").unwrap();
        std::fs::hard_link(&protected, &alias).unwrap();
        assert!(reject_protected_file_aliases(&protected, false).is_err());
        std::fs::remove_dir_all(root).unwrap();
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

    #[test]
    fn catalog_cache_identity_is_bounded_safe_and_uses_only_catalog_inputs() {
        let root = test_path("catalog-cache");
        let config_root = root.join("config");
        let nested = config_root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let config_file = nested.join("system.toml");
        std::fs::write(
            &config_file,
            b"[system]\noperator='test'\nbot_identity='test'\n",
        )
        .unwrap();
        let config_root = std::fs::canonicalize(config_root).unwrap();
        let config_file = std::fs::canonicalize(config_file).unwrap();
        let executable = std::env::current_exe().unwrap().canonicalize().unwrap();

        let host_config = root.join("host-run.json");
        std::fs::write(
            &host_config,
            serde_json::to_vec(&serde_json::json!({
                "schema": HOST_CONFIG_SCHEMA,
                "config_root": config_root,
                "gateway": {"path": executable},
                "policy": {"path": executable},
                "managed_images": {"job-code": "agent-hub/job-code:test"},
                "backend_metadata_revision": "revision-1"
            }))
            .unwrap(),
        )
        .unwrap();
        let host_config = std::fs::canonicalize(host_config).unwrap();

        let public = load_catalog_config(&host_config, false).unwrap();
        assert!(public.cache_identity.is_none());
        let internal = load_catalog_config(&host_config, true).unwrap();
        let cache = internal.cache_identity.unwrap();
        assert_eq!(cache.schema, "agent_hub_catalog_cache_identity_v2");
        assert!(cache.watch.len() <= MAX_CATALOG_CACHE_WATCH_ENTRIES);
        let paths = cache
            .watch
            .iter()
            .map(|stamp| stamp.path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
        for stamp in &cache.watch {
            assert!(Path::new(&stamp.path).is_absolute());
            assert!(stamp.path.len() <= 4096);
            assert!(!stamp.path.chars().any(char::is_control));
            assert!(matches!(stamp.file_type, "file" | "directory"));
        }
        for expected in [&host_config, &config_root, &config_file, &executable] {
            assert!(paths.contains(&expected.to_str().unwrap()));
        }
        let metadata = std::fs::metadata(&host_config).unwrap();
        assert!(catalog_watch_stamp(Path::new("relative"), &metadata, "file").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_inventory_rejects_wide_and_deep_directory_trees_during_walk() {
        let wide = test_path("catalog-wide");
        std::fs::create_dir_all(&wide).unwrap();
        for index in 0..=256 {
            std::fs::create_dir(wide.join(format!("entry-{index:03}"))).unwrap();
        }
        let error = config_root_inventory(&wide).unwrap_err();
        assert!(error.to_string().contains("entry limit"), "{error:#}");
        std::fs::remove_dir_all(wide).unwrap();

        let deep = test_path("catalog-deep");
        let mut current = deep.clone();
        for _ in 0..=17 {
            current.push("nested");
            std::fs::create_dir_all(&current).unwrap();
        }
        let error = config_root_inventory(&deep).unwrap_err();
        assert!(error.to_string().contains("depth limit"), "{error:#}");
        std::fs::remove_dir_all(deep).unwrap();
    }

    #[test]
    fn config_inventory_accepts_realistic_bounded_tree() {
        let root = test_path("catalog-realistic");
        for relative in [
            "system.toml",
            "images/job-code/image.toml",
            "environments/code-auto/environment.toml",
            "shared/agents/coder.md",
        ] {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"fixture").unwrap();
        }
        let (files, directories) = config_root_inventory(&root).unwrap();
        assert_eq!(files.len(), 4);
        assert_eq!(directories.len(), 7);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn catalog_cache_accepts_the_maximum_valid_inventory_and_rejects_each_excess() {
        let root = test_path("catalog-max-boundary");
        let config_root = root.join("config");
        std::fs::create_dir_all(&config_root).unwrap();
        let root_file = config_root.join("root.toml");
        std::fs::write(&root_file, b"fixture").unwrap();
        for index in 0..127 {
            let directory = config_root.join(format!("directory-{index:03}"));
            std::fs::create_dir(&directory).unwrap();
            let file = directory.join("artifact.toml");
            std::fs::write(&file, b"fixture").unwrap();
        }
        let config_root = std::fs::canonicalize(config_root).unwrap();
        let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
        let host_config = root.join("host-run.json");
        std::fs::write(
            &host_config,
            serde_json::to_vec(&serde_json::json!({
                "schema": HOST_CONFIG_SCHEMA,
                "config_root": config_root,
                "gateway": {"path": executable},
                "policy": {"path": executable},
                "managed_images": {"job-code": "agent-hub/job-code:test"},
                "backend_metadata_revision": "revision-max"
            }))
            .unwrap(),
        )
        .unwrap();
        let host_config = std::fs::canonicalize(host_config).unwrap();

        let snapshot = load_catalog_config(&host_config, true).unwrap();
        assert_eq!(snapshot.files.len(), 128);
        assert_eq!(
            snapshot.cache_identity.unwrap().watch.len(),
            MAX_CATALOG_CACHE_WATCH_ENTRIES
        );

        let excess_file_root = root.join("excess-file");
        std::fs::create_dir(&excess_file_root).unwrap();
        for index in 0..129 {
            std::fs::write(excess_file_root.join(format!("file-{index:03}")), b"x").unwrap();
        }
        let error = config_root_inventory(&excess_file_root).unwrap_err();
        assert!(error.to_string().contains("file limit"), "{error:#}");

        let excess_directory_root = root.join("excess-directory");
        std::fs::create_dir(&excess_directory_root).unwrap();
        for index in 0..128 {
            std::fs::create_dir(excess_directory_root.join(format!("directory-{index:03}")))
                .unwrap();
        }
        let error = config_root_inventory(&excess_directory_root).unwrap_err();
        assert!(error.to_string().contains("directory limit"), "{error:#}");
        std::fs::remove_dir_all(root).unwrap();
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
