use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Read;
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
    AttendedApprovalBundleIdentity, AttendedAuthorizationArtifact, AttendedAuthorizationRequestV2,
    AttendedAuthorizationRequestV3, AttendedExecutionSpec, AttendedLifecycleContract,
    AttendedOutputContract, AttendedProposal, AuthorizedCredentialProvider, AuthorizedOverlay,
    CanonicalRuntimeSpec, ExecutionMode, NetworkResourceKind, NetworkResourceRequest,
    PlannedEnvironmentTier, PromptRef, ResolvedAuthorizationBehavior, ToolApprovalMode,
    WorkspaceAccess,
    canonical_environment_for_execution, canonical_permissions_for_environment,
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

pub const DIPPY_SELECTOR_PREFIX: &str = "agent-hub local authorize";
pub const RESULT_SCHEMA_V3: &str = "attended_result_v3";
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
    if std::env::var("AGENT_HUB_ATTENDED_FAULT").as_deref() == Ok(boundary) {
        anyhow::bail!("injected attended fault at {boundary}");
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    Prelaunch,
    ProviderServiceUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
}

#[derive(Debug, Clone)]
pub(crate) struct RevalidatedAuthorization {
    pub artifact: AttendedAuthorizationArtifact,
    pub authorization_path: String,
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
    #[serde(default)]
    pub cleanup_outcome: CleanupOutcome,
    #[serde(default)]
    pub agent_final: Option<protocol::AgentResultV1>,
    pub retry_class: Option<RetryClass>,
    pub retry_eligible: bool,
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

#[derive(Debug, Clone, Serialize)]
struct CleanupCheck {
    status: &'static str,
    error: Option<String>,
}

impl CleanupCheck {
    fn not_applicable() -> Self {
        Self {
            status: "not_applicable",
            error: None,
        }
    }

    fn from_result(result: &anyhow::Result<()>) -> Self {
        match result {
            Ok(()) => Self {
                status: "verified_absent",
                error: None,
            },
            Err(error) => Self {
                status: "failed",
                error: Some(format!("{error:#}")),
            },
        }
    }

    fn complete(&self) -> bool {
        matches!(self.status, "verified_absent" | "not_applicable")
    }
}

#[derive(Debug, Clone, Serialize)]
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
}

#[derive(Debug, Deserialize)]
struct BundleNetworkPolicy {
    name: String,
}

pub async fn build_proposal(input: ProposalInput) -> anyhow::Result<AttendedAuthorizationArtifact> {
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
    ensure!(
        !overlays.windows(2).any(|items| items[0] == items[1]),
        "duplicate overlay selector"
    );
    ensure!(
        overlays
            .iter()
            .all(|overlay| environment.allowed_overlays.contains(overlay)),
        "requested overlay is not allowed by the environment"
    );
    ensure!(
        overlays.is_empty(),
        "attended execution cannot apply environment overlays; reject the proposal instead of silently ignoring them"
    );
    let approved_roots = approved_roots(&deployment.local_agent)?;
    let mounts = resolve_mounts(
        &approved_roots,
        &input.workspaces,
        &input.context_roots,
        &input.primary_workspace,
    )?;
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
    validate_credential_descriptors(&deployment, &runner)?;
    let credential_providers = authorized_credential_providers(&deployment, &runner)?;
    let overlay_descriptors = authorized_overlays(&deployment, &overlays)?;
    let docker =
        which::which("docker").context("Docker CLI is required to resolve the reviewed image")?;
    let image_id = inspect_image_identity(
        &docker,
        &environment.image,
        &Deadline::new(Some(30))?,
        &Cancellation::never(),
    )
    .await?;
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
        allow_remote_credentials: !runner.credential_providers.is_empty(),
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
        git_views: mounts.git_views,
        approval_bundle_digest,
        approval_bundle,
        output: AttendedOutputContract::default(),
        lifecycle: AttendedLifecycleContract::default(),
    }
    .canonical_artifact()
    .map_err(anyhow::Error::msg)
}

