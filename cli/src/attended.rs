use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_hub_executor::{
    ArtifactPaths, Cancellation, CleanupStatus, Deadline, DockerCli, DockerExecutionPlan,
    DockerMount, DockerOwnership, ExecutionRequest, ExecutionStatus, NetworkMode, OutputLimits,
    credential_provider::{CredentialProvider, resolve_all_for_job},
    egress::{
        PreparedRestrictedEgress, ProxyRuntimeSpec, ResolverRuntimeSpec, RestrictedEgressRequest,
        RestrictedPolicy,
    },
    execute_output_cancellable, execute_process, inspect_image_identity, verify_image_identity,
};
use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use protocol::{
    ATTENDED_AUTHORIZATION_REQUEST_VERSION_V3, ApprovedExecutionSpec,
    AttendedApprovalBundleIdentity, AttendedAuthorizationArtifact, AttendedAuthorizationRequestV3,
    AttendedExecutionSpec, AttendedLifecycleContract, AttendedOutputContract, AttendedProposal,
    AttendedWorkspaceExportContract, AuthorizedCredentialProvider, AuthorizedOverlay,
    CanonicalRuntimeSpec, ExecutionMode, NetworkResourceKind, NetworkResourceRequest,
    PlannedEnvironmentTier, PromptRef, ResolvedAuthorizationBehavior, ToolApprovalMode,
    WorkspaceAccess, canonical_environment_for_execution, canonical_permissions_for_environment,
    default_approval_rules_for_tier,
};
use serde::{Deserialize, Serialize};

use crate::attended_relay::{
    AttendedRelay, AttendedRelayConfig, RelaySocketLocation, SupervisionOutcome,
    host_relay_socket_location, recover_attended_relay,
};
use crate::local::{
    ApprovedRoots, RequestedContextRoot, RequestedWorkspace, ResolvedMounts, resolve_mounts,
};
use crate::state::StateStore;

pub const RESULT_SCHEMA_V3: &str = "attended_result_v3";
pub const RESULT_SCHEMA_V4: &str = "attended_result_v4";
const LOCAL_RESULT_SCHEMA_V2: &str = "local_result_v2";
const CLEANUP_RECEIPT_SCHEMA_V1: &str = "attended_cleanup_receipt_v1";
const PRIMARY_CLEANUP_TIMEOUT: Duration = Duration::from_secs(70);
const ATTENDED_CLEANUP_TIMEOUT_SECS: u64 = 150;
const CANCELLATION_TERMINAL_TIMEOUT: Duration = Duration::from_secs(180);
const HOST_ONLY_APPROVAL_ENVIRONMENT: [&str; 4] = [
    "AGENT_HUB_SERVER",
    "AGENT_HUB_TOKEN",
    "AGENT_HUB_APPROVALS_SERVER",
    "AGENT_HUB_APPROVALS_TOKEN",
];

