use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, ensure};
use chrono::{Duration, Utc};
use protocol::{
    AgentHubRunDecision, AgentHubRunDecisionV1, AgentHubRunIntentV1, AgentHubRunReceiptV1,
    AgentHubRunResolvedV1,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::attended::{
    AuthorizationRecord, AuthorizationSelectors, LaunchState, ProposalInput, RelayEndpointOverride,
    RunRecord,
};
use crate::local::{RequestedContextRoot, RequestedWorkspace};
use crate::state::StateStore;

const HOST_CONFIG_SCHEMA: &str = "agent_hub_run_host_v1";
const MAX_APPROVAL_TIMEOUT_SECS: u64 = 600;
pub(crate) const DECISION_RECEIPT_DOMAIN: &str = "agent_hub_run";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfig {
    schema: String,
    config_root: String,
    gateway_path: String,
    gateway_config: String,
    image_digests: std::collections::BTreeMap<String, String>,
}

#[derive(Clone)]
pub(crate) struct HostRunOptions {
    pub host_config: PathBuf,
    pub trusted_host_config: PathBuf,
    pub approvals_server: String,
    pub approvals_token: Option<protocol::Secret>,
    pub approval_timeout_secs: u64,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostRunOutput {
    pub receipt: AgentHubRunReceiptV1,
    pub run: Option<RunRecord>,
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
    let resolved = resolve_intent(intent, &config)?;
    let image = trusted_image_identity(&resolved, &config)?;
    crate::attended::prepare_canonical_execution(proposal_input(&resolved, &config), image).await
}

/// Prepare, obtain a gateway decision, persist it, and execute. There is no
/// local policy fallback.
pub(crate) async fn run(
    store: &StateStore,
    intent: AgentHubRunIntentV1,
    options: HostRunOptions,
) -> anyhow::Result<HostRunOutput> {
    let (_, trusted_identity) = load_host_config(&options.trusted_host_config)?;
    let (config, selected_identity) = load_host_config(&options.host_config)?;
    ensure!(
        selected_identity == trusted_identity,
        "automatic host rules require the trusted host configuration identity"
    );
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
    let retry = artifact.verify_v3().map_err(anyhow::Error::msg)?.retry;
    let attempts = i64::from(retry.max_attempts);
    let requested_at = Utc::now();
    let request = AgentHubRunResolvedV1::new(
        uuid::Uuid::new_v4().to_string(),
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

    let decision = request_gateway(&request, &config, &options).await?;
    decision
        .validate(&request, Utc::now())
        .map_err(anyhow::Error::msg)?;
    let receipt =
        AgentHubRunReceiptV1::issue(&request, decision, Utc::now()).map_err(anyhow::Error::msg)?;
    receipt
        .verify(&request, Utc::now())
        .map_err(anyhow::Error::msg)?;
    let invocation_root = PathBuf::from("agent-hub-run/invocations").join(&request.invocation_id);
    store.create_json(&invocation_root.join("receipt.json"), &receipt)?;
    if receipt.decision() == AgentHubRunDecision::Deny {
        return Ok(HostRunOutput { receipt, run: None });
    }

    revalidate_workspace_paths(&resolved_intent)?;
    let canonical_relative =
        PathBuf::from("agent-hub-run/canonical").join(format!("{}.json", artifact.digest));
    store.create_or_verify_json(&canonical_relative, &artifact)?;
    let authorization_path = store.path(&canonical_relative)?;
    let authorized_relative =
        PathBuf::from("authorizations/records").join(format!("{}.json", request.invocation_id));
    store.create_json(
        &authorized_relative,
        &AuthorizationRecord {
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
        },
    )?;

    let relay = RelayEndpointOverride {
        server_url: Some(options.approvals_server),
        token: options.approvals_token,
    };
    let final_record = loop {
        receipt
            .verify(&request, Utc::now())
            .map_err(anyhow::Error::msg)?;
        revalidate_workspace_paths(&resolved_intent)?;
        let record = crate::attended::run(
            store,
            &authorization_path,
            Some(&request.invocation_id),
            selectors(&resolved_intent, &artifact.digest),
            relay.clone(),
        )
        .await?;
        if !record.retry_eligible {
            break record;
        }
    };
    Ok(HostRunOutput {
        receipt,
        run: Some(final_record),
    })
}

fn load_host_config(path: &Path) -> anyhow::Result<(HostConfig, PathBuf)> {
    let identity = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize host config {}", path.display()))?;
    let config: HostConfig = serde_json::from_reader(std::fs::File::open(&identity)?)?;
    ensure!(
        config.schema == HOST_CONFIG_SCHEMA,
        "unsupported host run config schema"
    );
    ensure!(
        Path::new(&config.config_root).is_absolute()
            && Path::new(&config.gateway_path).is_absolute()
            && Path::new(&config.gateway_config).is_absolute(),
        "host run config paths must be absolute"
    );
    ensure!(
        std::fs::canonicalize(&config.config_root)? == Path::new(&config.config_root),
        "host config root must be canonical"
    );
    ensure!(
        std::fs::canonicalize(&config.gateway_path)? == Path::new(&config.gateway_path),
        "trusted gateway path must be canonical"
    );
    ensure!(
        std::fs::canonicalize(&config.gateway_config)? == Path::new(&config.gateway_config),
        "gateway config path must be canonical"
    );
    Ok((config, identity))
}

fn resolve_intent(
    mut intent: AgentHubRunIntentV1,
    _config: &HostConfig,
) -> anyhow::Result<AgentHubRunIntentV1> {
    intent.validate_shape().map_err(anyhow::Error::msg)?;
    for item in &mut intent.workspaces {
        item.path = canonical_directory(&item.path)?;
    }
    for item in &mut intent.context_roots {
        item.path = canonical_directory(&item.path)?;
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
    let mut command = tokio::process::Command::new(&config.gateway_path);
    command
        .env_clear()
        .arg("agent-hub-run-resolved")
        .arg("--server")
        .arg(&options.approvals_server)
        .arg("--config")
        .arg(&config.gateway_config)
        .arg("--timeout")
        .arg(options.approval_timeout_secs.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(token) = &options.approvals_token {
        command.env("AGENT_HUB_TOKEN", token.expose());
    }
    let mut child = command
        .spawn()
        .context("start trusted agent_hub_run permission gateway")?;
    child
        .stdin
        .take()
        .context("permission gateway stdin unavailable")?
        .write_all(&serde_json::to_vec(&input)?)
        .await?;
    let output = child.wait_with_output().await?;
    ensure!(
        output.status.success(),
        "agent_hub_run permission gateway failed closed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    ensure!(
        output.stdout.len() <= 64 * 1024,
        "agent_hub_run gateway decision exceeds limit"
    );
    serde_json::from_slice(&output.stdout).context("parse agent_hub_run gateway decision")
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .context("agent_hub_run path is not UTF-8")
}
