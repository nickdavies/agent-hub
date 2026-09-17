pub mod deployment_validation;
pub mod environment;
pub mod environment_bundle;
pub mod image;
pub mod network_policy;
pub mod root_registry;
mod rules;
pub mod runner;
pub mod system;
mod tools;
pub mod triage;
pub mod triage_mounts;

pub const SUPERVISED_OPENCODE_RUNTIME_VERSION: &str = "1.18.18";

pub use deployment_validation::{
    CapturedAuthoritativeFile, GithubOverlayAccess, GithubOverlayConfig, ValidatedDeployment,
    ValidatedImageManifest, ValidatedNetworkPolicy, ValidatedOverlay, ValidatedOverlayRegistry,
    ValidatedTriagePrompt, ValidatedTriagePromptContent, parse_overlay_registry,
    validate_authoritative_config_root,
};
pub use environment::EnvironmentConfig;
pub use environment_bundle::{
    DURABLE_OPENCODE_IMAGE, DURABLE_OPENCODE_MODEL, EnvironmentBundle, EnvironmentBundleFile,
    OPENCODE_CONFIG_SENTINEL, OPENCODE_CONFIG_SENTINEL_SHA256, materialize_environment_bundle,
    materialize_environment_bundle_with_snapshot, materialize_environment_bundle_with_system,
    validate_captured_approval_bundle, validate_durable_opencode_config,
};
pub use image::{
    ImageBuildContext, ImageConfig, ImageToolFactsManifest, JOB_CODE_PREFLIGHT_PATH,
    JOB_CODE_TOOL_MANIFEST_PATH, job_code_preflight_docker_args, job_code_tool_manifest_sha256,
    resolve_build_order, sha256_hex, validate_job_code_preflight_report,
    validate_supervised_opencode_preflight_report,
};
pub use network_policy::{
    FlattenedPolicy, NetworkPolicyConfig, PolicyAllowRule, canonical_policy_hosts, flatten_policy,
};
pub use root_registry::{RootConfig, RootRegistryConfig, RootRole, root_registry};
pub use rules::{
    ConfigAction, ConfigDecision, DefaultAction, RuleAction, RuleSummary, ToolConfig,
    default_to_resolved, load_tool_config, resolve_action, resolve_action_with_input,
    resolve_action_with_input_and_rule, validate_tool_config,
};
pub use runner::{
    CredentialProviderDescriptor, DURABLE_OPENCODE_PATH, HostApprovalsConfig,
    ManagedRunnerFileMount, ManagedRunnerKind, ManagedRunnerSpec, McpBridgeDescriptor,
    OPENCODE_PROJECT_CONFIG_ALIASES, OpencodeRunnerConfig, ResolvedRunnerProfile,
    RunnerProfilesConfig, TrustedMountDescriptor, canonicalize_resolved_runner_profile,
    digest_profile, directory_content_digests, supervised_opencode_runtime_env,
    trusted_opencode_runtime_env, validate_untrusted_opencode_env,
    validate_untrusted_opencode_env_name,
};
pub use system::{SystemApprovalBundleTrustConfig, SystemConfig, SystemScmHostConfig};
pub use tools::{is_in_workspace, normalize_path, resolve_path};
pub use triage::TriagePromptDefinition;
pub use triage_mounts::{
    TriageRepositoryMount, checked_in_triage_repository_mounts,
    project_server_repository_discovery, validate_triage_repository_snapshots,
};

/// Expand a leading `~/` to the user's home directory.
pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}