fn attended_fault(boundary: &str) -> anyhow::Result<()> {
    #[cfg(any(test, feature = "test-fault-hooks"))]
    {
        if std::env::var("AGENT_HUB_TEST_ATTENDED_FAULT").as_deref() == Ok(boundary) {
            eprintln!("injected attended fault at {boundary}");
            std::process::exit(86);
        }
    }
    #[cfg(not(any(test, feature = "test-fault-hooks")))]
    {
        let _ = boundary;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ProposalInput {
    pub config_root: PathBuf,
    pub environment: String,
    pub image_family: String,
    pub network_class: String,
    pub network_resources: Vec<NetworkResourceKind>,
    pub tool_approval: String,
    pub overlays: Vec<String>,
    pub timeout_secs: u64,
    pub prompt: String,
    pub workspaces: Vec<RequestedWorkspace>,
    pub context_roots: Vec<RequestedContextRoot>,
    pub primary_workspace: String,
    pub workspace_export_root: Option<PathBuf>,
}

pub struct AuthorizationSelectors<'a> {
    pub digest: &'a str,
    pub environment: &'a str,
    pub image_family: &'a str,
    pub network_class: &'a str,
    pub network_resources: &'a [NetworkResourceKind],
    pub tool_approval: &'a str,
    pub overlays: &'a [String],
    pub timeout_secs: u64,
}

#[derive(Clone)]
pub struct RelayEndpoint {
    pub server_url: String,
    pub token: Option<protocol::Secret>,
}

#[derive(Clone)]
pub struct RelayEndpointOverride {
    pub server_url: Option<String>,
    pub token: Option<protocol::Secret>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RelayEndpointIdentity {
    url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelayEpochState {
    run_epoch: uuid::Uuid,
    binding: protocol::AttendedGatewayBindingV1,
    plugin_nonce: String,
    deadline: DateTime<Utc>,
    endpoint: RelayEndpointIdentity,
    socket: RelaySocketLocation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Preparing,
    LaunchPending,
    Consumed,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    CleanupFailed,
    Cleaned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchState {
    Proposed,
    Authorized,
    Preparing,
    LaunchPending,
    Consumed,
    Running,
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    Prelaunch,
    ProviderServiceUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttemptLineage {
    pub attempt_id: String,
    pub run_id: String,
    pub state: LaunchState,
    pub retry_class: Option<RetryClass>,
    pub retry_eligible: bool,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingAttempt {
    authorization_digest: String,
    authorization_id: String,
    record: RunRecord,
    lineage: AttemptLineage,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    #[default]
    NotApplicable,
    NoDenials,
    Denied,
    Cancelled,
    ReadinessFailed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOutcome {
    #[default]
    NotStarted,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    OutputLimitExceeded,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutcome {
    #[default]
    NotProduced,
    Succeeded,
    Incomplete,
    Missing,
    Malformed,
    Contradictory,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    #[default]
    Pending,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultOutcome {
    #[default]
    Unknown,
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CleanupOutcome {
    #[default]
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizationRecord {
    pub authorization_id: String,
    pub artifact: AttendedAuthorizationArtifact,
    pub authorization_path: String,
    pub authorized_at: DateTime<Utc>,
    pub receipt_id: String,
    pub domain: String,
    pub state: LaunchState,
    pub attempts: Vec<AttemptLineage>,
    pub active_attempt_id: Option<String>,
    #[serde(default)]
    pub decision_receipt: Option<protocol::AgentHubRunReceiptV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    #[serde(default = "result_schema_v3")]
    pub schema: String,
    pub id: String,
    pub run_id: String,
    pub attempt_id: String,
    pub attempt_number: u32,
    pub parent_attempt_id: Option<String>,
    pub attempt_ancestry: Vec<String>,
    pub authorization_id: String,
    pub receipt_id: String,
    pub authorization_digest: String,
    pub owner_nonce: String,
    pub launch_state: LaunchState,
    pub status: RunStatus,
    pub container_name: String,
    pub ownership: BTreeMap<String, String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    #[serde(default)]
    pub execution_status: Option<RunStatus>,
    #[serde(default)]
    pub process_status: Option<RunStatus>,
    #[serde(default)]
    pub permission_outcome: PermissionOutcome,
    #[serde(default)]
    pub process_outcome: ProcessOutcome,
    #[serde(default)]
    pub agent_outcome: AgentOutcome,
    #[serde(default)]
    pub execution_outcome: ExecutionOutcome,
    #[serde(default, skip_serializing_if = "result_outcome_unknown")]
    pub result_outcome: ResultOutcome,
    #[serde(default)]
    pub cleanup_outcome: CleanupOutcome,
    #[serde(default)]
    pub agent_final: Option<protocol::AgentResultV1>,
    pub retry_class: Option<RetryClass>,
    pub retry_eligible: bool,
    #[serde(default)]
    pub workspace_export: Option<WorkspaceExportStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceExportStatus {
    pub root: String,
    pub run_path: String,
    pub workspace_alias: String,
    pub workspace_identity: protocol::HostDirectoryIdentity,
    pub state: String,
    pub error: Option<String>,
    pub authoritative: bool,
    #[serde(default)]
    files: BTreeMap<String, WorkspaceExportFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RunOwnerMarker {
    run_id: String,
    attempt_id: String,
    authorization_id: String,
    receipt_id: String,
    authorization_digest: String,
    nonce: String,
    uid: u32,
    gid: u32,
    mode: String,
    ancestry: Vec<String>,
}

fn result_schema_v3() -> String {
    RESULT_SCHEMA_V3.to_string()
}

fn result_schema_current() -> String {
    RESULT_SCHEMA_V4.to_string()
}

fn result_outcome_unknown(outcome: &ResultOutcome) -> bool {
    *outcome == ResultOutcome::Unknown
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CleanupCheck {
    status: String,
    error: Option<String>,
}

impl CleanupCheck {
    fn not_applicable() -> Self {
        Self {
            status: "not_applicable".into(),
            error: None,
        }
    }

    fn from_result(result: &anyhow::Result<()>) -> Self {
        match result {
            Ok(()) => Self {
                status: "verified_absent".into(),
                error: None,
            },
            Err(error) => Self {
                status: "failed".into(),
                error: Some(format!("{error:#}")),
            },
        }
    }

    fn complete(&self) -> bool {
        matches!(self.status.as_str(), "verified_absent" | "not_applicable")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttendedCleanupEvidence {
    process: CleanupCheck,
    container: CleanupCheck,
    relay: CleanupCheck,
    egress: CleanupCheck,
    credentials: CleanupCheck,
    runtime_data: CleanupCheck,
    effects: CleanupCheck,
}

impl Default for AttendedCleanupEvidence {
    fn default() -> Self {
        Self {
            process: CleanupCheck::not_applicable(),
            container: CleanupCheck::not_applicable(),
            relay: CleanupCheck::not_applicable(),
            egress: CleanupCheck::not_applicable(),
            credentials: CleanupCheck::not_applicable(),
            runtime_data: CleanupCheck::not_applicable(),
            effects: CleanupCheck::not_applicable(),
        }
    }
}

impl AttendedCleanupEvidence {
    fn complete(&self) -> bool {
        self.process.complete()
            && self.container.complete()
            && self.relay.complete()
            && self.egress.complete()
            && self.credentials.complete()
            && self.runtime_data.complete()
            && self.effects.complete()
    }

    fn verified_absent() -> Self {
        let absent = || CleanupCheck {
            status: "verified_absent".into(),
            error: None,
        };
        Self {
            process: absent(),
            container: absent(),
            relay: absent(),
            egress: absent(),
            credentials: absent(),
            runtime_data: absent(),
            effects: absent(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct BundleNetworkPolicy {
    name: String,
}

/// Prepare canonical V3 execution bytes from a trusted host image identity.
pub(crate) async fn prepare_canonical_execution(
    input: ProposalInput,
    image_identity: String,
) -> anyhow::Result<AttendedAuthorizationArtifact> {
    build_proposal_resolved(input, image_identity).await
}

async fn build_proposal_resolved(
    input: ProposalInput,
    image_id: String,
) -> anyhow::Result<AttendedAuthorizationArtifact> {
    ensure!(
        !input.prompt.is_empty(),
        "attended prompt must not be empty"
    );
    ensure_unique_resources(&input.network_resources)?;
    let prompt = prompt_with_result_contract(&input.prompt);
    let config_root = std::fs::canonicalize(&input.config_root)
        .with_context(|| format!("canonicalize config root {}", input.config_root.display()))?;
    let deployment =
        config::validate_authoritative_config_root(&config_root).map_err(anyhow::Error::msg)?;
    let bundle = deployment
        .environment_bundles
        .get(&input.environment)
        .context("selected environment is not installed")?;
    let environment = bundle_environment(bundle)?;
    ensure!(
        environment.name == input.environment,
        "environment selector does not match the resolved environment"
    );
    ensure!(
        resolve_image_family(&deployment, &environment.image)? == input.image_family,
        "image family selector does not match the resolved environment"
    );
    ensure!(
        network_class_within_ceiling(&input.network_class, &environment.network_policy)?,
        "network class selector exceeds the resolved environment ceiling"
    );
    let tool_approval = requested_tool_approval_mode(&input.tool_approval)?;
    ensure!(
        tool_approval_within_ceiling(
            tool_approval,
            tool_approval_mode(&environment.approval_mode)?
        ),
        "tool approval selector exceeds the resolved environment ceiling"
    );
    ensure!(
        input.timeout_secs <= environment.max_timeout_secs,
        "timeout selector exceeds the resolved environment ceiling"
    );
    let mut overlays = input.overlays;
    overlays.sort();
    if let Some(duplicate) = overlays
        .windows(2)
        .find(|items| items[0] == items[1])
        .map(|items| &items[0])
    {
        anyhow::bail!("overlay selector '{duplicate}' was provided more than once");
    }
    if let Some(disallowed) = overlays
        .iter()
        .find(|overlay| !environment.allowed_overlays.contains(overlay))
    {
        anyhow::bail!(
            "overlay selector '{disallowed}' is not allowed by environment '{}'; allowed overlays: {}",
            input.environment,
            if environment.allowed_overlays.is_empty() {
                "none".to_string()
            } else {
                environment.allowed_overlays.join(", ")
            }
        );
    }
    let approved_roots = approved_roots(&deployment.local_agent)?;
    let mut mounts = resolve_mounts(
        &approved_roots,
        &input.workspaces,
        &input.context_roots,
        &input.primary_workspace,
    )?;
    let workspace_export = resolve_workspace_export_contract(
        input.workspace_export_root.as_deref(),
        &mounts.workspaces,
    )?;
    if let Some(export) = &workspace_export {
        for repository in &mut mounts.repositories {
            omit_workspace_export_paths(
                &mut repository.starting,
                &repository.workspace_alias,
                export,
            );
        }
    }
    let network = bundle_network_policy(bundle)?;
    ensure!(
        network.name == environment.network_policy,
        "materialized network policy does not match the environment"
    );
    let network_resources =
        resolve_network_resources(&input.network_class, input.network_resources)?;
    let runtime = canonical_runtime(
        &environment,
        &mounts,
        &network_resources,
        input.timeout_secs,
    );
    let runner = deployment
        .runner_profiles
        .resolve_agent_profile(None)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        runner
            .container_image
            .as_deref()
            .is_none_or(|image| image == environment.image),
        "runner and environment container images differ"
    );
    ensure!(
        runner.file_mounts.is_empty() && runner.mcp_bridges.is_empty(),
        "attended execution does not support runner host mounts or MCP bridges"
    );
    let overlay_descriptors = authorized_overlays(
        &deployment,
        &overlays,
        &mounts.repositories,
        &network_resources,
    )?;
    validate_credential_descriptors(&deployment, &runner, &overlays)?;
    let credential_providers = authorized_credential_providers(&deployment, &runner, &overlays)?;
    let prompt_digest =
        protocol::sha256_hex_with_domain(b"agent-hub:attended-prompt:v1\0", prompt.as_bytes());
    let policy_environment = network_policy_environment(&deployment, &input.network_class)?;
    let behavior = ResolvedAuthorizationBehavior {
        execution_kind: "agent".to_string(),
        runner_profile: serde_json::to_value(&runner)?,
        command: runner.command,
        args: runner.args,
        env: policy_environment,
        prompt: Some(PromptRef {
            system: None,
            user: prompt.clone(),
        }),
        primary_repo: None,
        primary_repo_branch: None,
        execution_context: serde_json::json!({"mode": "attended"}),
        triage: false,
        allow_remote_credentials: !credential_providers.is_empty(),
    };
    let approval_bundle = attended_approval_bundle_identity(bundle, tool_approval)?;
    let approval_bundle_digest = approval_bundle
        .as_ref()
        .map(|identity| identity.digest.clone())
        .unwrap_or(
            protocol::attended_approval_bundle_digest(&serde_json::json!({
                "environment_bundle_digest": bundle.digest,
                "mode": "auto"
            }))
            .map_err(anyhow::Error::msg)?,
        );
    AttendedAuthorizationRequestV3 {
        version: ATTENDED_AUTHORIZATION_REQUEST_VERSION_V3.to_string(),
        proposal: AttendedProposal {
            environment: input.environment,
            image_family: input.image_family,
            image_tag: environment.image.clone(),
            image_id,
            network_class: input.network_class,
            network_resources,
            tool_approval,
            overlays,
            timeout_secs: input.timeout_secs,
            prompt_digest,
            prompt,
        },
        execution: AttendedExecutionSpec {
            config_root: path_string(&config_root)?,
            config_digest: attended_config_digest(&deployment)?,
            config_provenance: attended_config_provenance(&deployment)?,
            environment_bundle_digest: bundle.digest.clone(),
            approved: ApprovedExecutionSpec {
                runtime,
                behavior,
                pids_limit: protocol::APPROVED_EXECUTION_PIDS_LIMIT,
            },
            credential_providers,
            overlays: overlay_descriptors,
            workspaces: mounts.workspaces,
            context_roots: mounts.context_roots,
            repositories: mounts.repositories,
            primary_workspace: mounts.primary_workspace,
        },
        roots: mounts.roots,
        workspace_root_identities: mounts.workspace_root_identities,
        context_root_identities: mounts.context_root_identities,
        git_views: mounts.git_views,
        approval_bundle_digest,
        approval_bundle,
        output: AttendedOutputContract::default(),
        workspace_export,
        lifecycle: AttendedLifecycleContract::default(),
        retry: protocol::AttendedRetryConstraints::for_mode(tool_approval),
    }
    .canonical_artifact()
    .map_err(anyhow::Error::msg)
}

fn prompt_with_result_contract(prompt: &str) -> String {
    format!(
        "{prompt}\n\nYour terminal response MUST be exactly one JSON object matching this schema. Use OpenCode StructuredOutput when available; otherwise emit the object as the final response. Do not wrap it in Markdown. A complete result cannot contain blockers or failed acceptance results.\n{{\"schema\":\"agent_result_v1\",\"complete\":true|false,\"summary\":\"non-empty summary\",\"acceptance_results\":[{{\"criterion\":\"criterion\",\"passed\":true|false,\"evidence\":\"non-empty evidence\"}}],\"blockers\":[\"blocker\"],\"artifact_references\":[{{\"path\":\"path\",\"description\":\"non-empty description\"}}]}}\n"
    )
}

fn resolve_workspace_export_contract(
    requested: Option<&Path>,
    workspaces: &[protocol::WorkspaceSpec],
) -> anyhow::Result<Option<AttendedWorkspaceExportContract>> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    ensure!(
        requested.is_absolute(),
        "workspace export root must be absolute"
    );
    ensure!(
        requested.components().all(|component| matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Normal(_)
        )),
        "workspace export root must not contain path escapes or noncanonical components"
    );
    let mut matches = workspaces.iter().filter(|workspace| {
        workspace.access == WorkspaceAccess::ReadWrite
            && requested == Path::new(&workspace.host_path).join(".agent-hub/runs")
    });
    let workspace = matches.next().context(
        "workspace export root must be exactly .agent-hub/runs under a reviewed read-write workspace",
    )?;
    ensure!(
        matches.next().is_none(),
        "workspace export root overlaps multiple reviewed workspaces"
    );
    let workspace_alias = workspace.alias.clone();
    let workspace_path = Path::new(&workspace.host_path);
    for path in [workspace_path.join(".agent-hub"), requested.to_path_buf()] {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "workspace export path '{}' must be a real directory",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        }
    }
    Ok(Some(AttendedWorkspaceExportContract {
        root: path_string(requested)?,
        workspace_alias,
        workspace_identity: crate::git_view::directory_identity(workspace_path)?,
    }))
}

fn omit_workspace_export_paths(
    state: &mut protocol::RepositoryState,
    workspace_alias: &str,
    export: &AttendedWorkspaceExportContract,
) {
    if workspace_alias != export.workspace_alias {
        return;
    }
    state
        .dirty_paths
        .retain(|path| path != ".agent-hub/runs" && !path.starts_with(".agent-hub/runs/"));
}

fn verify_authorization_selectors(
    request: &AttendedAuthorizationRequestV3,
    artifact: &AttendedAuthorizationArtifact,
    selectors: &AuthorizationSelectors<'_>,
) -> anyhow::Result<()> {
    ensure!(
        artifact.digest == selectors.digest,
        "authorization digest selector mismatch"
    );
    ensure!(
        request.proposal.environment == selectors.environment,
        "environment selector mismatch"
    );
    ensure!(
        request.proposal.image_family == selectors.image_family,
        "image family selector mismatch"
    );
    ensure!(
        request.proposal.network_class == selectors.network_class,
        "network class selector mismatch"
    );
    ensure!(
        request.proposal.network_resources == selectors.network_resources,
        "network resource selectors mismatch"
    );
    ensure!(
        tool_approval_name(request.proposal.tool_approval) == selectors.tool_approval,
        "tool approval selector mismatch"
    );
    ensure!(
        request.proposal.overlays == selectors.overlays,
        "overlay selectors mismatch"
    );
    ensure!(
        request.proposal.timeout_secs == selectors.timeout_secs,
        "timeout selector mismatch"
    );
    Ok(())
}

pub async fn run(
    store: &StateStore,
    artifact_path: &Path,
    authorization_key: Option<&str>,
    selectors: AuthorizationSelectors<'_>,
    relay_override: RelayEndpointOverride,
) -> anyhow::Result<RunRecord> {
    let (supplied, supplied_path) = read_authorization_file(artifact_path)?;
    let request = supplied.verify_v3().map_err(anyhow::Error::msg)?;
    let digest = supplied.digest.clone();
    verify_authorization_selectors(&request, &supplied, &selectors)?;
    let authorization_key = authorization_key.unwrap_or(&digest);
    let authorized_path = authorization_path("records", authorization_key);
    let authorized: AuthorizationRecord = store
        .read_json(&authorized_path)
        .context("attended authorization has not been approved")?;
    ensure!(
        authorized.artifact == supplied && authorized.authorization_path == supplied_path,
        "authorized artifact does not match supplied artifact"
    );
    if let Some(previous) = authorized.attempts.last() {
        if authorized.domain == "agent_hub_run" && !previous.retry_eligible {
            let record: RunRecord = store.read_json(&run_record_path(&previous.run_id))?;
            ensure!(
                record.launch_state == LaunchState::Terminal,
                "terminal invocation lineage points to a non-terminal run"
            );
            return Ok(record);
        }
        ensure!(
            previous.retry_eligible,
            "attended authorization was already consumed"
        );
    }
    verify_execution_inputs(&request)?;
    let deployment =
        config::validate_authoritative_config_root(Path::new(&request.execution.config_root))
            .map_err(anyhow::Error::msg)?;
    // Automatic executions never use the approval relay. Do not execute a
    // configured host credential provider for an unused endpoint.
    let relay_endpoint = if request.proposal.tool_approval == ToolApprovalMode::Dippy {
        Some(resolve_host_relay_endpoint(&deployment, relay_override).await?)
    } else {
        None
    };
    let _lock = store.lock(&lock_path(authorization_key))?;
    let mut authorized: AuthorizationRecord = store.read_json(&authorized_path)?;
    ensure!(
        authorized.artifact == supplied && authorized.authorization_path == supplied_path,
        "authorized artifact does not match supplied artifact"
    );
    ensure!(
        matches!(authorized.domain.as_str(), "attended" | "agent_hub_run"),
        "authorization receipt domain mismatch"
    );
    if authorized.domain == "agent_hub_run" {
        let receipt = authorized
            .decision_receipt
            .as_ref()
            .context("agent_hub_run authorization has no decision receipt")?;
        receipt
            .verify_binding(&authorized.authorization_id, &digest, Utc::now())
            .map_err(anyhow::Error::msg)?;
        ensure!(
            receipt.decision() == protocol::AgentHubRunDecision::Allow
                && receipt.receipt_id() == authorized.receipt_id,
            "agent_hub_run authorization is not bound to an allow receipt"
        );
    }
    let runner: config::ResolvedRunnerProfile =
        serde_json::from_value(request.execution.behavior.runner_profile.clone())?;
    ensure!(
        request.execution.behavior.allow_remote_credentials
            == !request.execution.credential_providers.is_empty(),
        "attended credential envelope does not match the authorized runner"
    );
    ensure!(
        runner.mcp_bridges.is_empty(),
        "attended MCP bridge material is unavailable from the executor substrate"
    );
    verify_tool_approval(request.proposal.tool_approval, relay_endpoint.as_ref())?;
    let docker_path =
        which::which("docker").context("Docker CLI is required for attended execution")?;
    reconcile_pending_attempts(store, &authorized_path, &mut authorized)?;
    if let Some(active) = authorized.active_attempt_id.clone() {
        let active_run_id = active_attempt_run_id(&authorized, &active)?.to_string();
        let mut active_record: RunRecord = store.read_json(&run_record_path(&active_run_id))?;
        ensure!(
            active_record.attempt_id == active && active_record.run_id == active_run_id,
            "authorization active attempt resolves to the wrong execution record"
        );
        let active_dir = store.path(&PathBuf::from("runs").join(&active_record.id))?;
        let expected = owner_marker_for_record(&active_record);
        recover_owner_staging_directory(&active_dir, &expected)?;
        let lineage_index = authorized
            .attempts
            .iter()
            .position(|attempt| attempt.attempt_id == active)
            .expect("active lineage was checked above");
        let durable_state = furthest_durable_launch_state(
            &active_record.launch_state,
            &authorized.state,
            &authorized.attempts[lineage_index].state,
        );
        if durable_state == LaunchState::Terminal {
            let previous = &mut authorized.attempts[lineage_index];
            previous.state = LaunchState::Terminal;
            previous.retry_class = active_record.retry_class.clone();
            previous.retry_eligible = active_record.retry_eligible;
            previous.finished_at = active_record.finished_at;
            authorized.active_attempt_id = None;
            authorized.state = if active_record.retry_eligible {
                LaunchState::Authorized
            } else {
                LaunchState::Terminal
            };
            store.replace_json(&authorized_path, &authorized)?;
            if authorized.domain == "agent_hub_run" && !active_record.retry_eligible {
                return Ok(active_record);
            }
            ensure!(
                active_record.retry_eligible,
                "attended authorization was already consumed"
            );
        } else {
            if active_dir.exists() {
                verify_run_owner_marker(&active_dir, &expected)?;
                recover_owned_run_resources(
                    &docker_path,
                    &active_dir,
                    &active_record.id,
                    &active_record.container_name,
                    &active_record.ownership,
                    relay_endpoint.as_ref(),
                    &expected,
                )
                .await
                .context("reconcile interrupted attended attempt")?;
            }
            let previous = &mut authorized.attempts[lineage_index];
            previous.state = LaunchState::Terminal;
            // A durable copy at launch_pending or later is the fail-closed
            // consumption boundary, even if another copy was not yet replaced.
            let prelaunch = durable_state == LaunchState::Preparing
                && !store.path(&cancel_path(&active_record.id))?.exists();
            previous.retry_class = prelaunch.then_some(RetryClass::Prelaunch);
            previous.retry_eligible = prelaunch;
            previous.finished_at = Some(Utc::now());
            authorized.active_attempt_id = None;
            authorized.state = if prelaunch {
                LaunchState::Authorized
            } else {
                LaunchState::Terminal
            };
            active_record.launch_state = LaunchState::Terminal;
            active_record.status = RunStatus::Failed;
            active_record.retry_class = previous.retry_class.clone();
            active_record.retry_eligible = prelaunch;
            active_record.finished_at = Some(Utc::now());
            active_record.error = Some(if prelaunch {
                "interrupted prelaunch attempt was fully reconciled".into()
            } else {
                "interrupted attempt reached launch_pending; authorization consumed fail-closed"
                    .into()
            });
            store.replace_json(&run_record_path(&active_record.run_id), &active_record)?;
            store.replace_json(&authorized_path, &authorized)?;
            ensure!(prelaunch, "attended authorization was consumed fail-closed");
        }
    }
    let retry = &request.retry;
    if let Some(previous) = authorized.attempts.last() {
        if authorized.domain == "agent_hub_run" && !previous.retry_eligible {
            let record: RunRecord = store.read_json(&run_record_path(&previous.run_id))?;
            ensure!(
                record.launch_state == LaunchState::Terminal,
                "terminal invocation lineage points to a non-terminal run"
            );
            return Ok(record);
        }
        ensure!(
            previous.retry_eligible,
            "attended authorization was already consumed"
        );
    }
    ensure!(
        authorized.attempts.len() < retry.max_attempts as usize,
        "attended authorization exhausted its reviewed attempt limit"
    );
    ensure!(
        authorized
            .attempts
            .iter()
            .filter(|attempt| attempt.retry_class == Some(RetryClass::Prelaunch))
            .count()
            <= retry.max_prelaunch_retries as usize,
        "attended authorization exhausted its reviewed prelaunch retry limit"
    );
    ensure!(
        authorized
            .attempts
            .iter()
            .filter(|attempt| attempt.retry_class == Some(RetryClass::ProviderServiceUnavailable))
            .count()
            <= retry.max_provider_unavailable_retries as usize,
        "attended authorization exhausted its reviewed provider retry limit"
    );
    let run_id = uuid::Uuid::new_v4().to_string();
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let owner_nonce = uuid::Uuid::new_v4().to_string();
    let attempt_ancestry = authorized
        .attempts
        .iter()
        .map(|attempt| attempt.attempt_id.clone())
        .collect::<Vec<_>>();
    let parent_attempt_id = attempt_ancestry.last().cloned();
    let attempt_number = u32::try_from(authorized.attempts.len() + 1)?;
    let id = run_id.clone();
    let workspace_export = request.workspace_export.as_ref().map(|export| {
        let run_path = Path::new(&export.root).join(&run_id);
        WorkspaceExportStatus {
            root: export.root.clone(),
            run_path: run_path.display().to_string(),
            workspace_alias: export.workspace_alias.clone(),
            workspace_identity: export.workspace_identity.clone(),
            state: "pending".into(),
            error: None,
            authoritative: false,
            files: BTreeMap::new(),
        }
    });
    let container_name = format!("agent-hub-attended-{}", &attempt_id[..8]);
    let run_dir = store.path(&PathBuf::from("runs").join(&id))?;
    let ownership = BTreeMap::from([
        (
            "agent-hub.reddit.com/attended-digest".to_string(),
            digest.clone(),
        ),
        (
            "agent-hub.reddit.com/attended-run".to_string(),
            run_id.clone(),
        ),
        (
            "agent-hub.reddit.com/attended-attempt".to_string(),
            attempt_id.clone(),
        ),
        (
            "agent-hub.reddit.com/attended-authorization".to_string(),
            authorized.authorization_id.clone(),
        ),
        (
            "agent-hub.reddit.com/attended-receipt".to_string(),
            authorized.receipt_id.clone(),
        ),
        (
            "agent-hub.reddit.com/attended-nonce".to_string(),
            owner_nonce.clone(),
        ),
    ]);
    let mut record = RunRecord {
        schema: result_schema_current(),
        id: id.clone(),
        run_id: run_id.clone(),
        attempt_id: attempt_id.clone(),
        attempt_number,
        parent_attempt_id,
        attempt_ancestry,
        authorization_id: authorized.authorization_id.clone(),
        receipt_id: authorized.receipt_id.clone(),
        authorization_digest: digest.clone(),
        owner_nonce,
        launch_state: LaunchState::Preparing,
        status: RunStatus::Preparing,
        container_name: container_name.clone(),
        ownership: ownership.clone(),
        started_at: Utc::now(),
        finished_at: None,
        exit_code: None,
        error: None,
        execution_status: None,
        process_status: None,
        permission_outcome: PermissionOutcome::NotApplicable,
        process_outcome: ProcessOutcome::NotStarted,
        agent_outcome: AgentOutcome::NotProduced,
        execution_outcome: ExecutionOutcome::Pending,
        result_outcome: ResultOutcome::Pending,
        cleanup_outcome: CleanupOutcome::Pending,
        agent_final: None,
        retry_class: None,
        retry_eligible: false,
        workspace_export,
    };
    let lineage = AttemptLineage {
        attempt_id: attempt_id.clone(),
        run_id: run_id.clone(),
        state: LaunchState::Preparing,
        retry_class: None,
        retry_eligible: false,
        started_at: record.started_at,
        finished_at: None,
    };
    let pending = PendingAttempt {
        authorization_digest: digest.clone(),
        authorization_id: authorized.authorization_id.clone(),
        record: record.clone(),
        lineage: lineage.clone(),
    };
    store.create_json(&attempt_journal_path(&id), &pending)?;
    attended_fault("attempt_journaled")?;
    store.create_json(&run_record_path(&id), &record)?;
    attended_fault("execution_record_published")?;
    authorized.active_attempt_id = Some(attempt_id);
    authorized.attempts.push(lineage);
    authorized.state = LaunchState::Preparing;
    store.replace_json(&authorized_path, &authorized)?;
    attended_fault("attempt_lineage_published")?;
    attended_fault("preparing")?;

    let mut captured_execution_outcome = None;
    let mut captured_process_status = None;
    let mut captured_permission_outcome = None;
    let mut captured_cleanup_evidence = None;
    let host_redaction_values = relay_endpoint
        .as_ref()
        .and_then(|endpoint| endpoint.token.as_ref())
        .map(|token| vec![token.expose().to_string()])
        .unwrap_or_default();
    let mut export_redaction_values = request
        .execution
        .credential_providers
        .is_empty()
        .then(|| host_redaction_values.clone());
    let result = async {
    let cancel_marker = store.path(&cancel_path(&id))?;
    let cancelled_before_preflight = cancel_marker.exists();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(cancelled_before_preflight);
    let cancellation = Cancellation::new(cancel_rx);
    ensure!(
        !cancelled_before_preflight,
        "execution was cancelled before preflight"
    );
    let poll_cancel_tx = cancel_tx.clone();
    let polled_cancel_marker = cancel_marker.clone();
    let poll = AbortTask(tokio::spawn(async move {
        loop {
            if polled_cancel_marker.exists() {
                let _ = poll_cancel_tx.send(true);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }));
    store.ensure_directory(Path::new("runs"))?;

    let deadline = Deadline::new(request.execution.runtime.environment.timeout_secs)?;
    let configured_image = request
        .execution
        .runtime
        .environment
        .container_image
        .as_deref()
        .context("authorized runtime has no container image")?;
    let image_identity = request.proposal.image_id.clone();
    let current_identity =
        inspect_image_identity(&docker_path, configured_image, &deadline, &cancellation).await?;
    verify_image_identity(&image_identity, &current_identity)?;
    let (runtime_uid, runtime_gid) = runtime_identity(&request)?;
    #[cfg(unix)]
    if request.proposal.tool_approval == ToolApprovalMode::Dippy {
        ensure!(
            runtime_uid == unsafe { libc::geteuid() },
            "attended relay socket UID is incompatible with the sandbox runtime UID"
        );
    }
    let preflight_args = config::job_code_preflight_docker_args(
        &image_identity,
        runtime_uid,
        runtime_gid,
        request.execution.approved.pids_limit,
    )
    .map_err(anyhow::Error::msg)?;
    let preflight = execute_output_cancellable(
        &docker_path,
        &preflight_args,
        &deadline,
        &cancellation,
        "job-code preflight",
    )
    .await?;
    ensure!(
        preflight.status.success(),
        "job-code preflight failed: {}",
        String::from_utf8_lossy(&preflight.stderr).trim()
    );
    if request.proposal.tool_approval == ToolApprovalMode::Dippy {
        config::validate_supervised_opencode_preflight_report(
            &preflight.stdout,
            &image_identity,
            runtime_uid,
            runtime_gid,
        )
    } else {
        config::validate_job_code_preflight_report(
            &preflight.stdout,
            &image_identity,
            runtime_uid,
            runtime_gid,
        )
    }
    .map_err(anyhow::Error::msg)?;
    attended_fault("preflight_complete")?;
    ensure!(
        !cancellation.requested(),
        "execution was cancelled before owner publication"
    );
    let runs_dir = run_dir.parent().context("run directory has no parent")?;
    if !runs_dir.exists() {
        std::fs::create_dir(runs_dir)?;
        set_private_directory(runs_dir)?;
        sync_directory_and_parent(runs_dir)?;
    }
        ensure!(!run_dir.exists(), "fresh attended run directory already exists");
        let owner_marker = owner_marker_for_record(&record);
        create_owned_run_directory(&run_dir, &owner_marker)?;
        attended_fault("owner_published")?;
        let bundle_dir = run_dir.join("config");
        let data_dir = run_dir.join("data");
        let state_dir = run_dir.join("state");
        let cache_dir = run_dir.join("cache");
        let project_config_mask_dir = run_dir.join("project-config-mask");
        let plugin_run_nonce = (request.proposal.tool_approval == ToolApprovalMode::Dippy)
            .then(|| uuid::Uuid::new_v4().simple().to_string());
        let prompt_path = run_dir.join("prompt.txt");
        write_private_durable(&prompt_path, request.proposal.prompt.as_bytes())?;
        let deployment =
            config::validate_authoritative_config_root(Path::new(&request.execution.config_root))
                .map_err(anyhow::Error::msg)?;
        deployment.environment_bundles[&request.proposal.environment]
            .write_to(&bundle_dir)
            .map_err(anyhow::Error::msg)?;
        sync_private_tree(&bundle_dir)?;
        verify_attended_approval_material(&bundle_dir, request.proposal.tool_approval)?;
        for directory in [
            &data_dir,
            &state_dir,
            &cache_dir,
            &project_config_mask_dir,
        ] {
            std::fs::create_dir(directory)?;
            set_private_directory(directory)?;
            sync_directory_and_parent(directory)?;
        }
        validate_credential_runtime_conflicts(&request, &runner)?;
        let effects = agent_hub_executor::ExecutionCancellationContext::new(
            &run_dir.join("effects"),
            cancellation.clone(),
        )?;
        let runtime_data_dir = data_dir.join("opencode");
        effects.create_owned_runtime_data(
            &data_dir,
            &runtime_data_dir,
            runtime_uid,
            runtime_gid,
        )?;
        let mut credential_cleanup = TemporaryCredentialCleanup(Vec::new());
        let mut prepared_egress = None;
        let mut attended_relay = None;
        let mut materialized_git_views = None;
        let operation = async {
        if request.proposal.tool_approval == ToolApprovalMode::Dippy {
            let relay_endpoint = relay_endpoint
                .as_ref()
                .context("supervised execution has no host relay endpoint")?;
            let run_epoch = uuid::Uuid::parse_str(&record.owner_nonce)
                .context("attended attempt nonce is not a UUID")?;
            let relay_socket = host_relay_socket_location(run_epoch)?;
            let network_digest = protocol::attended_network_digest(&serde_json::json!({
                "class": request.proposal.network_class,
                "resources": request.proposal.network_resources,
                "runtime": request.execution.runtime.environment.network_resources,
            })).map_err(anyhow::Error::msg)?;
            let binding = protocol::AttendedGatewayBindingV1 {
                run_epoch: run_epoch.to_string(),
                authorization_digest: digest.clone(),
                environment_bundle_digest: request.execution.environment_bundle_digest.clone(),
                image_digest: image_identity.clone(),
                network_digest,
                approval_bundle_digest: request.approval_bundle_digest.clone(),
            };
            let run_deadline = DateTime::<Utc>::from(
                deadline.wall_clock().context("attended relay requires a wall-clock deadline")?
            );
            let endpoint = relay_endpoint_identity(&relay_endpoint.server_url, relay_endpoint.token.is_some())?;
            write_private_durable(
                &run_dir.join("approval-relay-epoch.json"),
                &serde_json::to_vec(&RelayEpochState {
                    run_epoch,
                    binding: binding.clone(),
                    plugin_nonce: plugin_run_nonce
                        .clone()
                        .context("supervised execution has no plugin run nonce")?,
                    deadline: run_deadline,
                    endpoint,
                    socket: relay_socket.clone(),
                })?,
            )?;
            attended_relay = Some(AttendedRelay::start(
                AttendedRelayConfig {
                    socket_path: relay_socket.socket_path,
                    binding: binding.clone(),
                    approval_bundle: request
                        .approval_bundle
                        .clone()
                        .context("supervised authorization has no approval bundle identity")?,
                    run_deadline,
                    server_url: relay_endpoint.server_url.clone(),
                    token: relay_endpoint.token.clone(),
                    docker_path: docker_path.clone(),
                    container_name: container_name.clone(),
                    ledger_path: run_dir.join("supervision-transitions.jsonl"),
                    plugin_nonce: plugin_run_nonce
                        .clone()
                        .context("supervised execution has no plugin run nonce")?,
                },
                &effects,
                cancel_tx.clone(),
            ).await?);
        }
        let credentials = resolve_attended_credentials(
            &deployment,
            &request,
            deadline.wall_clock(),
            &data_dir,
            &effects,
        )
        .await?;
        credential_cleanup.0 = credentials.temp_files.clone();
        verify_materialized_credential_identities(&request, &credentials)?;
        let credential_material = credentials.material()?;
        let redaction_values = credential_material.redaction_values.clone();
        export_redaction_values = Some(
            redaction_values
                .iter()
                .cloned()
                .chain(host_redaction_values.iter().cloned())
                .collect(),
        );
        let mut mounts = request
            .execution
            .workspaces
            .iter()
            .map(|mount| {
                DockerMount::new(
                    &mount.host_path,
                    &mount.container_path,
                    mount.access == WorkspaceAccess::ReadOnly,
                )
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        mounts.extend(
            request
                .execution
                .context_roots
                .iter()
                .map(|mount| DockerMount::new(&mount.host_path, &mount.container_path, true))
                .collect::<anyhow::Result<Vec<_>>>()?,
        );
        mounts.extend(project_config_mask_mounts(
            &project_config_mask_dir,
            &request.execution.workspaces,
        )?);
        let git_views = crate::git_view::materialize_git_views(
            &run_dir,
            &request.git_views,
            &effects,
        )?;
        mounts.extend(git_views.mounts.iter().cloned());
        mounts.push(git_views.home_mount.clone());
        materialized_git_views = Some(git_views);
        mounts.push(DockerMount::new(
            &bundle_dir,
            "/opencode/config/opencode",
            true,
        )?);
        mounts.push(DockerMount::new(&data_dir, "/opencode/data", false)?);
        mounts.push(DockerMount::new(&state_dir, "/opencode/state", false)?);
        mounts.push(DockerMount::new(&cache_dir, "/opencode/cache", false)?);
        mounts.push(DockerMount::new(
            &prompt_path,
            "/agent-hub/prompt.txt",
            true,
        )?);
        let mut environment = request.execution.behavior.env.clone();
        for (name, value) in credential_material.environment {
            ensure!(
                environment.insert(name.clone(), value).is_none(),
                "credential environment conflicts with authorized variable '{name}'"
            );
        }
        mounts.extend(credential_material.mounts);
        environment.insert(
            "AGENT_PROMPT_USER_PATH".into(),
            "/agent-hub/prompt.txt".into(),
        );
        let trusted_opencode_environment =
            if request.proposal.tool_approval == ToolApprovalMode::Dippy {
                config::supervised_opencode_runtime_env(&runner.opencode.trusted_env)
                    .map_err(anyhow::Error::msg)?
            } else {
                runner.opencode.trusted_env.clone()
            };
        for (name, value) in &trusted_opencode_environment {
            ensure!(
                environment.insert(name.clone(), value.clone()).is_none(),
                "trusted OpenCode environment conflicts with authorized variable '{name}'"
            );
        }
        apply_git_runtime_environment(&mut environment)?;
        verify_host_only_approval_environment(&environment)?;
        if let Some(relay) = &attended_relay {
            relay.verify_for_mount()?;
            mounts.push(DockerMount::new(
                relay.socket_path.parent().context("approval relay socket has no parent")?,
                "/agent-hub/approval",
                true,
            )?);
            ensure!(
                environment.insert(
                    "AGENT_HUB_APPROVAL_MODE".into(),
                    "attended_socket".into(),
                ).is_none(),
                "authorized environment conflicts with attended approval mode"
            );
            ensure!(
                environment.insert(
                    "AGENT_HUB_ATTENDED_APPROVAL_SOCKET".into(),
                    protocol::ATTENDED_GATEWAY_SOCKET_PATH.into(),
                ).is_none(),
                "authorized environment conflicts with attended relay socket"
            );
            ensure!(
                environment.insert(
                    "AGENT_HUB_ATTENDED_APPROVAL_BINDING".into(),
                    serde_json::to_string(relay.binding())?,
                ).is_none(),
                "authorized environment conflicts with attended relay binding"
            );
            ensure!(
                environment.insert(
                    "AGENT_HUB_ATTENDED_RUN_DEADLINE".into(),
                    relay.deadline().to_rfc3339(),
                ).is_none(),
                "authorized environment conflicts with attended relay deadline"
            );
            ensure!(
                environment.insert(
                    "AGENT_HUB_CONFIG".into(),
                    "/opencode/config/opencode/approval/agent-hub-permissions.json".into(),
                ).is_none(),
                "authorized environment conflicts with attended gateway config"
            );
            let plugin_run_nonce = plugin_run_nonce
                .as_deref()
                .context("supervised execution has no plugin run nonce")?;
            ensure!(
                environment
                    .insert("AGENT_HUB_PLUGIN_RUN_NONCE".into(), plugin_run_nonce.into())
                    .is_none(),
                "authorized environment conflicts with supervised plugin run nonce"
            );
            verify_exact_relay_mount(&mounts, &relay.socket_path)?;
        }
        if !request.proposal.network_resources.is_empty() {
            for name in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "NO_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
                "no_proxy",
                "AGENT_RESTRICTED_EGRESS_HOSTS",
            ] {
                ensure!(
                    !environment.contains_key(name),
                    "authorized environment conflicts with executor-owned network variable '{name}'"
                );
            }
        }
        ensure!(
            !cancellation.requested(),
            "execution was cancelled before launch"
        );
            prepared_egress = prepare_execution_egress(
            &deployment,
            &request,
            &docker_path,
            &deadline,
            AttendedEgressPreparation {
                state_dir: &run_dir.join("egress"),
                namespace: &id,
                resolver_image: &image_identity,
            },
            cancellation.clone(),
        )
        .await?;
            let (network, network_environment, dns, extra_hosts) = match &prepared_egress {
            Some(prepared) => {
                let mut network_environment = HashMap::new();
                for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                    network_environment
                        .insert(name.to_string(), prepared.runtime.proxy_url.clone());
                }
                for name in ["NO_PROXY", "no_proxy"] {
                    network_environment.insert(name.to_string(), prepared.runtime.no_proxy.clone());
                }
                for name in ["ALL_PROXY", "all_proxy"] {
                    network_environment.insert(name.to_string(), String::new());
                }
                network_environment.insert(
                    "AGENT_RESTRICTED_EGRESS_HOSTS".to_string(),
                    prepared.runtime.allowed_hosts.join(","),
                );
                (
                    NetworkMode::Named(prepared.runtime.network_name.clone()),
                    network_environment,
                    vec!["127.0.0.1".to_string()],
                    vec![format!(
                        "{}:{}",
                        prepared.runtime.proxy_host, prepared.runtime.proxy_ip
                    )],
                )
            }
            None => (NetworkMode::None, HashMap::new(), Vec::new(), Vec::new()),
            };
            for (name, value) in network_environment {
                ensure!(
                    environment.insert(name.clone(), value).is_none(),
                    "network policy environment conflicts with authorized environment variable '{name}'"
                );
            }
            let plan = DockerExecutionPlan {
            image_identity: image_identity.clone(),
            container_name: container_name.clone(),
            entrypoint: request.execution.behavior.command.clone(),
            command_args: request.execution.behavior.args.clone(),
            mounts,
            environment,
            network,
            dns,
            extra_hosts,
            ownership: DockerOwnership::new(ownership.clone())?,
            user: format!("{runtime_uid}:{runtime_gid}"),
            workdir: (!request.execution.primary_workspace.is_empty())
                .then(|| format!("/workspace/{}", request.execution.primary_workspace)),
            memory_limit: request
                .execution
                .runtime
                .environment
                .memory_limit
                .clone()
                .unwrap_or_else(|| "8g".to_string()),
            cpu_limit: request
                .execution
                .runtime
                .environment
                .cpu_limit
                .clone()
                .unwrap_or_else(|| "4".to_string()),
            pids_limit: request.execution.approved.pids_limit,
            read_only: true,
            tmpfs: vec!["/tmp:rw,size=1g".to_string()],
            };
            verify_mount_filesystem_identity(&request)?;
            effects.verify_owned_runtime_data(
                &data_dir,
                &runtime_data_dir,
                runtime_uid,
                runtime_gid,
            )?;
             let args = plan.materialize_args()?;
             ensure!(
                 !cancellation.requested(),
                 "execution was cancelled before launch"
             );
             attended_fault("preparing_complete")?;
             record.launch_state = LaunchState::LaunchPending;
             record.status = RunStatus::LaunchPending;
             store.replace_json(&run_record_path(&id), &record)?;
             attended_fault("launch_pending_record_published")?;
             update_authorization_attempt_state(
                 store,
                 &authorized_path,
                 &mut authorized,
                 &record.attempt_id,
                 LaunchState::LaunchPending,
             )?;
             attended_fault("launch_pending_lineage_published")?;
             authorized.state = LaunchState::Consumed;
             update_authorization_attempt_state(
                 store,
                 &authorized_path,
                 &mut authorized,
                 &record.attempt_id,
                 LaunchState::Consumed,
             )?;
             attended_fault("consumption_lineage_published")?;
             record.launch_state = LaunchState::Consumed;
             record.status = RunStatus::Consumed;
             store.replace_json(&run_record_path(&id), &record)?;
             attended_fault("consumption_record_published")?;
             record.launch_state = LaunchState::Running;
             record.status = RunStatus::Running;
             store.replace_json(&run_record_path(&id), &record)?;
             attended_fault("running_record_published")?;
             update_authorization_attempt_state(
                 store,
                 &authorized_path,
                 &mut authorized,
                 &record.attempt_id,
                 LaunchState::Running,
             )?;
             attended_fault("running_lineage_published")?;
             let process_result = execute_process(ExecutionRequest {
                command: docker_path.clone(),
                args,
                current_dir: run_dir.clone(),
                environment: BTreeMap::new(),
                artifacts: ArtifactPaths::in_directory(&run_dir),
                deadline: deadline.clone(),
                cancellation: cancellation.clone(),
                append: false,
                redaction_values,
                output_limits: OutputLimits {
                    stdout_bytes: request.output.limits.stdout_bytes,
                    stderr_bytes: request.output.limits.stderr_bytes,
                    transcript_bytes: request.output.limits.transcript_bytes,
                },
                effects: Some(effects.clone()),
                operation_identity: Some(format!(
                    "container={container_name};digest={digest};authorization={};receipt={};attempt={};run={};nonce={};image={image_identity}",
                    record.authorization_id,
                    record.receipt_id,
                    record.attempt_id,
                    record.run_id,
                    record.owner_nonce,
                 )),
            })
            .await?;
            captured_process_status = Some(run_status_for_execution_status(&process_result.status));
            record.process_outcome = process_outcome(&process_result.status);
            if let ExecutionStatus::Exited(code) = process_result.status {
                record.exit_code = Some(code);
            }
            Ok(process_result)
        }
        .await;
        let mut operation = operation;
        let run_cancelled = cancel_marker.exists()
            || cancellation.requested()
            || matches!(&operation, Ok(result) if result.status == ExecutionStatus::Cancelled);
        let mut relay_cleanup = relay_cleanup_requirement(
            request.proposal.tool_approval,
            attended_relay.is_some(),
        );
        if run_cancelled && let Some(relay) = &mut attended_relay {
            relay_cleanup = relay.stop().await;
        }
        if let Some(relay) = &attended_relay {
            match relay.supervision_outcome(run_cancelled).await {
                Ok(SupervisionOutcome::NoDenials) => {
                    captured_permission_outcome = Some(PermissionOutcome::NoDenials);
                }
                Ok(SupervisionOutcome::Denied) => {
                    captured_permission_outcome = Some(PermissionOutcome::Denied);
                    if operation.is_ok() {
                        operation = Err(anyhow::anyhow!(
                            "OpenCode process completed after a host-recorded permission denial"
                        ));
                    }
                }
                Ok(SupervisionOutcome::Cancelled) => {
                    captured_permission_outcome = Some(PermissionOutcome::Cancelled);
                }
                Err(error) => {
                    captured_permission_outcome = Some(PermissionOutcome::ReadinessFailed);
                    if operation.is_ok() {
                        operation = Err(error);
                    }
                }
            }
        }
        let execution_outcome = attended_execution_outcome(&operation, run_cancelled);
        captured_execution_outcome = Some(execution_outcome.clone());
        let parsed_agent = parse_agent_result_from_opencode(&run_dir.join("stdout.log"));
        record.agent_outcome = parsed_agent.outcome;
        record.agent_final = parsed_agent.result;
        if execution_outcome == AttendedExecutionOutcome::Succeeded
            && record.agent_outcome != AgentOutcome::Succeeded
        {
            operation = Err(anyhow::anyhow!(
                "agent terminal result is {:?}: {}",
                record.agent_outcome,
                parsed_agent.error.unwrap_or_else(|| "non-success terminal result".into())
            ));
            captured_execution_outcome = Some(AttendedExecutionOutcome::Failed);
        }
        drop(poll);
        if !run_cancelled {
            relay_cleanup = match &mut attended_relay {
                Some(relay) => relay.stop().await,
                None => Ok(()),
            };
        }
        let cleanup_deadline = Deadline::new(Some(ATTENDED_CLEANUP_TIMEOUT_SECS))?;
        let egress_cleanup = match &mut prepared_egress {
            Some(prepared) => prepared.cleanup(&cleanup_deadline).await,
            None if run_dir.join("egress/effects").join(&id).exists() => {
                agent_hub_executor::egress::recover_restricted_egress(
                    &docker_path,
                    &run_dir.join("egress"),
                    &id,
                    &cleanup_deadline,
                )
                .await
            }
            None => Ok(()),
        };
        let helper_cleanup = effects.settle_process_helpers().await;
        let primary_cleanup = match &helper_cleanup {
            Ok(()) => {
                DockerCli::new(docker_path.clone())
                    .with_effects(effects.clone())
                    .remove_owned(
                        &container_name,
                        &DockerOwnership::new(ownership.clone())?,
                        PRIMARY_CLEANUP_TIMEOUT,
                    )
                    .await
            }
            Err(error) => Err(anyhow::anyhow!(
                "primary containment blocked by unsettled helper: {error:#}"
            )),
        };
        let credential_cleanup = credential_cleanup.cleanup();
        let containment_absent = relay_cleanup.is_ok() && helper_cleanup.is_ok()
            && primary_cleanup.is_ok()
            && egress_cleanup.is_ok();
        let git_view_cleanup = if containment_absent {
            match materialized_git_views.take() {
                Some(views) => views.cleanup(&effects),
                None => crate::git_view::recover_git_views(&run_dir, &effects),
            }
        } else {
            Err(anyhow::anyhow!(
                "Git view cleanup blocked until process, container, helper, and egress absence is proven"
            ))
        };
        let credential_effect_cleanup = if containment_absent {
            effects.reconcile_owned_credentials(&data_dir, Some(&runtime_data_dir))
        } else {
            Err(anyhow::anyhow!(
                "credential effect cleanup blocked until process, container, helper, and egress absence is proven"
            ))
        };
        let runtime_data_cleanup = if containment_absent {
            effects.reconcile_owned_runtime_data(&data_dir, &runtime_data_dir)
        } else {
            Err(anyhow::anyhow!(
                "OpenCode runtime-data cleanup blocked until process, container, helper, and egress absence is proven"
            ))
        };
        let credential_check = match (&credential_cleanup, &credential_effect_cleanup) {
            (CleanupStatus::Complete, Ok(())) => CleanupCheck {
                status: "verified_absent".into(),
                error: None,
            },
            (CleanupStatus::Failed(error), _) => CleanupCheck {
                status: "failed".into(),
                error: Some(error.clone()),
            },
            (CleanupStatus::Complete, Err(error)) => CleanupCheck {
                status: "failed".into(),
                error: Some(format!("{error:#}")),
            },
        };
        let effect_cleanup = match (
            &credential_effect_cleanup,
            &runtime_data_cleanup,
            &git_view_cleanup,
        ) {
            (Ok(()), Ok(()), Ok(())) => effects.finalize(),
            (Err(error), _, _) => Err(anyhow::anyhow!("credential effects: {error:#}")),
            (_, Err(error), _) => Err(anyhow::anyhow!("runtime-data effects: {error:#}")),
            (_, _, Err(error)) => Err(anyhow::anyhow!("Git view effects: {error:#}")),
        };
        captured_cleanup_evidence = Some(AttendedCleanupEvidence {
            process: CleanupCheck::from_result(&helper_cleanup),
            container: CleanupCheck::from_result(&primary_cleanup),
            relay: if request.proposal.tool_approval == ToolApprovalMode::Auto {
                CleanupCheck::not_applicable()
            } else {
                CleanupCheck::from_result(&relay_cleanup)
            },
            egress: CleanupCheck::from_result(&egress_cleanup),
            credentials: credential_check,
            runtime_data: CleanupCheck::from_result(&runtime_data_cleanup),
            effects: CleanupCheck::from_result(&effect_cleanup),
        });
        merge_execution_cleanup(
            operation,
            merge_cleanup_results(
                merge_cleanup_results(
                    merge_cleanup_results(
                        merge_cleanup_results(egress_cleanup, relay_cleanup),
                        helper_cleanup,
                    ),
                    primary_cleanup,
                ),
                effect_cleanup,
            ),
            credential_cleanup,
        )
    }
    .await;
    record.finished_at = Some(Utc::now());
    let mut cleanup_evidence = captured_cleanup_evidence.unwrap_or_default();
    match result {
        Ok(result) => {
            if store.path(&cancel_path(&id))?.exists() {
                record.status = RunStatus::Cancelled;
            } else {
                match result.status {
                    ExecutionStatus::Exited(0) => {
                        record.status = RunStatus::Succeeded;
                        record.exit_code = Some(0);
                    }
                    ExecutionStatus::Exited(code) => {
                        record.status = RunStatus::Failed;
                        record.exit_code = Some(code);
                    }
                    ExecutionStatus::Cancelled => record.status = RunStatus::Cancelled,
                    ExecutionStatus::TimedOut => record.status = RunStatus::TimedOut,
                    ExecutionStatus::OutputLimitExceeded => {
                        record.status = RunStatus::Failed;
                        record.error = Some("reviewed host-output limit exceeded".into());
                    }
                }
            }
            if let CleanupStatus::Failed(error) = result.cleanup {
                record.status = RunStatus::CleanupFailed;
                record.error = Some(error);
            }
            record.execution_status = captured_execution_outcome
                .as_ref()
                .map(AttendedExecutionOutcome::run_status);
        }
        Err(error) => {
            if error.downcast_ref::<FinalizationError>().is_some() {
                if cleanup_evidence.complete() {
                    cleanup_evidence.effects = CleanupCheck {
                        status: "failed".into(),
                        error: Some(format!("{error:#}")),
                    };
                }
                record.status = RunStatus::CleanupFailed;
                record.error = Some(format!("{error:#}"));
            } else if store.path(&cancel_path(&id))?.exists() {
                record.status = RunStatus::Cancelled;
            } else {
                record.status = RunStatus::Failed;
                record.error = Some(format!("{error:#}"));
            }
            if let Some(execution_outcome) = captured_execution_outcome.clone() {
                record.execution_status = Some(execution_outcome.run_status());
            }
        }
    }
    if record.execution_status.is_none() && record.status != RunStatus::CleanupFailed {
        record.execution_status = Some(record.status.clone());
    }
    record.process_status = captured_process_status;
    record.permission_outcome =
        captured_permission_outcome.unwrap_or(PermissionOutcome::NotApplicable);
    let outer_container_cleanup = async {
        let effects = if run_dir.join("effects").exists() {
            Some(agent_hub_executor::ExecutionCancellationContext::new(
                &run_dir.join("effects"),
                Cancellation::never(),
            )?)
        } else {
            None
        };
        if let Some(effects) = &effects {
            effects.settle_process_helpers().await?;
        }
        let mut docker = DockerCli::new(&docker_path);
        if let Some(effects) = effects {
            docker = docker.with_effects(effects);
        }
        docker
            .remove_owned(
                &container_name,
                &DockerOwnership::new(ownership.clone())?,
                PRIMARY_CLEANUP_TIMEOUT,
            )
            .await
    }
    .await;
    cleanup_evidence.container = CleanupCheck::from_result(&outer_container_cleanup);
    if let Err(error) = outer_container_cleanup {
        record.status = RunStatus::CleanupFailed;
        let container_error = format!("container cleanup failed: {error:#}");
        record.error = Some(match record.error.take() {
            Some(existing) => format!("{existing}; {container_error}"),
            None => container_error,
        });
    }
    let prelaunch_reconciliation =
        if record.launch_state == LaunchState::Preparing && run_dir.exists() {
            recover_owned_run_resources(
                &docker_path,
                &run_dir,
                &id,
                &container_name,
                &ownership,
                relay_endpoint.as_ref(),
                &owner_marker_for_record(&record),
            )
            .await
        } else {
            Ok(())
        };
    if let Err(error) = &prelaunch_reconciliation {
        record.status = RunStatus::CleanupFailed;
        let cleanup_error = format!("prelaunch reconciliation failed: {error:#}");
        record.error = Some(match record.error.take() {
            Some(existing) => format!("{existing}; {cleanup_error}"),
            None => cleanup_error,
        });
    }
    let cleanup_complete = cleanup_evidence.complete() && prelaunch_reconciliation.is_ok();
    record.cleanup_outcome = if cleanup_complete {
        CleanupOutcome::Succeeded
    } else {
        CleanupOutcome::Failed
    };
    record.execution_outcome = execution_outcome_from_record(&record);
    if record.status != RunStatus::CleanupFailed {
        record.status = match record.execution_outcome {
            ExecutionOutcome::Pending => record.status,
            ExecutionOutcome::Succeeded => RunStatus::Succeeded,
            ExecutionOutcome::Failed => RunStatus::Failed,
            ExecutionOutcome::Cancelled => RunStatus::Cancelled,
            ExecutionOutcome::TimedOut => RunStatus::TimedOut,
        };
    }
    if cleanup_complete {
        attended_fault("cleanup_complete")?;
    }
    let output_result: anyhow::Result<()> = (|| {
        if run_dir.is_dir() {
            let artifacts_path = run_dir.join(&request.output.artifacts);
            let repositories_path = run_dir.join(&request.output.repositories);
            let cleanup_path = run_dir.join(&request.output.cleanup);
            let mut repositories = Vec::new();
            for repository in &request.execution.repositories {
                let ending = if repository.starting.head.is_some() {
                    let mut state =
                        crate::local::repository_state(Path::new(&repository.repository_root))?;
                    if let Some(export) = &request.workspace_export {
                        omit_workspace_export_paths(
                            &mut state,
                            &repository.workspace_alias,
                            export,
                        );
                    }
                    state
                } else {
                    repository.starting.clone()
                };
                repositories.push(serde_json::json!({
                    "workspace_alias": repository.workspace_alias,
                    "repository_root": repository.repository_root,
                    "mount_path": repository.mount_path,
                    "remote": repository.remote,
                    "access": repository.access,
                    "starting": repository.starting,
                    "ending": ending,
                }));
            }
            repositories.sort_by_key(|repository| {
                repository["workspace_alias"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            });
            write_private_durable_bounded(
                &repositories_path,
                &serialize_pretty_bounded(&repositories, request.output.limits.artifact_bytes)?,
                request.output.limits.artifact_bytes,
            )?;
            let artifact_manifest = serde_json::json!({
                "schema": "attended_artifacts_v1",
                "stdout": "stdout.log",
                "stderr": "stderr.log",
                "transcript": request.output.transcript,
                "result": request.output.result,
                "repositories": request.output.repositories,
                "cleanup": request.output.cleanup,
            });
            write_private_durable_bounded(
                &artifacts_path,
                &serialize_pretty_bounded(
                    &artifact_manifest,
                    request.output.limits.artifact_bytes,
                )?,
                request.output.limits.artifact_bytes,
            )?;
            let cleanup_document = serde_json::json!({
                "schema": "attended_cleanup_result_v1",
                "outcome": if cleanup_complete { "succeeded" } else { "failed" },
                "error": if cleanup_complete { None } else { record.error.as_ref() },
                "containment": &cleanup_evidence,
            });
            write_private_durable_bounded(
                &cleanup_path,
                &serialize_pretty_bounded(&cleanup_document, request.output.limits.artifact_bytes)?,
                request.output.limits.artifact_bytes,
            )?;
            record.result_outcome = ResultOutcome::Succeeded;
        }
        Ok(())
    })();
    let output_finalized = output_result.is_ok();
    if let Err(error) = output_result {
        record.result_outcome = ResultOutcome::Failed;
        if record.status != RunStatus::CleanupFailed {
            record.status = RunStatus::Failed;
            if record.execution_status.is_none() {
                record.execution_status = Some(RunStatus::Failed);
            }
        }
        let output_error = format!("attended output finalization failed: {error:#}");
        record.error = Some(match record.error.take() {
            Some(existing) => format!("{existing}; {output_error}"),
            None => output_error,
        });
        if run_dir.is_dir() {
            if let Ok(bytes) =
                serialize_pretty_bounded(&record, request.output.limits.artifact_bytes)
            {
                let _ = write_private_durable_bounded(
                    &run_dir.join(&request.output.result),
                    &bytes,
                    request.output.limits.artifact_bytes,
                );
            }
            let cleanup_path = run_dir.join(&request.output.cleanup);
            let incomplete = serde_json::json!({
                "status": "incomplete",
                "error": &record.error,
            });
            if let Ok(bytes) =
                serialize_pretty_bounded(&incomplete, request.output.limits.artifact_bytes)
            {
                let _ = write_private_durable_bounded(
                    &cleanup_path,
                    &bytes,
                    request.output.limits.artifact_bytes,
                );
            }
        }
    }
    let prelaunch_retry = record.launch_state == LaunchState::Preparing
        && record.result_outcome != ResultOutcome::Failed
        && cleanup_complete
        && !store.path(&cancel_path(&id))?.exists()
        && verify_execution_inputs(&request).is_ok();
    let provider_retry = record.launch_state == LaunchState::Running
        && record.result_outcome != ResultOutcome::Failed
        && cleanup_complete
        && output_finalized
        && provider_unavailable_retry_evidence(&record, &run_dir, &request)?;
    record.retry_class = if prelaunch_retry {
        Some(RetryClass::Prelaunch)
    } else if provider_retry {
        Some(RetryClass::ProviderServiceUnavailable)
    } else {
        None
    };
    record.retry_eligible = match record.retry_class {
        Some(RetryClass::Prelaunch) => retry_budget_allows(
            authorized.attempts.len(),
            authorized
                .attempts
                .iter()
                .filter(|attempt| attempt.retry_class == Some(RetryClass::Prelaunch))
                .count(),
            request.retry.max_attempts,
            request.retry.max_prelaunch_retries,
        ),
        Some(RetryClass::ProviderServiceUnavailable) => retry_budget_allows(
            authorized.attempts.len(),
            authorized
                .attempts
                .iter()
                .filter(|attempt| {
                    attempt.retry_class == Some(RetryClass::ProviderServiceUnavailable)
                })
                .count(),
            request.retry.max_attempts,
            request.retry.max_provider_unavailable_retries,
        ),
        None => false,
    };
    record.launch_state = LaunchState::Terminal;
    if run_dir.is_dir() {
        publish_terminal_result(
            &mut record,
            &run_dir.join(&request.output.result),
            request.output.limits.artifact_bytes,
        );
    } else {
        record.result_outcome = ResultOutcome::Failed;
    }
    let lineage = authorized
        .attempts
        .iter_mut()
        .find(|attempt| attempt.attempt_id == record.attempt_id)
        .context("authorization receipt lost its active attempt")?;
    ensure!(
        lineage.attempt_id == record.attempt_id
            && authorized.active_attempt_id.as_deref() == Some(record.attempt_id.as_str()),
        "authorization receipt active attempt mismatch"
    );
    lineage.state = LaunchState::Terminal;
    lineage.retry_class = record.retry_class.clone();
    lineage.retry_eligible = record.retry_eligible;
    lineage.finished_at = record.finished_at;
    authorized.active_attempt_id = None;
    authorized.state = if record.retry_eligible {
        LaunchState::Authorized
    } else {
        LaunchState::Terminal
    };
    publish_workspace_export(
        &mut record,
        &run_dir,
        &request.output.limits,
        export_redaction_values.as_deref(),
        false,
        cleanup_complete,
    );
    store.replace_json(&run_record_path(&id), &record)?;
    attended_fault("terminal_record_published")?;
    store.replace_json(&authorized_path, &authorized)?;
    attended_fault("terminal_lineage_published")?;
    Ok(record)
}

async fn resolve_host_relay_endpoint(
    deployment: &config::ValidatedDeployment,
    overrides: RelayEndpointOverride,
) -> anyhow::Result<RelayEndpoint> {
    let approvals = &deployment.runner_profiles.host_approvals;
    let server_url = overrides
        .server_url
        .unwrap_or_else(|| approvals.url.clone());
    let token = match (overrides.token, approvals.credential_provider.as_deref()) {
        (Some(token), _) => Some(token),
        (None, None) => None,
        (None, Some(name)) => {
            let definition = deployment
                .local_agent
                .get("credential_providers")
                .and_then(|value| value.get(name))
                .with_context(|| {
                    format!("host approvals credential provider '{name}' disappeared")
                })?;
            let provider: CredentialProvider = serde_json::from_value(definition.clone())?;
            let CredentialProvider::Command {
                command,
                args,
                capture_env,
                capture_file,
                ..
            } = provider
            else {
                anyhow::bail!("host approvals credential provider is not a command provider")
            };
            ensure!(
                capture_env.is_some() && capture_file.is_none(),
                "host approvals credential provider must return one host-only token"
            );
            let mut command = tokio::process::Command::new(command);
            command.args(args).kill_on_drop(true);
            let output = tokio::time::timeout(Duration::from_secs(30), command.output())
                .await
                .context("host approvals credential provider timed out")??;
            ensure!(
                output.status.success(),
                "host approvals credential provider failed"
            );
            let value = String::from_utf8(output.stdout)?;
            let value = value.trim();
            ensure!(
                !value.is_empty() && !value.chars().any(char::is_control),
                "host approvals credential provider returned an invalid token"
            );
            Some(protocol::Secret::new(value.to_string()))
        }
    };
    Ok(RelayEndpoint { server_url, token })
}

fn relay_endpoint_identity(value: &str, has_token: bool) -> anyhow::Result<RelayEndpointIdentity> {
    let url = crate::attended_relay::validated_endpoint(value, has_token)?;
    Ok(RelayEndpointIdentity {
        url: url.to_string(),
    })
}

pub fn list(store: &StateStore) -> anyhow::Result<Vec<RunRecord>> {
    store
        .list_json::<RunRecord>(Path::new("executions"))?
        .into_iter()
        .map(|record| status(store, &record.id))
        .collect()
}

pub fn status(store: &StateStore, id: &str) -> anyhow::Result<RunRecord> {
    validate_id(id)?;
    match read_receipt(store, id) {
        Ok(receipt) => {
            let record = normalize_record(receipt.payload.record)?;
            validate_run_record(&record, id)?;
            Ok(record)
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            let record = store
                .read_json(&run_record_path(id))
                .and_then(normalize_record)?;
            validate_run_record(&record, id)?;
            Ok(record)
        }
        Err(error) => Err(error),
    }
}

fn artifact_relative(id: &str, name: &str) -> anyhow::Result<PathBuf> {
    validate_id(id)?;
    ensure!(
        [
            "stdout.log",
            "stderr.log",
            "transcript.jsonl",
            "result.json",
            "artifacts.json",
            "repositories.json",
            "cleanup.json"
        ]
        .contains(&name),
        "unknown attended artifact"
    );
    Ok(PathBuf::from("runs").join(id).join(name))
}

#[derive(Debug, Serialize)]
pub struct BoundedArtifact {
    pub path: String,
    pub content: String,
    pub limit_bytes: u64,
    pub returned_bytes: u64,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupReceiptPayload {
    record: RunRecord,
    owner: RunOwnerMarker,
    result: serde_json::Value,
    artifacts: serde_json::Value,
    repositories: serde_json::Value,
    cleanup: serde_json::Value,
    cleanup_evidence: AttendedCleanupEvidence,
    host_ledger: HostLedgerSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostLedgerSummary {
    present: bool,
    entry_count: u64,
    byte_count: u64,
    sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupReceipt {
    schema: String,
    run_id: String,
    authorization_digest: String,
    cleaned_at: DateTime<Utc>,
    payload: CleanupReceiptPayload,
    checksum: String,
}

#[derive(Debug, Serialize)]
pub struct ParsedDocument {
    pub reference: BoundedArtifactReference,
    pub document: Option<serde_json::Value>,
    pub parse_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BoundedArtifactReference {
    pub path: String,
    pub available: bool,
    pub limit_bytes: u64,
    pub returned_bytes: u64,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Serialize)]
struct WorkspaceExportManifest {
    schema: &'static str,
    run_id: String,
    authoritative: bool,
    trusted: bool,
    finalized_after_cleanup: bool,
    files: BTreeMap<String, WorkspaceExportFile>,
    omitted: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceExportFile {
    bytes: u64,
    sha256: String,
    truncated: bool,
}

fn publish_workspace_export(
    record: &mut RunRecord,
    run_dir: &Path,
    limits: &protocol::AttendedOutputLimits,
    redaction_values: Option<&[String]>,
    preserve_manifest_files: bool,
    finalized_after_cleanup: bool,
) {
    let Some(export) = record.workspace_export.as_mut() else {
        return;
    };
    if record.retry_eligible {
        export.state = "deferred_for_retry".into();
        export.error = None;
        return;
    }
    export.state = if finalized_after_cleanup {
        "finalized"
    } else {
        "published"
    }
    .into();
    export.error = None;
    let published = publish_workspace_export_inner(
        record,
        run_dir,
        limits,
        redaction_values,
        preserve_manifest_files,
        finalized_after_cleanup,
    );
    let export = record
        .workspace_export
        .as_mut()
        .expect("workspace export remains configured");
    match published {
        Ok(files) => export.files = files,
        Err(error) => {
            export.state = "failed".into();
            export.error = Some(format!("workspace export failed: {error:#}"));
        }
    }
}

fn publish_workspace_export_inner(
    record: &RunRecord,
    run_dir: &Path,
    limits: &protocol::AttendedOutputLimits,
    redaction_values: Option<&[String]>,
    preserve_manifest_files: bool,
    finalized_after_cleanup: bool,
) -> anyhow::Result<BTreeMap<String, WorkspaceExportFile>> {
    let export = record
        .workspace_export
        .as_ref()
        .context("workspace export is not configured")?;
    ensure!(
        !export.authoritative,
        "workspace export cannot be authoritative"
    );
    let directory = open_workspace_export_directory(
        &protocol::AttendedWorkspaceExportContract {
            root: export.root.clone(),
            workspace_alias: export.workspace_alias.clone(),
            workspace_identity: export.workspace_identity.clone(),
        },
        &record.run_id,
    )?;
    let status = serde_json::json!({
        "schema": "attended_workspace_status_v1",
        "run_id": record.run_id,
        "attempt_number": record.attempt_number,
        "launch_state": record.launch_state,
        "status": record.status,
        "started_at": record.started_at,
        "finished_at": record.finished_at,
        "exit_code": record.exit_code,
        "permission_outcome": record.permission_outcome,
        "process_outcome": record.process_outcome,
        "agent_outcome": record.agent_outcome,
        "execution_outcome": record.execution_outcome,
        "cleanup_outcome": record.cleanup_outcome,
        "retry_class": record.retry_class,
        "retry_eligible": record.retry_eligible,
        "authoritative": false,
        "trusted": false,
    });
    let cleanup = serde_json::json!({
        "schema": "attended_workspace_cleanup_summary_v1",
        "run_id": record.run_id,
        "cleanup_outcome": record.cleanup_outcome,
        "finished_at": record.finished_at,
        "authoritative": false,
        "trusted": false,
    });
    let mut outputs = BTreeMap::<String, (Vec<u8>, bool)>::new();
    let mut omitted = BTreeMap::new();
    outputs.insert(
        "status.json".into(),
        (
            serialize_pretty_bounded(&status, limits.artifact_bytes)?,
            false,
        ),
    );
    outputs.insert(
        "cleanup.json".into(),
        (
            serialize_pretty_bounded(&cleanup, limits.artifact_bytes)?,
            false,
        ),
    );
    match redaction_values {
        Some(redactions) if redactions.iter().all(|value| !value.is_empty()) => {
            for (source, target, limit) in [
                ("stdout.log", "stdout.log", limits.stdout_bytes),
                ("stderr.log", "stderr.log", limits.stderr_bytes),
                (
                    "transcript.jsonl",
                    "transcript.jsonl",
                    limits.transcript_bytes,
                ),
            ] {
                if let Some((bytes, truncated)) = read_export_source(&run_dir.join(source), limit)?
                {
                    if truncated {
                        omitted.insert(target.into(), "source_exceeds_reviewed_bound".into());
                    } else {
                        let redacted = redact_exact_values(&bytes, redactions);
                        if redacted.len() as u64 <= limit {
                            outputs.insert(target.into(), (redacted, false));
                        } else {
                            omitted.insert(target.into(), "redacted_output_exceeds_bound".into());
                        }
                    }
                }
            }
            if let Some(result) = &record.agent_final {
                let bytes = serialize_pretty_bounded(result, limits.artifact_bytes)?;
                let redacted = redact_exact_values(&bytes, redactions);
                if redacted.len() as u64 <= limits.artifact_bytes {
                    outputs.insert("agent-result.json".into(), (redacted, false));
                } else {
                    omitted.insert(
                        "agent-result.json".into(),
                        "redacted_output_exceeds_bound".into(),
                    );
                }
            }
            for (source, target) in [
                ("repositories.json", "repositories.json"),
                ("artifacts.json", "artifacts.json"),
            ] {
                if let Some(value) = read_export_json(&run_dir.join(source), limits.artifact_bytes)?
                {
                    let bytes =
                        serialize_pretty_bounded(&scrub_export_json(value), limits.artifact_bytes)?;
                    let redacted = redact_exact_values(&bytes, redactions);
                    if redacted.len() as u64 <= limits.artifact_bytes {
                        outputs.insert(target.into(), (redacted, false));
                    } else {
                        omitted.insert(target.into(), "redacted_output_exceeds_bound".into());
                    }
                }
            }
        }
        _ => {
            for name in [
                "stdout.log",
                "stderr.log",
                "transcript.jsonl",
                "agent-result.json",
                "repositories.json",
                "artifacts.json",
            ] {
                omitted.insert(name.into(), "redaction_proof_unavailable".into());
            }
        }
    }
    let mut files = if preserve_manifest_files {
        export.files.clone()
    } else {
        BTreeMap::new()
    };
    remove_workspace_export_file(&directory, "manifest.json")?;
    for &name in workspace_export_allowlist() {
        if name != "manifest.json" && !outputs.contains_key(name) && !files.contains_key(name) {
            remove_workspace_export_file(&directory, name)?;
        }
    }
    for (name, (bytes, truncated)) in outputs {
        write_workspace_export_file(&directory, &name, &bytes)?;
        files.insert(
            name,
            WorkspaceExportFile {
                bytes: bytes.len() as u64,
                sha256: protocol::sha256_hex(&bytes),
                truncated,
            },
        );
    }
    let manifest = WorkspaceExportManifest {
        schema: "attended_workspace_export_manifest_v1",
        run_id: record.run_id.clone(),
        authoritative: false,
        trusted: false,
        finalized_after_cleanup,
        files: files.clone(),
        omitted,
    };
    write_workspace_export_file(
        &directory,
        "manifest.json",
        &serialize_pretty_bounded(&manifest, limits.artifact_bytes)?,
    )?;
    Ok(files)
}

fn redact_exact_values(bytes: &[u8], redactions: &[String]) -> Vec<u8> {
    let mut redactions = redactions
        .iter()
        .map(String::as_bytes)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    redactions.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut output = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while offset < bytes.len() {
        if let Some(value) = redactions
            .iter()
            .find(|value| bytes[offset..].starts_with(value))
        {
            output.extend_from_slice(b"[REDACTED]");
            offset += value.len();
        } else {
            output.push(bytes[offset]);
            offset += 1;
        }
    }
    output
}

fn workspace_export_allowlist() -> &'static [&'static str] {
    &[
        "status.json",
        "stdout.log",
        "stderr.log",
        "transcript.jsonl",
        "agent-result.json",
        "repositories.json",
        "artifacts.json",
        "cleanup.json",
        "manifest.json",
    ]
}

fn read_export_source(path: &Path, limit: u64) -> anyhow::Result<Option<(Vec<u8>, bool)>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_CLOEXEC,
    );
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    ensure!(
        file.metadata()?.is_file(),
        "workspace export source is not a regular file"
    );
    let mut bytes = Vec::new();
    let total = file.metadata()?.len();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(Some((bytes, total > limit)))
}

fn read_export_json(path: &Path, limit: u64) -> anyhow::Result<Option<serde_json::Value>> {
    let Some((bytes, truncated)) = read_export_source(path, limit)? else {
        return Ok(None);
    };
    ensure!(
        !truncated,
        "workspace export JSON source exceeds its reviewed bound"
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn scrub_export_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .filter(|(key, _)| !export_key_is_private(key))
                .map(|(key, value)| (key, scrub_export_json(value)))
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(scrub_export_json).collect())
        }
        value => value,
    }
}

fn export_key_is_private(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "credential",
        "token",
        "secret",
        "effect",
        "owner",
        "receipt",
        "authorization",
        "ownership",
    ]
    .iter()
    .any(|private| key.contains(private))
}

#[cfg(unix)]
fn open_workspace_export_directory(
    contract: &protocol::AttendedWorkspaceExportContract,
    run_id: &str,
) -> anyhow::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd as _};
    use std::os::unix::fs::MetadataExt as _;

    validate_id(run_id)?;
    let root = Path::new(&contract.root);
    let workspace = Path::new(&contract.workspace_identity.canonical_path);
    ensure!(
        root == workspace.join(".agent-hub/runs"),
        "workspace export root is outside the reserved .agent-hub/runs location"
    );
    let workspace_fd = open_absolute_directory_nofollow(workspace)?;
    let metadata = workspace_fd.metadata()?;
    ensure!(
        metadata.dev() == contract.workspace_identity.device
            && metadata.ino() == contract.workspace_identity.inode,
        "workspace export workspace identity changed after authorization"
    );
    let open_child = |parent: &File, name: &str| -> anyhow::Result<File> {
        let name = std::ffi::CString::new(name)?;
        let mkdir = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if mkdir != 0 {
            let error = std::io::Error::last_os_error();
            ensure!(
                error.kind() == std::io::ErrorKind::AlreadyExists,
                "create workspace export directory: {error}"
            );
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("open workspace export directory");
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    };
    let agent_hub = open_child(&workspace_fd, ".agent-hub")?;
    let runs = open_child(&agent_hub, "runs")?;
    let run = open_child(&runs, run_id)?;
    Ok(run)
}

#[cfg(not(unix))]
fn open_workspace_export_directory(
    _contract: &protocol::AttendedWorkspaceExportContract,
    _run_id: &str,
) -> anyhow::Result<File> {
    anyhow::bail!("safe workspace export requires Unix no-follow directory operations")
}

#[cfg(unix)]
fn open_absolute_directory_nofollow(path: &Path) -> anyhow::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd as _};
    use std::os::unix::fs::OpenOptionsExt as _;

    ensure!(
        path.is_absolute(),
        "workspace identity path must be absolute"
    );
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    for component in path.components() {
        let std::path::Component::Normal(component) = component else {
            continue;
        };
        let component = std::ffi::CString::new(component.as_encoded_bytes())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open workspace path component in {}", path.display()));
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(unix)]
fn write_workspace_export_file(directory: &File, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd as _};

    ensure!(
        name.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-.".contains(&byte)),
        "workspace export filename is not canonical"
    );
    let temporary = format!(".{name}.tmp-{}", uuid::Uuid::new_v4());
    let temporary = std::ffi::CString::new(temporary)?;
    let name = std::ffi::CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create workspace export file");
    }
    struct TemporaryFile<'a> {
        directory: &'a File,
        name: std::ffi::CString,
        published: bool,
    }
    impl Drop for TemporaryFile<'_> {
        fn drop(&mut self) {
            if !self.published {
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
                }
            }
        }
    }
    let mut temporary_guard = TemporaryFile {
        directory,
        name: temporary,
        published: false,
    };
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    file.sync_all()?;
    let renamed = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temporary_guard.name.as_ptr(),
            directory.as_raw_fd(),
            name.as_ptr(),
        )
    };
    if renamed != 0 {
        return Err(std::io::Error::last_os_error()).context("publish workspace export file");
    }
    temporary_guard.published = true;
    directory.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn remove_workspace_export_file(directory: &File, name: &str) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd as _;

    let name = std::ffi::CString::new(name)?;
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        directory.sync_all()?;
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error).context("remove stale workspace export file")
    }
}

#[cfg(not(unix))]
fn remove_workspace_export_file(_directory: &File, _name: &str) -> anyhow::Result<()> {
    anyhow::bail!("safe workspace export requires Unix atomic directory operations")
}

#[cfg(not(unix))]
fn write_workspace_export_file(
    _directory: &File,
    _name: &str,
    _bytes: &[u8],
) -> anyhow::Result<()> {
    anyhow::bail!("safe workspace export requires Unix atomic directory operations")
}

#[derive(Debug, Serialize)]
pub struct LocalResult {
    pub schema: &'static str,
    pub run_id: String,
    pub launch_state: LaunchState,
    pub status: RunStatus,
    pub error: Option<String>,
    pub agent_final: Option<protocol::AgentResultV1>,
    pub process_outcome: ProcessOutcome,
    pub agent_outcome: AgentOutcome,
    pub execution_outcome: ExecutionOutcome,
    pub result_outcome: ResultOutcome,
    pub permission_outcome: PermissionOutcome,
    pub cleanup_outcome: CleanupOutcome,
    pub exit_code: Option<i32>,
    pub retry_class: Option<RetryClass>,
    pub retry_eligible: bool,
    pub paths: BTreeMap<String, String>,
    pub result: ParsedDocument,
    pub artifacts: ParsedDocument,
    pub repositories: ParsedDocument,
    pub cleanup: ParsedDocument,
    pub stdout: BoundedArtifactReference,
    pub stderr: BoundedArtifactReference,
    pub transcript: BoundedArtifactReference,
}

pub fn read_artifact_bounded(
    store: &StateStore,
    id: &str,
    name: &str,
    limit_bytes: u64,
) -> anyhow::Result<BoundedArtifact> {
    ensure!(
        (1..=16_777_216).contains(&limit_bytes),
        "artifact read limit is out of bounds"
    );
    let (bytes, total_bytes) = store.read_limited(&artifact_relative(id, name)?, limit_bytes)?;
    let returned_bytes = bytes.len() as u64;
    Ok(BoundedArtifact {
        path: artifact_path_string(store, id, name)?,
        content: String::from_utf8_lossy(&bytes).into_owned(),
        limit_bytes,
        returned_bytes,
        total_bytes,
        truncated: returned_bytes < total_bytes,
    })
}

pub fn local_result(store: &StateStore, id: &str, limit_bytes: u64) -> anyhow::Result<LocalResult> {
    let record = status(store, id)?;
    let result = read_document_bounded(store, id, "result.json", limit_bytes)?;
    let artifacts = read_document_bounded(store, id, "artifacts.json", limit_bytes)?;
    let repositories = read_document_bounded(store, id, "repositories.json", limit_bytes)?;
    let cleanup = read_document_bounded(store, id, "cleanup.json", limit_bytes)?;
    let stdout = artifact_reference(store, id, "stdout.log", limit_bytes)?;
    let stderr = artifact_reference(store, id, "stderr.log", limit_bytes)?;
    let transcript = artifact_reference(store, id, "transcript.jsonl", limit_bytes)?;
    let mut paths = BTreeMap::from([
        (
            "run".into(),
            store
                .path(&PathBuf::from("runs").join(id))?
                .display()
                .to_string(),
        ),
        (
            "receipt".into(),
            store.path(&receipt_path(id))?.display().to_string(),
        ),
        ("result".into(), result.reference.path.clone()),
        ("artifacts".into(), artifacts.reference.path.clone()),
        ("repositories".into(), repositories.reference.path.clone()),
        ("cleanup".into(), cleanup.reference.path.clone()),
    ]);
    if let Some(export) = &record.workspace_export {
        paths.insert("workspace_export".into(), export.run_path.clone());
    }
    Ok(LocalResult {
        schema: LOCAL_RESULT_SCHEMA_V2,
        run_id: id.to_string(),
        launch_state: record.launch_state.clone(),
        status: record.status.clone(),
        error: record.error.clone(),
        agent_final: record.agent_final.clone(),
        process_outcome: record.process_outcome.clone(),
        agent_outcome: record.agent_outcome.clone(),
        execution_outcome: record.execution_outcome.clone(),
        result_outcome: record.result_outcome.clone(),
        permission_outcome: record.permission_outcome.clone(),
        cleanup_outcome: record.cleanup_outcome.clone(),
        exit_code: record.exit_code,
        retry_class: record.retry_class.clone(),
        retry_eligible: record.retry_eligible,
        paths,
        result,
        artifacts,
        repositories,
        cleanup,
        stdout,
        stderr,
        transcript,
    })
}

pub fn read_document_bounded(
    store: &StateStore,
    id: &str,
    name: &str,
    limit_bytes: u64,
) -> anyhow::Result<ParsedDocument> {
    validate_id(id)?;
    ensure!(
        (1..=16_777_216).contains(&limit_bytes),
        "artifact read limit is out of bounds"
    );
    ensure!(
        matches!(
            name,
            "result.json" | "artifacts.json" | "repositories.json" | "cleanup.json"
        ),
        "unknown attended document"
    );
    let run_relative = PathBuf::from("runs").join(id).join(name);
    let run_path = store.path(&run_relative)?;
    let (bytes, total, path) = match store.read_limited(&run_relative, limit_bytes) {
        Ok((bytes, total)) => (bytes, total, run_path.clone()),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            let receipt = match read_receipt(store, id) {
                Ok(receipt) => receipt,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    return Ok(ParsedDocument {
                        reference: BoundedArtifactReference {
                            path: run_path.display().to_string(),
                            available: false,
                            limit_bytes,
                            returned_bytes: 0,
                            total_bytes: 0,
                            truncated: false,
                        },
                        document: None,
                        parse_error: Some("document is unavailable".into()),
                    });
                }
                Err(error) => return Err(error),
            };
            let value = match name {
                "result.json" => receipt.payload.result,
                "artifacts.json" => receipt.payload.artifacts,
                "repositories.json" => receipt.payload.repositories,
                "cleanup.json" => receipt.payload.cleanup,
                _ => unreachable!(),
            };
            let encoded = serialize_pretty_bounded(&value, 16_777_216)
                .context("retained cleanup receipt document exceeds read bound")?;
            let total = encoded.len() as u64;
            (
                encoded.into_iter().take(limit_bytes as usize).collect(),
                total,
                store.path(&receipt_path(id))?,
            )
        }
        Err(error) => return Err(error),
    };
    let returned = bytes.len() as u64;
    let truncated = returned < total;
    let (document, parse_error) = if truncated {
        (None, Some("document was truncated before parsing".into()))
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(document) => (Some(document), None),
            Err(error) => (None, Some(error.to_string())),
        }
    };
    Ok(ParsedDocument {
        reference: BoundedArtifactReference {
            path: path.display().to_string(),
            available: true,
            limit_bytes,
            returned_bytes: returned,
            total_bytes: total,
            truncated,
        },
        document,
        parse_error,
    })
}

pub fn ensure_document_complete(document: &ParsedDocument) -> anyhow::Result<()> {
    ensure!(
        document.reference.available
            && !document.reference.truncated
            && document.document.is_some()
            && document.parse_error.is_none(),
        "attended document {} is unavailable, truncated, or malformed: {}",
        document.reference.path,
        document
            .parse_error
            .as_deref()
            .unwrap_or("no parsed document")
    );
    Ok(())
}

pub fn ensure_local_result_documents(result: &LocalResult) -> anyhow::Result<()> {
    ensure_document_complete(&result.result)?;
    ensure_document_complete(&result.artifacts)?;
    ensure_document_complete(&result.repositories)?;
    ensure_document_complete(&result.cleanup)
}

fn artifact_reference(
    store: &StateStore,
    id: &str,
    name: &str,
    limit: u64,
) -> anyhow::Result<BoundedArtifactReference> {
    let relative = artifact_relative(id, name)?;
    let path = store.path(&relative)?;
    match store.read_limited(&relative, limit) {
        Ok((bytes, total)) => Ok(BoundedArtifactReference {
            path: path.display().to_string(),
            available: true,
            limit_bytes: limit,
            returned_bytes: bytes.len() as u64,
            total_bytes: total,
            truncated: (bytes.len() as u64) < total,
        }),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(BoundedArtifactReference {
                path: path.display().to_string(),
                available: false,
                limit_bytes: limit,
                returned_bytes: 0,
                total_bytes: 0,
                truncated: false,
            })
        }
        Err(error) => Err(error),
    }
}

fn artifact_path_string(store: &StateStore, id: &str, name: &str) -> anyhow::Result<String> {
    Ok(store
        .path(&artifact_relative(id, name)?)?
        .display()
        .to_string())
}

pub fn ensure_successful_outcome(record: &RunRecord) -> anyhow::Result<()> {
    ensure!(
        record.execution_outcome == ExecutionOutcome::Succeeded
            && record.result_outcome == ResultOutcome::Succeeded
            && record.cleanup_outcome == CleanupOutcome::Succeeded,
        "local run {} has execution outcome {:?}, result outcome {:?}, and cleanup outcome {:?}",
        record.id,
        record.execution_outcome,
        record.result_outcome,
        record.cleanup_outcome,
    );
    Ok(())
}

pub fn ensure_cleanup_successful(record: &RunRecord) -> anyhow::Result<()> {
    ensure!(
        record.cleanup_outcome == CleanupOutcome::Succeeded,
        "local cleanup {} has outcome {:?}",
        record.id,
        record.cleanup_outcome,
    );
    Ok(())
}

pub async fn cancel(store: &StateStore, id: &str) -> anyhow::Result<()> {
    let record = status(store, id)?;
    ensure!(
        matches!(
            record.status,
            RunStatus::Preparing
                | RunStatus::LaunchPending
                | RunStatus::Consumed
                | RunStatus::Running
        ),
        "only an active execution can be cancelled"
    );
    store.create_json(
        &cancel_path(id),
        &serde_json::json!({"requested_at": Utc::now()}),
    )?;
    let run_dir = store.path(&PathBuf::from("runs").join(id))?;
    let deadline = std::time::Instant::now()
        .checked_add(CANCELLATION_TERMINAL_TIMEOUT)
        .context("cancellation deadline overflow")?;
    // Before owner publication there cannot be an owned container. The
    // durable cancellation marker is sufficient; the running command will
    // observe it and terminalize without requiring Docker to be installed.
    if run_dir.exists() {
        verify_run_owner_marker(&run_dir, &owner_marker_for_record(&record))?;
        let effects = if run_dir.join("effects").exists() {
            Some(agent_hub_executor::ExecutionCancellationContext::new(
                &run_dir.join("effects"),
                Cancellation::never(),
            )?)
        } else {
            None
        };
        if let Some(effects) = &effects {
            effects.settle_process_helpers().await?;
        }
        let mut docker = DockerCli::new(which::which("docker")?);
        if let Some(effects) = effects {
            docker = docker.with_effects(effects);
        }
        docker
            .remove_owned(
                &record.container_name,
                &DockerOwnership::new(record.ownership)?,
                PRIMARY_CLEANUP_TIMEOUT,
            )
            .await?;
    }
    loop {
        let current = status(store, id)?;
        if !matches!(
            current.status,
            RunStatus::Preparing
                | RunStatus::LaunchPending
                | RunStatus::Consumed
                | RunStatus::Running
        ) {
            return Ok(());
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "cancellation did not reach a durable terminal state"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn cleanup(
    store: &StateStore,
    id: &str,
    relay_endpoint: RelayEndpoint,
) -> anyhow::Result<RunRecord> {
    validate_id(id)?;
    let _lock = store.lock(&PathBuf::from("locks").join(format!("cleanup-{id}.lock")))?;
    match read_receipt(store, id) {
        Ok(receipt) => {
            // Repair the convenience execution index if cleanup previously
            // crashed after publishing its immutable receipt.
            store.replace_json(&run_record_path(id), &receipt.payload.record)?;
            return normalize_record(receipt.payload.record);
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let run_dir = store.path(&PathBuf::from("runs").join(id))?;
    let pending = pending_receipt_path(id);
    let pending_path = store.path(&pending)?;
    let mut record = status(store, id)?;
    if pending_path.exists() {
        let receipt: CleanupReceipt = store.read_json(&pending)?;
        verify_receipt(&receipt, id)?;
        verify_receipt_for_record(&receipt, &record)?;
        if run_dir.exists() {
            verify_run_owner_marker(&run_dir, &receipt.payload.owner)?;
            std::fs::remove_dir_all(&run_dir)?;
            File::open(run_dir.parent().context("run directory has no parent")?)?.sync_all()?;
        }
        publish_pending_receipt(store, id, &pending_path)?;
        store.replace_json(&run_record_path(id), &receipt.payload.record)?;
        return normalize_record(receipt.payload.record);
    }
    ensure!(
        !matches!(
            record.status,
            RunStatus::Preparing
                | RunStatus::LaunchPending
                | RunStatus::Consumed
                | RunStatus::Running
        ),
        "execution cannot be cleaned in its current state"
    );
    if !run_dir.exists() {
        let journal: PendingAttempt = store.read_json(&attempt_journal_path(id))?;
        let expected_owner = owner_marker_for_record(&record);
        let staging = owner_staging_directory(&run_dir, &expected_owner)?;
        ensure!(
            record.launch_state == LaunchState::Terminal
                && record.retry_class == Some(RetryClass::Prelaunch)
                && !staging.exists()
                && journal.authorization_digest == record.authorization_digest
                && journal.authorization_id == record.authorization_id
                && journal.record.run_id == record.run_id
                && journal.record.attempt_id == record.attempt_id
                && journal.record.authorization_id == record.authorization_id
                && journal.record.receipt_id == record.receipt_id
                && journal.record.authorization_digest == record.authorization_digest
                && journal.record.owner_nonce == record.owner_nonce
                && journal.record.attempt_number == record.attempt_number
                && journal.record.parent_attempt_id == record.parent_attempt_id
                && journal.record.attempt_ancestry == record.attempt_ancestry
                && journal.lineage.run_id == record.run_id
                && journal.lineage.attempt_id == record.attempt_id
                && journal.record.launch_state == LaunchState::Preparing,
            "missing runtime directory has no no-resource journal proof"
        );
        record.status = RunStatus::Cleaned;
        record.cleanup_outcome = CleanupOutcome::Succeeded;
        record.finished_at = Some(Utc::now());
        let cleanup_evidence = AttendedCleanupEvidence::default();
        let cleanup_document = serde_json::json!({
            "schema": "attended_cleanup_result_v1",
            "outcome": "succeeded",
            "cleaned_at": record.finished_at,
            "error": null,
            "containment": &cleanup_evidence,
            "journal_proof": {
                "kind": "prelaunch_no_resources",
                "attempt_id": record.attempt_id,
                "run_id": record.run_id,
                "owner_marker_published": false
            }
        });
        let payload = CleanupReceiptPayload {
            record: record.clone(),
            owner: owner_marker_for_record(&record),
            result: serde_json::to_value(&record)?,
            artifacts: serde_json::json!({
                "schema": "attended_artifacts_v1",
                "available": false,
                "reason": "prelaunch_no_resources"
            }),
            repositories: serde_json::json!([]),
            cleanup: cleanup_document,
            cleanup_evidence,
            host_ledger: HostLedgerSummary::default(),
        };
        let limits = AttendedOutputContract::default().limits;
        publish_workspace_export(&mut record, &run_dir, &limits, None, true, true);
        let mut payload = payload;
        payload.record = record.clone();
        payload.result = serde_json::to_value(&record)?;
        publish_cleanup_receipt(store, id, &record, payload)?;
        return status(store, id);
    }
    let docker = which::which("docker")?;
    let owner_marker = owner_marker_for_record(&record);
    let containment = recover_owned_run_resources(
        &docker,
        &run_dir,
        id,
        &record.container_name,
        &record.ownership,
        Some(&relay_endpoint),
        &owner_marker,
    )
    .await;
    record.finished_at = Some(Utc::now());
    if let Err(error) = containment {
        record.status = RunStatus::CleanupFailed;
        record.cleanup_outcome = CleanupOutcome::Failed;
        record.error = Some(format!("{error:#}"));
        store.replace_json(&run_record_path(id), &record)?;
        return Ok(record);
    }

    record.status = RunStatus::Cleaned;
    record.cleanup_outcome = CleanupOutcome::Succeeded;
    let result_document = serde_json::to_value(&record)?;
    let artifacts_document = runtime_artifact_summary(&run_dir)?;
    let repositories_document = read_optional_json_document(
        store,
        &PathBuf::from("runs").join(id).join("repositories.json"),
    )?
    .unwrap_or_else(|| {
        serde_json::json!({
            "schema": "attended_repositories_unavailable_v1",
            "reason": "interrupted_before_final_documents"
        })
    });
    let execution_cleanup =
        read_optional_json_document(store, &PathBuf::from("runs").join(id).join("cleanup.json"))?;
    // The recovery immediately above re-proved absence for every owned
    // resource. Retain any earlier execution evidence as history, but do not
    // reuse stale failed checks as the explicit-cleanup proof.
    let cleanup_evidence = AttendedCleanupEvidence::verified_absent();
    let cleanup_document = serde_json::json!({
        "schema": "attended_cleanup_result_v1",
        "outcome": "succeeded",
        "cleaned_at": record.finished_at,
        "error": null,
        "execution_cleanup": execution_cleanup.unwrap_or_else(|| serde_json::json!({
            "schema": "attended_cleanup_recovery_v1",
            "reason": "interrupted_before_final_documents"
        })),
        "containment": &cleanup_evidence,
    });
    let host_ledger = host_ledger_summary(&run_dir.join("supervision-transitions.jsonl"))?;
    let payload = CleanupReceiptPayload {
        record: record.clone(),
        owner: owner_marker,
        result: result_document,
        artifacts: artifacts_document,
        repositories: repositories_document,
        cleanup: cleanup_document,
        cleanup_evidence,
        host_ledger,
    };
    let limits = AttendedOutputContract::default().limits;
    publish_workspace_export(&mut record, &run_dir, &limits, None, true, true);
    let mut payload = payload;
    payload.record = record.clone();
    payload.result = serde_json::to_value(&record)?;
    let receipt = CleanupReceipt {
        schema: CLEANUP_RECEIPT_SCHEMA_V1.into(),
        run_id: id.into(),
        authorization_digest: record.authorization_digest.clone(),
        cleaned_at: record.finished_at.expect("cleanup finish time is set"),
        checksum: receipt_checksum(&payload)?,
        payload,
    };
    store.create_json(&pending, &receipt)?;
    let parent = run_dir.parent().context("run directory has no parent")?;
    let removal = match std::fs::remove_dir_all(&run_dir) {
        Ok(()) => File::open(parent)?.sync_all(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    if let Err(error) = removal {
        let _ = std::fs::remove_file(&pending_path);
        record.status = RunStatus::CleanupFailed;
        record.cleanup_outcome = CleanupOutcome::Failed;
        record.error = Some(format!("runtime payload cleanup failed: {error}"));
        store.replace_json(&run_record_path(id), &record)?;
        return Ok(record);
    }
    publish_pending_receipt(store, id, &pending_path)?;
    attended_fault("cleanup_receipt_published")?;
    store.replace_json(&run_record_path(id), &record)?;
    status(store, id)
}

fn publish_cleanup_receipt(
    store: &StateStore,
    id: &str,
    record: &RunRecord,
    payload: CleanupReceiptPayload,
) -> anyhow::Result<()> {
    let pending = pending_receipt_path(id);
    let pending_path = store.path(&pending)?;
    let receipt = CleanupReceipt {
        schema: CLEANUP_RECEIPT_SCHEMA_V1.into(),
        run_id: id.into(),
        authorization_digest: record.authorization_digest.clone(),
        cleaned_at: record
            .finished_at
            .context("cleanup finish time is absent")?,
        checksum: receipt_checksum(&payload)?,
        payload,
    };
    store.create_json(&pending, &receipt)?;
    publish_pending_receipt(store, id, &pending_path)?;
    attended_fault("cleanup_receipt_published")?;
    store.replace_json(&run_record_path(id), record)
}

fn runtime_artifact_summary(run_dir: &Path) -> anyhow::Result<serde_json::Value> {
    let mut artifacts = serde_json::Map::new();
    for name in [
        "stdout.log",
        "stderr.log",
        "transcript.jsonl",
        "result.json",
        "artifacts.json",
        "repositories.json",
        "cleanup.json",
    ] {
        let path = run_dir.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => {
                artifacts.insert(
                    name.into(),
                    serde_json::json!({
                        "byte_count": bytes.len(),
                        "sha256": protocol::sha256_hex(&bytes),
                    }),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(serde_json::json!({
        "schema": "attended_artifact_summary_v1",
        "artifacts": artifacts,
    }))
}

fn read_optional_json_document(
    store: &StateStore,
    path: &Path,
) -> anyhow::Result<Option<serde_json::Value>> {
    match store.read_json(path) {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn host_ledger_summary(path: &Path) -> anyhow::Result<HostLedgerSummary> {
    const HOST_LEDGER_LIMIT: u64 = 16_777_216;

    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HostLedgerSummary::default());
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "host ledger is not a regular file");
    ensure!(
        metadata.len() <= HOST_LEDGER_LIMIT,
        "host ledger exceeds receipt evidence limit"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len())?);
    file.take(HOST_LEDGER_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "host ledger changed while collecting receipt evidence"
    );
    let text = std::str::from_utf8(&bytes).context("host ledger is not UTF-8")?;
    let mut entry_count = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line).context("host ledger entry is invalid")?;
        entry_count += 1;
    }
    Ok(HostLedgerSummary {
        present: true,
        entry_count,
        byte_count: bytes.len() as u64,
        sha256: Some(protocol::sha256_hex(&bytes)),
    })
}

async fn recover_owned_run_resources(
    docker: &Path,
    run_dir: &Path,
    id: &str,
    container_name: &str,
    ownership: &BTreeMap<String, String>,
    relay_endpoint: Option<&RelayEndpoint>,
    owner_marker: &RunOwnerMarker,
) -> anyhow::Result<()> {
    verify_run_owner_marker(run_dir, owner_marker)?;
    let mut failures = Vec::new();
    let effects = if run_dir.join("effects").exists() {
        Some(agent_hub_executor::ExecutionCancellationContext::new(
            &run_dir.join("effects"),
            Cancellation::never(),
        )?)
    } else {
        None
    };
    if let Some(effects) = &effects {
        effects
            .settle_process_helpers()
            .await
            .context("settle mutation helpers before containment")?;
    }
    let mut docker_cli = DockerCli::new(docker);
    if let Some(effects) = &effects {
        docker_cli = docker_cli.with_effects(effects.clone());
    }
    if let Err(error) = docker_cli
        .remove_owned(
            container_name,
            &DockerOwnership::new(ownership.clone())?,
            PRIMARY_CLEANUP_TIMEOUT,
        )
        .await
    {
        failures.push(format!("primary container: {error:#}"));
    }
    if run_dir.exists()
        && let Err(error) = agent_hub_executor::egress::recover_restricted_egress(
            docker,
            &run_dir.join("egress"),
            id,
            &Deadline::new(Some(ATTENDED_CLEANUP_TIMEOUT_SECS))?,
        )
        .await
    {
        failures.push(format!("egress: {error:#}"));
    }
    if let Some(effects) = &effects
        && run_dir.join("approval-relay-epoch.json").exists()
    {
        let result = match relay_endpoint {
            Some(endpoint) => {
                let relay_state = verify_recovery_relay_endpoint(run_dir, endpoint)?;
                recover_attended_relay(
                    &relay_state.socket.socket_path,
                    &endpoint.server_url,
                    endpoint.token.as_ref(),
                    effects,
                )
                .await
            }
            None => Err(anyhow::anyhow!(
                "host approval endpoint is required for attended relay recovery"
            )),
        };
        if let Err(error) = result {
            failures.push(format!("approval relay: {error:#}"));
        }
    }
    if let Some(effects) = &effects {
        let data_dir = run_dir.join("data");
        let runtime_data_dir = data_dir.join("opencode");
        if failures.is_empty() {
            if let Err(error) = crate::git_view::recover_git_views(run_dir, effects) {
                failures.push(format!("Git view effects: {error:#}"));
            }
            if let Err(error) =
                effects.reconcile_owned_credentials(&data_dir, Some(&runtime_data_dir))
            {
                failures.push(format!("credential effects: {error:#}"));
            }
            if let Err(error) = effects.reconcile_owned_runtime_data(&data_dir, &runtime_data_dir) {
                failures.push(format!("runtime-data effects: {error:#}"));
            }
            if failures.is_empty()
                && let Err(error) = effects.finalize()
            {
                failures.push(format!("effect reconciliation: {error:#}"));
            }
        } else {
            failures.push(
                "runtime-data effects: cleanup blocked until container and egress absence is proven"
                    .to_string(),
            );
        }
    }
    ensure!(
        failures.is_empty(),
        "owned run cleanup failed: {}",
        failures.join("; ")
    );
    Ok(())
}

fn verify_recovery_relay_endpoint(
    run_dir: &Path,
    supplied: &RelayEndpoint,
) -> anyhow::Result<RelayEpochState> {
    let state: RelayEpochState = serde_json::from_slice(
        &std::fs::read(run_dir.join("approval-relay-epoch.json"))
            .context("attended relay endpoint state is missing")?,
    )
    .context("attended relay endpoint state is invalid")?;
    ensure!(
        state.endpoint == relay_endpoint_identity(&supplied.server_url, supplied.token.is_some())?,
        "supplied approval endpoint does not match the attended run"
    );
    ensure!(
        state.socket == host_relay_socket_location(state.run_epoch)?,
        "persisted attended relay socket location is invalid"
    );
    ensure!(
        state.binding.run_epoch == state.run_epoch.to_string(),
        "persisted attended relay epoch is inconsistent"
    );
    Ok(state)
}

fn verify_execution_inputs(request: &AttendedAuthorizationRequestV3) -> anyhow::Result<()> {
    let config_root = Path::new(&request.execution.config_root);
    ensure!(
        std::fs::canonicalize(config_root)? == config_root,
        "authorized config root filesystem identity changed"
    );
    let deployment =
        config::validate_authoritative_config_root(config_root).map_err(anyhow::Error::msg)?;
    ensure!(
        attended_config_digest(&deployment)? == request.execution.config_digest,
        "configuration changed after authorization"
    );
    ensure!(
        attended_config_provenance(&deployment)? == request.execution.config_provenance,
        "configuration provenance changed after authorization"
    );
    let bundle = deployment
        .environment_bundles
        .get(&request.proposal.environment)
        .context("authorized environment is no longer installed")?;
    bundle
        .verify_digest(&request.execution.environment_bundle_digest)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        attended_approval_bundle_identity(bundle, request.proposal.tool_approval)?
            == request.approval_bundle,
        "attended approval bundle changed after authorization"
    );
    ensure!(
        request
            .approval_bundle
            .as_ref()
            .map(|identity| identity.digest.as_str())
            == (request.proposal.tool_approval == ToolApprovalMode::Dippy)
                .then_some(request.approval_bundle_digest.as_str()),
        "attended approval bundle digest and identity differ"
    );
    let environment = bundle_environment(bundle)?;
    ensure!(
        environment.name == request.proposal.environment,
        "authorized environment name changed"
    );
    ensure!(
        resolve_image_family(&deployment, &environment.image)? == request.proposal.image_family
            && request.proposal.image_tag == environment.image
            && request
                .execution
                .runtime
                .environment
                .container_image
                .as_deref()
                == Some(environment.image.as_str()),
        "authorized image family or exact image changed"
    );
    ensure!(
        request
            .proposal
            .overlays
            .iter()
            .all(|overlay| environment.allowed_overlays.contains(overlay)),
        "authorized overlay is no longer allowed by the environment"
    );
    ensure!(
        network_class_within_ceiling(&request.proposal.network_class, &environment.network_policy,)?
            && request.proposal.network_resources
                == resolve_network_resources(
                    &request.proposal.network_class,
                    request.proposal.network_resources.clone(),
                )?
            && request.execution.runtime.environment.network_resources
                == Some(network_request(&request.proposal.network_resources)),
        "authorized network selector, resources, or ceiling changed"
    );
    ensure!(
        tool_approval_within_ceiling(
            request.proposal.tool_approval,
            tool_approval_mode(&environment.approval_mode)?,
        ),
        "authorized tool approval exceeds the environment ceiling"
    );
    ensure!(
        request.proposal.timeout_secs <= environment.max_timeout_secs,
        "authorized timeout exceeds the environment ceiling"
    );
    let runner = deployment
        .runner_profiles
        .resolve_agent_profile(None)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        serde_json::to_value(&runner)? == request.execution.behavior.runner_profile
            && runner.command == request.execution.behavior.command
            && runner.args == request.execution.behavior.args
            && network_policy_environment(&deployment, &request.proposal.network_class)?
                == request.execution.behavior.env,
        "authorized runner profile changed"
    );
    let expected_runtime = canonical_runtime(
        &environment,
        &ResolvedMounts {
            roots: request.roots.clone(),
            workspace_root_identities: request.workspace_root_identities.clone(),
            context_root_identities: request.context_root_identities.clone(),
            workspaces: request.execution.workspaces.clone(),
            context_roots: request.execution.context_roots.clone(),
            repositories: request.execution.repositories.clone(),
            git_views: request.git_views.clone(),
            primary_workspace: request.execution.primary_workspace.clone(),
        },
        &request.proposal.network_resources,
        request.proposal.timeout_secs,
    );
    ensure!(
        expected_runtime == request.execution.runtime,
        "authorized canonical runtime changed"
    );
    ensure!(
        authorized_credential_providers(&deployment, &runner, &request.proposal.overlays)?
            == request.execution.credential_providers,
        "authorized credential provider descriptors changed"
    );
    ensure!(
        authorized_overlays(
            &deployment,
            &request.proposal.overlays,
            &request.execution.repositories,
            &request.proposal.network_resources,
        )? == request.execution.overlays,
        "authorized overlay descriptors changed"
    );
    let approved = approved_roots(&deployment.local_agent)?;
    let workspaces = request
        .execution
        .workspaces
        .iter()
        .map(|mount| RequestedWorkspace {
            alias: mount.alias.clone(),
            path: PathBuf::from(&mount.host_path),
            access: mount.access,
        })
        .collect::<Vec<_>>();
    let contexts = request
        .execution
        .context_roots
        .iter()
        .map(|mount| RequestedContextRoot {
            alias: mount.alias.clone(),
            path: PathBuf::from(&mount.host_path),
            access: WorkspaceAccess::ReadOnly,
        })
        .collect::<Vec<_>>();
    let mut resolved = resolve_mounts(
        &approved,
        &workspaces,
        &contexts,
        &request.execution.primary_workspace,
    )?;
    if let Some(export) = &request.workspace_export {
        for repository in &mut resolved.repositories {
            omit_workspace_export_paths(
                &mut repository.starting,
                &repository.workspace_alias,
                export,
            );
        }
    }
    ensure!(
        resolved.workspaces == request.execution.workspaces
            && resolved.context_roots == request.execution.context_roots
            && resolved.repositories == request.execution.repositories
            && resolved.git_views == request.git_views,
        "workspace or repository identity changed after authorization"
    );
    ensure!(
        resolve_workspace_export_contract(
            request
                .workspace_export
                .as_ref()
                .map(|export| Path::new(&export.root)),
            &resolved.workspaces,
        )? == request.workspace_export,
        "workspace export root identity changed after authorization"
    );
    Ok(())
}

fn verify_mount_filesystem_identity(
    request: &AttendedAuthorizationRequestV3,
) -> anyhow::Result<()> {
    verify_execution_inputs(request)
        .context("revalidate attended filesystem identity immediately before launch")
}

fn provider_unavailable_retry_evidence(
    record: &RunRecord,
    run_dir: &Path,
    request: &AttendedAuthorizationRequestV3,
) -> anyhow::Result<bool> {
    let stderr = read_optional_bounded(
        &run_dir.join("stderr.log"),
        request.output.limits.stderr_bytes,
    )?;
    let retained_failure = format!(
        "{}\n{}",
        record.error.as_deref().unwrap_or_default(),
        String::from_utf8_lossy(&stderr)
    );
    let stdout = read_optional_bounded(
        &run_dir.join("stdout.log"),
        request.output.limits.stdout_bytes,
    )?;
    if !is_provider_service_unavailable(&stdout, &retained_failure) {
        return Ok(false);
    }
    for repository in &request.execution.repositories {
        let mut state = crate::local::repository_state(Path::new(&repository.repository_root))?;
        if let Some(export) = &request.workspace_export {
            omit_workspace_export_paths(&mut state, &repository.workspace_alias, export);
        }
        if state != repository.starting {
            return Ok(false);
        }
    }
    let evidence_complete = match request.proposal.tool_approval {
        ToolApprovalMode::Dippy => supervised_provider_retry_evidence(record, run_dir, request)?,
        ToolApprovalMode::Auto => automatic_provider_retry_evidence(record, run_dir, request)?,
    };
    Ok(provider_retry_proofs_complete(
        true,
        false,
        false,
        false,
        evidence_complete,
    ))
}

fn is_provider_service_unavailable(stdout: &[u8], retained_failure: &str) -> bool {
    const MESSAGE: &str = "error: service unavailable";
    if !retained_failure
        .lines()
        .any(|line| line.trim().to_ascii_lowercase() == MESSAGE)
    {
        return false;
    }
    let Ok(stdout) = std::str::from_utf8(stdout) else {
        return false;
    };
    let mut events = stdout.lines().filter(|line| !line.trim().is_empty());
    let Some(line) = events.next() else {
        return false;
    };
    if events.next().is_some() {
        return false;
    }
    let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    event.get("type").and_then(serde_json::Value::as_str) == Some("error")
        && event
            .get("timestamp")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        && event
            .get("sessionID")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && event
            .pointer("/error/name")
            .and_then(serde_json::Value::as_str)
            == Some("APIError")
        && event
            .pointer("/error/data/message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value.trim().to_ascii_lowercase() == MESSAGE)
        && event
            .pointer("/error/data/statusCode")
            .and_then(serde_json::Value::as_u64)
            == Some(503)
        && event
            .pointer("/error/data/isRetryable")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}

fn supervised_provider_retry_evidence(
    record: &RunRecord,
    run_dir: &Path,
    request: &AttendedAuthorizationRequestV3,
) -> anyhow::Result<bool> {
    if !provider_retry_output_evidence(record, run_dir, request)? {
        return Ok(false);
    }
    supervision_ledger_has_only_plugin_ready(run_dir)
}

fn supervision_ledger_has_only_plugin_ready(run_dir: &Path) -> anyhow::Result<bool> {
    let epoch_bytes = read_optional_bounded(&run_dir.join("approval-relay-epoch.json"), 1_048_576)?;
    if epoch_bytes.is_empty() {
        return Ok(false);
    }
    let epoch: RelayEpochState = serde_json::from_slice(&epoch_bytes)
        .context("invalid relay epoch while proving retry eligibility")?;
    let ledger =
        match read_optional_bounded(&run_dir.join("supervision-transitions.jsonl"), 16_777_216) {
            Ok(ledger) if !ledger.is_empty() => ledger,
            Ok(_) => return Ok(false),
            Err(error) => return Err(error),
        };
    let ledger = std::str::from_utf8(&ledger).context("host supervision ledger is not UTF-8")?;
    let mut transitions = 0;
    for line in ledger.lines().filter(|line| !line.trim().is_empty()) {
        let transition: protocol::AttendedGatewayTransitionV1 = serde_json::from_str(line)
            .context("invalid host supervision ledger while proving retry eligibility")?;
        if transition
            .validate(&epoch.binding, &epoch.plugin_nonce)
            .is_err()
            || transition.transition != protocol::AttendedGatewayTransitionKind::PluginReady
        {
            return Ok(false);
        }
        transitions += 1;
    }
    Ok(transitions == 1)
}

fn automatic_provider_retry_evidence(
    record: &RunRecord,
    run_dir: &Path,
    request: &AttendedAuthorizationRequestV3,
) -> anyhow::Result<bool> {
    if run_dir.join("supervision-transitions.jsonl").exists() {
        return Ok(false);
    }
    provider_retry_output_evidence(record, run_dir, request)
}

fn provider_retry_output_evidence(
    record: &RunRecord,
    run_dir: &Path,
    request: &AttendedAuthorizationRequestV3,
) -> anyhow::Result<bool> {
    if record.process_outcome != ProcessOutcome::Failed
        || record.process_status != Some(RunStatus::Failed)
    {
        return Ok(false);
    }
    let stdout = read_optional_bounded(
        &run_dir.join("stdout.log"),
        request.output.limits.stdout_bytes,
    )?;
    let stderr = read_optional_bounded(
        &run_dir.join("stderr.log"),
        request.output.limits.stderr_bytes,
    )?;
    let transcript = read_optional_bounded(
        &run_dir.join(&request.output.transcript),
        request.output.limits.transcript_bytes,
    )?;
    let Some((transcript_stdout, transcript_stderr)) = verified_transcript_streams(&transcript)?
    else {
        return Ok(false);
    };
    if transcript_stdout != stdout || transcript_stderr != stderr {
        return Ok(false);
    }
    Ok(is_provider_service_unavailable(
        &stdout,
        &String::from_utf8_lossy(&stderr),
    ))
}

fn verified_transcript_streams(transcript: &[u8]) -> anyhow::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let transcript = std::str::from_utf8(transcript).context("host transcript is not UTF-8")?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for line in transcript.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        let Some(stream) = value.get("stream").and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        let Some(text) = value.get("line").and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        let Some(partial) = value.get("partial").and_then(serde_json::Value::as_bool) else {
            return Ok(None);
        };
        let output = match stream {
            "stdout" => &mut stdout,
            "stderr" => &mut stderr,
            _ => return Ok(None),
        };
        output.extend_from_slice(text.as_bytes());
        if !partial {
            output.push(b'\n');
        }
    }
    Ok(Some((stdout, stderr)))
}

fn provider_retry_proofs_complete(
    service_unavailable: bool,
    assistant_result: bool,
    tool_result: bool,
    workspace_delta: bool,
    cleanup_complete: bool,
) -> bool {
    service_unavailable && !assistant_result && !tool_result && !workspace_delta && cleanup_complete
}

fn retry_budget_allows(
    attempt_count: usize,
    completed_class_retries: usize,
    max_attempts: u32,
    max_class_retries: u32,
) -> bool {
    attempt_count < max_attempts as usize && completed_class_retries < max_class_retries as usize
}

fn read_optional_bounded(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
    match File::open(path) {
        Ok(file) => {
            ensure!(
                file.metadata()?.len() <= limit,
                "retry evidence exceeds reviewed output limit"
            );
            let mut bytes = Vec::new();
            file.take(limit).read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn bundle_environment(
    bundle: &config::EnvironmentBundle,
) -> anyhow::Result<config::EnvironmentConfig> {
    let file = bundle
        .files
        .iter()
        .find(|file| file.path == "environment.toml")
        .context("environment bundle has no environment manifest")?;
    config::EnvironmentConfig::from_toml(
        std::str::from_utf8(&file.bytes).context("environment manifest is not UTF-8")?,
        "captured environment.toml",
    )
    .map_err(anyhow::Error::msg)
}

async fn resolve_attended_credentials(
    deployment: &config::ValidatedDeployment,
    request: &AttendedAuthorizationRequestV3,
    valid_through: Option<std::time::SystemTime>,
    credential_dir: &Path,
    cancellation: &agent_hub_executor::ExecutionCancellationContext,
) -> anyhow::Result<agent_hub_executor::credential_provider::ResolvedCredentials> {
    let configured = configured_credential_providers(deployment)?;
    let providers = request
        .execution
        .credential_providers
        .iter()
        .map(|descriptor| {
            let provider = configured.get(&descriptor.name).cloned().with_context(|| {
                format!(
                    "required credential provider '{}' is unavailable",
                    descriptor.name
                )
            })?;
            ensure!(
                provider.name() == descriptor.name
                    && provider.provider_type() == descriptor.provider_type
                    && provider.targets() == descriptor.targets,
                "credential provider '{}' does not match its authorized descriptor",
                descriptor.name
            );
            Ok(provider)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    resolve_all_for_job(
        &providers,
        valid_through,
        Some(credential_dir),
        cancellation,
    )
    .await
}

fn validate_credential_runtime_conflicts(
    request: &AttendedAuthorizationRequestV3,
    runner: &config::ResolvedRunnerProfile,
) -> anyhow::Result<()> {
    let mut environment = request
        .execution
        .behavior
        .env
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    environment.extend(runner.opencode.trusted_env.keys().cloned());
    environment.insert("AGENT_PROMPT_USER_PATH".to_string());
    environment.extend(
        [
            "HOME",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_GLOBAL",
            "GIT_OPTIONAL_LOCKS",
            "GIT_PAGER",
            "PAGER",
            "GIT_TERMINAL_PROMPT",
            "GIT_NO_REPLACE_OBJECTS",
        ]
        .into_iter()
        .map(str::to_string),
    );
    if !request.proposal.network_resources.is_empty() {
        environment.extend(
            [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "NO_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
                "no_proxy",
                "AGENT_RESTRICTED_EGRESS_HOSTS",
            ]
            .into_iter()
            .map(str::to_string),
        );
    }
    let mut mounts = request
        .execution
        .workspaces
        .iter()
        .map(|mount| mount.container_path.clone())
        .chain(
            request
                .execution
                .context_roots
                .iter()
                .map(|mount| mount.container_path.clone()),
        )
        .collect::<std::collections::BTreeSet<_>>();
    mounts.extend(
        [
            "/opencode/config/opencode",
            "/opencode/data",
            "/opencode/state",
            "/opencode/cache",
            "/agent-hub/prompt.txt",
        ]
        .into_iter()
        .map(str::to_string),
    );
    mounts.insert("/agent-hub/git/home".to_string());
    for view in &request.git_views {
        mounts.insert(view.pointer_container_path.clone());
        mounts.insert(view.git_dir_container_path.clone());
        mounts.extend(
            view.object_stores
                .iter()
                .map(|store| store.container_path.clone()),
        );
    }

    for provider in &request.execution.credential_providers {
        for target in &provider.targets {
            if target.starts_with('/') {
                ensure!(
                    !mounts.contains(target),
                    "credential target '{target}' conflicts with an executor mount"
                );
            } else {
                ensure!(
                    !environment.contains(target),
                    "credential target '{target}' conflicts with an authorized environment variable"
                );
            }
        }
    }
    Ok(())
}

fn apply_git_runtime_environment(environment: &mut BTreeMap<String, String>) -> anyhow::Result<()> {
    ensure!(
        !environment.contains_key("GIT_CONFIG_NOSYSTEM"),
        "generated Git environment cannot disable the isolated system config"
    );
    environment
        .entry("GIT_CONFIG_GLOBAL".into())
        .or_insert_with(|| "/agent-hub/git/home/.gitconfig".into());
    for (name, value) in [
        ("GIT_CONFIG_SYSTEM", "/agent-hub/git/home/.gitconfig"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_PAGER", "cat"),
        ("PAGER", "cat"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
    ] {
        match environment.entry(name.into()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(value.into());
            }
            std::collections::btree_map::Entry::Occupied(entry) => ensure!(
                entry.get() == value,
                "generated Git environment conflicts with authorized variable '{name}'"
            ),
        }
    }
    Ok(())
}

fn project_config_mask_mounts(
    mask_dir: &Path,
    workspaces: &[protocol::WorkspaceSpec],
) -> anyhow::Result<Vec<DockerMount>> {
    let mut mounts = Vec::new();
    for workspace in workspaces {
        let project_config = Path::new(&workspace.host_path).join(".opencode");
        match std::fs::symlink_metadata(&project_config) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "workspace project configuration is not a real directory: {}",
                project_config.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        mounts.push(DockerMount::new(
            mask_dir,
            format!("{}/.opencode", workspace.container_path),
            true,
        )?);
    }
    Ok(mounts)
}

fn verify_materialized_credential_identities(
    request: &AttendedAuthorizationRequestV3,
    credentials: &agent_hub_executor::credential_provider::ResolvedCredentials,
) -> anyhow::Result<()> {
    for (name, identity) in &credentials.source_identities {
        let authorized = request
            .execution
            .credential_providers
            .iter()
            .find(|provider| provider.name == *name)
            .with_context(|| {
                format!("materialized credential provider '{name}' was not authorized")
            })?;
        ensure!(
            authorized.source == identity.canonical_path
                && authorized.constraints.get("source_sha256") == Some(&identity.sha256),
            "credential source '{name}' changed after authorization"
        );
    }
    Ok(())
}

fn configured_credential_providers(
    deployment: &config::ValidatedDeployment,
) -> anyhow::Result<HashMap<String, CredentialProvider>> {
    serde_json::from_value(
        deployment
            .local_agent
            .get("credential_providers")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .context("parse configured credential providers")
}

fn authorized_overlays(
    deployment: &config::ValidatedDeployment,
    selected: &[String],
    repositories: &[protocol::RepositoryMountSpec],
    network_resources: &[NetworkResourceKind],
) -> anyhow::Result<Vec<AuthorizedOverlay>> {
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    let registry = deployment
        .overlay_registry
        .as_ref()
        .context("selected overlays require an installed overlay registry")?;
    let mut overlays = selected
        .iter()
        .map(|name| {
            let overlay = registry
                .overlays
                .iter()
                .find(|overlay| overlay.name == *name)
                .with_context(|| format!("selected overlay '{name}' is not installed"))?;
            let config =
                if let Some(github) = overlay.github_config().map_err(anyhow::Error::msg)? {
                    ensure!(
                        network_resources.contains(&NetworkResourceKind::Github),
                        "GitHub credential overlay requires the github network resource"
                    );
                    let matching = repositories
                        .iter()
                        .filter_map(|repository| repository.remote.as_deref())
                        .filter(|remote| remote_host(remote).as_deref() == Some(&github.host))
                        .map(str::to_string)
                        .collect::<Vec<_>>();
                    ensure!(
                        !matching.is_empty(),
                        "GitHub overlay '{}' has no matching repository remote",
                        overlay.name
                    );
                    serde_json::json!({
                        "host": github.host,
                        "api_base_url": github.api_base_url,
                        "access": github.access,
                        "network_resource": github.network_resource,
                        "credential_provider": github.credential_provider,
                        "credential_target": github.credential_target,
                        "authenticated": github.authenticated,
                        "private_repositories": github.private_repositories,
                        "repository_remotes": matching,
                    })
                } else {
                    overlay.config.clone()
                };
            Ok(AuthorizedOverlay {
                name: overlay.name.clone(),
                provider: overlay.provider.clone(),
                config: serde_json::to_string(&config)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    overlays.sort();
    let mut github_hosts = std::collections::BTreeSet::new();
    for overlay in &overlays {
        if overlay.provider == "github" {
            let binding: protocol::GithubOverlayBinding = serde_json::from_str(&overlay.config)?;
            ensure!(
                github_hosts.insert(binding.host),
                "multiple GitHub overlays target the same host"
            );
        }
    }
    Ok(overlays)
}

fn authorized_credential_providers(
    deployment: &config::ValidatedDeployment,
    runner: &config::ResolvedRunnerProfile,
    overlays: &[String],
) -> anyhow::Result<Vec<AuthorizedCredentialProvider>> {
    let configured = configured_credential_providers(deployment)?;
    let mut names = runner
        .credential_providers
        .iter()
        .map(|required| required.name.clone())
        .collect::<Vec<_>>();
    if let Some(registry) = &deployment.overlay_registry {
        for name in overlays {
            let overlay = registry
                .overlays
                .iter()
                .find(|overlay| overlay.name == *name)
                .with_context(|| format!("selected overlay '{name}' is not installed"))?;
            if let Some(github) = overlay.github_config().map_err(anyhow::Error::msg)? {
                names.push(github.credential_provider);
            }
        }
    }
    names.sort();
    ensure!(
        !names.windows(2).any(|pair| pair[0] == pair[1]),
        "credential provider selected more than once"
    );
    let mut descriptors = names
        .iter()
        .map(|required| {
            let provider = configured.get(required).with_context(|| {
                format!("required credential provider '{}' is unavailable", required)
            })?;
            authorized_credential_provider(provider)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    descriptors.sort();
    Ok(descriptors)
}

fn authorized_credential_provider(
    provider: &CredentialProvider,
) -> anyhow::Result<AuthorizedCredentialProvider> {
    let (source, command, args, validity, access, constraints) = match provider {
        CredentialProvider::File {
            host_path,
            container_path,
            read_only,
            ..
        } => {
            ensure!(*read_only, "credential file mounts must be read-only");
            let identity = agent_hub_executor::credential_provider::private_file_identity(
                host_path,
                "credential file",
            )?;
            (
                identity.canonical_path,
                None,
                Vec::new(),
                None,
                "read_only".to_string(),
                BTreeMap::from([
                    ("container_path".to_string(), container_path.clone()),
                    ("source_sha256".to_string(), identity.sha256),
                ]),
            )
        }
        CredentialProvider::Env { .. } => (
            "configured_environment".to_string(),
            None,
            Vec::new(),
            None,
            "inject_environment".to_string(),
            BTreeMap::new(),
        ),
        CredentialProvider::Command {
            command,
            args,
            capture_env,
            capture_file,
            ..
        } => (
            command.clone(),
            Some(command.clone()),
            args.clone(),
            None,
            "execute_host_command".to_string(),
            [
                capture_env
                    .as_ref()
                    .map(|value| ("capture_env".to_string(), value.clone())),
                capture_file
                    .as_ref()
                    .map(|value| ("capture_file".to_string(), value.clone())),
            ]
            .into_iter()
            .flatten()
            .collect(),
        ),
        CredentialProvider::KubeconfigFilter {
            source_path,
            allowed_contexts,
            container_path,
            ..
        } => {
            let source = source_path.as_deref().unwrap_or("~/.kube/config");
            let identity = agent_hub_executor::credential_provider::private_file_identity(
                source,
                "kubeconfig credential",
            )?;
            (
                identity.canonical_path,
                None,
                Vec::new(),
                None,
                "read_only".to_string(),
                BTreeMap::from([
                    (
                        "allowed_contexts".to_string(),
                        serde_json::to_string(allowed_contexts)?,
                    ),
                    (
                        "container_path".to_string(),
                        container_path
                            .clone()
                            .unwrap_or_else(|| "/home/agent/.kube/config".to_string()),
                    ),
                    ("source_sha256".to_string(), identity.sha256),
                ]),
            )
        }
        CredentialProvider::OpencodeChatgptOauth {
            host_auth_path,
            safety_margin_secs,
            ..
        } => (
            host_auth_path.clone(),
            None,
            Vec::new(),
            Some("must exceed authorized deadline".to_string()),
            "read_only".to_string(),
            BTreeMap::from([(
                "safety_margin_secs".to_string(),
                safety_margin_secs.to_string(),
            )]),
        ),
        CredentialProvider::GithubToken {
            host,
            api_base_url,
            access,
            command,
            args,
            target,
            ..
        } => (
            format!("{command}:{}", args.join("\u{0}")),
            Some(command.clone()),
            args.clone(),
            Some("must remain valid through authorized deadline".to_string()),
            access.clone(),
            BTreeMap::from([
                ("host".to_string(), host.clone()),
                ("api_base_url".to_string(), api_base_url.clone()),
                ("target".to_string(), target.clone()),
            ]),
        ),
    };
    Ok(AuthorizedCredentialProvider {
        name: provider.name().to_string(),
        provider_type: provider.provider_type().to_string(),
        source,
        command,
        args,
        validity,
        targets: provider.targets(),
        access,
        constraints,
    })
}

fn validate_credential_descriptors(
    deployment: &config::ValidatedDeployment,
    runner: &config::ResolvedRunnerProfile,
    overlays: &[String],
) -> anyhow::Result<()> {
    let configured = configured_credential_providers(deployment)?;
    let authorized = authorized_credential_providers(deployment, runner, overlays)?;
    for descriptor in &authorized {
        let provider = configured.get(&descriptor.name).with_context(|| {
            format!(
                "required credential provider '{}' is unavailable",
                descriptor.name
            )
        })?;
        ensure!(
            provider.name() == descriptor.name
                && provider.provider_type() == descriptor.provider_type
                && provider.targets() == descriptor.targets,
            "credential provider '{}' does not match its canonical identity and targets",
            descriptor.name
        );
    }
    Ok(())
}

fn remote_host(remote: &str) -> Option<String> {
    url::Url::parse(remote)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
}

fn attended_config_digest(deployment: &config::ValidatedDeployment) -> anyhow::Result<String> {
    let mut local_agent = deployment.local_agent.clone();
    if let Some(object) = local_agent.as_object_mut() {
        let mut providers = serde_json::Map::new();
        for (configured_name, provider) in configured_credential_providers(deployment)? {
            providers.insert(
                configured_name,
                serde_json::to_value(authorized_credential_provider(&provider)?)?,
            );
        }
        object.insert("credential_providers".to_string(), providers.into());
    }
    let snapshot = serde_json::json!({
        "system_toml": deployment.system_toml,
        "local_agent": local_agent,
        "environment_bundle_digests": deployment.environment_bundle_digests,
        "runner_profile_digests": deployment.runner_profile_digests,
        "network_policy_digests": deployment.network_policies.iter()
            .map(|(name, policy)| (name, &policy.directory_digest))
            .collect::<BTreeMap<_, _>>(),
        "images": deployment.images.iter()
            .map(|(name, image)| (name, &image.directory_digest))
            .collect::<BTreeMap<_, _>>(),
        "overlay_registry": deployment.overlay_registry.as_ref().map(|registry| {
            registry.overlays.iter().map(|overlay| serde_json::json!({
                "name": overlay.name,
                "provider": overlay.provider,
                "config": overlay.config,
                "description": overlay.description,
            })).collect::<Vec<_>>()
        }),
        "authoritative_paths": deployment.authoritative_paths,
    });
    Ok(protocol::sha256_hex_with_domain(
        b"agent-hub:attended-config:v1\0",
        &serde_json::to_vec(&snapshot)?,
    ))
}

fn attended_config_provenance(
    deployment: &config::ValidatedDeployment,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut provenance = BTreeMap::new();
    provenance.insert(
        "system.toml".to_string(),
        protocol::sha256_hex_with_domain(
            b"agent-hub:attended-config-file:v1\0",
            deployment.system_toml.as_bytes(),
        ),
    );
    let mut local_agent = deployment.local_agent.clone();
    if let Some(object) = local_agent.as_object_mut() {
        let mut providers = serde_json::Map::new();
        for (name, provider) in configured_credential_providers(deployment)? {
            providers.insert(
                name,
                serde_json::to_value(authorized_credential_provider(&provider)?)?,
            );
        }
        object.insert("credential_providers".to_string(), providers.into());
    }
    provenance.insert(
        "local-agent.json".to_string(),
        protocol::sha256_hex_with_domain(
            b"agent-hub:attended-config-file:v1\0",
            &serde_json::to_vec(&local_agent)?,
        ),
    );
    provenance.extend(
        deployment
            .environment_bundle_digests
            .iter()
            .map(|(name, digest)| (format!("environment_bundle:{name}"), digest.clone())),
    );
    provenance.extend(
        deployment
            .runner_profile_digests
            .iter()
            .map(|(name, digest)| (format!("runner_profile:{name}"), digest.clone())),
    );
    provenance.extend(deployment.network_policies.iter().map(|(name, policy)| {
        (
            format!("network_policy:{name}"),
            policy.directory_digest.clone(),
        )
    }));
    provenance.extend(
        deployment
            .images
            .iter()
            .map(|(name, image)| (format!("image:{name}"), image.directory_digest.clone())),
    );
    if let Some(registry) = &deployment.overlay_registry {
        provenance.insert(
            registry.file.relative_path.clone(),
            registry.file.digest.clone(),
        );
    }
    if let Some(triage) = &deployment.triage_prompt {
        provenance.insert(
            triage.file.relative_path.clone(),
            triage.file.digest.clone(),
        );
    }
    provenance.insert(
        "authoritative_inventory".to_string(),
        protocol::sha256_hex_with_domain(
            b"agent-hub:attended-config-inventory:v1\0",
            &serde_json::to_vec(&deployment.authoritative_paths)?,
        ),
    );
    Ok(provenance)
}

struct TemporaryCredentialCleanup(Vec<PathBuf>);

struct ParsedAgentResult {
    outcome: AgentOutcome,
    result: Option<protocol::AgentResultV1>,
    error: Option<String>,
}

fn parse_agent_result_from_opencode(path: &Path) -> ParsedAgentResult {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ParsedAgentResult {
                outcome: AgentOutcome::Missing,
                result: None,
                error: Some("OpenCode stdout is missing".into()),
            };
        }
        Err(error) => {
            return ParsedAgentResult {
                outcome: AgentOutcome::Malformed,
                result: None,
                error: Some(format!("read OpenCode stdout: {error}")),
            };
        }
    };
    let output = match std::str::from_utf8(&bytes) {
        Ok(output) => output,
        Err(error) => {
            return ParsedAgentResult {
                outcome: AgentOutcome::Malformed,
                result: None,
                error: Some(format!("OpenCode stdout is not UTF-8: {error}")),
            };
        }
    };
    parse_agent_result_events(output)
}

fn parse_agent_result_events(output: &str) -> ParsedAgentResult {
    let trimmed = output.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && value.get("schema").and_then(serde_json::Value::as_str)
            == Some(protocol::AGENT_RESULT_V1_SCHEMA)
    {
        return parse_agent_result_values(vec![value]);
    }

    let mut structured = Vec::new();
    let mut final_text = None;
    for (index, line) in output.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(error) => {
                return ParsedAgentResult {
                    outcome: AgentOutcome::Malformed,
                    result: None,
                    error: Some(format!(
                        "OpenCode event {} is malformed: {error}",
                        index + 1
                    )),
                };
            }
        };
        if event.get("schema").and_then(serde_json::Value::as_str)
            == Some(protocol::AGENT_RESULT_V1_SCHEMA)
        {
            return ParsedAgentResult {
                outcome: AgentOutcome::Malformed,
                result: None,
                error: Some("OpenCode terminal document must be the only stdout document".into()),
            };
        }
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("tool_use") => {
                let part = &event["part"];
                let tool = part.get("tool").and_then(serde_json::Value::as_str);
                if tool.is_some_and(|tool| {
                    matches!(
                        tool,
                        "StructuredOutput" | "structured_output" | "structuredoutput"
                    )
                }) && part["state"]["status"] == "completed"
                {
                    structured.push(part["state"]["input"].clone());
                }
            }
            Some("text") => {
                if let Some(text) = event["part"]["text"].as_str() {
                    final_text = Some(text.to_string());
                }
            }
            _ => {}
        }
    }

    let values = if structured.is_empty() {
        let Some(text) = final_text else {
            return ParsedAgentResult {
                outcome: AgentOutcome::Missing,
                result: None,
                error: Some("OpenCode emitted no StructuredOutput or final text".into()),
            };
        };
        match serde_json::from_str(&text) {
            Ok(value) => vec![value],
            Err(error) => {
                return ParsedAgentResult {
                    outcome: AgentOutcome::Malformed,
                    result: None,
                    error: Some(format!(
                        "OpenCode final text is not an AgentResultV1 object: {error}"
                    )),
                };
            }
        }
    } else {
        if let Some(text) = final_text
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            && value.get("schema").and_then(serde_json::Value::as_str)
                == Some(protocol::AGENT_RESULT_V1_SCHEMA)
        {
            structured.push(value);
        }
        structured
    };

    parse_agent_result_values(values)
}

fn parse_agent_result_values(values: Vec<serde_json::Value>) -> ParsedAgentResult {
    let mut parsed = Vec::new();
    for value in values {
        let result = match serde_json::from_value::<protocol::AgentResultV1>(value) {
            Ok(result) => result,
            Err(error) => {
                return ParsedAgentResult {
                    outcome: AgentOutcome::Malformed,
                    result: None,
                    error: Some(format!("agent terminal envelope is malformed: {error}")),
                };
            }
        };
        if result.complete
            && (!result.blockers.is_empty()
                || result
                    .acceptance_results
                    .iter()
                    .any(|acceptance| !acceptance.passed))
        {
            return ParsedAgentResult {
                outcome: AgentOutcome::Contradictory,
                result: Some(result),
                error: Some(
                    "complete result contains blockers or failed acceptance results".into(),
                ),
            };
        }
        if let Err(error) = result.validate() {
            return ParsedAgentResult {
                outcome: AgentOutcome::Malformed,
                result: Some(result),
                error: Some(error),
            };
        }
        parsed.push(result);
    }
    let result = parsed.pop().expect("at least one terminal candidate");
    if parsed.iter().any(|candidate| candidate != &result) {
        return ParsedAgentResult {
            outcome: AgentOutcome::Contradictory,
            result: Some(result),
            error: Some("OpenCode emitted contradictory terminal envelopes".into()),
        };
    }
    ParsedAgentResult {
        outcome: if result.complete {
            AgentOutcome::Succeeded
        } else {
            AgentOutcome::Incomplete
        },
        result: Some(result),
        error: None,
    }
}

fn process_outcome(status: &ExecutionStatus) -> ProcessOutcome {
    match status {
        ExecutionStatus::Exited(0) => ProcessOutcome::Succeeded,
        ExecutionStatus::Exited(_) => ProcessOutcome::Failed,
        ExecutionStatus::Cancelled => ProcessOutcome::Cancelled,
        ExecutionStatus::TimedOut => ProcessOutcome::TimedOut,
        ExecutionStatus::OutputLimitExceeded => ProcessOutcome::OutputLimitExceeded,
    }
}

fn execution_outcome_from_record(record: &RunRecord) -> ExecutionOutcome {
    match record.process_outcome {
        _ if record.status == RunStatus::Cancelled
            || record.execution_status == Some(RunStatus::Cancelled) =>
        {
            ExecutionOutcome::Cancelled
        }
        _ if record.permission_outcome == PermissionOutcome::Cancelled => {
            ExecutionOutcome::Cancelled
        }
        ProcessOutcome::Cancelled => ExecutionOutcome::Cancelled,
        ProcessOutcome::TimedOut => ExecutionOutcome::TimedOut,
        ProcessOutcome::Succeeded
            if record.agent_outcome == AgentOutcome::Succeeded
                && !matches!(
                    record.permission_outcome,
                    PermissionOutcome::Denied
                        | PermissionOutcome::Cancelled
                        | PermissionOutcome::ReadinessFailed
                ) =>
        {
            ExecutionOutcome::Succeeded
        }
        ProcessOutcome::NotStarted if record.status == RunStatus::Cancelled => {
            ExecutionOutcome::Cancelled
        }
        _ => ExecutionOutcome::Failed,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttendedExecutionOutcome {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

impl AttendedExecutionOutcome {
    fn run_status(&self) -> RunStatus {
        match self {
            Self::Succeeded => RunStatus::Succeeded,
            Self::Failed => RunStatus::Failed,
            Self::Cancelled => RunStatus::Cancelled,
            Self::TimedOut => RunStatus::TimedOut,
        }
    }
}

#[derive(Debug)]
enum CleanupFailureKind {
    RequiredResources,
    Credentials,
}

#[derive(Debug)]
struct CleanupFailureDetail {
    kind: CleanupFailureKind,
    error: String,
}

#[derive(Debug)]
struct FinalizationError {
    execution_error: Option<String>,
    cleanup_failures: Vec<CleanupFailureDetail>,
}

fn attended_execution_outcome(
    operation: &anyhow::Result<agent_hub_executor::ExecutionResult>,
    cancellation_requested: bool,
) -> AttendedExecutionOutcome {
    match operation {
        Ok(_) if cancellation_requested => AttendedExecutionOutcome::Cancelled,
        Ok(result) => match result.status {
            ExecutionStatus::Exited(0) => AttendedExecutionOutcome::Succeeded,
            ExecutionStatus::Exited(_) | ExecutionStatus::OutputLimitExceeded => {
                AttendedExecutionOutcome::Failed
            }
            ExecutionStatus::Cancelled => AttendedExecutionOutcome::Cancelled,
            ExecutionStatus::TimedOut => AttendedExecutionOutcome::TimedOut,
        },
        Err(_) if cancellation_requested => AttendedExecutionOutcome::Cancelled,
        Err(_) => AttendedExecutionOutcome::Failed,
    }
}

fn run_status_for_execution_status(status: &ExecutionStatus) -> RunStatus {
    match status {
        ExecutionStatus::Exited(0) => RunStatus::Succeeded,
        ExecutionStatus::Exited(_) | ExecutionStatus::OutputLimitExceeded => RunStatus::Failed,
        ExecutionStatus::Cancelled => RunStatus::Cancelled,
        ExecutionStatus::TimedOut => RunStatus::TimedOut,
    }
}

impl std::fmt::Display for FinalizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut details = Vec::new();
        if let Some(error) = &self.execution_error {
            details.push(format!("execution failed: {error}"));
        }
        details.extend(self.cleanup_failures.iter().map(|failure| {
            let kind = match failure.kind {
                CleanupFailureKind::RequiredResources => "required resource cleanup",
                CleanupFailureKind::Credentials => "credential cleanup",
            };
            format!("{kind} failed: {}", failure.error)
        }));
        formatter.write_str(&details.join("; "))
    }
}

impl std::error::Error for FinalizationError {}

fn merge_execution_cleanup<T>(
    operation: anyhow::Result<T>,
    egress_cleanup: anyhow::Result<()>,
    credential_cleanup: CleanupStatus,
) -> anyhow::Result<T> {
    match (operation, egress_cleanup, credential_cleanup) {
        (Ok(result), Ok(()), CleanupStatus::Complete) => Ok(result),
        (Err(error), Ok(()), CleanupStatus::Complete) => Err(error),
        (operation, required_cleanup, credential_cleanup) => {
            let execution_error = operation.err().map(|error| format!("{error:#}"));
            let mut cleanup_failures = Vec::new();
            if let Err(error) = required_cleanup {
                cleanup_failures.push(CleanupFailureDetail {
                    kind: CleanupFailureKind::RequiredResources,
                    error: format!("{error:#}"),
                });
            }
            if let CleanupStatus::Failed(error) = credential_cleanup {
                cleanup_failures.push(CleanupFailureDetail {
                    kind: CleanupFailureKind::Credentials,
                    error,
                });
            }
            Err(anyhow::Error::new(FinalizationError {
                execution_error,
                cleanup_failures,
            }))
        }
    }
}

fn merge_cleanup_results(
    first: anyhow::Result<()>,
    second: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(anyhow::anyhow!("{first:#}; {second:#}")),
    }
}

struct AbortTask(tokio::task::JoinHandle<()>);

impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl TemporaryCredentialCleanup {
    fn cleanup(mut self) -> CleanupStatus {
        let status = agent_hub_executor::cleanup_temporary_files(&self.0);
        self.0.clear();
        status
    }
}

impl Drop for TemporaryCredentialCleanup {
    fn drop(&mut self) {
        if let CleanupStatus::Failed(error) = agent_hub_executor::cleanup_temporary_files(&self.0) {
            eprintln!("attended credential cleanup failed during setup: {error}");
        }
    }
}

fn canonical_runtime(
    environment: &config::EnvironmentConfig,
    mounts: &ResolvedMounts,
    network_resources: &[NetworkResourceKind],
    timeout_secs: u64,
) -> CanonicalRuntimeSpec {
    let mut planned = canonical_environment_for_execution(
        ExecutionMode::Docker,
        Some(environment.image.clone()),
        None,
    );
    planned.network_resources = Some(if network_resources.is_empty() {
        NetworkResourceRequest::None
    } else {
        NetworkResourceRequest::Resources(network_resources.to_vec())
    });
    planned.timeout_secs = Some(timeout_secs);
    let mut permissions = canonical_permissions_for_environment(&planned);
    permissions.can_write_workspace = mounts
        .workspaces
        .iter()
        .any(|mount| mount.access == WorkspaceAccess::ReadWrite);
    permissions.can_access_network = !matches!(
        planned.network_resources,
        Some(NetworkResourceRequest::None)
    );
    let approval_rules = Some(default_approval_rules_for_tier(
        &PlannedEnvironmentTier::Docker,
        planned.network_resources.as_ref(),
    ));
    CanonicalRuntimeSpec {
        runtime_version: "v2".into(),
        environment: planned,
        permissions,
        approval_rules,
        repos: vec![],
        github_capabilities: vec![],
    }
}

fn approved_roots(local_agent: &serde_json::Value) -> anyhow::Result<ApprovedRoots> {
    ApprovedRoots::new(config::root_registry(local_agent).map_err(anyhow::Error::msg)?)
}

fn bundle_network_policy(
    bundle: &config::EnvironmentBundle,
) -> anyhow::Result<BundleNetworkPolicy> {
    let file = bundle
        .files
        .iter()
        .find(|file| file.path == "network_policy.json")
        .context("environment bundle has no network policy")?;
    Ok(serde_json::from_slice(&file.bytes)?)
}

fn network_policy_environment(
    deployment: &config::ValidatedDeployment,
    policy_name: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let system = config::SystemConfig::from_toml(&deployment.system_toml, "captured system.toml")
        .map_err(anyhow::Error::msg)?;
    let policies = deployment
        .network_policies
        .iter()
        .map(|(name, policy)| (name.clone(), policy.policy.clone()))
        .collect::<HashMap<_, _>>();
    let flattened =
        config::flatten_policy(policy_name, &policies, &system).map_err(anyhow::Error::msg)?;
    Ok(flattened.env.into_iter().collect())
}

#[derive(Debug, Deserialize)]
struct AttendedEgressConfig {
    proxy: AttendedProxyConfig,
    resolver: AttendedResolverConfig,
    llm: AttendedLlmConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttendedLlmConfig {
    default_provider: String,
    providers: HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttendedResolverConfig {
    upstream: String,
    readiness_host: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttendedProxyConfig {
    image: String,
    listen_port: u16,
    runtime_uid: u32,
    runtime_gid: u32,
    writable_tmpfs_mode: String,
    declared_volumes: Vec<String>,
}

struct AttendedEgressPreparation<'a> {
    state_dir: &'a Path,
    namespace: &'a str,
    resolver_image: &'a str,
}

async fn prepare_execution_egress(
    deployment: &config::ValidatedDeployment,
    request: &AttendedAuthorizationRequestV3,
    docker: &Path,
    deadline: &Deadline,
    preparation: AttendedEgressPreparation<'_>,
    cancellation: Cancellation,
) -> anyhow::Result<Option<PreparedRestrictedEgress>> {
    if request.proposal.network_resources.is_empty() {
        return Ok(None);
    }
    let system = config::SystemConfig::from_toml(&deployment.system_toml, "captured system.toml")
        .map_err(anyhow::Error::msg)?;
    let policies = deployment
        .network_policies
        .iter()
        .map(|(name, policy)| (name.clone(), policy.policy.clone()))
        .collect::<HashMap<_, _>>();
    let egress: AttendedEgressConfig = serde_json::from_value(
        deployment
            .local_agent
            .get("egress")
            .cloned()
            .context("local-agent egress configuration is unavailable")?,
    )
    .context("parse attended egress configuration")?;
    let mut restricted_policies = Vec::new();
    for resource in &request.proposal.network_resources {
        let name = policy_name(resource);
        let policy =
            config::flatten_policy(name, &policies, &system).map_err(anyhow::Error::msg)?;
        ensure!(
            !policy.unrestricted,
            "typed attended resource '{resource}' resolved to unrestricted egress"
        );
        ensure!(
            policy.docker_network.is_none(),
            "typed attended resources must use executor-managed Docker networks"
        );
        restricted_policies.push(if *resource == NetworkResourceKind::GuardedWeb {
            ensure!(
                policy.hosts.is_empty(),
                "guarded-web policy must not declare hosts"
            );
            RestrictedPolicy::GuardedWeb
        } else if *resource == NetworkResourceKind::Llm {
            let provider = request
                .execution
                .behavior
                .env
                .get("AGENT_LLM_PROVIDER")
                .or_else(|| request.execution.behavior.env.get("LLM_PROVIDER"))
                .map(String::as_str)
                .unwrap_or(&egress.llm.default_provider)
                .trim()
                .to_ascii_lowercase();
            ensure!(!provider.is_empty(), "attended LLM provider is empty");
            let hosts = egress
                .llm
                .providers
                .get(&provider)
                .with_context(|| format!("unknown attended LLM provider '{provider}'"))?;
            let hosts = config::canonical_policy_hosts(hosts).map_err(anyhow::Error::msg)?;
            let system_hosts = system
                .llm_providers
                .iter()
                .find(|configured| configured.name.eq_ignore_ascii_case(&provider))
                .with_context(|| format!("system LLM provider '{provider}' is unavailable"))?;
            ensure!(
                config::canonical_policy_hosts(&system_hosts.hosts).map_err(anyhow::Error::msg)?
                    == hosts,
                "system and attended LLM provider hosts differ"
            );
            ensure!(
                hosts.iter().all(|host| policy.hosts.contains(host)),
                "llm-only policy does not include the exact selected provider hosts"
            );
            RestrictedPolicy::Hosts(hosts)
        } else {
            ensure!(
                !policy.hosts.is_empty(),
                "typed attended resource '{resource}' resolved to an empty host set"
            );
            RestrictedPolicy::Hosts(policy.hosts)
        });
    }
    ensure!(
        egress.proxy.writable_tmpfs_mode == "0755"
            && egress.proxy.declared_volumes
                == ["/var/log/squid".to_string(), "/var/spool/squid".to_string()],
        "attended proxy runtime contract is not canonical"
    );
    RestrictedEgressRequest {
        namespace: preparation.namespace.to_string(),
        state_dir: preparation.state_dir.to_path_buf(),
        proxy: ProxyRuntimeSpec {
            image: egress.proxy.image,
            listen_port: egress.proxy.listen_port,
            uid: egress.proxy.runtime_uid,
            gid: egress.proxy.runtime_gid,
        },
        resolver: ResolverRuntimeSpec {
            image: preparation.resolver_image.to_string(),
            uid: 100,
            gid: 101,
            upstream: egress.resolver.upstream,
            readiness_host: egress.resolver.readiness_host,
        },
        policies: restricted_policies,
        cancellation,
        external_subnets: Vec::new(),
        failure_after: None,
    }
    .prepare(docker, deadline)
    .await
    .map(Some)
}

fn policy_name(resource: &NetworkResourceKind) -> &'static str {
    match resource {
        NetworkResourceKind::Llm => "llm-only",
        NetworkResourceKind::Github => "github-only",
        NetworkResourceKind::GoRegistry => "go-registry",
        NetworkResourceKind::CratesRegistry => "crates-registry",
        NetworkResourceKind::PypiRegistry => "pypi-registry",
        NetworkResourceKind::NpmRegistry => "npm-registry",
        NetworkResourceKind::GuardedWeb => "guarded-web",
    }
}

fn resolve_image_family(
    deployment: &config::ValidatedDeployment,
    image: &str,
) -> anyhow::Result<String> {
    let family = image_family(image)
        .rsplit('/')
        .next()
        .context("configured image has no family")?
        .to_string();
    let manifest = deployment
        .images
        .get(&family)
        .with_context(|| format!("image family '{family}' is not installed"))?;
    ensure!(
        manifest.image.get("name").and_then(|value| value.as_str()) == Some(family.as_str())
            && manifest.image.get("tag").and_then(|value| value.as_str()) == Some(image),
        "installed image manifest does not resolve family '{family}' to '{image}'"
    );
    Ok(family)
}

fn image_family(image: &str) -> String {
    let without_digest = image.split('@').next().unwrap_or(image);
    let slash = without_digest.rfind('/');
    match without_digest.rfind(':') {
        Some(colon) if slash.is_none_or(|slash| colon > slash) => {
            without_digest[..colon].to_string()
        }
        _ => without_digest.to_string(),
    }
}

fn tool_approval_mode(value: &str) -> anyhow::Result<ToolApprovalMode> {
    match value {
        "auto" => Ok(ToolApprovalMode::Auto),
        "gateway" => Ok(ToolApprovalMode::Dippy),
        other => anyhow::bail!("unsupported attended tool approval mode '{other}'"),
    }
}

fn requested_tool_approval_mode(value: &str) -> anyhow::Result<ToolApprovalMode> {
    match value {
        "auto" => Ok(ToolApprovalMode::Auto),
        "dippy" => Ok(ToolApprovalMode::Dippy),
        other => anyhow::bail!("unsupported attended tool approval selector '{other}'"),
    }
}

fn tool_approval_within_ceiling(selected: ToolApprovalMode, ceiling: ToolApprovalMode) -> bool {
    selected == ToolApprovalMode::Dippy || ceiling == ToolApprovalMode::Auto
}

fn tool_approval_name(value: ToolApprovalMode) -> &'static str {
    match value {
        ToolApprovalMode::Auto => "auto",
        ToolApprovalMode::Dippy => "dippy",
    }
}

fn attended_approval_bundle_identity(
    bundle: &config::EnvironmentBundle,
    mode: ToolApprovalMode,
) -> anyhow::Result<Option<AttendedApprovalBundleIdentity>> {
    match (mode, bundle.approval_bundle.as_ref()) {
        (ToolApprovalMode::Auto, None) => Ok(None),
        (ToolApprovalMode::Auto, Some(_)) => {
            anyhow::bail!("automatic environment contains approval bundle material")
        }
        (ToolApprovalMode::Dippy, None) => {
            anyhow::bail!("supervised environment has no approval bundle")
        }
        (ToolApprovalMode::Dippy, Some(identity)) => Ok(Some(AttendedApprovalBundleIdentity {
            name: identity.name.clone(),
            digest: identity.digest.clone(),
            protocol: identity.protocol.clone(),
            manifest_sha256: identity.manifest_sha256.clone(),
            gateway_commit: identity.gateway_commit.clone(),
            gateway_archive_sha256: identity.gateway_archive_sha256.clone(),
            plugin_sha256: identity.plugin_sha256.clone(),
            dippy_commit: identity.dippy_commit.clone(),
            dippy_archive_sha256: identity.dippy_archive_sha256.clone(),
            dippy_wheel_sha256: identity.dippy_wheel_sha256.clone(),
        })),
    }
}

fn canonical_resources(mut resources: Vec<NetworkResourceKind>) -> Vec<NetworkResourceKind> {
    resources.sort_by_key(ToString::to_string);
    resources.dedup();
    resources
}

fn ensure_unique_resources(resources: &[NetworkResourceKind]) -> anyhow::Result<()> {
    let mut canonical = resources.to_vec();
    canonical.sort_by_key(ToString::to_string);
    ensure!(
        !canonical.windows(2).any(|pair| pair[0] == pair[1]),
        "duplicate network resource selector"
    );
    Ok(())
}

fn selector_resources(selector: &str) -> anyhow::Result<Vec<NetworkResourceKind>> {
    let resources = match selector {
        "none" => vec![],
        "llm-only" => vec![NetworkResourceKind::Llm],
        "build" => vec![
            NetworkResourceKind::Llm,
            NetworkResourceKind::Github,
            NetworkResourceKind::GoRegistry,
            NetworkResourceKind::CratesRegistry,
            NetworkResourceKind::PypiRegistry,
            NetworkResourceKind::NpmRegistry,
        ],
        "guarded-web" => vec![NetworkResourceKind::GuardedWeb],
        other => anyhow::bail!("unsupported attended network selector '{other}'"),
    };
    Ok(canonical_resources(resources))
}

fn network_class_within_ceiling(selected: &str, ceiling: &str) -> anyhow::Result<bool> {
    let selected = selector_resources(selected)?;
    let ceiling = selector_resources(ceiling)?;
    Ok(selected.iter().all(|resource| ceiling.contains(resource)))
}

fn resolve_network_resources(
    selector: &str,
    requested: Vec<NetworkResourceKind>,
) -> anyhow::Result<Vec<NetworkResourceKind>> {
    let ceiling = selector_resources(selector)?;
    ensure_unique_resources(&requested)?;
    let requested = canonical_resources(requested);
    ensure!(
        !requested.contains(&NetworkResourceKind::GuardedWeb) || requested.len() == 1,
        "guarded_web cannot be combined with other network resources"
    );
    ensure!(
        requested.iter().all(|resource| ceiling.contains(resource)),
        "requested network resources exceed '{selector}' environment ceiling"
    );
    Ok(requested)
}

fn network_request(resources: &[NetworkResourceKind]) -> NetworkResourceRequest {
    if resources.is_empty() {
        NetworkResourceRequest::None
    } else {
        NetworkResourceRequest::Resources(resources.to_vec())
    }
}

fn authorization_path(state: &str, digest: &str) -> PathBuf {
    PathBuf::from("authorizations")
        .join(state)
        .join(format!("{digest}.json"))
}

fn update_authorization_attempt_state(
    store: &StateStore,
    path: &Path,
    authorization: &mut AuthorizationRecord,
    attempt_id: &str,
    state: LaunchState,
) -> anyhow::Result<()> {
    let attempt = authorization
        .attempts
        .iter_mut()
        .find(|attempt| attempt.attempt_id == attempt_id)
        .context("authorization receipt has no active attempt")?;
    ensure!(
        authorization.active_attempt_id.as_deref() == Some(attempt_id)
            && attempt.attempt_id == attempt_id,
        "authorization receipt active attempt mismatch"
    );
    ensure!(
        launch_state_rank(&state) >= launch_state_rank(&attempt.state),
        "authorization attempt state cannot move backwards"
    );
    attempt.state = state.clone();
    authorization.state = state;
    store.replace_json(path, authorization)
}

fn reconcile_pending_attempts(
    store: &StateStore,
    authorization_path: &Path,
    authorization: &mut AuthorizationRecord,
) -> anyhow::Result<()> {
    let mut pending = store
        .list_json::<PendingAttempt>(Path::new("attempt-journals"))?
        .into_iter()
        .filter(|attempt| {
            attempt.authorization_digest == authorization.artifact.digest
                && attempt.authorization_id == authorization.authorization_id
                && !authorization
                    .attempts
                    .iter()
                    .any(|lineage| lineage.attempt_id == attempt.lineage.attempt_id)
        })
        .collect::<Vec<_>>();
    ensure!(
        pending.len() <= 1,
        "authorization has multiple unpublished pending attempts"
    );
    let Some(pending) = pending.pop() else {
        return Ok(());
    };
    let expected_ancestry = authorization
        .attempts
        .iter()
        .map(|attempt| attempt.attempt_id.clone())
        .collect::<Vec<_>>();
    ensure!(
        pending.record.attempt_id == pending.lineage.attempt_id
            && pending.record.run_id == pending.lineage.run_id
            && pending.record.id == pending.lineage.run_id
            && pending.record.authorization_id == authorization.authorization_id
            && pending.record.authorization_digest == authorization.artifact.digest
            && pending.record.receipt_id == authorization.receipt_id
            && pending.record.attempt_ancestry == expected_ancestry
            && pending.record.parent_attempt_id == expected_ancestry.last().cloned()
            && pending.record.attempt_number == u32::try_from(expected_ancestry.len() + 1)?
            && pending.lineage.started_at == pending.record.started_at
            && pending.record.launch_state == LaunchState::Preparing
            && pending.lineage.state == LaunchState::Preparing,
        "pending attempt journal is inconsistent"
    );
    let record_path = run_record_path(&pending.record.run_id);
    match store.read_json::<RunRecord>(&record_path) {
        Ok(record) => ensure!(
            record.id == pending.record.id
                && record.run_id == pending.record.run_id
                && record.attempt_id == pending.record.attempt_id
                && record.attempt_number == pending.record.attempt_number
                && record.parent_attempt_id == pending.record.parent_attempt_id
                && record.attempt_ancestry == pending.record.attempt_ancestry
                && record.authorization_id == pending.record.authorization_id
                && record.receipt_id == pending.record.receipt_id
                && record.authorization_digest == pending.record.authorization_digest
                && record.owner_nonce == pending.record.owner_nonce
                && record.container_name == pending.record.container_name
                && record.ownership == pending.record.ownership,
            "pending attempt execution record is inconsistent"
        ),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            store.create_json(&record_path, &pending.record)?;
        }
        Err(error) => return Err(error),
    }
    ensure!(
        authorization.active_attempt_id.is_none(),
        "unpublished pending attempt conflicts with an active attempt"
    );
    ensure!(
        pending.record.attempt_number == u32::try_from(authorization.attempts.len() + 1)?,
        "pending attempt number does not extend receipt lineage"
    );
    authorization.active_attempt_id = Some(pending.lineage.attempt_id.clone());
    authorization.attempts.push(pending.lineage);
    authorization.state = LaunchState::Preparing;
    store.replace_json(authorization_path, authorization)
}

fn active_attempt_run_id<'a>(
    authorization: &'a AuthorizationRecord,
    attempt_id: &str,
) -> anyhow::Result<&'a str> {
    let mut matches = authorization
        .attempts
        .iter()
        .filter(|attempt| attempt.attempt_id == attempt_id);
    let run_id = matches
        .next()
        .context("authorization active attempt is absent from receipt lineage")?
        .run_id
        .as_str();
    ensure!(
        matches.next().is_none(),
        "authorization active attempt is duplicated in receipt lineage"
    );
    Ok(run_id)
}

fn launch_state_rank(state: &LaunchState) -> u8 {
    match state {
        LaunchState::Proposed => 0,
        LaunchState::Authorized => 1,
        LaunchState::Preparing => 2,
        LaunchState::LaunchPending => 3,
        LaunchState::Consumed => 4,
        LaunchState::Running => 5,
        LaunchState::Terminal => 6,
    }
}

fn furthest_durable_launch_state(
    record: &LaunchState,
    authorization: &LaunchState,
    lineage: &LaunchState,
) -> LaunchState {
    [record, authorization, lineage]
        .into_iter()
        .max_by_key(|state| launch_state_rank(state))
        .expect("three durable launch states")
        .clone()
}

fn owner_marker_for_record(record: &RunRecord) -> RunOwnerMarker {
    let mut ancestry = vec![record.authorization_id.clone(), record.receipt_id.clone()];
    ancestry.extend(record.attempt_ancestry.clone());
    RunOwnerMarker {
        run_id: record.run_id.clone(),
        attempt_id: record.attempt_id.clone(),
        authorization_id: record.authorization_id.clone(),
        receipt_id: record.receipt_id.clone(),
        authorization_digest: record.authorization_digest.clone(),
        nonce: record.owner_nonce.clone(),
        #[cfg(unix)]
        uid: unsafe { libc::geteuid() },
        #[cfg(not(unix))]
        uid: 0,
        #[cfg(unix)]
        gid: unsafe { libc::getegid() },
        #[cfg(not(unix))]
        gid: 0,
        mode: "attended".into(),
        ancestry,
    }
}

fn verify_run_owner_marker(run_dir: &Path, expected: &RunOwnerMarker) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(run_dir)
        .with_context(|| format!("inspect attended run directory {}", run_dir.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "attended run directory is not a real directory"
    );
    verify_owner_directory_metadata(&metadata, expected)?;
    let marker_path = run_dir.join("owner.json");
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_CLOEXEC,
    );
    let marker_file = options
        .open(&marker_path)
        .with_context(|| format!("open run owner marker {}", marker_path.display()))?;
    verify_owner_file_metadata(&marker_file.metadata()?, expected)?;
    let marker: RunOwnerMarker = serde_json::from_reader(marker_file)?;
    ensure!(
        marker == *expected,
        "attended run owner marker does not exactly match the execution journal"
    );
    Ok(())
}

#[cfg(unix)]
fn verify_owner_directory_metadata(
    metadata: &std::fs::Metadata,
    expected: &RunOwnerMarker,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    ensure!(
        metadata.uid() == expected.uid && metadata.gid() == expected.gid,
        "attended run directory ownership does not match the owner marker"
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "attended run directory is accessible by another user"
    );
    Ok(())
}

#[cfg(not(unix))]
fn verify_owner_directory_metadata(
    _metadata: &std::fs::Metadata,
    _expected: &RunOwnerMarker,
) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn verify_owner_file_metadata(
    metadata: &std::fs::Metadata,
    expected: &RunOwnerMarker,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    ensure!(
        metadata.is_file(),
        "attended owner marker is not a regular file"
    );
    ensure!(
        metadata.uid() == expected.uid && metadata.gid() == expected.gid,
        "attended owner marker ownership does not match its journal"
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "attended owner marker is accessible by another user"
    );
    Ok(())
}

#[cfg(not(unix))]
fn verify_owner_file_metadata(
    _metadata: &std::fs::Metadata,
    _expected: &RunOwnerMarker,
) -> anyhow::Result<()> {
    Ok(())
}

fn owner_staging_directory(run_dir: &Path, marker: &RunOwnerMarker) -> anyhow::Result<PathBuf> {
    Ok(run_dir
        .parent()
        .context("run directory has no parent")?
        .join(format!(".owner-{}", marker.nonce)))
}

fn recover_owner_staging_directory(run_dir: &Path, marker: &RunOwnerMarker) -> anyhow::Result<()> {
    let staging = owner_staging_directory(run_dir, marker)?;
    if run_dir.exists() {
        ensure!(
            !staging.exists(),
            "both final and staging owner directories exist"
        );
        return verify_run_owner_marker(run_dir, marker);
    }
    if !staging.exists() {
        return Ok(());
    }
    verify_run_owner_marker(&staging, marker)
        .context("staged attended owner marker is not trustworthy")?;
    std::fs::rename(&staging, run_dir)?;
    File::open(run_dir.parent().context("run directory has no parent")?)?.sync_all()?;
    verify_run_owner_marker(run_dir, marker)
}

fn create_owned_run_directory(run_dir: &Path, marker: &RunOwnerMarker) -> anyhow::Result<()> {
    ensure!(!run_dir.exists(), "attended run directory already exists");
    let parent = run_dir.parent().context("run directory has no parent")?;
    let staging = owner_staging_directory(run_dir, marker)?;
    std::fs::create_dir(&staging)?;
    set_private_directory(&staging)?;
    write_new_private_durable(
        &staging.join("owner.json"),
        &serde_json::to_vec_pretty(marker)?,
    )?;
    std::fs::rename(&staging, run_dir)?;
    File::open(parent)?.sync_all()?;
    verify_run_owner_marker(run_dir, marker)
}

fn lock_path(digest: &str) -> PathBuf {
    PathBuf::from("locks").join(format!("{digest}.lock"))
}

fn run_record_path(id: &str) -> PathBuf {
    PathBuf::from("executions").join(format!("{id}.json"))
}

fn attempt_journal_path(id: &str) -> PathBuf {
    PathBuf::from("attempt-journals").join(format!("{id}.json"))
}

fn receipt_path(id: &str) -> PathBuf {
    PathBuf::from("receipts").join(format!("{id}.json"))
}

fn pending_receipt_path(id: &str) -> PathBuf {
    PathBuf::from("receipts").join(format!(".{id}.pending.json"))
}

fn normalize_record(mut record: RunRecord) -> anyhow::Result<RunRecord> {
    validate_record_ids(&record)?;
    match record.schema.as_str() {
        RESULT_SCHEMA_V3 => ensure!(
            record.result_outcome == ResultOutcome::Unknown,
            "attended result v3 cannot contain result outcome evidence"
        ),
        RESULT_SCHEMA_V4 => {
            ensure!(
                record.result_outcome != ResultOutcome::Unknown,
                "attended result v4 is missing result outcome evidence"
            );
            return Ok(record);
        }
        _ => anyhow::bail!("unsupported attended result schema {}", record.schema),
    }
    if record.process_outcome == ProcessOutcome::NotStarted {
        record.process_outcome = match record.process_status.as_ref() {
            Some(RunStatus::Succeeded) => ProcessOutcome::Succeeded,
            Some(RunStatus::Cancelled) => ProcessOutcome::Cancelled,
            Some(RunStatus::TimedOut) => ProcessOutcome::TimedOut,
            Some(RunStatus::Failed | RunStatus::CleanupFailed) => ProcessOutcome::Failed,
            _ => ProcessOutcome::NotStarted,
        };
    }
    if record.execution_outcome == ExecutionOutcome::Pending
        && !matches!(
            record.status,
            RunStatus::Preparing
                | RunStatus::LaunchPending
                | RunStatus::Consumed
                | RunStatus::Running
        )
    {
        record.execution_outcome = match record.execution_status.as_ref().unwrap_or(&record.status)
        {
            RunStatus::Succeeded | RunStatus::Cleaned => ExecutionOutcome::Succeeded,
            RunStatus::Cancelled => ExecutionOutcome::Cancelled,
            RunStatus::TimedOut => ExecutionOutcome::TimedOut,
            _ => ExecutionOutcome::Failed,
        };
    }
    if record.cleanup_outcome == CleanupOutcome::Pending {
        record.cleanup_outcome = match record.status {
            RunStatus::CleanupFailed => CleanupOutcome::Failed,
            RunStatus::Succeeded
            | RunStatus::Failed
            | RunStatus::Cancelled
            | RunStatus::TimedOut
            | RunStatus::Cleaned => CleanupOutcome::Succeeded,
            _ => CleanupOutcome::Pending,
        };
    }
    Ok(record)
}

fn receipt_checksum(payload: &CleanupReceiptPayload) -> anyhow::Result<String> {
    Ok(protocol::sha256_hex_with_domain(
        b"agent-hub:attended-cleanup-receipt:v1\0",
        &serde_json::to_vec(payload)?,
    ))
}

fn read_receipt(store: &StateStore, id: &str) -> anyhow::Result<CleanupReceipt> {
    let receipt: CleanupReceipt = store.read_json(&receipt_path(id))?;
    verify_receipt(&receipt, id)?;
    Ok(receipt)
}

fn verify_receipt(receipt: &CleanupReceipt, id: &str) -> anyhow::Result<()> {
    ensure!(
        receipt.schema == CLEANUP_RECEIPT_SCHEMA_V1,
        "unsupported cleanup receipt schema"
    );
    ensure!(receipt.run_id == id, "cleanup receipt run ID mismatch");
    validate_run_record(&receipt.payload.record, id)?;
    ensure!(
        receipt.payload.record.id == id && receipt.payload.record.run_id == id,
        "cleanup receipt record run ID mismatch"
    );
    normalize_record(receipt.payload.record.clone())
        .context("unsupported cleanup receipt result record")?;
    ensure!(
        receipt.payload.record.status == RunStatus::Cleaned
            && receipt.payload.record.launch_state == LaunchState::Terminal
            && receipt.payload.record.cleanup_outcome == CleanupOutcome::Succeeded,
        "cleanup receipt does not contain a successfully cleaned terminal record"
    );
    ensure!(
        receipt.payload.cleanup_evidence.complete(),
        "cleanup receipt containment evidence is incomplete"
    );
    ensure!(
        receipt.payload.record.finished_at == Some(receipt.cleaned_at),
        "cleanup receipt completion time mismatch"
    );
    ensure!(
        receipt.payload.result == serde_json::to_value(&receipt.payload.record)?,
        "cleanup receipt retained result does not match its record"
    );
    ensure!(
        receipt
            .payload
            .cleanup
            .get("outcome")
            .and_then(serde_json::Value::as_str)
            == Some("succeeded"),
        "cleanup receipt document outcome mismatch"
    );
    let cleaned_at = serde_json::to_value(receipt.cleaned_at)?;
    ensure!(
        receipt
            .payload
            .cleanup
            .get("cleaned_at")
            .is_none_or(|value| value == &cleaned_at),
    );
    ensure!(
        receipt.authorization_digest == receipt.payload.record.authorization_digest,
        "cleanup receipt authorization mismatch"
    );
    ensure!(
        receipt.payload.owner == owner_marker_for_record(&receipt.payload.record),
        "cleanup receipt owner marker mismatch"
    );
    ensure!(
        receipt.payload.cleanup_evidence.complete(),
        "cleanup receipt does not contain a complete absence proof"
    );
    ensure!(
        receipt.checksum == receipt_checksum(&receipt.payload)?,
        "cleanup receipt checksum mismatch"
    );
    Ok(())
}

fn verify_receipt_for_record(receipt: &CleanupReceipt, current: &RunRecord) -> anyhow::Result<()> {
    let retained = &receipt.payload.record;
    ensure!(
        retained.id == current.id
            && retained.run_id == current.run_id
            && retained.attempt_id == current.attempt_id
            && retained.authorization_id == current.authorization_id
            && retained.receipt_id == current.receipt_id
            && retained.authorization_digest == current.authorization_digest
            && retained.owner_nonce == current.owner_nonce
            && retained.attempt_number == current.attempt_number
            && retained.parent_attempt_id == current.parent_attempt_id
            && retained.attempt_ancestry == current.attempt_ancestry,
        "cleanup receipt is not bound to the current execution journal"
    );
    Ok(())
}

fn publish_pending_receipt(
    store: &StateStore,
    id: &str,
    pending_path: &Path,
) -> anyhow::Result<()> {
    set_immutable_receipt_permissions(pending_path)?;
    let receipt_path = store.path(&receipt_path(id))?;
    let parent = receipt_path.parent().context("receipt has no parent")?;
    if receipt_path.exists() {
        let pending: CleanupReceipt = serde_json::from_reader(File::open(pending_path)?)?;
        let published: CleanupReceipt = serde_json::from_reader(File::open(&receipt_path)?)?;
        verify_receipt(&pending, id)?;
        verify_receipt(&published, id)?;
        ensure!(
            pending.checksum == published.checksum,
            "published cleanup receipt conflicts with pending receipt"
        );
    } else {
        std::fs::hard_link(pending_path, &receipt_path)
            .with_context(|| format!("publish immutable cleanup receipt for {id}"))?;
        File::open(parent)?.sync_all()?;
    }
    match std::fs::remove_file(pending_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn set_immutable_receipt_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400))?;
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn set_immutable_receipt_permissions(path: &Path) -> anyhow::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)?;
    File::open(path)?.sync_all()?;
    Ok(())
}
fn cancel_path(id: &str) -> PathBuf {
    PathBuf::from("cancellations").join(format!("{id}.json"))
}

fn validate_id(id: &str) -> anyhow::Result<()> {
    let parsed = uuid::Uuid::parse_str(id).context("invalid attended execution ID")?;
    ensure!(
        parsed.hyphenated().to_string() == id,
        "attended execution ID must be canonical lowercase hyphenated UUID"
    );
    Ok(())
}

fn validate_record_ids(record: &RunRecord) -> anyhow::Result<()> {
    for (name, id) in [
        ("id", record.id.as_str()),
        ("run_id", record.run_id.as_str()),
        ("attempt_id", record.attempt_id.as_str()),
        ("authorization_id", record.authorization_id.as_str()),
        ("receipt_id", record.receipt_id.as_str()),
        ("owner_nonce", record.owner_nonce.as_str()),
    ] {
        validate_id(id).with_context(|| format!("attended result has invalid {name}"))?;
    }
    if let Some(parent_attempt_id) = &record.parent_attempt_id {
        validate_id(parent_attempt_id).context("attended result has invalid parent_attempt_id")?;
    }
    for attempt_id in &record.attempt_ancestry {
        validate_id(attempt_id).context("attended result has invalid attempt_ancestry")?;
    }
    ensure!(
        record.id == record.run_id,
        "attended result ID does not match its run ID"
    );
    Ok(())
}

fn validate_run_record(record: &RunRecord, expected_run_id: &str) -> anyhow::Result<()> {
    validate_record_ids(record)?;
    ensure!(
        record.id == expected_run_id && record.run_id == expected_run_id,
        "execution record identity does not match requested run ID"
    );
    ensure!(
        usize::try_from(record.attempt_number)? == record.attempt_ancestry.len() + 1,
        "execution attempt number must equal ancestry length plus one"
    );
    let mut ancestry = std::collections::BTreeSet::new();
    for ancestor in &record.attempt_ancestry {
        validate_id(ancestor).context("execution attempt ancestry contains an invalid UUID")?;
        ensure!(
            ancestry.insert(ancestor),
            "execution attempt ancestry contains a duplicate UUID"
        );
        ensure!(
            ancestor != &record.attempt_id,
            "execution attempt ancestry contains the current attempt"
        );
    }
    match &record.parent_attempt_id {
        Some(parent) => {
            validate_id(parent).context("execution parent_attempt_id is invalid")?;
            ensure!(
                record.attempt_ancestry.last() == Some(parent),
                "execution parent attempt must be the final ancestry entry"
            );
        }
        None => ensure!(
            record.attempt_ancestry.is_empty(),
            "execution without a parent attempt must not have ancestry"
        ),
    }
    Ok(())
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_string)
        .context("attended paths must be valid UTF-8")
}

#[cfg(unix)]
fn read_authorization_file(path: &Path) -> anyhow::Result<(AttendedAuthorizationArtifact, String)> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    ensure!(path.is_absolute(), "authorization path must be absolute");
    ensure!(!path.as_os_str().is_empty(), "authorization path is empty");
    let canonical_path = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize authorization {}", path.display()))?;
    ensure!(
        canonical_path == path,
        "authorization path must be canonical; use '{}'",
        canonical_path.display()
    );
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options
        .open(path)
        .with_context(|| format!("open authorization {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "authorization must be a regular file");
    ensure!(
        metadata.uid() == std::fs::metadata("/proc/self")?.uid(),
        "authorization must be owned by the current user"
    );
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "authorization must be private"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let artifact = serde_json::from_slice(&bytes)?;
    Ok((artifact, path_string(&canonical_path)?))
}

#[cfg(not(unix))]
fn read_authorization_file(
    _path: &Path,
) -> anyhow::Result<(AttendedAuthorizationArtifact, String)> {
    anyhow::bail!("secure attended authorization files require Unix no-follow semantics")
}

fn write_private_durable(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    ensure!(
        !path.as_os_str().is_empty(),
        "attended output path is empty"
    );
    let parent = path.parent().context("attended output has no parent")?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .context("attended output file name is invalid")?,
        uuid::Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary)?;
    std::io::Write::write_all(&mut file, bytes)?;
    set_private_file(&temporary)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn write_new_private_durable(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    std::io::Write::write_all(&mut file, bytes)?;
    set_private_file(path)?;
    file.sync_all()?;
    File::open(path.parent().context("attended output has no parent")?)?.sync_all()?;
    Ok(())
}

fn write_private_durable_bounded(path: &Path, bytes: &[u8], limit: u64) -> anyhow::Result<()> {
    ensure!(limit > 0, "attended artifact output limit must be positive");
    ensure!(
        bytes.len() as u64 <= limit,
        "attended artifact exceeded reviewed limit of {limit} bytes"
    );
    write_private_durable(path, bytes)
}

fn serialize_pretty_bounded<T: Serialize>(value: &T, limit: u64) -> anyhow::Result<Vec<u8>> {
    ensure!(limit > 0, "attended artifact output limit must be positive");
    let mut counter = JsonCountingWriter::default();
    serde_json::to_writer_pretty(&mut counter, value)?;
    ensure!(
        counter.written <= limit,
        "attended artifact exceeded reviewed limit of {limit} bytes"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(counter.written)?);
    serde_json::to_writer_pretty(&mut bytes, value)?;
    Ok(bytes)
}

fn publish_terminal_result(record: &mut RunRecord, path: &Path, limit: u64) {
    if record.result_outcome != ResultOutcome::Failed {
        record.result_outcome = ResultOutcome::Succeeded;
    }
    let publication = attended_fault("result_publication")
        .and_then(|()| serialize_pretty_bounded(record, limit))
        .and_then(|bytes| write_private_durable_bounded(path, &bytes, limit));
    if let Err(error) = publication {
        record.result_outcome = ResultOutcome::Failed;
        record.status = RunStatus::Failed;
        record.retry_class = None;
        record.retry_eligible = false;
        let publication_error = format!("attended result publication failed: {error:#}");
        record.error = Some(match record.error.take() {
            Some(existing) => format!("{existing}; {publication_error}"),
            None => publication_error,
        });
        if let Ok(bytes) = serialize_pretty_bounded(record, limit) {
            let _ = write_private_durable_bounded(path, &bytes, limit);
        }
    }
}

#[derive(Default)]
struct JsonCountingWriter {
    written: u64,
}

impl std::io::Write for JsonCountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written = self.written.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn sync_directory_and_parent(path: &Path) -> anyhow::Result<()> {
    File::open(path)?.sync_all()?;
    File::open(path.parent().context("directory has no parent")?)?.sync_all()?;
    Ok(())
}

fn sync_private_tree(root: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(root)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "attended materialized tree root is not a real directory"
    );
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "attended materialized tree contains a symlink"
        );
        if metadata.is_dir() {
            sync_private_tree(&path)?;
        } else {
            ensure!(
                metadata.is_file(),
                "attended materialized tree contains a special file"
            );
            set_private_file(&path)?;
            File::open(&path)?.sync_all()?;
        }
    }
    set_private_directory(root)?;
    sync_directory_and_parent(root)
}

#[cfg(unix)]
fn runtime_identity(request: &AttendedAuthorizationRequestV3) -> anyhow::Result<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;

    let path = request
        .execution
        .workspaces
        .iter()
        .find(|workspace| workspace.alias == request.execution.primary_workspace)
        .map(|workspace| workspace.host_path.as_str())
        .unwrap_or(request.execution.config_root.as_str());
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("inspect attended runtime identity source '{path}'"))?;
    ensure!(
        metadata.uid() != 0 && metadata.gid() != 0,
        "attended job-code runtime requires non-root workspace ownership"
    );
    Ok((metadata.uid(), metadata.gid()))
}

fn verify_tool_approval(
    mode: ToolApprovalMode,
    relay: Option<&RelayEndpoint>,
) -> anyhow::Result<()> {
    if mode == ToolApprovalMode::Dippy {
        let relay = relay.context("supervised approval requires the host attended relay")?;
        ensure!(
            !relay.server_url.trim().is_empty(),
            "host approvals server is empty"
        );
    }
    Ok(())
}

fn relay_cleanup_requirement(mode: ToolApprovalMode, relay_started: bool) -> anyhow::Result<()> {
    ensure!(
        mode == ToolApprovalMode::Auto || relay_started,
        "supervised execution did not establish the attended relay"
    );
    Ok(())
}

fn verify_attended_approval_material(
    bundle_dir: &Path,
    mode: ToolApprovalMode,
) -> anyhow::Result<()> {
    if mode == ToolApprovalMode::Auto {
        ensure!(
            !bundle_dir.join("approval").exists(),
            "automatic environment leaked approval bundle material"
        );
        return Ok(());
    }
    for name in [
        "manifest.json",
        "agent-hub-opencode-plugin.js",
        "agent-hub-permissions.json",
        "dippy-0.2.7-py3-none-any.whl",
        "dippy.policy",
    ] {
        let path = bundle_dir.join("approval").join(name);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect bundled approval artifact {}", path.display()))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "bundled approval artifact is not a regular file"
        );
    }
    Ok(())
}

fn verify_exact_relay_mount(mounts: &[DockerMount], socket_path: &Path) -> anyhow::Result<()> {
    let parent = socket_path
        .parent()
        .context("approval relay socket has no parent")?;
    let matches = mounts
        .iter()
        .filter(|mount| mount.container_path == "/agent-hub/approval")
        .collect::<Vec<_>>();
    ensure!(
        matches.len() == 1,
        "attended relay requires one exact parent bind mount"
    );
    let mount = matches[0];
    ensure!(
        mount.host_path == parent,
        "attended relay parent bind source mismatch"
    );
    ensure!(
        mount.read_only,
        "attended relay parent bind must be read-only"
    );
    ensure!(
        mount.bind_argument()
            == format!(
                "type=bind,src={},dst=/agent-hub/approval,readonly",
                parent.to_string_lossy(),
            ),
        "attended relay parent bind is not exact"
    );
    Ok(())
}

fn verify_host_only_approval_environment(
    environment: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    for forbidden in HOST_ONLY_APPROVAL_ENVIRONMENT {
        ensure!(
            !environment.contains_key(forbidden),
            "host approval endpoint or credential variable '{forbidden}' must not enter the sandbox"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn runtime_identity(_request: &AttendedAuthorizationRequestV3) -> anyhow::Result<(u32, u32)> {
    anyhow::bail!("attended job-code preflight requires Unix ownership metadata")
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AbortTask, AttendedExecutionOutcome, FinalizationError, RelayEndpoint, RelayEpochState,
        RunOwnerMarker, RunRecord, RunStatus, apply_git_runtime_environment,
        attended_execution_outcome, merge_execution_cleanup, network_class_within_ceiling,
        provider_retry_proofs_complete, resolve_network_resources, selector_resources,
        serialize_pretty_bounded, verify_exact_relay_mount, verify_host_only_approval_environment,
        verify_recovery_relay_endpoint, verify_run_owner_marker, verify_tool_approval,
        write_new_private_durable,
    };
    use crate::state::StateStore;
    use agent_hub_executor::CleanupStatus;
    use anyhow::Context as _;
    use chrono::Utc;
    use protocol::NetworkResourceKind;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    struct IntegrationRootCleanup(PathBuf);

    impl Drop for IntegrationRootCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn supervised_tool_approval_requires_host_relay_endpoint() {
        verify_tool_approval(protocol::ToolApprovalMode::Auto, None).unwrap();
        assert!(verify_tool_approval(protocol::ToolApprovalMode::Dippy, None).is_err());
        verify_tool_approval(
            protocol::ToolApprovalMode::Dippy,
            Some(&RelayEndpoint {
                server_url: "https://approvals.example".into(),
                token: None,
            }),
        )
        .unwrap();
        super::relay_cleanup_requirement(protocol::ToolApprovalMode::Auto, false).unwrap();
        super::relay_cleanup_requirement(protocol::ToolApprovalMode::Dippy, true).unwrap();
        assert!(
            super::relay_cleanup_requirement(protocol::ToolApprovalMode::Dippy, false).is_err()
        );
    }

    #[test]
    fn relay_mount_must_be_one_exact_read_only_parent_bind() {
        let socket = PathBuf::from("/run/agent-hub/approval.sock");
        let exact = agent_hub_executor::DockerMount::new(
            socket.parent().unwrap(),
            "/agent-hub/approval",
            true,
        )
        .unwrap();
        verify_exact_relay_mount(std::slice::from_ref(&exact), &socket).unwrap();
        let writable = agent_hub_executor::DockerMount::new(
            socket.parent().unwrap(),
            "/agent-hub/approval",
            false,
        )
        .unwrap();
        assert!(verify_exact_relay_mount(&[writable], &socket).is_err());
        assert!(verify_exact_relay_mount(&[exact.clone(), exact], &socket).is_err());
    }

    #[test]
    fn relay_environment_contains_bindings_but_never_host_endpoint_or_bearer() {
        let mut environment = BTreeMap::from([
            (
                "AGENT_HUB_ATTENDED_APPROVAL_SOCKET".into(),
                protocol::ATTENDED_GATEWAY_SOCKET_PATH.into(),
            ),
            ("AGENT_HUB_ATTENDED_APPROVAL_BINDING".into(), "{}".into()),
        ]);
        verify_host_only_approval_environment(&environment).unwrap();
        environment.insert("AGENT_HUB_TOKEN".into(), "secret".into());
        assert!(verify_host_only_approval_environment(&environment).is_err());
    }

    #[test]
    fn recovery_requires_exact_normalized_nonsecret_relay_endpoint() {
        let root = std::env::current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join(format!("relay-endpoint-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let endpoint = RelayEndpoint {
            server_url: "https://approvals.example/base".into(),
            token: Some(protocol::Secret::new("host-only-secret")),
        };
        let run_epoch = uuid::Uuid::new_v4();
        let binding = protocol::AttendedGatewayBindingV1 {
            run_epoch: run_epoch.to_string(),
            authorization_digest: "a".repeat(64),
            environment_bundle_digest: "b".repeat(64),
            image_digest: format!("sha256:{}", "c".repeat(64)),
            network_digest: "d".repeat(64),
            approval_bundle_digest: "e".repeat(64),
        };
        let state = RelayEpochState {
            run_epoch,
            binding,
            plugin_nonce: "plugin-nonce".into(),
            deadline: Utc::now() + chrono::Duration::minutes(1),
            endpoint: super::relay_endpoint_identity("https://approvals.example/base", true)
                .unwrap(),
            socket: super::host_relay_socket_location(run_epoch).unwrap(),
        };
        let bytes = serde_json::to_vec(&state).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("host-only-secret"));
        std::fs::write(root.join("approval-relay-epoch.json"), bytes).unwrap();
        verify_recovery_relay_endpoint(&root, &endpoint).unwrap();
        assert!(
            verify_recovery_relay_endpoint(
                &root,
                &RelayEndpoint {
                    server_url: "https://other.example/base".into(),
                    token: endpoint.token,
                },
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires AGENT_HUB_ATTENDED_OPENCODE_INTEGRATION=1 and tester-managed Docker cleanup"]
    async fn real_docker_attended_opencode_reaches_model_initialization() {
        run_real_docker_attended_opencode_startup().await.unwrap();
    }

    #[cfg(any())]
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires explicitly approved real github.snooguts.net credentials, two existing linked worktrees, canonical attended config, and Docker cleanup consent"]
    async fn real_docker_attended_v3_private_go_multi_worktree_host_proof() {
        run_real_docker_attended_v3_private_go_multi_worktree_host_proof()
            .await
            .unwrap();
    }

    #[cfg(any())]
    async fn run_real_docker_attended_v3_private_go_multi_worktree_host_proof() -> anyhow::Result<()>
    {
        use crate::local::{RequestedWorkspace, WorkspaceAccess};
        use std::os::unix::fs::PermissionsExt as _;

        for (name, purpose) in [
            (
                "AGENT_HUB_ATTENDED_PRIVATE_GO_INTEGRATION",
                "enable this destructive-proof test",
            ),
            (
                "AGENT_HUB_ATTENDED_PRIVATE_GO_REAL_CREDENTIALS",
                "permit execution of the configured gh credential command",
            ),
            (
                "AGENT_HUB_ATTENDED_PRIVATE_GO_ALLOW_CLEANUP",
                "permit removal of test-owned Docker and attended runtime resources",
            ),
        ] {
            anyhow::ensure!(
                std::env::var(name).as_deref() == Ok("1"),
                "{name}=1 is required to {purpose}"
            );
        }
        let config_root =
            std::fs::canonicalize(std::env::var("AGENT_HUB_ATTENDED_PRIVATE_GO_CONFIG_ROOT")?)?;
        let primary = std::fs::canonicalize(std::env::var(
            "AGENT_HUB_ATTENDED_PRIVATE_GO_PRIMARY_WORKTREE",
        )?)?;
        let sibling = std::fs::canonicalize(std::env::var(
            "AGENT_HUB_ATTENDED_PRIVATE_GO_SIBLING_WORKTREE",
        )?)?;
        let state_root =
            std::fs::canonicalize(std::env::var("AGENT_HUB_ATTENDED_PRIVATE_GO_STATE_PARENT")?)?
                .join(format!(
                    "attended-private-go-{}",
                    uuid::Uuid::new_v4().simple()
                ));
        let mut cleanup_root = DisarmableIntegrationRootCleanup(Some(state_root.clone()));
        let operation = async {
        let environment = std::env::var("AGENT_HUB_ATTENDED_PRIVATE_GO_ENVIRONMENT")?;
        let image_family = std::env::var("AGENT_HUB_ATTENDED_PRIVATE_GO_IMAGE_FAMILY")?;
        let network_class = std::env::var("AGENT_HUB_ATTENDED_PRIVATE_GO_NETWORK_CLASS")?;
        let module = std::env::var("AGENT_HUB_ATTENDED_PRIVATE_GO_MODULE")?;
        anyhow::ensure!(
            module.starts_with("github.snooguts.net/"),
            "the host proof module must be on github.snooguts.net"
        );

        let git_output = |tree: &std::path::Path, args: &[&str]| -> anyhow::Result<String> {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(tree)
                .args(args)
                .output()?;
            anyhow::ensure!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(String::from_utf8(output.stdout)?.trim().to_string())
        };
        let primary_git = git_output(&primary, &["rev-parse", "--git-dir"])?;
        let sibling_git = git_output(&sibling, &["rev-parse", "--git-dir"])?;
        anyhow::ensure!(
            primary_git != ".git" && sibling_git != ".git",
            "both supplied repositories must be linked worktrees"
        );
        let sibling_head = git_output(&sibling, &["rev-parse", "HEAD"])?;
        let sibling_status = git_output(&sibling, &["status", "--porcelain=v1"])?;

        let prompt = format!(
            "This is the attended V3 private-Go host proof. In /workspace/primary, fetch the existing private module {module} with GOPROXY=direct, then run gofmt on all project Go files, go test ./..., go vet ./..., and go build ./.... Record git status --short, git diff, and git rev-parse HEAD. Read the same Git facts from /workspace/sibling, prove it is read-only by making no changes there, and verify its sentinel content. Do not commit, checkout, branch, add, reset, clean, or otherwise mutate Git metadata. Finish with one complete agent_result_v1 object whose acceptance evidence names each command."
        );
        let overlay = "github:github.snooguts.net:read_write".to_string();
        let artifact = super::build_proposal(super::ProposalInput {
            config_root,
            environment: environment.clone(),
            image_family: image_family.clone(),
            network_class: network_class.clone(),
            network_resources: vec![NetworkResourceKind::Github],
            tool_approval: "auto".into(),
            overlays: vec![overlay.clone()],
            timeout_secs: 900,
            prompt,
            workspaces: vec![
                RequestedWorkspace {
                    alias: "primary".into(),
                    path: primary.clone(),
                    access: WorkspaceAccess::ReadWrite,
                },
                RequestedWorkspace {
                    alias: "sibling".into(),
                    path: sibling.clone(),
                    access: WorkspaceAccess::ReadOnly,
                },
            ],
            context_roots: Vec::new(),
            primary_workspace: "primary".into(),
        })
        .await?;
        let request = artifact.verify_v3().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(request.proposal.network_resources == [NetworkResourceKind::Github]);
        anyhow::ensure!(request.proposal.overlays == [overlay.clone()]);
        anyhow::ensure!(request.git_views.len() == 2);
        anyhow::ensure!(request.git_views.iter().all(|view| {
            view.repository_type == protocol::GitRepositoryType::Linked
                && view.object_stores.len() == 1
        }));
        let binding: protocol::GithubOverlayBinding = serde_json::from_str(
            &request
                .execution
                .overlays
                .iter()
                .find(|candidate| candidate.name == overlay)
                .context("exact GitHub overlay is absent")?
                .config,
        )?;
        anyhow::ensure!(
            binding.host == "github.snooguts.net"
                && binding.api_base_url == "https://github.snooguts.net/api/v3"
                && binding.access == "read_write"
                && binding.network_resource == "github"
                && binding.credential_provider == overlay
                && binding.credential_target
                    == "/agent-hub/github/tokens/github.snooguts.net"
                && binding.authenticated
                && binding.private_repositories,
            "GitHub host/network/credential binding is not exact"
        );
        let provider = request
            .execution
            .credential_providers
            .iter()
            .find(|candidate| candidate.name == overlay)
            .context("exact GitHub credential provider is absent")?;
        let credential_target = "/agent-hub/github/tokens/github.snooguts.net";
        anyhow::ensure!(
            provider.provider_type == "github-token"
                && provider.command.as_deref() == Some("gh")
                && provider.args == ["auth", "token", "--hostname", "github.snooguts.net"]
                && provider.targets == [credential_target]
                && provider.access == "read_write"
                && provider.constraints.get("host").map(String::as_str)
                    == Some("github.snooguts.net")
                && provider.constraints.get("api_base_url").map(String::as_str)
                    == Some("https://github.snooguts.net/api/v3")
                && provider.constraints.get("target").map(String::as_str)
                    == Some(credential_target),
            "real GitHub credential provider command, args, target, host, or access is not exact"
        );
        let gh_status = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("gh")
                .args(["auth", "status", "--hostname", "github.snooguts.net"])
                .env("GH_PROMPT_DISABLED", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .output(),
        )
        .await
        .context("gh auth status timed out")??;
        anyhow::ensure!(
            gh_status.status.success(),
            "noninteractive gh auth status failed: {}",
            String::from_utf8_lossy(&gh_status.stderr)
        );
        let canonical_module = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("go")
                .arg("list")
                .arg("-m")
                .arg("-f={{.Path}}")
                .arg(&module)
                .current_dir(&primary)
                .env("GOTOOLCHAIN", "local")
                .env("GOPROXY", "off")
                .output(),
        )
        .await
        .context("canonical Go module validation timed out")??;
        anyhow::ensure!(
            canonical_module.status.success()
                && String::from_utf8(canonical_module.stdout)?.trim() == module,
            "private Go module is not an exact canonical requirement: {}",
            String::from_utf8_lossy(&canonical_module.stderr)
        );
        let primary_common = git_output(&primary, &["rev-parse", "--git-common-dir"])?;
        let sibling_common = git_output(&sibling, &["rev-parse", "--git-common-dir"])?;
        let resolve_common = |tree: &std::path::Path, value: &str| {
            let path = PathBuf::from(value);
            std::fs::canonicalize(if path.is_absolute() { path } else { tree.join(path) })
        };
        let primary_common = resolve_common(&primary, &primary_common)?;
        let sibling_common = resolve_common(&sibling, &sibling_common)?;
        anyhow::ensure!(primary_common == sibling_common, "worktrees do not share one common Git directory");
        fn metadata_snapshot(path: &std::path::Path) -> anyhow::Result<Vec<(PathBuf, Vec<u8>)>> {
            fn visit(root: &std::path::Path, path: &std::path::Path, values: &mut Vec<(PathBuf, Vec<u8>)>) -> anyhow::Result<()> {
                let mut entries = std::fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
                entries.sort_by_key(std::fs::DirEntry::file_name);
                for entry in entries {
                    let path = entry.path();
                    let metadata = std::fs::symlink_metadata(&path)?;
                    anyhow::ensure!(!metadata.file_type().is_symlink(), "common Git metadata contains a symlink");
                    if metadata.is_dir() {
                        visit(root, &path, values)?;
                    } else {
                        values.push((path.strip_prefix(root)?.to_path_buf(), std::fs::read(path)?));
                    }
                }
                Ok(())
            }
            let mut values = Vec::new();
            visit(path, path, &mut values)?;
            Ok(values)
        }
        let common_metadata = metadata_snapshot(&primary_common)?;

        std::fs::create_dir(&state_root)?;
        std::fs::set_permissions(&state_root, std::fs::Permissions::from_mode(0o700))?;
        let store = StateStore::open(state_root.clone())?;
        let authorization_path = state_root.join("authorization.json");
        super::write_new_private_durable(
            &authorization_path,
            &serde_json::to_vec_pretty(&artifact)?,
        )?;
        let selected_resources = vec![NetworkResourceKind::Github];
        let selected_overlays = vec![overlay.clone()];
        let selectors = || super::AuthorizationSelectors {
            digest: &artifact.digest,
            environment: &environment,
            image_family: &image_family,
            network_class: &network_class,
            network_resources: &selected_resources,
            tool_approval: "auto",
            overlays: &selected_overlays,
            timeout_secs: 900,
        };
        crate::local_authorize::authorize(&store, &authorization_path, selectors()).await?;

        // Prove the reviewed one-shot authorization survives exactly one fully
        // reconciled transient failure before any agent/container launch.
        let transient = {
            let _fault = ScopedAttendedFault::set("preparing");
            super::run(
                &store,
                &authorization_path,
                selectors(),
                super::RelayEndpointOverride {
                    server_url: None,
                    token: None,
                },
            )
            .await
        };
        anyhow::ensure!(
            transient.is_err(),
            "injected pre-agent fault unexpectedly completed"
        );

        let completed = super::run(
            &store,
            &authorization_path,
            selectors(),
            super::RelayEndpointOverride {
                server_url: None,
                token: None,
            },
        )
        .await?;
        let transient = super::list(&store)?
            .into_iter()
            .find(|record| record.id != completed.id)
            .context("reconciled transient attempt is absent")?;
        let transient_id = transient.id.clone();
        let transient_container_name = transient.container_name.clone();
        anyhow::ensure!(
            transient.retry_class == Some(super::RetryClass::Prelaunch)
                && transient.retry_eligible,
            "transient pre-agent failure was not safely retryable"
        );
        let transient = super::cleanup(
            &store,
            &transient.id,
            super::RelayEndpoint {
                server_url: "http://127.0.0.1".into(),
                token: None,
            },
        )
        .await?;
        anyhow::ensure!(transient.cleanup_outcome == super::CleanupOutcome::Succeeded);
        let transient_receipt = super::read_receipt(&store, &transient.id)?;
        anyhow::ensure!(
            transient_receipt.payload.cleanup["journal_proof"]["kind"]
                == "prelaunch_no_resources"
        );
        anyhow::ensure!(
            completed.process_outcome == super::ProcessOutcome::Succeeded
                && completed.agent_outcome == super::AgentOutcome::Succeeded
                && completed.execution_outcome == super::ExecutionOutcome::Succeeded
                && completed.agent_final.as_ref().is_some_and(|result| result.complete),
            "attended host proof did not produce a structured complete result: {:?}",
            completed.error
        );
        // The parser contract also explicitly distinguishes a valid incomplete
        // terminal envelope; incomplete output is never retryable success.
        let incomplete_event = serde_json::json!({
            "type":"text",
            "part":{"text":serde_json::json!({
                "schema":"agent_result_v1","complete":false,"summary":"blocked",
                "acceptance_results":[],"blockers":["fixture blocker"],
                "artifact_references":[]
            }).to_string()}
        })
        .to_string();
        anyhow::ensure!(
            super::parse_agent_result_events(&incomplete_event).outcome
                == super::AgentOutcome::Incomplete
        );

        let completed = super::cleanup(
            &store,
            &completed.id,
            super::RelayEndpoint {
                server_url: "http://127.0.0.1".into(),
                token: None,
            },
        )
        .await?;
        anyhow::ensure!(completed.cleanup_outcome == super::CleanupOutcome::Succeeded);
        let receipt = super::read_receipt(&store, &completed.id)?;
        anyhow::ensure!(receipt.payload.cleanup_evidence.complete());
        for id in [&transient_id, &completed.id] {
            anyhow::ensure!(!store.path(&PathBuf::from("runs").join(id))?.exists(), "attended run directory leaked: {id}");
        }
        for name in [&transient_container_name, &completed.container_name] {
            let inspect = tokio::process::Command::new("docker")
                .args(["container", "inspect", name])
                .output()
                .await?;
            let stderr = String::from_utf8_lossy(&inspect.stderr).to_ascii_lowercase();
            anyhow::ensure!(
                !inspect.status.success()
                    && (stderr.contains("no such container") || stderr.contains("no such object")),
                "attended container absence was not proven exactly for {name}: {}",
                String::from_utf8_lossy(&inspect.stderr)
            );
            let by_name = tokio::process::Command::new("docker")
                .args(["ps", "-aq", "--filter", &format!("name=^/{name}$")])
                .output()
                .await?;
            anyhow::ensure!(
                by_name.status.success() && by_name.stdout.iter().all(u8::is_ascii_whitespace),
                "attended container remains discoverable by name: {name}"
            );
        }
        anyhow::ensure!(git_output(&sibling, &["rev-parse", "HEAD"])? == sibling_head);
        anyhow::ensure!(git_output(&sibling, &["status", "--porcelain=v1"])? == sibling_status);
        anyhow::ensure!(metadata_snapshot(&primary_common)? == common_metadata, "shared Git metadata changed during attended proof");

        Ok(())
        }
        .await;
        if operation.is_err() {
            cleanup_root.disarm();
        }
        operation
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires the attended OpenCode integration environment and a built plugin artifact"]
    async fn real_docker_supervised_opencode_routes_permissions() {
        run_real_docker_supervised_opencode_permissions(false)
            .await
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "requires the attended OpenCode integration environment, a built plugin artifact, and real OpenAI credentials"]
    async fn real_docker_supervised_opencode_reaches_real_llm() {
        run_real_docker_supervised_opencode_permissions(true)
            .await
            .unwrap();
    }

    #[cfg(unix)]
    async fn run_real_docker_attended_opencode_startup() -> anyhow::Result<()> {
        anyhow::ensure!(
            std::env::var("AGENT_HUB_ATTENDED_OPENCODE_INTEGRATION").as_deref() == Ok("1"),
            "AGENT_HUB_ATTENDED_OPENCODE_INTEGRATION=1 is required"
        );
        anyhow::ensure!(
            std::env::var("AGENT_HUB_ATTENDED_OPENCODE_ALLOW_CLEANUP").as_deref() == Ok("1"),
            "AGENT_HUB_ATTENDED_OPENCODE_ALLOW_CLEANUP=1 is required"
        );
        let config_root = std::fs::canonicalize(PathBuf::from(std::env::var(
            "AGENT_HUB_ATTENDED_OPENCODE_CONFIG_ROOT",
        )?))?;
        let integration_root = std::fs::canonicalize(PathBuf::from(std::env::var(
            "AGENT_HUB_ATTENDED_OPENCODE_ROOT",
        )?))?;
        let expected_image_id = std::env::var("AGENT_HUB_ATTENDED_OPENCODE_IMAGE_ID")?;
        let deployment =
            config::validate_authoritative_config_root(&config_root).map_err(anyhow::Error::msg)?;
        let bundle = deployment
            .environment_bundles
            .get("code-auto")
            .context("code-auto environment bundle is required")?;
        let runner = deployment
            .runner_profiles
            .resolve_agent_profile(None)
            .map_err(anyhow::Error::msg)?;
        let root = integration_root.join(format!(
            "attended-opencode-startup-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&root)?;
        super::set_private_directory(&root)?;
        let bundle_dir = root.join("config");
        let data_dir = root.join("data");
        let state_dir = root.join("state");
        let cache_dir = root.join("cache");
        let workspace = root.join("workspace");
        let project_config_mask = root.join("project-config-mask");
        bundle.write_to(&bundle_dir).map_err(anyhow::Error::msg)?;
        for directory in [
            &data_dir,
            &state_dir,
            &cache_dir,
            &workspace,
            &project_config_mask,
        ] {
            std::fs::create_dir(directory)?;
            super::set_private_directory(directory)?;
        }
        std::fs::create_dir(state_dir.join("home"))?;
        std::fs::create_dir(data_dir.join("opencode"))?;
        super::set_private_directory(&data_dir.join("opencode"))?;
        std::fs::create_dir_all(workspace.join(".opencode/plugin"))?;
        std::fs::create_dir_all(workspace.join(".opencode/skills/hostile"))?;
        std::fs::write(
            workspace.join("opencode.json"),
            br#"{"model":"anthropic/hostile","plugin":["./.opencode/plugin/hostile.js"]}"#,
        )?;
        std::fs::write(
            workspace.join(".opencode/plugin/hostile.js"),
            r#"import fs from "node:fs"; fs.writeFileSync("/workspace/primary/hostile-plugin", "loaded"); export default async () => ({});"#,
        )?;
        std::fs::write(
            workspace.join(".opencode/skills/hostile/SKILL.md"),
            "---\nname: hostile\ndescription: hostile\n---\nhostile\n",
        )?;
        let expires = std::time::SystemTime::now()
            .checked_add(Duration::from_secs(3600))
            .context("synthetic OAuth expiry overflow")?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();
        let auth_path = data_dir.join("opencode/auth.json");
        std::fs::write(
            &auth_path,
            serde_json::to_vec(&serde_json::json!({
                "openai": {
                    "type": "oauth",
                    "access": "synthetic-attended-startup-token",
                    "refresh": "",
                    "expires": expires,
                    "accountId": "synthetic-attended-account"
                }
            }))?,
        )?;
        super::set_private_file(&auth_path)?;

        let docker = which::which("docker")?;
        let image_id = agent_hub_executor::inspect_image_identity(
            &docker,
            config::DURABLE_OPENCODE_IMAGE,
            &agent_hub_executor::Deadline::new(Some(15))?,
            &agent_hub_executor::Cancellation::never(),
        )
        .await?;
        anyhow::ensure!(
            image_id == expected_image_id,
            "Docker image identity changed"
        );
        let container_name = format!(
            "agent-hub-attended-opencode-test-{}",
            uuid::Uuid::new_v4().simple()
        );
        let ownership = agent_hub_executor::DockerOwnership::new(BTreeMap::from([
            ("agent-hub.owner".to_string(), "attended-test".to_string()),
            ("agent-hub.run".to_string(), container_name.clone()),
        ]))?;
        let script = r#"set -eu
test -r /opencode/config/opencode/opencode.json
test -r /opencode/config/opencode/agents/coder.md
test -r /opencode/config/opencode/skills/local-workspaces/SKILL.md
test "$(stat -c %a /opencode/data/opencode)" = 700
! touch /opencode/config/opencode/hostile-write
touch /opencode/data/writable /opencode/state/writable /opencode/cache/writable
/usr/local/bin/opencode debug config > /workspace/primary/debug-config.json
/usr/local/bin/opencode debug skill > /workspace/primary/debug-skills.json
set +e
timeout 20 /usr/local/bin/opencode run "Reply with the word ready." > /workspace/primary/model-initialization.log 2>&1
status=$?
test "$(stat -c %a /opencode/data/opencode)" = 700
printf '%s\n' "$status" > /workspace/primary/model-initialization.status
exit 0
"#;
        let plan = agent_hub_executor::DockerExecutionPlan {
            image_identity: image_id,
            container_name: container_name.clone(),
            entrypoint: "/usr/bin/sh".to_string(),
            command_args: vec!["-lc".to_string(), script.to_string()],
            mounts: vec![
                agent_hub_executor::DockerMount::new(
                    &bundle_dir,
                    "/opencode/config/opencode",
                    true,
                )?,
                agent_hub_executor::DockerMount::new(&data_dir, "/opencode/data", false)?,
                agent_hub_executor::DockerMount::new(&state_dir, "/opencode/state", false)?,
                agent_hub_executor::DockerMount::new(&cache_dir, "/opencode/cache", false)?,
                agent_hub_executor::DockerMount::new(&workspace, "/workspace/primary", false)?,
                agent_hub_executor::DockerMount::new(
                    &project_config_mask,
                    "/workspace/primary/.opencode",
                    true,
                )?,
            ],
            environment: runner.opencode.trusted_env.clone(),
            network: agent_hub_executor::NetworkMode::None,
            dns: Vec::new(),
            extra_hosts: Vec::new(),
            ownership: ownership.clone(),
            user: format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
                libc::getegid()
            }),
            workdir: Some("/workspace/primary".to_string()),
            memory_limit: "2g".to_string(),
            cpu_limit: "2".to_string(),
            pids_limit: protocol::APPROVED_EXECUTION_PIDS_LIMIT,
            read_only: true,
            tmpfs: vec!["/tmp:rw,size=256m".to_string()],
        };
        let output = tokio::process::Command::new(&docker)
            .args(plan.materialize_args()?)
            .output()
            .await;
        let cleanup = agent_hub_executor::DockerCli::new(&docker)
            .remove_owned(&container_name, &ownership, Duration::from_secs(15))
            .await;
        cleanup?;
        let output = output?;
        let model_log =
            std::fs::read_to_string(workspace.join("model-initialization.log")).unwrap_or_default();
        anyhow::ensure!(
            output.status.success(),
            "attended OpenCode startup failed: status={:?} stdout={:?} stderr={:?} model_log={:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            model_log
        );
        let debug_config = std::fs::read_to_string(workspace.join("debug-config.json"))?;
        let debug_skills = std::fs::read_to_string(workspace.join("debug-skills.json"))?;
        let model_status = std::fs::read_to_string(workspace.join("model-initialization.status"))?;
        anyhow::ensure!(
            debug_config.contains("openai/gpt-5.6-sol") && !debug_config.contains("hostile"),
            "hostile project configuration affected attended OpenCode"
        );
        anyhow::ensure!(
            debug_skills.contains("local-workspaces") && !debug_skills.contains("hostile"),
            "hostile project skill affected attended OpenCode"
        );
        anyhow::ensure!(
            model_log.contains("gpt-5.6-sol") && !model_log.contains("EROFS"),
            "OpenCode did not reach authorized model initialization: {model_log:?}"
        );
        anyhow::ensure!(
            !matches!(model_status.trim(), "126" | "127"),
            "OpenCode executable did not start"
        );
        anyhow::ensure!(
            !workspace.join("hostile-plugin").exists()
                && !bundle_dir.join("hostile-write").exists()
                && data_dir.join("writable").is_file()
                && state_dir.join("writable").is_file()
                && cache_dir.join("writable").is_file(),
            "attended immutable or writable XDG mount contract failed"
        );
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    async fn run_real_docker_supervised_opencode_permissions(real_llm: bool) -> anyhow::Result<()> {
        use crate::attended_relay::{
            AttendedRelay, AttendedRelayConfig, host_relay_socket_location,
        };
        use agent_hub_executor::{Cancellation, ExecutionCancellationContext};
        use protocol::{
            Approval, ApprovalRequest, ApprovalResponse, ApprovalStatus, ApprovalWaitResponse,
        };
        use std::collections::{HashMap, VecDeque};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::Mutex;

        anyhow::ensure!(
            std::env::var("AGENT_HUB_ATTENDED_OPENCODE_INTEGRATION").as_deref() == Ok("1"),
            "AGENT_HUB_ATTENDED_OPENCODE_INTEGRATION=1 is required"
        );
        anyhow::ensure!(
            std::env::var("AGENT_HUB_ATTENDED_OPENCODE_ALLOW_CLEANUP").as_deref() == Ok("1"),
            "AGENT_HUB_ATTENDED_OPENCODE_ALLOW_CLEANUP=1 is required"
        );
        let config_root = std::fs::canonicalize(PathBuf::from(std::env::var(
            "AGENT_HUB_ATTENDED_OPENCODE_CONFIG_ROOT",
        )?))?;
        let integration_root = std::fs::canonicalize(PathBuf::from(std::env::var(
            "AGENT_HUB_ATTENDED_OPENCODE_ROOT",
        )?))?;
        let expected_image_id = std::env::var("AGENT_HUB_ATTENDED_OPENCODE_IMAGE_ID")?;
        let plugin_artifact = std::fs::canonicalize(PathBuf::from(std::env::var(
            "AGENT_HUB_ATTENDED_OPENCODE_PLUGIN_ARTIFACT",
        )?))?;
        let deployment =
            config::validate_authoritative_config_root(&config_root).map_err(anyhow::Error::msg)?;
        let bundle = deployment
            .environment_bundles
            .get("code-supervised")
            .context("code-supervised environment bundle is required")?;
        let approval_bundle = bundle
            .approval_bundle
            .as_ref()
            .context("code-supervised approval bundle is required")?;
        let runner = deployment
            .runner_profiles
            .resolve_agent_profile(None)
            .map_err(anyhow::Error::msg)?;
        let docker = which::which("docker")?;
        let image_id = agent_hub_executor::inspect_image_identity(
            &docker,
            config::DURABLE_OPENCODE_IMAGE,
            &agent_hub_executor::Deadline::new(Some(15))?,
            &Cancellation::never(),
        )
        .await?;
        anyhow::ensure!(
            image_id == expected_image_id,
            "Docker image identity changed"
        );

        let root = integration_root.join(format!(
            "supervised-opencode-permissions-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&root)?;
        let root_cleanup = IntegrationRootCleanup(root.clone());
        super::set_private_directory(&root)?;
        let bundle_dir = root.join("config");
        let data_dir = root.join("data");
        let state_dir = root.join("state");
        let cache_dir = root.join("cache");
        let workspace = root.join("workspace");
        bundle.write_to(&bundle_dir).map_err(anyhow::Error::msg)?;
        std::fs::copy(
            &plugin_artifact,
            bundle_dir.join("approval/agent-hub-opencode-plugin.js"),
        )?;
        for directory in [&data_dir, &state_dir, &cache_dir, &workspace] {
            std::fs::create_dir(directory)?;
            super::set_private_directory(directory)?;
        }
        std::fs::create_dir(state_dir.join("home"))?;
        std::fs::create_dir(data_dir.join("opencode"))?;
        super::set_private_directory(&data_dir.join("opencode"))?;
        std::fs::create_dir(data_dir.join("opencode/log"))?;
        super::set_private_directory(&data_dir.join("opencode/log"))?;
        if real_llm {
            let auth_path = std::fs::canonicalize(PathBuf::from(std::env::var(
                "AGENT_HUB_ATTENDED_OPENCODE_AUTH",
            )?))?;
            let materialized_auth = data_dir.join("opencode/auth.json");
            std::fs::copy(auth_path, &materialized_auth)?;
            super::set_private_file(&materialized_auth)?;
        }

        #[derive(Default)]
        struct FakeState {
            approvals: HashMap<uuid::Uuid, Approval>,
            registrations: Vec<String>,
            cancellations: usize,
            terminal_verifications: usize,
            model_calls: VecDeque<(String, serde_json::Value)>,
        }

        async fn fake_server(listener: tokio::net::TcpListener, state: Arc<Mutex<FakeState>>) {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut byte = [0u8; 1];
                    while !request.ends_with(b"\r\n\r\n") {
                        if stream.read_exact(&mut byte).await.is_err() || request.len() >= 64 * 1024
                        {
                            return;
                        }
                        request.push(byte[0]);
                    }
                    let headers = String::from_utf8_lossy(&request);
                    let request_line = headers.lines().next().unwrap_or_default().to_string();
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0; content_length];
                    if stream.read_exact(&mut body).await.is_err() {
                        return;
                    }

                    let (content_type, response) = if request_line
                        .starts_with("POST /v1/responses ")
                    {
                        let model_request: serde_json::Value = match serde_json::from_slice(&body) {
                            Ok(request) => request,
                            Err(_) => return,
                        };
                        let available_tools = model_request
                            .get("tools")
                            .and_then(serde_json::Value::as_array)
                            .map(|tools| {
                                tools
                                    .iter()
                                    .filter_map(|tool| {
                                        tool.get("name").and_then(serde_json::Value::as_str)
                                    })
                                    .collect::<std::collections::BTreeSet<_>>()
                            })
                            .unwrap_or_default();
                        let call = if available_tools.is_empty() {
                            None
                        } else {
                            let mut state = state.lock().await;
                            state
                                .model_calls
                                .iter()
                                .position(|(name, _)| available_tools.contains(name.as_str()))
                                .and_then(|index| state.model_calls.remove(index))
                        };
                        let chunks = if let Some((tool_name, arguments)) = call {
                            let item_id = format!("item-{}", uuid::Uuid::new_v4().simple());
                            let call_id = format!("call-{}", uuid::Uuid::new_v4().simple());
                            let arguments = arguments.to_string();
                            vec![
                                serde_json::json!({
                                    "type":"response.created",
                                    "response":{"id":format!("response-{}", uuid::Uuid::new_v4().simple()),"created_at":1787932800,"model":"gpt-5.2","service_tier":null}
                                }),
                                serde_json::json!({
                                    "type":"response.output_item.added",
                                    "output_index":0,
                                    "item":{"type":"function_call","id":item_id,"call_id":call_id,"name":tool_name,"arguments":"","status":"in_progress"}
                                }),
                                serde_json::json!({
                                    "type":"response.function_call_arguments.delta",
                                    "item_id":item_id,
                                    "output_index":0,
                                    "delta":arguments
                                }),
                                serde_json::json!({
                                    "type":"response.function_call_arguments.done",
                                    "item_id":item_id,
                                    "output_index":0,
                                    "arguments":arguments
                                }),
                                serde_json::json!({
                                    "type":"response.output_item.done",
                                    "output_index":0,
                                    "item":{"type":"function_call","id":item_id,"call_id":call_id,"name":tool_name,"arguments":arguments,"status":"completed"}
                                }),
                                serde_json::json!({
                                    "type":"response.completed",
                                    "response":{"status":"completed","incomplete_details":null,"output":[{"type":"function_call","id":item_id,"call_id":call_id,"name":tool_name,"arguments":arguments,"status":"completed"}],"usage":{"input_tokens":1,"input_tokens_details":null,"output_tokens":1,"output_tokens_details":null},"service_tier":null}
                                }),
                            ]
                        } else {
                            vec![
                                serde_json::json!({
                                    "type":"response.created",
                                    "response":{"id":"response-complete","created_at":1787932800,"model":"gpt-5.2","service_tier":null}
                                }),
                                serde_json::json!({
                                    "type":"response.output_item.added",
                                    "output_index":0,
                                    "item":{"type":"message","id":"item-complete","status":"in_progress","role":"assistant","content":[]}
                                }),
                                serde_json::json!({
                                    "type":"response.content_part.added",
                                    "item_id":"item-complete",
                                    "output_index":0,
                                    "content_index":0,
                                    "part":{"type":"output_text","text":"","annotations":[]}
                                }),
                                serde_json::json!({
                                    "type":"response.output_text.delta",
                                    "item_id":"item-complete",
                                    "output_index":0,
                                    "content_index":0,
                                    "delta":"done",
                                    "logprobs":null
                                }),
                                serde_json::json!({
                                    "type":"response.completed",
                                    "response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":1}}
                                }),
                            ]
                        };
                        let mut response = chunks
                            .into_iter()
                            .map(|chunk| format!("data: {chunk}\n\n"))
                            .collect::<String>();
                        response.push_str("data: [DONE]\n\n");
                        ("text/event-stream", response.into_bytes())
                    } else if request_line.starts_with("GET /fixture-web ") {
                        ("text/plain", b"fixture web response".to_vec())
                    } else if request_line.starts_with("POST /api/v1/hooks/approval ") {
                        let request: ApprovalRequest = match serde_json::from_slice(&body) {
                            Ok(request) => request,
                            Err(_) => return,
                        };
                        let command = request
                            .tool_input
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let status = match command.as_str() {
                            "touch approved-marker" => ApprovalStatus::Approved {
                                message: Some("fixture approved".into()),
                            },
                            "touch denied-marker" => ApprovalStatus::Denied {
                                reason: "fixture denied".into(),
                            },
                            "touch pending-marker" => ApprovalStatus::Pending,
                            _ => ApprovalStatus::Approved {
                                message: Some("fixture non-shell approval".into()),
                            },
                        };
                        let registration = if command.is_empty() {
                            request.tool.to_string()
                        } else {
                            command
                        };
                        let id = uuid::Uuid::new_v4();
                        let approval = Approval {
                            id,
                            request_id: request.id,
                            request_digest: request.request_digest,
                            session_id: request.session_id,
                            session_display_name: request.session_display_name,
                            project: "primary".into(),
                            tool: request.tool,
                            tool_input: request.tool_input,
                            provider: request.provider,
                            request_type: request.request_type,
                            context: request.context,
                            created_at: Utc::now(),
                            status: status.clone(),
                        };
                        let mut state = state.lock().await;
                        state.registrations.push(registration);
                        state.approvals.insert(id, approval);
                        (
                            "application/json",
                            serde_json::to_vec(&ApprovalResponse { id, status }).unwrap(),
                        )
                    } else {
                        let path = request_line.split_whitespace().nth(1).unwrap_or_default();
                        let Some(id) = path
                            .split('/')
                            .find_map(|part| uuid::Uuid::parse_str(part).ok())
                        else {
                            return;
                        };
                        let mut state = state.lock().await;
                        if request_line.starts_with("POST ") && path.ends_with("/resolve") {
                            let Ok(resolve) =
                                serde_json::from_slice::<protocol::ApprovalResolveRequest>(&body)
                            else {
                                return;
                            };
                            if resolve.decision != protocol::ApprovalDecision::Cancel
                                || resolve.message.is_some()
                            {
                                return;
                            }
                            let Some(approval) = state.approvals.get_mut(&id) else {
                                return;
                            };
                            approval.status = ApprovalStatus::Cancelled;
                            state.cancellations += 1;
                        }
                        if path.ends_with("/wait") {
                            let Some(approval) = state.approvals.get(&id).cloned() else {
                                return;
                            };
                            let status = approval.status.clone();
                            (
                                "application/json",
                                serde_json::to_vec(&ApprovalWaitResponse { status }).unwrap(),
                            )
                        } else {
                            let Some(approval) = state.approvals.get(&id).cloned() else {
                                return;
                            };
                            if approval.status.is_resolved() {
                                state.terminal_verifications += 1;
                            }
                            ("application/json", serde_json::to_vec(&approval).unwrap())
                        }
                    };
                    let header = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        response.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(&response).await;
                });
            }
        }

        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await?;
        let server_port = listener.local_addr()?.port();
        let server_url = format!("http://127.0.0.1:{server_port}");
        let fake_state = Arc::new(Mutex::new(FakeState {
            model_calls: VecDeque::from([
                (
                    "read".to_string(),
                    serde_json::json!({"filePath":"native-read.txt","offset":1,"limit":10}),
                ),
                (
                    "apply_patch".to_string(),
                    serde_json::json!({"patchText":"*** Begin Patch\n*** Add File: native-patch.txt\n+patched\n*** End Patch"}),
                ),
                (
                    "bash".to_string(),
                    serde_json::json!({"command":"git status","description":"local policy"}),
                ),
                (
                    "bash".to_string(),
                    serde_json::json!({"command":"touch approved-marker","description":"remote approval"}),
                ),
                (
                    "bash".to_string(),
                    serde_json::json!({"command":"touch denied-marker","description":"remote denial"}),
                ),
                (
                    "bash".to_string(),
                    serde_json::json!({"command":"touch pending-marker","description":"remote cancellation"}),
                ),
                (
                    "webfetch".to_string(),
                    serde_json::json!({"url":format!("http://host.docker.internal:{server_port}/fixture-web"),"format":"text","timeout":5}),
                ),
                (
                    "task".to_string(),
                    serde_json::json!({"description":"fixture subagent","prompt":"Reply done","subagent_type":"general"}),
                ),
                (
                    "write".to_string(),
                    serde_json::json!({"filePath":"native-write.txt","content":"before\n"}),
                ),
                (
                    "edit".to_string(),
                    serde_json::json!({"filePath":"native-write.txt","oldString":"before","newString":"after","replaceAll":false}),
                ),
            ]),
            ..FakeState::default()
        }));
        let _server = AbortTask(tokio::spawn(fake_server(listener, Arc::clone(&fake_state))));
        let opencode_config_path = bundle_dir.join("fixture-opencode.json");
        let mut opencode_config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(bundle_dir.join("opencode.json"))?)?;
        opencode_config["model"] = serde_json::json!("openai/gpt-5.2");
        opencode_config["small_model"] = serde_json::json!("openai/gpt-5.2");
        opencode_config["provider"] = serde_json::json!({
            "openai": {
                "options": {
                    "apiKey": "supervised-fixture-key",
                    "baseURL": format!("http://host.docker.internal:{server_port}/v1")
                }
            }
        });
        opencode_config["permission"] = serde_json::json!({"*":"allow"});
        if let Some(agents) = opencode_config["agent"].as_object_mut() {
            for agent in agents.values_mut() {
                agent["permission"] = serde_json::json!({"*":"allow"});
            }
        }
        std::fs::write(&opencode_config_path, serde_json::to_vec(&opencode_config)?)?;
        let run_epoch = uuid::Uuid::new_v4();
        let socket = host_relay_socket_location(run_epoch)?;
        let binding = protocol::AttendedGatewayBindingV1 {
            run_epoch: run_epoch.to_string(),
            authorization_digest: "a".repeat(64),
            environment_bundle_digest: bundle.digest.clone(),
            image_digest: image_id.clone(),
            network_digest: "b".repeat(64),
            approval_bundle_digest: approval_bundle.digest.clone(),
        };
        let run_deadline = Utc::now() + chrono::Duration::minutes(5);
        let effects =
            ExecutionCancellationContext::new(&root.join("effects"), Cancellation::never())?;
        let container_name = format!(
            "agent-hub-supervised-opencode-test-{}",
            uuid::Uuid::new_v4().simple()
        );
        let ownership = agent_hub_executor::DockerOwnership::new(BTreeMap::from([
            ("agent-hub.owner".to_string(), "supervised-test".to_string()),
            ("agent-hub.run".to_string(), container_name.clone()),
        ]))?;
        let mut script = r#"set -eu
test -z "${AGENT_HUB_SERVER+x}"
test -z "${AGENT_HUB_TOKEN+x}"
test -z "${AGENT_HUB_APPROVALS_SERVER+x}"
test -z "${AGENT_HUB_APPROVALS_TOKEN+x}"
test -z "${OPENCODE_PURE+x}"
test -z "${OPENCODE_DISABLE_DEFAULT_PLUGINS+x}"
if ! /usr/local/bin/node -e 'import("file:///opencode/config/opencode/approval/agent-hub-opencode-plugin.js").then(module => { if (typeof module.default !== "function") throw new Error("bundled OpenCode plugin default export is not a function") })' 2> plugin-import.err; then
  cat plugin-import.err >&2
  exit 1
fi
printf 'read fixture\n' > native-read.txt
set +e
OPENCODE_DISABLE_DEFAULT_PLUGINS=1 OPENCODE_CONFIG=/opencode/config/opencode/fixture-opencode.json timeout 10 /usr/local/bin/opencode --print-logs --log-level DEBUG run --title fixture --model openai/gpt-5.6-sol 'Use Bash as instructed.' > opencode.log 2>&1
first_status=$?
set -e
test "$first_status" = 124
OPENCODE_DISABLE_DEFAULT_PLUGINS=1 OPENCODE_CONFIG=/opencode/config/opencode/fixture-opencode.json timeout 60 /usr/local/bin/opencode --print-logs --log-level DEBUG run --title fixture --model openai/gpt-4.1 'Use file tools as instructed.' >> opencode.log 2>&1
! grep -q 'schema rejection' opencode.log
test -e approved-marker
test ! -e denied-marker
test ! -e pending-marker
test "$(cat native-patch.txt)" = patched
test "$(cat native-write.txt)" = after
"#.to_string();
        if real_llm {
            script.push_str(r#"env -u OPENAI_API_KEY timeout 120 /usr/local/bin/opencode --print-logs --log-level DEBUG run 'Reply with exactly: ready. Do not use tools.' > real-llm.log 2>&1
grep -qx 'ready\|ready\.' real-llm.log
! grep -q 'schema rejection\|UnknownError\|ProviderModelNotFoundError' real-llm.log
"#);
        }
        let mut environment = config::supervised_opencode_runtime_env(&runner.opencode.trusted_env)
            .map_err(anyhow::Error::msg)?;
        environment.insert("AGENT_HUB_APPROVAL_MODE".into(), "attended_socket".into());
        environment.insert(
            "AGENT_HUB_ATTENDED_APPROVAL_SOCKET".into(),
            protocol::ATTENDED_GATEWAY_SOCKET_PATH.into(),
        );
        environment.insert(
            "AGENT_HUB_ATTENDED_APPROVAL_BINDING".into(),
            serde_json::to_string(&binding)?,
        );
        environment.insert(
            "AGENT_HUB_ATTENDED_RUN_DEADLINE".into(),
            run_deadline.to_rfc3339(),
        );
        environment.insert(
            "AGENT_HUB_CONFIG".into(),
            "/opencode/config/opencode/approval/agent-hub-permissions.json".into(),
        );
        let plugin_nonce = run_epoch.simple().to_string();
        environment.insert("AGENT_HUB_PLUGIN_RUN_NONCE".into(), plugin_nonce.clone());
        environment.insert("OPENAI_API_KEY".into(), "supervised-fixture-key".into());
        super::verify_host_only_approval_environment(&environment)?;
        let mounts = vec![
            agent_hub_executor::DockerMount::new(&bundle_dir, "/opencode/config/opencode", true)?,
            agent_hub_executor::DockerMount::new(&data_dir, "/opencode/data", false)?,
            agent_hub_executor::DockerMount::new(&state_dir, "/opencode/state", false)?,
            agent_hub_executor::DockerMount::new(&cache_dir, "/opencode/cache", false)?,
            agent_hub_executor::DockerMount::new(&workspace, "/workspace/primary", false)?,
            agent_hub_executor::DockerMount::new(
                socket
                    .socket_path
                    .parent()
                    .context("relay socket has no parent")?,
                "/agent-hub/approval",
                true,
            )?,
        ];
        super::verify_exact_relay_mount(&mounts, &socket.socket_path)?;
        let plan = agent_hub_executor::DockerExecutionPlan {
            image_identity: image_id.clone(),
            container_name: container_name.clone(),
            entrypoint: "/usr/bin/sh".into(),
            command_args: vec!["-lc".into(), script],
            mounts,
            environment,
            network: agent_hub_executor::NetworkMode::Unrestricted,
            dns: Vec::new(),
            extra_hosts: vec!["host.docker.internal:host-gateway".into()],
            ownership: ownership.clone(),
            user: format!("{}:{}", unsafe { libc::geteuid() }, unsafe {
                libc::getegid()
            }),
            workdir: Some("/workspace/primary".into()),
            memory_limit: "2g".into(),
            cpu_limit: "2".into(),
            pids_limit: protocol::APPROVED_EXECUTION_PIDS_LIMIT,
            read_only: true,
            tmpfs: vec!["/tmp:rw,size=256m".into()],
        };
        let docker_args = plan.materialize_args()?;
        let mut relay = AttendedRelay::start(
            AttendedRelayConfig {
                socket_path: socket.socket_path.clone(),
                binding: binding.clone(),
                approval_bundle: protocol::AttendedApprovalBundleIdentity {
                    name: approval_bundle.name.clone(),
                    digest: approval_bundle.digest.clone(),
                    protocol: approval_bundle.protocol.clone(),
                    manifest_sha256: approval_bundle.manifest_sha256.clone(),
                    gateway_commit: approval_bundle.gateway_commit.clone(),
                    gateway_archive_sha256: approval_bundle.gateway_archive_sha256.clone(),
                    plugin_sha256: approval_bundle.plugin_sha256.clone(),
                    dippy_commit: approval_bundle.dippy_commit.clone(),
                    dippy_archive_sha256: approval_bundle.dippy_archive_sha256.clone(),
                    dippy_wheel_sha256: approval_bundle.dippy_wheel_sha256.clone(),
                },
                run_deadline,
                server_url,
                token: None,
                docker_path: docker.clone(),
                container_name: container_name.clone(),
                ledger_path: root.join("supervision-transitions.jsonl"),
                plugin_nonce: plugin_nonce.clone(),
            },
            &effects,
            tokio::sync::watch::channel(false).0,
        )
        .await?;
        let output = tokio::process::Command::new(&docker)
            .args(docker_args)
            .output()
            .await;
        let supervision_outcome = relay.supervision_outcome(false).await;
        let relay_cleanup = relay.stop().await;
        let container_cleanup = agent_hub_executor::DockerCli::new(&docker)
            .remove_owned(&container_name, &ownership, Duration::from_secs(15))
            .await;
        relay_cleanup?;
        container_cleanup?;
        effects.finalize()?;
        let output = output?;
        anyhow::ensure!(
            supervision_outcome? == crate::attended_relay::SupervisionOutcome::Denied,
            "host supervision ledger did not preserve authoritative denials"
        );
        let supervision_ledger =
            std::fs::read_to_string(root.join("supervision-transitions.jsonl"))?;
        let supervision_transitions = supervision_ledger
            .lines()
            .map(serde_json::from_str::<protocol::AttendedGatewayTransitionV1>)
            .collect::<Result<Vec<_>, _>>()?;
        let transcript_only = supervision_transitions
            .iter()
            .filter(|transition| {
                transition.transition == protocol::AttendedGatewayTransitionKind::TranscriptOnly
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !transcript_only.is_empty()
                && transcript_only
                    .iter()
                    .all(|transition| transition.tool_name.as_deref() == Some("StructuredOutput")),
            "StructuredOutput was not the sole transcript-only supervision exclusion"
        );
        for tool in ["Read", "Write", "Bash", "WebFetch", "Task"] {
            anyhow::ensure!(
                supervision_transitions.iter().any(|transition| {
                    transition.transition == protocol::AttendedGatewayTransitionKind::Created
                        && transition.tool_name.as_deref() == Some(tool)
                }),
                "eligible tool {tool} bypassed host supervision"
            );
        }
        let opencode_log =
            std::fs::read_to_string(workspace.join("opencode.log")).unwrap_or_default();
        anyhow::ensure!(
            output.status.success(),
            "supervised OpenCode fixture failed: status={:?} stdout={:?} stderr={:?} log={opencode_log:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let fake_state = fake_state.lock().await;
        anyhow::ensure!(
            !fake_state.registrations.contains(&"git status".to_string())
                && fake_state
                    .registrations
                    .contains(&"touch approved-marker".to_string())
                && fake_state
                    .registrations
                    .contains(&"touch denied-marker".to_string())
                && fake_state
                    .registrations
                    .contains(&"touch pending-marker".to_string()),
            "local/remote policy routing did not match the fixture: {:?}",
            fake_state.registrations
        );
        anyhow::ensure!(
            fake_state.cancellations == 1,
            "pending approval was not cancelled"
        );
        anyhow::ensure!(
            fake_state.terminal_verifications >= 1,
            "cancelled approval did not receive terminal GET verification"
        );
        anyhow::ensure!(
            workspace.join("approved-marker").is_file()
                && !workspace.join("denied-marker").exists()
                && !workspace.join("pending-marker").exists(),
            "supervised permission decisions did not control command execution"
        );
        drop(fake_state);
        let evidence_commit = std::env::var("AGENT_HUB_ATTENDED_OPENCODE_EVIDENCE_COMMIT")?;
        anyhow::ensure!(
            evidence_commit.len() == 40
                && evidence_commit
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "E2E evidence commit must be a lowercase 40-character Git SHA"
        );
        let plugin_digest = protocol::sha256_hex_with_domain(
            b"agent-hub:supervised-e2e-plugin:v1\0",
            &std::fs::read(&plugin_artifact)?,
        );
        let evidence = serde_json::to_vec(&serde_json::json!({
            "status": "PASS",
            "commit": evidence_commit,
            "agent_hub_run_plugin_sdk_opencode_version": config::AGENT_HUB_RUN_PLUGIN_SDK_OPENCODE_VERSION,
            "supervised_opencode_runtime_version": config::SUPERVISED_OPENCODE_RUNTIME_VERSION,
            "plugin_artifact_digest": plugin_digest,
            "plugin_artifact_digest_protocol": "agent-hub:supervised-e2e-plugin:v1",
            "image_digest": image_id,
            "approval_bundle_digest": approval_bundle.digest,
            "run_epoch": run_epoch,
        }))?;
        let evidence_path = integration_root.join(format!(
            "PASS-supervised-opencode-{}-{}.json",
            evidence_commit, run_epoch
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&evidence_path)?;
            std::io::Write::write_all(&mut file, &evidence)?;
            std::io::Write::write_all(&mut file, b"\n")?;
            file.sync_all()?;
            std::fs::set_permissions(&evidence_path, std::fs::Permissions::from_mode(0o400))?;
        }
        #[cfg(not(unix))]
        anyhow::bail!("immutable supervised E2E evidence requires Unix");
        drop(root_cleanup);
        Ok(())
    }

    #[test]
    fn attended_network_selectors_have_exact_canonical_ceilings() {
        assert_eq!(selector_resources("none").unwrap(), []);
        assert_eq!(
            selector_resources("llm-only").unwrap(),
            [NetworkResourceKind::Llm]
        );
        assert_eq!(
            selector_resources("guarded-web").unwrap(),
            [NetworkResourceKind::GuardedWeb]
        );
        assert_eq!(
            selector_resources("build").unwrap(),
            [
                NetworkResourceKind::CratesRegistry,
                NetworkResourceKind::Github,
                NetworkResourceKind::GoRegistry,
                NetworkResourceKind::Llm,
                NetworkResourceKind::NpmRegistry,
                NetworkResourceKind::PypiRegistry,
            ]
        );
    }

    #[test]
    fn attended_network_resources_are_repeatable_typed_stable_and_ceiling_bounded() {
        assert_eq!(resolve_network_resources("build", vec![]).unwrap(), []);
        assert_eq!(
            resolve_network_resources(
                "build",
                vec![
                    NetworkResourceKind::PypiRegistry,
                    NetworkResourceKind::NpmRegistry,
                ],
            )
            .unwrap(),
            [
                NetworkResourceKind::NpmRegistry,
                NetworkResourceKind::PypiRegistry,
            ]
        );
        assert!(
            resolve_network_resources(
                "build",
                vec![
                    NetworkResourceKind::PypiRegistry,
                    NetworkResourceKind::PypiRegistry
                ],
            )
            .is_err()
        );
        assert!(
            resolve_network_resources("llm-only", vec![NetworkResourceKind::PypiRegistry]).is_err()
        );
        assert!(
            resolve_network_resources(
                "guarded-web",
                vec![NetworkResourceKind::GuardedWeb, NetworkResourceKind::Llm],
            )
            .is_err()
        );
    }

    #[test]
    fn attended_environment_network_class_is_only_a_ceiling() {
        assert!(network_class_within_ceiling("none", "build").unwrap());
        assert!(network_class_within_ceiling("llm-only", "build").unwrap());
        assert!(network_class_within_ceiling("build", "build").unwrap());
        assert!(!network_class_within_ceiling("build", "llm-only").unwrap());
        assert!(!network_class_within_ceiling("guarded-web", "build").unwrap());
    }

    #[test]
    fn run_owner_marker_must_match_before_reconciliation() {
        let root =
            std::env::temp_dir().join(format!("agent-hub-run-owner-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        super::set_private_directory(&root).unwrap();
        let marker = RunOwnerMarker {
            run_id: uuid::Uuid::new_v4().to_string(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
            authorization_id: uuid::Uuid::new_v4().to_string(),
            receipt_id: uuid::Uuid::new_v4().to_string(),
            authorization_digest: "a".repeat(64),
            nonce: uuid::Uuid::new_v4().to_string(),
            #[cfg(unix)]
            uid: unsafe { libc::geteuid() },
            #[cfg(not(unix))]
            uid: 0,
            #[cfg(unix)]
            gid: unsafe { libc::getegid() },
            #[cfg(not(unix))]
            gid: 0,
            mode: "attended".into(),
            ancestry: vec!["authorization".into(), "receipt".into()],
        };
        write_new_private_durable(
            &root.join("owner.json"),
            &serde_json::to_vec_pretty(&marker).unwrap(),
        )
        .unwrap();
        verify_run_owner_marker(&root, &marker).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(verify_run_owner_marker(&root, &marker).is_err());
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut mismatched = marker.clone();
        mismatched.nonce = uuid::Uuid::new_v4().to_string();
        assert!(verify_run_owner_marker(&root, &mismatched).is_err());
        assert!(root.join("owner.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_503_retry_requires_every_host_proof() {
        assert!(provider_retry_proofs_complete(
            true, false, false, false, true
        ));
        assert!(!provider_retry_proofs_complete(
            false, false, false, false, true
        ));
        let event = br#"{"type":"error","timestamp":1788220800000,"sessionID":"session-1","error":{"name":"APIError","data":{"message":"ERROR: SERVICE UNAVAILABLE","statusCode":503,"isRetryable":true}}}"#;
        assert!(super::is_provider_service_unavailable(
            event,
            "Error: Service Unavailable"
        ));
        for (stdout, retained) in [
            (event.as_slice(), "tool output mentioned Error: Service Unavailable later"),
            (br#"{"type":"text","timestamp":1788220800000,"sessionID":"session-1","error":{"name":"APIError","data":{"message":"Error: Service Unavailable"}}}"#.as_slice(), "Error: Service Unavailable"),
            (br#"{"type":"error","timestamp":1788220800000,"sessionID":"session-1","error":{"name":"UnknownError","data":{"message":"Error: Service Unavailable"}}}"#.as_slice(), "Error: Service Unavailable"),
            (br#"{"type":"error","timestamp":1788220800000,"sessionID":"session-1","error":{"name":"APIError","data":{"message":"503 from upstream"}}}"#.as_slice(), "Error: Service Unavailable"),
            (br#"{"type":"error","timestamp":1788220800000,"sessionID":"session-1","error":{"name":"APIError","data":{"message":"Error: Service Unavailable","isRetryable":true}}}"#.as_slice(), "Error: Service Unavailable"),
            (br#"{"type":"error","timestamp":1788220800000,"sessionID":"session-1","error":{"name":"APIError","data":{"message":"Error: Service Unavailable","statusCode":502,"isRetryable":true}}}"#.as_slice(), "Error: Service Unavailable"),
            (br#"{"type":"error","timestamp":1788220800000,"sessionID":"session-1","error":{"name":"APIError","data":{"message":"Error: Service Unavailable","statusCode":503,"isRetryable":false}}}"#.as_slice(), "Error: Service Unavailable"),
            (b"Error: Service Unavailable".as_slice(), "Error: Service Unavailable"),
        ] {
            assert!(!super::is_provider_service_unavailable(stdout, retained));
        }
    }

    #[test]
    fn supervised_provider_retry_uses_only_the_bounded_host_ledger() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-provider-ledger-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        let run_epoch = uuid::Uuid::new_v4();
        let binding = protocol::AttendedGatewayBindingV1 {
            run_epoch: run_epoch.to_string(),
            authorization_digest: "a".repeat(64),
            environment_bundle_digest: "b".repeat(64),
            image_digest: format!("sha256:{}", "c".repeat(64)),
            network_digest: "d".repeat(64),
            approval_bundle_digest: "e".repeat(64),
        };
        let plugin_nonce = "plugin-nonce".to_string();
        let epoch = super::RelayEpochState {
            run_epoch,
            binding: binding.clone(),
            plugin_nonce: plugin_nonce.clone(),
            deadline: Utc::now() + chrono::Duration::minutes(1),
            endpoint: super::RelayEndpointIdentity {
                url: "https://approvals.example/".into(),
            },
            socket: crate::attended_relay::RelaySocketLocation {
                runtime_dir: root.clone(),
                socket_root: root.clone(),
                socket_path: root.join("approval.sock"),
            },
        };
        assert!(!super::supervision_ledger_has_only_plugin_ready(&root).unwrap());
        std::fs::write(
            root.join("approval-relay-epoch.json"),
            serde_json::to_vec(&epoch).unwrap(),
        )
        .unwrap();
        let ready = protocol::AttendedGatewayTransitionV1 {
            version: protocol::ATTENDED_GATEWAY_TRANSITION_VERSION.into(),
            binding,
            transition: protocol::AttendedGatewayTransitionKind::PluginReady,
            request_id: None,
            call_id: None,
            session_id: None,
            tool_name: None,
            raw_args_digest: None,
            canonical_args_digest: None,
            plugin_nonce: Some(plugin_nonce),
        };
        let ledger = root.join("supervision-transitions.jsonl");
        assert!(!super::supervision_ledger_has_only_plugin_ready(&root).unwrap());
        std::fs::write(
            &ledger,
            format!("{}\n", serde_json::to_string(&ready).unwrap()),
        )
        .unwrap();
        assert!(super::supervision_ledger_has_only_plugin_ready(&root).unwrap());
        let summary = super::host_ledger_summary(&ledger).unwrap();
        assert_eq!(summary.entry_count, 1);
        assert_eq!(
            summary.byte_count,
            std::fs::metadata(&ledger).unwrap().len()
        );
        assert!(summary.sha256.is_some());
        std::fs::write(
            &ledger,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&ready).unwrap(),
                serde_json::to_string(&ready).unwrap()
            ),
        )
        .unwrap();
        assert!(!super::supervision_ledger_has_only_plugin_ready(&root).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn automatic_provider_transcript_must_be_complete_and_event_free() {
        let transcript = b"{\"timestamp\":\"now\",\"stream\":\"stderr\",\"line\":\"503 service unavailable\",\"partial\":false}\n";
        let (stdout, stderr) = super::verified_transcript_streams(transcript)
            .unwrap()
            .unwrap();
        assert!(stdout.is_empty());
        assert_eq!(stderr, b"503 service unavailable\n");
        assert!(
            super::verified_transcript_streams(b"{\"stream\":\"stdout\"}\n")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn assistant_or_first_tool_result_forbids_provider_retry() {
        assert!(!provider_retry_proofs_complete(
            true, true, false, false, true
        ));
        assert!(!provider_retry_proofs_complete(
            true, false, true, false, true
        ));
    }

    #[test]
    fn workspace_delta_or_cleanup_failure_forbids_provider_retry() {
        assert!(!provider_retry_proofs_complete(
            true, false, false, true, true
        ));
        assert!(!provider_retry_proofs_complete(
            true, false, false, false, false
        ));
    }

    #[test]
    fn reviewed_retry_budgets_hold_at_every_exhaustion_boundary() {
        for mode in [
            protocol::ToolApprovalMode::Auto,
            protocol::ToolApprovalMode::Dippy,
        ] {
            let limits = protocol::AttendedRetryConstraints::for_mode(mode);
            for attempts in 0..=limits.max_attempts as usize + 1 {
                for retries in 0..=limits.max_prelaunch_retries as usize + 1 {
                    assert_eq!(
                        super::retry_budget_allows(
                            attempts,
                            retries,
                            limits.max_attempts,
                            limits.max_prelaunch_retries,
                        ),
                        attempts < limits.max_attempts as usize
                            && retries < limits.max_prelaunch_retries as usize
                    );
                }
                for retries in 0..=limits.max_provider_unavailable_retries as usize + 1 {
                    assert_eq!(
                        super::retry_budget_allows(
                            attempts,
                            retries,
                            limits.max_attempts,
                            limits.max_provider_unavailable_retries,
                        ),
                        attempts < limits.max_attempts as usize
                            && retries < limits.max_provider_unavailable_retries as usize
                    );
                }
            }
        }
    }

    #[test]
    fn isolated_git_view_and_scoped_github_config_coexist() {
        let mut environment = BTreeMap::from([
            (
                "GIT_CONFIG_GLOBAL".into(),
                "/agent-hub/github/gitconfig".into(),
            ),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ]);
        apply_git_runtime_environment(&mut environment).unwrap();
        assert_eq!(
            environment["GIT_CONFIG_GLOBAL"],
            "/agent-hub/github/gitconfig"
        );
        assert_eq!(
            environment["GIT_CONFIG_SYSTEM"],
            "/agent-hub/git/home/.gitconfig"
        );
    }

    #[test]
    fn attended_finally_aggregates_later_and_cleanup_failures() {
        let error = merge_execution_cleanup::<()>(
            Err(anyhow::anyhow!("materialization failed")),
            Err(anyhow::anyhow!("owned egress remains")),
            CleanupStatus::Failed("credential file remains".to_string()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("materialization failed"));
        assert!(error.contains("owned egress remains"));
        assert!(error.contains("credential file remains"));
    }

    #[test]
    fn attended_finally_preserves_cancel_and_timeout_results_after_cleanup() {
        for status in [
            agent_hub_executor::ExecutionStatus::Cancelled,
            agent_hub_executor::ExecutionStatus::TimedOut,
        ] {
            assert_eq!(
                merge_execution_cleanup(Ok(status), Ok(()), CleanupStatus::Complete,).unwrap(),
                status
            );
        }
    }

    #[test]
    fn attended_output_serialization_rejects_huge_values_before_allocation() {
        let value = "x".repeat(2 * 1024 * 1024);
        assert!(serialize_pretty_bounded(&value, 1024).is_err());
        assert_eq!(
            serialize_pretty_bounded(&"bounded", 64).unwrap(),
            br#""bounded""#
        );
    }

    #[test]
    fn attended_execution_outcome_survives_later_cleanup_failure() {
        for (status, expected) in [
            (
                agent_hub_executor::ExecutionStatus::Cancelled,
                AttendedExecutionOutcome::Cancelled,
            ),
            (
                agent_hub_executor::ExecutionStatus::TimedOut,
                AttendedExecutionOutcome::TimedOut,
            ),
        ] {
            let operation = Ok(agent_hub_executor::ExecutionResult {
                status,
                containment: agent_hub_executor::ContainmentProof {
                    process: agent_hub_executor::ContainmentStatus::VerifiedAbsent,
                    container: agent_hub_executor::ContainmentStatus::NotApplicable,
                    egress: agent_hub_executor::ContainmentStatus::NotApplicable,
                },
                cleanup: CleanupStatus::Complete,
            });
            assert_eq!(attended_execution_outcome(&operation, false), expected);
            assert!(
                merge_execution_cleanup(
                    operation,
                    Err(anyhow::anyhow!("injected cleanup failure")),
                    CleanupStatus::Complete,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn cleanup_classification_is_typed_not_message_based() {
        let ordinary = merge_execution_cleanup::<()>(
            Err(anyhow::anyhow!("ordinary execution said cleanup failed")),
            Ok(()),
            CleanupStatus::Complete,
        )
        .unwrap_err();
        assert!(ordinary.downcast_ref::<FinalizationError>().is_none());

        let cleanup = merge_execution_cleanup(
            Ok(()),
            Err(anyhow::anyhow!("resource remains")),
            CleanupStatus::Complete,
        )
        .unwrap_err();
        assert!(cleanup.downcast_ref::<FinalizationError>().is_some());
    }

    fn terminal_event(result: serde_json::Value) -> String {
        serde_json::json!({
            "type": "text",
            "part": {
                "text": serde_json::to_string(&result).unwrap()
            }
        })
        .to_string()
    }

    fn structured_event(tool: &str, result: serde_json::Value) -> String {
        serde_json::json!({
            "type": "tool_use",
            "part": {
                "tool": tool,
                "state": {"status": "completed", "input": result}
            }
        })
        .to_string()
    }

    fn complete_agent_result() -> serde_json::Value {
        serde_json::json!({
            "schema": "agent_result_v1",
            "complete": true,
            "summary": "done",
            "acceptance_results": [{"criterion":"tests", "passed":true, "evidence":"passed"}],
            "blockers": [],
            "artifact_references": [{"path":"result.json", "description":"result"}]
        })
    }

    #[test]
    fn exit_zero_requires_complete_agent_terminal_envelope() {
        let complete = super::parse_agent_result_events(&terminal_event(complete_agent_result()));
        assert_eq!(complete.outcome, super::AgentOutcome::Succeeded);

        let mut incomplete = complete_agent_result();
        incomplete["complete"] = serde_json::json!(false);
        assert_eq!(
            super::parse_agent_result_events(&terminal_event(incomplete)).outcome,
            super::AgentOutcome::Incomplete
        );
        assert_eq!(
            super::parse_agent_result_events("").outcome,
            super::AgentOutcome::Missing
        );
        assert_eq!(
            super::parse_agent_result_events("not-json").outcome,
            super::AgentOutcome::Malformed
        );
        let mut contradictory = complete_agent_result();
        contradictory["blockers"] = serde_json::json!(["blocked"]);
        assert_eq!(
            super::parse_agent_result_events(&terminal_event(contradictory)).outcome,
            super::AgentOutcome::Contradictory
        );
    }

    #[test]
    fn opencode_1_18_18_structured_result_fixture_is_accepted() {
        let output = include_str!("../tests/fixtures/opencode-1.18.18-structured-result.jsonl");
        let parsed = super::parse_agent_result_events(output);
        assert_eq!(parsed.outcome, super::AgentOutcome::Succeeded);
        assert_eq!(
            parsed.result.unwrap().summary,
            "Implemented and verified the requested lifecycle."
        );
    }

    #[test]
    fn opencode_1_18_18_plain_terminal_result_fixture_is_accepted() {
        let output = include_str!("../tests/fixtures/opencode-1.18.18-plain-terminal-result.json");
        let parsed = super::parse_agent_result_events(output);
        assert_eq!(parsed.outcome, super::AgentOutcome::Succeeded);
        assert_eq!(
            parsed.result.unwrap().summary,
            "All requested Git commands completed successfully; README.md and opencode.json have no working-tree changes."
        );
    }

    #[test]
    fn plain_terminal_result_must_be_the_only_stdout_document() {
        let result = complete_agent_result().to_string();
        for output in [
            format!("log prefix {result}"),
            format!("log prefix\n{result}"),
            format!("{result}\n{result}"),
        ] {
            assert_eq!(
                super::parse_agent_result_events(&output).outcome,
                super::AgentOutcome::Malformed
            );
        }

        let mut conflicting = complete_agent_result();
        conflicting["summary"] = serde_json::json!("different");
        assert_eq!(
            super::parse_agent_result_events(&format!("{result}\n{conflicting}")).outcome,
            super::AgentOutcome::Malformed
        );

        let embedded = serde_json::json!({
            "type": "text",
            "part": {"text": format!("log prefix\n{result}")}
        });
        assert_eq!(
            super::parse_agent_result_events(&embedded.to_string()).outcome,
            super::AgentOutcome::Malformed
        );
    }

    #[test]
    fn plain_terminal_result_preserves_strict_schema_and_complete_semantics() {
        let mut incomplete = complete_agent_result();
        incomplete["complete"] = serde_json::json!(false);
        incomplete["blockers"] = serde_json::json!(["follow-up required"]);
        assert_eq!(
            super::parse_agent_result_events(&incomplete.to_string()).outcome,
            super::AgentOutcome::Incomplete
        );

        let mut contradictory = complete_agent_result();
        contradictory["blockers"] = serde_json::json!(["blocked"]);
        assert_eq!(
            super::parse_agent_result_events(&contradictory.to_string()).outcome,
            super::AgentOutcome::Contradictory
        );

        let mut malformed = complete_agent_result();
        malformed["unexpected"] = serde_json::json!(true);
        assert_eq!(
            super::parse_agent_result_events(&malformed.to_string()).outcome,
            super::AgentOutcome::Malformed
        );
    }

    #[test]
    fn opencode_structured_output_result_matrix_is_typed() {
        for alias in ["StructuredOutput", "structured_output", "structuredoutput"] {
            let parsed =
                super::parse_agent_result_events(&structured_event(alias, complete_agent_result()));
            assert_eq!(parsed.outcome, super::AgentOutcome::Succeeded);
        }

        let mut incomplete = complete_agent_result();
        incomplete["complete"] = serde_json::json!(false);
        incomplete["blockers"] = serde_json::json!(["follow-up required"]);
        assert_eq!(
            super::parse_agent_result_events(&structured_event("StructuredOutput", incomplete))
                .outcome,
            super::AgentOutcome::Incomplete
        );

        let mut conflicting = complete_agent_result();
        conflicting["summary"] = serde_json::json!("different");
        let events = format!(
            "{}\n{}",
            structured_event("StructuredOutput", complete_agent_result()),
            structured_event("StructuredOutput", conflicting)
        );
        assert_eq!(
            super::parse_agent_result_events(&events).outcome,
            super::AgentOutcome::Contradictory
        );

        let mut malformed = complete_agent_result();
        malformed["unexpected"] = serde_json::json!(true);
        assert_eq!(
            super::parse_agent_result_events(&structured_event("StructuredOutput", malformed))
                .outcome,
            super::AgentOutcome::Malformed
        );

        let events = format!(
            "{}\n{}",
            structured_event("StructuredOutput", complete_agent_result()),
            serde_json::json!({"type":"text", "part":{"text":"{\"message\":\"ordinary text JSON\"}"}})
        );
        assert_eq!(
            super::parse_agent_result_events(&events).outcome,
            super::AgentOutcome::Succeeded
        );
    }

    fn terminal_record() -> RunRecord {
        let run_id = uuid::Uuid::new_v4().to_string();
        RunRecord {
            schema: super::result_schema_current(),
            id: run_id.clone(),
            run_id,
            attempt_id: uuid::Uuid::new_v4().to_string(),
            attempt_number: 1,
            parent_attempt_id: None,
            attempt_ancestry: vec![],
            authorization_id: uuid::Uuid::new_v4().to_string(),
            receipt_id: uuid::Uuid::new_v4().to_string(),
            authorization_digest: "a".repeat(64),
            owner_nonce: uuid::Uuid::new_v4().to_string(),
            launch_state: super::LaunchState::Terminal,
            status: RunStatus::Succeeded,
            container_name: "container".into(),
            ownership: BTreeMap::new(),
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            exit_code: Some(0),
            error: None,
            execution_status: Some(RunStatus::Succeeded),
            process_status: Some(RunStatus::Succeeded),
            permission_outcome: super::PermissionOutcome::NoDenials,
            process_outcome: super::ProcessOutcome::Succeeded,
            agent_outcome: super::AgentOutcome::Succeeded,
            execution_outcome: super::ExecutionOutcome::Succeeded,
            result_outcome: super::ResultOutcome::Succeeded,
            cleanup_outcome: super::CleanupOutcome::Succeeded,
            agent_final: serde_json::from_value(complete_agent_result()).ok(),
            retry_class: None,
            retry_eligible: false,
            workspace_export: None,
        }
    }

    #[test]
    fn pending_attempt_publication_recovers_at_each_transaction_boundary() {
        for execution_record_published in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "agent-hub-pending-attempt-{}",
                uuid::Uuid::new_v4()
            ));
            let store = StateStore::open(root.clone()).unwrap();
            let mut record = terminal_record();
            record.id = record.run_id.clone();
            record.launch_state = super::LaunchState::Preparing;
            record.status = RunStatus::Preparing;
            record.finished_at = None;
            record.retry_class = None;
            record.retry_eligible = false;
            let lineage = super::AttemptLineage {
                attempt_id: record.attempt_id.clone(),
                run_id: record.run_id.clone(),
                state: super::LaunchState::Preparing,
                retry_class: None,
                retry_eligible: false,
                started_at: record.started_at,
                finished_at: None,
            };
            let pending = super::PendingAttempt {
                authorization_digest: record.authorization_digest.clone(),
                authorization_id: record.authorization_id.clone(),
                record: record.clone(),
                lineage: lineage.clone(),
            };
            store
                .create_json(&super::attempt_journal_path(&record.run_id), &pending)
                .unwrap();
            if execution_record_published {
                store
                    .create_json(&super::run_record_path(&record.run_id), &record)
                    .unwrap();
            }
            let artifact = protocol::AttendedAuthorizationArtifact {
                version: protocol::ATTENDED_AUTHORIZATION_VERSION_V3,
                digest: record.authorization_digest.clone(),
                canonical_bytes: vec![],
            };
            let mut authorization = super::AuthorizationRecord {
                authorization_id: record.authorization_id.clone(),
                artifact,
                authorization_path: "/authorization.json".into(),
                authorized_at: Utc::now(),
                receipt_id: record.receipt_id.clone(),
                domain: "attended".into(),
                state: super::LaunchState::Authorized,
                attempts: vec![],
                active_attempt_id: None,
                decision_receipt: None,
            };
            let authorization_path = std::path::PathBuf::from("authorizations/test.json");
            store
                .create_json(&authorization_path, &authorization)
                .unwrap();
            if execution_record_published {
                let mut foreign = record.clone();
                foreign.receipt_id = uuid::Uuid::new_v4().to_string();
                store
                    .replace_json(&super::run_record_path(&record.run_id), &foreign)
                    .unwrap();
                assert!(
                    super::reconcile_pending_attempts(
                        &store,
                        &authorization_path,
                        &mut authorization
                    )
                    .is_err()
                );
                store
                    .replace_json(&super::run_record_path(&record.run_id), &record)
                    .unwrap();
            }
            super::reconcile_pending_attempts(&store, &authorization_path, &mut authorization)
                .unwrap();
            assert_eq!(
                super::active_attempt_run_id(&authorization, &record.attempt_id).unwrap(),
                record.run_id
            );
            assert!(
                store
                    .read_json::<RunRecord>(&super::run_record_path(&record.run_id))
                    .is_ok()
            );
            assert_ne!(record.attempt_id, record.run_id);
            assert!(
                store
                    .read_json::<RunRecord>(&super::run_record_path(&record.attempt_id))
                    .is_err()
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn owner_publication_recovers_the_atomic_staging_boundary() {
        let root =
            std::env::temp_dir().join(format!("agent-hub-owner-staging-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        super::set_private_directory(&root).unwrap();
        let record = terminal_record();
        let marker = super::owner_marker_for_record(&record);
        let run_dir = root.join(&record.id);
        let staging = super::owner_staging_directory(&run_dir, &marker).unwrap();
        std::fs::create_dir(&staging).unwrap();
        super::set_private_directory(&staging).unwrap();
        super::write_new_private_durable(
            &staging.join("owner.json"),
            &serde_json::to_vec_pretty(&marker).unwrap(),
        )
        .unwrap();
        super::recover_owner_staging_directory(&run_dir, &marker).unwrap();
        assert!(run_dir.is_dir());
        assert!(!staging.exists());
        super::verify_run_owner_marker(&run_dir, &marker).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn durable_output_replacement_always_exposes_a_complete_generation() {
        let root =
            std::env::temp_dir().join(format!("agent-hub-output-replace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("result.json");
        for generation in 0..128_u32 {
            let value = serde_json::json!({"generation": generation, "body": "x".repeat(256)});
            super::write_private_durable(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
            let observed: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(observed["generation"], generation);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn prelaunch_journal_proves_no_resource_cleanup_without_owner_directory() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-no-resource-cleanup-{}",
            uuid::Uuid::new_v4()
        ));
        let store = StateStore::open(root.clone()).unwrap();
        let mut record = terminal_record();
        record.id = record.run_id.clone();
        record.launch_state = super::LaunchState::Terminal;
        record.status = RunStatus::Failed;
        record.retry_class = Some(super::RetryClass::Prelaunch);
        record.retry_eligible = true;
        let pending = super::PendingAttempt {
            authorization_digest: record.authorization_digest.clone(),
            authorization_id: record.authorization_id.clone(),
            record: RunRecord {
                launch_state: super::LaunchState::Preparing,
                status: RunStatus::Preparing,
                finished_at: None,
                retry_class: None,
                retry_eligible: false,
                ..record.clone()
            },
            lineage: super::AttemptLineage {
                attempt_id: record.attempt_id.clone(),
                run_id: record.run_id.clone(),
                state: super::LaunchState::Preparing,
                retry_class: None,
                retry_eligible: false,
                started_at: record.started_at,
                finished_at: None,
            },
        };
        store
            .create_json(&super::attempt_journal_path(&record.id), &pending)
            .unwrap();
        store
            .create_json(&super::run_record_path(&record.id), &record)
            .unwrap();

        let marker = super::owner_marker_for_record(&record);
        store
            .ensure_directory(std::path::Path::new("runs"))
            .unwrap();
        let run_dir = store.path(&PathBuf::from("runs").join(&record.id)).unwrap();
        let staging = super::owner_staging_directory(&run_dir, &marker).unwrap();
        std::fs::create_dir(&staging).unwrap();
        super::set_private_directory(&staging).unwrap();
        super::write_new_private_durable(
            &staging.join("owner.json"),
            &serde_json::to_vec_pretty(&marker).unwrap(),
        )
        .unwrap();
        assert!(
            super::cleanup(
                &store,
                &record.id,
                RelayEndpoint {
                    server_url: "https://approvals.example".into(),
                    token: None,
                },
            )
            .await
            .is_err()
        );
        std::fs::remove_dir_all(&staging).unwrap();

        let cleaned = super::cleanup(
            &store,
            &record.id,
            RelayEndpoint {
                server_url: "https://approvals.example".into(),
                token: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(cleaned.status, RunStatus::Cleaned);
        let receipt = super::read_receipt(&store, &record.id).unwrap();
        assert_eq!(
            receipt.payload.cleanup["journal_proof"]["kind"],
            "prelaunch_no_resources"
        );
        assert_eq!(
            receipt.payload.artifacts["reason"],
            "prelaunch_no_resources"
        );
        assert_eq!(receipt.payload.artifacts["schema"], "attended_artifacts_v1");
        assert!(!serde_json::to_string(&receipt).unwrap().contains("/runs/"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execution_and_cleanup_outcomes_are_independent_and_typed() {
        let mut record = terminal_record();
        for (process, expected) in [
            (
                super::ProcessOutcome::Cancelled,
                super::ExecutionOutcome::Cancelled,
            ),
            (
                super::ProcessOutcome::TimedOut,
                super::ExecutionOutcome::TimedOut,
            ),
        ] {
            record.process_outcome = process;
            assert_eq!(super::execution_outcome_from_record(&record), expected);
        }
        record = terminal_record();
        record.permission_outcome = super::PermissionOutcome::Denied;
        assert_eq!(
            super::execution_outcome_from_record(&record),
            super::ExecutionOutcome::Failed
        );
        record.permission_outcome = super::PermissionOutcome::Cancelled;
        assert_eq!(
            super::execution_outcome_from_record(&record),
            super::ExecutionOutcome::Cancelled
        );
        record.permission_outcome = super::PermissionOutcome::ReadinessFailed;
        assert_eq!(
            super::execution_outcome_from_record(&record),
            super::ExecutionOutcome::Failed
        );

        for (status, expected) in [
            (
                agent_hub_executor::ExecutionStatus::Exited(0),
                super::ProcessOutcome::Succeeded,
            ),
            (
                agent_hub_executor::ExecutionStatus::Exited(7),
                super::ProcessOutcome::Failed,
            ),
            (
                agent_hub_executor::ExecutionStatus::Cancelled,
                super::ProcessOutcome::Cancelled,
            ),
            (
                agent_hub_executor::ExecutionStatus::TimedOut,
                super::ProcessOutcome::TimedOut,
            ),
            (
                agent_hub_executor::ExecutionStatus::OutputLimitExceeded,
                super::ProcessOutcome::OutputLimitExceeded,
            ),
        ] {
            assert_eq!(super::process_outcome(&status), expected);
        }

        record = terminal_record();
        record.execution_outcome = super::ExecutionOutcome::Failed;
        assert!(super::ensure_successful_outcome(&record).is_err());
        assert!(super::ensure_cleanup_successful(&record).is_ok());
        record.execution_outcome = super::ExecutionOutcome::Succeeded;
        record.result_outcome = super::ResultOutcome::Failed;
        record.exit_code = Some(0);
        assert!(super::ensure_successful_outcome(&record).is_err());
        record.result_outcome = super::ResultOutcome::Succeeded;
        record.cleanup_outcome = super::CleanupOutcome::Failed;
        assert!(super::ensure_successful_outcome(&record).is_err());
        assert!(super::ensure_cleanup_successful(&record).is_err());
    }

    fn outcome_snapshot(record: &RunRecord) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": record.schema,
            "launch_state": record.launch_state,
            "status": record.status,
            "error": record.error,
            "exit_code": record.exit_code,
            "permission_outcome": record.permission_outcome,
            "process_outcome": record.process_outcome,
            "agent_outcome": record.agent_outcome,
            "execution_outcome": record.execution_outcome,
            "result_outcome": record.result_outcome,
            "cleanup_outcome": record.cleanup_outcome,
            "retry_eligible": record.retry_eligible,
        }))
        .unwrap()
    }

    #[test]
    fn literal_v3_records_remain_unmodified_and_have_no_result_evidence() {
        let fixtures = [
            include_str!("../tests/fixtures/attended-result-v3-legacy.json"),
            include_str!("../tests/fixtures/attended-result-v3-legacy-status-only.json"),
        ];
        for fixture in fixtures {
            let record: RunRecord = serde_json::from_str(fixture).unwrap();
            assert_eq!(record.schema, super::RESULT_SCHEMA_V3);
            assert_eq!(record.result_outcome, super::ResultOutcome::Unknown);
            assert!(
                !serde_json::to_value(&record)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .contains_key("result_outcome")
            );

            let normalized = super::normalize_record(record).unwrap();
            assert_eq!(normalized.result_outcome, super::ResultOutcome::Unknown);
            assert!(super::ensure_successful_outcome(&normalized).is_err());
        }

        let mut record: RunRecord = serde_json::from_str(fixtures[0]).unwrap();
        record.result_outcome = super::ResultOutcome::Succeeded;
        assert!(super::normalize_record(record).is_err());
    }

    #[test]
    fn attended_v4_success_and_failure_snapshots_are_stable() {
        let record = terminal_record();
        assert_eq!(
            outcome_snapshot(&record).trim(),
            include_str!("../tests/fixtures/attended-result-v4-success.json").trim()
        );

        let mut failed = record;
        failed.status = RunStatus::Failed;
        failed.error = Some("execution failed".into());
        failed.execution_outcome = super::ExecutionOutcome::Failed;
        failed.result_outcome = super::ResultOutcome::Failed;
        assert_eq!(
            outcome_snapshot(&failed).trim(),
            include_str!("../tests/fixtures/attended-result-v4-failure.json").trim()
        );
    }

    #[test]
    fn result_publication_fault_is_typed_and_fails_closed() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-result-publication-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&root).unwrap();
        let mut record = terminal_record();
        super::publish_terminal_result(&mut record, &root.join("result.json"), 1);
        assert_eq!(
            outcome_snapshot(&record).trim(),
            include_str!("../tests/fixtures/attended-result-v4-publication-fault.json").trim()
        );
        assert!(super::ensure_successful_outcome(&record).is_err());
        assert!(!root.join("result.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn live_list_cleanup_and_receipts_share_version_aware_normalization() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-result-receipt-parity-{}",
            uuid::Uuid::new_v4()
        ));
        let store = StateStore::open(root.clone()).unwrap();
        let cleaned_at = Utc::now();
        let mut record: RunRecord = serde_json::from_str(include_str!(
            "../tests/fixtures/attended-result-v3-legacy-status-only.json"
        ))
        .unwrap();
        record.cleanup_outcome = super::CleanupOutcome::Succeeded;
        record.finished_at = Some(cleaned_at);
        store
            .create_json(&super::run_record_path(&record.id), &record)
            .unwrap();
        let live = super::status(&store, &record.id).unwrap();

        let mut receipt_record = record.clone();
        receipt_record.status = RunStatus::Cleaned;
        let payload = super::CleanupReceiptPayload {
            record: receipt_record.clone(),
            owner: super::owner_marker_for_record(&receipt_record),
            result: serde_json::to_value(&receipt_record).unwrap(),
            artifacts: serde_json::json!({"schema":"attended_artifacts_v1"}),
            repositories: serde_json::json!([]),
            cleanup: serde_json::json!({
                "schema":"attended_cleanup_result_v1",
                "outcome":"succeeded",
                "cleaned_at": cleaned_at,
            }),
            cleanup_evidence: super::AttendedCleanupEvidence::default(),
            host_ledger: super::HostLedgerSummary::default(),
        };
        let receipt = super::CleanupReceipt {
            schema: super::CLEANUP_RECEIPT_SCHEMA_V1.into(),
            run_id: record.id.clone(),
            authorization_digest: record.authorization_digest.clone(),
            cleaned_at,
            checksum: super::receipt_checksum(&payload).unwrap(),
            payload,
        };
        store
            .create_json(&super::receipt_path(&record.id), &receipt)
            .unwrap();
        let retained = super::status(&store, &record.id).unwrap();
        assert_eq!(live.status, RunStatus::Succeeded);
        assert_eq!(retained.status, RunStatus::Cleaned);
        assert_eq!(live.process_outcome, retained.process_outcome);
        assert_eq!(live.execution_outcome, retained.execution_outcome);
        assert_eq!(live.cleanup_outcome, retained.cleanup_outcome);
        assert_eq!(retained.result_outcome, super::ResultOutcome::Unknown);
        let listed = super::list(&store).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, RunStatus::Cleaned);
        let cleaned = super::cleanup(
            &store,
            &record.id,
            RelayEndpoint {
                server_url: "https://approvals.example".into(),
                token: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(cleaned.status, RunStatus::Cleaned);
        assert_eq!(cleaned.result_outcome, super::ResultOutcome::Unknown);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_documents_and_unknown_record_schemas_fail_closed() {
        let document = super::ParsedDocument {
            reference: super::BoundedArtifactReference {
                path: "result.json".into(),
                available: true,
                limit_bytes: 1,
                returned_bytes: 1,
                total_bytes: 2,
                truncated: true,
            },
            document: None,
            parse_error: Some("document was truncated before parsing".into()),
        };
        assert!(super::ensure_document_complete(&document).is_err());

        let mut record = terminal_record();
        record.run_id = uuid::Uuid::new_v4().to_string();
        assert!(super::normalize_record(record.clone()).is_err());
        record.run_id = record.id.clone();
        record.schema = "attended_result_v99".into();
        assert!(super::normalize_record(record).is_err());
        assert!(super::validate_id("00000000000000000000000000000000").is_err());
    }

    #[test]
    fn retained_receipt_is_private_bounded_and_readable_without_runtime_payload() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-retained-receipt-{}",
            uuid::Uuid::new_v4()
        ));
        let store = StateStore::open(root.clone()).unwrap();
        let cleaned_at = Utc::now();
        let mut record = terminal_record();
        record.run_id = record.id.clone();
        record.status = RunStatus::Cleaned;
        record.finished_at = Some(cleaned_at);
        store
            .create_json(&super::run_record_path(&record.id), &record)
            .unwrap();
        let live = super::status(&store, &record.id).unwrap();
        let payload = super::CleanupReceiptPayload {
            record: record.clone(),
            owner: super::owner_marker_for_record(&record),
            result: serde_json::to_value(&record).unwrap(),
            artifacts: serde_json::json!({"schema":"attended_artifacts_v1"}),
            repositories: serde_json::json!([]),
            cleanup: serde_json::json!({"schema":"attended_cleanup_result_v1","outcome":"succeeded"}),
            cleanup_evidence: super::AttendedCleanupEvidence::default(),
            host_ledger: super::HostLedgerSummary::default(),
        };
        let receipt = super::CleanupReceipt {
            schema: super::CLEANUP_RECEIPT_SCHEMA_V1.into(),
            run_id: record.id.clone(),
            authorization_digest: record.authorization_digest.clone(),
            cleaned_at,
            checksum: super::receipt_checksum(&payload).unwrap(),
            payload,
        };
        let mut mismatched = receipt.clone();
        mismatched.payload.record.run_id = uuid::Uuid::new_v4().to_string();
        mismatched.checksum = super::receipt_checksum(&mismatched.payload).unwrap();
        assert!(super::verify_receipt(&mismatched, &record.id).is_err());
        let mut incomplete = receipt.clone();
        incomplete.payload.record.cleanup_outcome = super::CleanupOutcome::Failed;
        incomplete.checksum = super::receipt_checksum(&incomplete.payload).unwrap();
        assert!(super::verify_receipt(&incomplete, &record.id).is_err());
        store
            .create_json(&super::pending_receipt_path(&record.id), &receipt)
            .unwrap();
        let pending = store
            .path(&super::pending_receipt_path(&record.id))
            .unwrap();
        super::publish_pending_receipt(&store, &record.id, &pending).unwrap();
        let result = super::local_result(&store, &record.id, 64).unwrap();
        assert!(result.result.reference.truncated);
        assert!(!result.stdout.available);
        assert_eq!(
            super::status(&store, &record.id).unwrap().exit_code,
            Some(0)
        );
        let aggregate = super::local_result(&store, &record.id, 1_048_576).unwrap();
        let retained = super::status(&store, &record.id).unwrap();
        assert_eq!(live.result_outcome, retained.result_outcome);
        assert_eq!(live.execution_outcome, retained.execution_outcome);
        assert_eq!(live.cleanup_outcome, retained.cleanup_outcome);
        assert_eq!(aggregate.status, RunStatus::Cleaned);
        assert_eq!(aggregate.result_outcome, super::ResultOutcome::Succeeded);
        assert_eq!(
            aggregate.artifacts.document.as_ref().unwrap()["schema"],
            "attended_artifacts_v1"
        );
        super::ensure_local_result_documents(&aggregate).unwrap();
        let result_json = serde_json::to_value(&result).unwrap();
        let result_keys = result_json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            result_keys,
            "agent_final\nagent_outcome\nartifacts\ncleanup\ncleanup_outcome\nerror\nexecution_outcome\nexit_code\nlaunch_state\npaths\npermission_outcome\nprocess_outcome\nrepositories\nresult\nresult_outcome\nretry_class\nretry_eligible\nrun_id\nschema\nstatus\nstderr\nstdout\ntranscript"
        );
        assert_eq!(
            result.paths["receipt"],
            store
                .path(&super::receipt_path(&record.id))
                .unwrap()
                .display()
                .to_string()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(store.path(&super::receipt_path(&record.id)).unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o400);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn published_cleanup_receipt_wins_over_stale_execution_index() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-receipt-authority-{}",
            uuid::Uuid::new_v4()
        ));
        let store = StateStore::open(root.clone()).unwrap();
        let mut stale = terminal_record();
        stale.status = RunStatus::CleanupFailed;
        stale.cleanup_outcome = super::CleanupOutcome::Failed;
        store
            .create_json(&super::run_record_path(&stale.id), &stale)
            .unwrap();

        let mut cleaned = stale.clone();
        cleaned.status = RunStatus::Cleaned;
        cleaned.cleanup_outcome = super::CleanupOutcome::Succeeded;
        let cleaned_at = Utc::now();
        cleaned.finished_at = Some(cleaned_at);
        let payload = super::CleanupReceiptPayload {
            record: cleaned.clone(),
            owner: super::owner_marker_for_record(&cleaned),
            result: serde_json::to_value(&cleaned).unwrap(),
            artifacts: serde_json::json!({"schema":"attended_artifact_summary_v1"}),
            repositories: serde_json::json!([]),
            cleanup: serde_json::json!({"schema":"attended_cleanup_result_v1","outcome":"succeeded"}),
            cleanup_evidence: super::AttendedCleanupEvidence::default(),
            host_ledger: super::HostLedgerSummary::default(),
        };
        let receipt = super::CleanupReceipt {
            schema: super::CLEANUP_RECEIPT_SCHEMA_V1.into(),
            run_id: cleaned.id.clone(),
            authorization_digest: cleaned.authorization_digest.clone(),
            cleaned_at,
            checksum: super::receipt_checksum(&payload).unwrap(),
            payload,
        };
        store
            .create_json(&super::receipt_path(&cleaned.id), &receipt)
            .unwrap();

        assert_eq!(
            super::status(&store, &cleaned.id).unwrap().status,
            RunStatus::Cleaned
        );
        assert_eq!(super::list(&store).unwrap()[0].status, RunStatus::Cleaned);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn run_record_uuid_lineage_is_strict_and_actionable() {
        let mut record = terminal_record();
        assert!(super::validate_run_record(&record, &record.run_id).is_ok());

        record.attempt_ancestry = vec![uuid::Uuid::new_v4().to_string()];
        record.attempt_number = 2;
        let error = super::validate_run_record(&record, &record.run_id).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("execution without a parent attempt must not have ancestry")
        );

        record.parent_attempt_id = record.attempt_ancestry.last().cloned();
        record.attempt_number = 1;
        let error = super::validate_run_record(&record, &record.run_id).unwrap_err();
        assert!(format!("{error:#}").contains("must equal ancestry length plus one"));
        record.attempt_number = 2;
        assert!(super::validate_run_record(&record, &record.run_id).is_ok());
        record.attempt_ancestry.push(record.attempt_id.clone());
        record.attempt_number = 3;
        record.parent_attempt_id = Some(record.attempt_id.clone());
        let error = super::validate_run_record(&record, &record.run_id).unwrap_err();
        assert!(format!("{error:#}").contains("contains the current attempt"));
    }

    fn configure_workspace_export(record: &mut RunRecord, workspace: &std::path::Path) -> PathBuf {
        let export_root = workspace.join(".agent-hub/runs");
        let export_run = export_root.join(&record.run_id);
        record.workspace_export = Some(super::WorkspaceExportStatus {
            root: export_root.display().to_string(),
            run_path: export_run.display().to_string(),
            workspace_alias: "primary".into(),
            workspace_identity: crate::git_view::directory_identity(workspace).unwrap(),
            state: "pending".into(),
            error: None,
            authoritative: false,
            files: BTreeMap::new(),
        });
        export_run
    }

    #[test]
    fn workspace_export_is_bounded_redacted_atomic_and_retained() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-workspace-export-{}",
            uuid::Uuid::new_v4()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("private-run");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("stdout.log"), b"credential-value output").unwrap();
        std::fs::write(run_dir.join("stderr.log"), b"oversized stderr").unwrap();
        std::fs::write(run_dir.join("transcript.jsonl"), b"{}\n").unwrap();
        std::fs::write(
            run_dir.join("repositories.json"),
            br#"[{"workspace_alias":"primary","token":"must-not-export"}]"#,
        )
        .unwrap();
        std::fs::write(
            run_dir.join("artifacts.json"),
            br#"{"schema":"attended_artifacts_v1","credentials":"must-not-export"}"#,
        )
        .unwrap();
        std::fs::write(
            run_dir.join("cleanup.json"),
            br#"{"outcome":"succeeded","effects":{"secret":"must-not-export"}}"#,
        )
        .unwrap();
        std::fs::write(run_dir.join("credentials.json"), b"must-not-export").unwrap();

        let mut record = terminal_record();
        record.agent_final.as_mut().unwrap().summary = "credential-value result".into();
        let export_run = configure_workspace_export(&mut record, &workspace);
        let limits = protocol::AttendedOutputLimits {
            stdout_bytes: 64,
            stderr_bytes: 4,
            transcript_bytes: 64,
            artifact_bytes: 16_384,
        };
        super::publish_workspace_export(
            &mut record,
            &run_dir,
            &limits,
            Some(&["credential-value".into()]),
            false,
            true,
        );
        assert_eq!(record.workspace_export.as_ref().unwrap().state, "finalized");
        assert_eq!(
            std::fs::read(export_run.join("stdout.log")).unwrap(),
            b"[REDACTED] output"
        );
        assert!(!export_run.join("stderr.log").exists());
        assert!(!export_run.join("credentials.json").exists());
        let all_exported = std::fs::read_dir(&export_run)
            .unwrap()
            .flat_map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
            .collect::<Vec<_>>();
        let all_exported = String::from_utf8_lossy(&all_exported);
        assert!(!all_exported.contains("must-not-export"));
        assert!(!all_exported.contains("credential-value"));
        assert!(!all_exported.contains("owner_nonce"));
        assert!(!all_exported.contains("authorization_digest"));
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(export_run.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["authoritative"], false);
        assert_eq!(manifest["trusted"], false);
        assert_eq!(manifest["files"]["stdout.log"]["truncated"], false);
        assert_eq!(
            manifest["omitted"]["stderr.log"],
            "source_exceeds_reviewed_bound"
        );

        std::fs::remove_dir_all(&run_dir).unwrap();
        assert!(export_run.join("manifest.json").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn absent_workspace_export_has_no_workspace_side_effects() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-no-workspace-export-{}",
            uuid::Uuid::new_v4()
        ));
        let run_dir = root.join("private-run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut record = terminal_record();
        super::publish_workspace_export(
            &mut record,
            &run_dir,
            &protocol::AttendedOutputContract::default().limits,
            None,
            false,
            true,
        );
        assert!(record.workspace_export.is_none());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_export_omits_unprovable_output_and_removes_stale_allowlisted_files() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-workspace-export-omitted-{}",
            uuid::Uuid::new_v4()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("private-run");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("stdout.log"), b"unproven free text").unwrap();
        let mut record = terminal_record();
        let export_run = configure_workspace_export(&mut record, &workspace);
        super::publish_workspace_export(
            &mut record,
            &run_dir,
            &protocol::AttendedOutputContract::default().limits,
            Some(&[]),
            false,
            true,
        );
        assert!(export_run.join("stdout.log").is_file());

        super::publish_workspace_export(
            &mut record,
            &run_dir,
            &protocol::AttendedOutputContract::default().limits,
            None,
            false,
            true,
        );
        assert!(!export_run.join("stdout.log").exists());
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(export_run.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(
            manifest["omitted"]["stdout.log"],
            "redaction_proof_unavailable"
        );
        assert!(manifest["files"].get("stdout.log").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_export_rejects_symlink_tampering_without_changing_run_outcome() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "agent-hub-workspace-export-tamper-{}",
            uuid::Uuid::new_v4()
        ));
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        let run_dir = root.join("private-run");
        std::fs::create_dir_all(workspace.join(".agent-hub/runs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut record = terminal_record();
        symlink(
            &outside,
            workspace.join(".agent-hub/runs").join(&record.run_id),
        )
        .unwrap();
        configure_workspace_export(&mut record, &workspace);
        let outcome = record.execution_outcome.clone();
        super::publish_workspace_export(
            &mut record,
            &run_dir,
            &protocol::AttendedOutputContract::default().limits,
            Some(&[]),
            false,
            true,
        );
        assert_eq!(record.execution_outcome, outcome);
        assert_eq!(record.workspace_export.as_ref().unwrap().state, "failed");
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_export_rejects_symlinked_workspace_ancestor() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "agent-hub-workspace-export-ancestor-{}",
            uuid::Uuid::new_v4()
        ));
        let real_parent = root.join("real");
        let workspace = real_parent.join("workspace");
        let linked_parent = root.join("linked");
        let run_dir = root.join("private-run");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&run_dir).unwrap();
        symlink(&real_parent, &linked_parent).unwrap();
        let linked_workspace = linked_parent.join("workspace");
        let mut record = terminal_record();
        configure_workspace_export(&mut record, &linked_workspace);
        super::publish_workspace_export(
            &mut record,
            &run_dir,
            &protocol::AttendedOutputContract::default().limits,
            Some(&[]),
            false,
            true,
        );
        assert_eq!(record.workspace_export.as_ref().unwrap().state, "failed");
        assert!(!workspace.join(".agent-hub").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_export_unlinks_temporary_file_when_atomic_replace_fails() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-workspace-export-temp-{}",
            uuid::Uuid::new_v4()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("private-run");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut record = terminal_record();
        let export_run = configure_workspace_export(&mut record, &workspace);
        std::fs::create_dir_all(export_run.join("status.json")).unwrap();
        super::publish_workspace_export(
            &mut record,
            &run_dir,
            &protocol::AttendedOutputContract::default().limits,
            Some(&[]),
            false,
            true,
        );
        assert_eq!(record.workspace_export.as_ref().unwrap().state, "failed");
        assert!(!std::fs::read_dir(&export_run).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_export_atomic_publish_survives_concurrent_workspace_writes() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-workspace-export-concurrent-{}",
            uuid::Uuid::new_v4()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("private-run");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("stdout.log"), b"complete output").unwrap();
        let mut record = terminal_record();
        let export_run = configure_workspace_export(&mut record, &workspace);
        super::publish_workspace_export(
            &mut record,
            &run_dir,
            &protocol::AttendedOutputContract::default().limits,
            Some(&[]),
            false,
            true,
        );
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_stop = stop.clone();
        let target = export_run.join("status.json");
        let writer = std::thread::spawn(move || {
            while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::write(&target, b"workspace writer");
            }
        });
        for _ in 0..20 {
            super::publish_workspace_export(
                &mut record,
                &run_dir,
                &protocol::AttendedOutputContract::default().limits,
                Some(&[]),
                false,
                true,
            );
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        super::publish_workspace_export(
            &mut record,
            &run_dir,
            &protocol::AttendedOutputContract::default().limits,
            Some(&[]),
            false,
            true,
        );
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(export_run.join("status.json")).unwrap(),
        )
        .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