fn prompt_with_result_contract(prompt: &str) -> String {
    format!(
        "{prompt}\n\nYour terminal response MUST be exactly one JSON object matching this schema. Use OpenCode StructuredOutput when available; otherwise emit the object as the final response. Do not wrap it in Markdown. A complete result cannot contain blockers or failed acceptance results.\n{{\"schema\":\"agent_result_v1\",\"complete\":true|false,\"summary\":\"non-empty summary\",\"acceptance_results\":[{{\"criterion\":\"criterion\",\"passed\":true|false,\"evidence\":\"non-empty evidence\"}}],\"blockers\":[\"blocker\"],\"artifact_references\":[{{\"path\":\"path\",\"description\":\"non-empty description\"}}]}}\n"
    )
}

pub fn render_proposal(
    path: &Path,
    artifact: &AttendedAuthorizationArtifact,
) -> anyhow::Result<String> {
    let request = artifact.verify_v3().map_err(anyhow::Error::msg)?;
    let canonical_path = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize authorization path {}", path.display()))?;
    let approval_bundle = request.approval_bundle.as_ref().map_or_else(
        || "Approval bundle: none (automatic)\n".to_string(),
        |bundle| {
            format!(
                "Approval bundle name: {}\nApproval bundle protocol: {}\nApproval bundle digest: {}\nApproval manifest SHA-256: {}\nGateway commit: {}\nGateway archive SHA-256: {}\nPlugin SHA-256: {}\nDippy commit: {}\nDippy archive SHA-256: {}\nDippy wheel SHA-256: {}\n",
                bundle.name,
                bundle.protocol,
                bundle.digest,
                bundle.manifest_sha256,
                bundle.gateway_commit,
                bundle.gateway_archive_sha256,
                bundle.plugin_sha256,
                bundle.dippy_commit,
                bundle.dippy_archive_sha256,
                bundle.dippy_wheel_sha256,
            )
        },
    );
    let git_capabilities = request
        .git_views
        .iter()
        .map(|view| {
            format!(
                "{}: isolated read-only Git metadata ({:?}), workspace content {:?}, HEAD {}, ref {}, remote {}",
                view.workspace_alias,
                view.repository_type,
                view.access,
                view.head,
                view.reference.as_deref().unwrap_or("detached"),
                view.remote.as_deref().unwrap_or("none"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "Environment: {}\nImage family: {}\nImage tag: {}\nImage ID: {}\nNetwork class: {}\nNetwork resources: {}\nTool approval: {}\n{}Overlays: {}\nTimeout seconds: {}\nGit capability:\n{}\nPrompt digest: {}\nPrompt:\n{}\n\nAuthorization digest: {}\nAuthorize:\n{} --environment {} --image-family {} --network-class {}{} --tool-approval {}{} --timeout-secs {} --authorization {} --digest {}\n",
        request.proposal.environment,
        request.proposal.image_family,
        request.proposal.image_tag,
        request.proposal.image_id,
        request.proposal.network_class,
        resource_list(&request.proposal.network_resources),
        tool_approval_name(request.proposal.tool_approval),
        approval_bundle,
        request.proposal.overlays.join(","),
        request.proposal.timeout_secs,
        git_capabilities,
        request.proposal.prompt_digest,
        request.proposal.prompt,
        artifact.digest,
        DIPPY_SELECTOR_PREFIX,
        shell_word(&request.proposal.environment),
        shell_word(&request.proposal.image_family),
        shell_word(&request.proposal.network_class),
        repeated_selector_args(
            "network-resource",
            request
                .proposal
                .network_resources
                .iter()
                .map(ToString::to_string)
        ),
        tool_approval_name(request.proposal.tool_approval),
        repeated_selector_args("overlay", request.proposal.overlays.iter().cloned()),
        request.proposal.timeout_secs,
        shell_word(&path_string(&canonical_path)?),
        artifact.digest,
    ))
}

pub(crate) async fn revalidate_authorization(
    path: &Path,
    selectors: AuthorizationSelectors<'_>,
) -> anyhow::Result<RevalidatedAuthorization> {
    let (artifact, original_authorization_path) = read_authorization_file(path)?;
    let request = artifact.verify_v3().map_err(anyhow::Error::msg)?;
    verify_authorization_selectors(&request, &artifact, &selectors)?;
    verify_execution_inputs(&request)?;
    let docker =
        which::which("docker").context("Docker CLI is required to authorize attended execution")?;
    let current_image = inspect_image_identity(
        &docker,
        &request.proposal.image_tag,
        &Deadline::new(Some(30))?,
        &Cancellation::never(),
    )
    .await?;
    verify_image_identity(&request.proposal.image_id, &current_image)?;
    Ok(RevalidatedAuthorization {
        artifact,
        authorization_path: original_authorization_path,
    })
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
    selectors: AuthorizationSelectors<'_>,
    relay_override: RelayEndpointOverride,
) -> anyhow::Result<RunRecord> {
    let (supplied, supplied_path) = read_authorization_file(artifact_path)?;
    let request = supplied.verify_v3().map_err(anyhow::Error::msg)?;
    let digest = supplied.digest.clone();
    verify_authorization_selectors(&request, &supplied, &selectors)?;
    let authorized_path = authorization_path("records", &digest);
    let authorized: AuthorizationRecord = store
        .read_json(&authorized_path)
        .context("attended authorization has not been approved")?;
    ensure!(
        authorized.artifact == supplied && authorized.authorization_path == supplied_path,
        "authorized artifact does not match supplied artifact"
    );
    verify_execution_inputs(&request)?;
    let deployment =
        config::validate_authoritative_config_root(Path::new(&request.execution.config_root))
            .map_err(anyhow::Error::msg)?;
    let relay_endpoint = resolve_host_relay_endpoint(&deployment, relay_override).await?;
    let _lock = store.lock(&lock_path(&digest))?;
    let mut authorized: AuthorizationRecord = store.read_json(&authorized_path)?;
    ensure!(
        authorized.artifact == supplied && authorized.authorization_path == supplied_path,
        "authorized artifact does not match supplied artifact"
    );
    ensure!(
        authorized.domain == "attended",
        "authorization receipt domain mismatch"
    );
    let runner: config::ResolvedRunnerProfile =
        serde_json::from_value(request.execution.behavior.runner_profile.clone())?;
    ensure!(
        request.execution.behavior.allow_remote_credentials
            == !runner.credential_providers.is_empty(),
        "attended credential envelope does not match the authorized runner"
    );
    ensure!(
        runner.mcp_bridges.is_empty(),
        "attended MCP bridge material is unavailable from the executor substrate"
    );
    verify_tool_approval(request.proposal.tool_approval, Some(&relay_endpoint))?;
    let docker_path =
        which::which("docker").context("Docker CLI is required for attended execution")?;
    if let Some(active) = authorized.active_attempt_id.clone() {
        let mut active_record: RunRecord = store.read_json(&run_record_path(&active))?;
        let active_dir = store.path(&PathBuf::from("runs").join(&active_record.id))?;
        let expected = owner_marker_for_record(&active_record);
        if active_dir.exists() {
            verify_run_owner_marker(&active_dir, &expected)?;
            recover_owned_run_resources(
                &docker_path,
                &active_dir,
                &active_record.id,
                &active_record.container_name,
                &active_record.ownership,
                Some(&relay_endpoint),
                &expected,
            )
            .await
            .context("reconcile interrupted attended attempt")?;
        }
        ensure!(
            authorized
                .attempts
                .last()
                .is_some_and(|attempt| attempt.attempt_id == active),
            "authorization active attempt does not match receipt lineage"
        );
        let previous = authorized.attempts.last_mut().expect("checked above");
        previous.state = LaunchState::Terminal;
        let prelaunch = active_record.launch_state == LaunchState::Preparing;
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
            "interrupted attempt reached launch_pending; authorization consumed fail-closed".into()
        });
        store.replace_json(&run_record_path(&active), &active_record)?;
        store.replace_json(&authorized_path, &authorized)?;
        ensure!(prelaunch, "attended authorization was consumed fail-closed");
    }
    let retry = &request.lifecycle.retry;
    ensure!(
        authorized.attempts.len() < retry.max_attempts as usize,
        "attended authorization exhausted its reviewed attempt limit"
    );
    if let Some(previous) = authorized.attempts.last() {
        ensure!(
            previous.retry_eligible,
            "attended authorization was already consumed"
        );
    }
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
        schema: result_schema_v3(),
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
        cleanup_outcome: CleanupOutcome::Pending,
        agent_final: None,
        retry_class: None,
        retry_eligible: false,
    };
    authorized.active_attempt_id = Some(attempt_id.clone());
    authorized.attempts.push(AttemptLineage {
        attempt_id,
        run_id,
        state: LaunchState::Preparing,
        retry_class: None,
        retry_eligible: false,
        started_at: record.started_at,
        finished_at: None,
    });
    store.create_json(&run_record_path(&id), &record)?;
    store.replace_json(&authorized_path, &authorized)?;
    attended_fault("preparing")?;

    let mut captured_execution_outcome = None;
    let mut captured_process_status = None;
    let mut captured_permission_outcome = None;
    let mut captured_cleanup_evidence = None;
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
    let poll = AbortTask(tokio::spawn(async move {
        loop {
            if cancel_marker.exists() {
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
            &runner,
            deadline.wall_clock(),
            &data_dir,
            &effects,
        )
        .await?;
        credential_cleanup.0 = credentials.temp_files.clone();
        verify_materialized_credential_identities(&request, &credentials)?;
        let credential_material = credentials.material()?;
        let redaction_values = credential_material.redaction_values.clone();
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
        for (name, value) in [
            ("HOME", "/agent-hub/git/home"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/agent-hub/git/home/.gitconfig"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_PAGER", "cat"),
            ("PAGER", "cat"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
        ] {
            environment.insert(name.into(), value.into());
        }
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
             update_authorization_attempt_state(
                 store,
                 &authorized_path,
                 &mut authorized,
                 &record.attempt_id,
                 LaunchState::LaunchPending,
             )?;
             attended_fault("launch_pending")?;
             authorized.state = LaunchState::Consumed;
             update_authorization_attempt_state(
                 store,
                 &authorized_path,
                 &mut authorized,
                 &record.attempt_id,
                 LaunchState::Consumed,
             )?;
             attended_fault("consumed")?;
             record.launch_state = LaunchState::Consumed;
             record.status = RunStatus::Consumed;
             store.replace_json(&run_record_path(&id), &record)?;
             record.launch_state = LaunchState::Running;
             record.status = RunStatus::Running;
             store.replace_json(&run_record_path(&id), &record)?;
             update_authorization_attempt_state(
                 store,
                 &authorized_path,
                 &mut authorized,
                 &record.attempt_id,
                 LaunchState::Running,
             )?;
             attended_fault("running")?;
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
        let run_cancelled = cancellation.requested()
            || matches!(&operation, Ok(result) if result.status == ExecutionStatus::Cancelled);
        let mut relay_cleanup = Ok(());
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
                status: "verified_absent",
                error: None,
            },
            (CleanupStatus::Failed(error), _) => CleanupCheck {
                status: "failed",
                error: Some(error.clone()),
            },
            (CleanupStatus::Complete, Err(error)) => CleanupCheck {
                status: "failed",
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
            relay: CleanupCheck::from_result(&relay_cleanup),
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
                    ExecutionStatus::Exited(0) => record.status = RunStatus::Succeeded,
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
                        status: "failed",
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
                Some(&relay_endpoint),
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
            let result_path = run_dir.join(&request.output.result);
            let artifacts_path = run_dir.join(&request.output.artifacts);
            let repositories_path = run_dir.join(&request.output.repositories);
            let cleanup_path = run_dir.join(&request.output.cleanup);
            let mut repositories = Vec::new();
            for repository in &request.execution.repositories {
                let ending = if repository.starting.head.is_some() {
                    crate::local::repository_state(Path::new(&repository.repository_root))?
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
            write_private_durable_bounded(
                &result_path,
                &serialize_pretty_bounded(&record, request.output.limits.artifact_bytes)?,
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
        }
        Ok(())
    })();
    if let Err(error) = output_result {
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
        && cleanup_complete
        && verify_execution_inputs(&request).is_ok();
    let provider_retry = record.launch_state == LaunchState::Running
        && cleanup_complete
        && provider_unavailable_retry_evidence(&record, &run_dir, &request)?;
    record.retry_class = if prelaunch_retry {
        Some(RetryClass::Prelaunch)
    } else if provider_retry {
        Some(RetryClass::ProviderServiceUnavailable)
    } else {
        None
    };
    record.retry_eligible = match record.retry_class {
        Some(RetryClass::Prelaunch) => {
            authorized
                .attempts
                .iter()
                .filter(|attempt| attempt.retry_class == Some(RetryClass::Prelaunch))
                .count()
                < request.lifecycle.retry.max_prelaunch_retries as usize
        }
        Some(RetryClass::ProviderServiceUnavailable) => {
            authorized
                .attempts
                .iter()
                .filter(|attempt| {
                    attempt.retry_class == Some(RetryClass::ProviderServiceUnavailable)
                })
                .count()
                < request.lifecycle.retry.max_provider_unavailable_retries as usize
        }
        None => false,
    } && authorized.attempts.len()
        < request.lifecycle.retry.max_attempts as usize;
    record.launch_state = LaunchState::Terminal;
    let lineage = authorized
        .attempts
        .last_mut()
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
    store.replace_json(&run_record_path(&id), &record)?;
    store.replace_json(&authorized_path, &authorized)?;
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
        .list_json(Path::new("executions"))
        .map(|records| records.into_iter().map(normalize_legacy_record).collect())
}

pub fn status(store: &StateStore, id: &str) -> anyhow::Result<RunRecord> {
    validate_id(id)?;
    match store.read_json(&run_record_path(id)) {
        Ok(record) => Ok(normalize_legacy_record(record)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(read_receipt(store, id)?.payload.record)
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
    result: serde_json::Value,
    artifacts: serde_json::Value,
    repositories: serde_json::Value,
    cleanup: serde_json::Value,
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

#[derive(Debug, Serialize)]
pub struct LocalResult {
    pub schema: &'static str,
    pub run_id: String,
    pub agent_final: Option<protocol::AgentResultV1>,
    pub process_outcome: ProcessOutcome,
    pub agent_outcome: AgentOutcome,
    pub execution_outcome: ExecutionOutcome,
    pub permission_outcome: PermissionOutcome,
    pub cleanup_outcome: CleanupOutcome,
    pub exit_code: Option<i32>,
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
    let paths = BTreeMap::from([
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
    Ok(LocalResult {
        schema: "local_result_v1",
        run_id: id.to_string(),
        agent_final: record.agent_final.clone(),
        process_outcome: record.process_outcome.clone(),
        agent_outcome: record.agent_outcome.clone(),
        execution_outcome: record.execution_outcome.clone(),
        permission_outcome: record.permission_outcome.clone(),
        cleanup_outcome: record.cleanup_outcome.clone(),
        exit_code: record.exit_code,
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
    let (bytes, total, path) = match store.read_limited(&run_relative, limit_bytes) {
        Ok((bytes, total)) => (bytes, total, store.path(&run_relative)?),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            let receipt = read_receipt(store, id)?;
            let value = match name {
                "result.json" => receipt.payload.result,
                "artifacts.json" => receipt.payload.artifacts,
                "repositories.json" => receipt.payload.repositories,
                "cleanup.json" => receipt.payload.cleanup,
                _ => unreachable!(),
            };
            let encoded = serde_json::to_vec_pretty(&value)?;
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
            && record.cleanup_outcome == CleanupOutcome::Succeeded,
        "local run {} has execution outcome {:?} and cleanup outcome {:?}",
        record.id,
        record.execution_outcome,
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
    let ownership = DockerOwnership::new(record.ownership)?;
    let deadline = std::time::Instant::now()
        .checked_add(CANCELLATION_TERMINAL_TIMEOUT)
        .context("cancellation deadline overflow")?;
    docker
        .remove_owned(&record.container_name, &ownership, PRIMARY_CLEANUP_TIMEOUT)
        .await?;
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
        Ok(receipt) => return Ok(receipt.payload.record),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let run_dir = store.path(&PathBuf::from("runs").join(id))?;
    let pending = pending_receipt_path(id);
    let pending_path = store.path(&pending)?;
    if pending_path.exists() && !run_dir.exists() {
        let receipt: CleanupReceipt = store.read_json(&pending)?;
        verify_receipt(&receipt, id)?;
        publish_pending_receipt(store, id, &pending_path)?;
        store.replace_json(&run_record_path(id), &receipt.payload.record)?;
        return Ok(receipt.payload.record);
    }
    if pending_path.exists() {
        std::fs::remove_file(&pending_path)?;
    }
    let mut record = status(store, id)?;
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
    let artifacts_document = store
        .read_json::<serde_json::Value>(&PathBuf::from("runs").join(id).join("artifacts.json"))?;
    let repositories_document = store.read_json::<serde_json::Value>(
        &PathBuf::from("runs").join(id).join("repositories.json"),
    )?;
    let cleanup_document = serde_json::json!({
        "schema": "attended_cleanup_result_v1",
        "outcome": "succeeded",
        "cleaned_at": record.finished_at,
        "error": null,
    });
    let payload = CleanupReceiptPayload {
        record: record.clone(),
        result: result_document,
        artifacts: artifacts_document,
        repositories: repositories_document,
        cleanup: cleanup_document,
    };
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
    store.replace_json(&run_record_path(id), &record)?;
    Ok(record)
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
        authorized_credential_providers(&deployment, &runner)?
            == request.execution.credential_providers,
        "authorized credential provider descriptors changed"
    );
    ensure!(
        authorized_overlays(&deployment, &request.proposal.overlays)? == request.execution.overlays,
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
    let resolved = resolve_mounts(
        &approved,
        &workspaces,
        &contexts,
        &request.execution.primary_workspace,
    )?;
    ensure!(
        resolved.workspaces == request.execution.workspaces
            && resolved.context_roots == request.execution.context_roots
            && resolved.repositories == request.execution.repositories
            && resolved.git_views == request.git_views,
        "workspace or repository identity changed after authorization"
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
    request: &AttendedAuthorizationRequestV2,
) -> anyhow::Result<bool> {
    let stderr = read_optional_bounded(
        &run_dir.join("stderr.log"),
        request.output.limits.stderr_bytes,
    )?;
    let failure = format!(
        "{}\n{}",
        record.error.as_deref().unwrap_or_default(),
        String::from_utf8_lossy(&stderr)
    );
    let service_unavailable = failure.contains("ServiceUnavailable")
        || failure.contains("service unavailable")
        || failure.contains("503");
    if !service_unavailable {
        return Ok(false);
    }
    for path in [
        run_dir.join("stdout.log"),
        run_dir.join(&request.output.transcript),
    ] {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && metadata.len() == 0 => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Ok(false),
        }
    }
    let ledger_path = run_dir.join("supervision-transitions.jsonl");
    let ledger = match std::fs::read_to_string(&ledger_path) {
        Ok(ledger) => ledger,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for line in ledger.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)
            .context("invalid host supervision ledger while proving retry eligibility")?;
        if value.get("transition").and_then(serde_json::Value::as_str) != Some("plugin_ready") {
            return Ok(false);
        }
    }
    for repository in &request.execution.repositories {
        if crate::local::repository_state(Path::new(&repository.repository_root))?
            != repository.starting
        {
            return Ok(false);
        }
    }
    Ok(provider_retry_proofs_complete(
        service_unavailable,
        false,
        false,
        false,
        true,
    ))
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
    runner: &config::ResolvedRunnerProfile,
    valid_through: Option<std::time::SystemTime>,
    credential_dir: &Path,
    cancellation: &agent_hub_executor::ExecutionCancellationContext,
) -> anyhow::Result<agent_hub_executor::credential_provider::ResolvedCredentials> {
    let configured = configured_credential_providers(deployment)?;
    let providers = runner
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
            Ok(AuthorizedOverlay {
                name: overlay.name.clone(),
                provider: overlay.provider.clone(),
                config: serde_json::to_string(&overlay.config)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    overlays.sort();
    Ok(overlays)
}

fn authorized_credential_providers(
    deployment: &config::ValidatedDeployment,
    runner: &config::ResolvedRunnerProfile,
) -> anyhow::Result<Vec<AuthorizedCredentialProvider>> {
    let configured = configured_credential_providers(deployment)?;
    let mut descriptors = runner
        .credential_providers
        .iter()
        .map(|required| {
            let provider = configured.get(&required.name).with_context(|| {
                format!(
                    "required credential provider '{}' is unavailable",
                    required.name
                )
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
) -> anyhow::Result<()> {
    let configured = configured_credential_providers(deployment)?;
    for descriptor in &runner.credential_providers {
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
        {
            structured.push(value);
        }
        structured
    };

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
    let roots = local_agent
        .get("approved_repository_roots")
        .and_then(serde_json::Value::as_array)
        .context("local-agent config requires approved_repository_roots")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(PathBuf::from)
                .context("approved root must be a string")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    ApprovedRoots::new(roots)
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

fn resource_list(resources: &[NetworkResourceKind]) -> String {
    if resources.is_empty() {
        "none".to_string()
    } else {
        resources
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn repeated_selector_args(name: &str, values: impl IntoIterator<Item = String>) -> String {
    values
        .into_iter()
        .map(|value| format!(" --{name} {}", shell_word(&value)))
        .collect()
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._/:@".contains(&byte))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
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
        .last_mut()
        .context("authorization receipt has no active attempt")?;
    ensure!(
        authorization.active_attempt_id.as_deref() == Some(attempt_id)
            && attempt.attempt_id == attempt_id,
        "authorization receipt active attempt mismatch"
    );
    attempt.state = state;
    store.replace_json(path, authorization)
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
    let marker_path = run_dir.join("owner.json");
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_CLOEXEC,
    );
    let marker: RunOwnerMarker = serde_json::from_reader(
        options
            .open(&marker_path)
            .with_context(|| format!("open run owner marker {}", marker_path.display()))?,
    )?;
    ensure!(
        marker == *expected,
        "attended run owner marker does not exactly match the execution journal"
    );
    Ok(())
}

fn create_owned_run_directory(run_dir: &Path, marker: &RunOwnerMarker) -> anyhow::Result<()> {
    ensure!(!run_dir.exists(), "attended run directory already exists");
    let parent = run_dir.parent().context("run directory has no parent")?;
    let staging = parent.join(format!(".owner-{}", marker.nonce));
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

fn receipt_path(id: &str) -> PathBuf {
    PathBuf::from("receipts").join(format!("{id}.json"))
}

fn pending_receipt_path(id: &str) -> PathBuf {
    PathBuf::from("receipts").join(format!(".{id}.pending.json"))
}

fn normalize_legacy_record(mut record: RunRecord) -> RunRecord {
    record.schema = result_schema_v3();
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
    record
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
    ensure!(
        receipt.authorization_digest == receipt.payload.record.authorization_digest,
        "cleanup receipt authorization mismatch"
    );
    ensure!(
        receipt.checksum == receipt_checksum(&receipt.payload)?,
        "cleanup receipt checksum mismatch"
    );
    Ok(())
}

fn publish_pending_receipt(
    store: &StateStore,
    id: &str,
    pending_path: &Path,
) -> anyhow::Result<()> {
    let receipt_path = store.path(&receipt_path(id))?;
    std::fs::hard_link(pending_path, &receipt_path)
        .with_context(|| format!("publish immutable cleanup receipt for {id}"))?;
    std::fs::remove_file(pending_path)?;
    File::open(receipt_path.parent().context("receipt has no parent")?)?.sync_all()?;
    Ok(())
}
fn cancel_path(id: &str) -> PathBuf {
    PathBuf::from("cancellations").join(format!("{id}.json"))
}

fn validate_id(id: &str) -> anyhow::Result<()> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok(),
        "invalid attended execution ID"
    );
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
    Ok((artifact, path_string(path)?))
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
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    std::io::Write::write_all(&mut file, bytes)?;
    set_private_file(path)?;
    file.sync_all()?;
    File::open(path.parent().context("attended output has no parent")?)?.sync_all()?;
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
        RunOwnerMarker, attended_execution_outcome, merge_execution_cleanup,
        network_class_within_ceiling, provider_retry_proofs_complete, resolve_network_resources,
        selector_resources, serialize_pretty_bounded, verify_exact_relay_mount,
        verify_host_only_approval_environment, verify_recovery_relay_endpoint,
        verify_run_owner_marker, verify_tool_approval, write_new_private_durable,
    };
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

    fn terminal_record() -> RunRecord {
        RunRecord {
            schema: super::result_schema_v3(),
            id: "a".repeat(24),
            authorization_digest: "a".repeat(64),
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
            cleanup_outcome: super::CleanupOutcome::Succeeded,
            agent_final: serde_json::from_value(complete_agent_result()).ok(),
        }
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

        record = terminal_record();
        record.execution_outcome = super::ExecutionOutcome::Failed;
        assert!(super::ensure_successful_outcome(&record).is_err());
        assert!(super::ensure_cleanup_successful(&record).is_ok());
        record.execution_outcome = super::ExecutionOutcome::Succeeded;
        record.cleanup_outcome = super::CleanupOutcome::Failed;
        assert!(super::ensure_successful_outcome(&record).is_err());
        assert!(super::ensure_cleanup_successful(&record).is_err());
    }

    #[test]
    fn retained_receipt_is_private_bounded_and_readable_without_runtime_payload() {
        let root = std::env::temp_dir().join(format!(
            "agent-hub-retained-receipt-{}",
            uuid::Uuid::new_v4()
        ));
        let store = StateStore::open(root.clone()).unwrap();
        let record = terminal_record();
        let payload = super::CleanupReceiptPayload {
            record: record.clone(),
            result: serde_json::to_value(&record).unwrap(),
            artifacts: serde_json::json!({"schema":"attended_artifacts_v1"}),
            repositories: serde_json::json!([]),
            cleanup: serde_json::json!({"schema":"attended_cleanup_result_v1","outcome":"succeeded"}),
        };
        let receipt = super::CleanupReceipt {
            schema: super::CLEANUP_RECEIPT_SCHEMA_V1.into(),
            run_id: record.id.clone(),
            authorization_digest: record.authorization_digest.clone(),
            cleaned_at: Utc::now(),
            checksum: super::receipt_checksum(&payload).unwrap(),
            payload,
        };
        store
            .create_json(&super::receipt_path(&record.id), &receipt)
            .unwrap();
        let result = super::local_result(&store, &record.id, 64).unwrap();
        assert!(result.result.reference.truncated);
        assert!(!result.stdout.available);
        assert_eq!(
            super::status(&store, &record.id).unwrap().exit_code,
            Some(0)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(store.path(&super::receipt_path(&record.id)).unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
