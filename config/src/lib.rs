pub mod deployment_validation;
pub mod environment;
pub mod environment_bundle;
pub mod image;
pub mod network_policy;
mod rules;
pub mod runner;
pub mod system;
mod tools;
pub mod triage;
pub mod triage_mounts;

pub use deployment_validation::{
    CapturedAuthoritativeFile, ValidatedDeployment, ValidatedImageManifest, ValidatedNetworkPolicy,
    ValidatedOverlay, ValidatedOverlayRegistry, ValidatedTriagePrompt,
    ValidatedTriagePromptContent, validate_authoritative_config_root,
};
pub use environment::EnvironmentConfig;
pub use environment_bundle::{
    DURABLE_OPENCODE_IMAGE, DURABLE_OPENCODE_MODEL, EnvironmentBundle, EnvironmentBundleFile,
    OPENCODE_CONFIG_SENTINEL, OPENCODE_CONFIG_SENTINEL_SHA256, materialize_environment_bundle,
    materialize_environment_bundle_with_snapshot, materialize_environment_bundle_with_system,
    validate_durable_opencode_config,
};
pub use image::{
    ImageBuildContext, ImageConfig, JOB_CODE_PREFLIGHT_PATH, JOB_CODE_TOOL_MANIFEST_PATH,
    job_code_preflight_docker_args, job_code_tool_manifest_sha256, resolve_build_order, sha256_hex,
    validate_job_code_preflight_report,
};
pub use network_policy::{
    FlattenedPolicy, NetworkPolicyConfig, PolicyAllowRule, canonical_policy_hosts, flatten_policy,
};
pub use rules::{
    ConfigAction, ConfigDecision, DefaultAction, RuleAction, RuleSummary, ToolConfig,
    default_to_resolved, load_tool_config, resolve_action, validate_tool_config,
};
pub use runner::{
    CredentialProviderDescriptor, DURABLE_OPENCODE_PATH, HostApprovalsConfig,
    ManagedRunnerFileMount, ManagedRunnerKind, ManagedRunnerSpec, McpBridgeDescriptor,
    OPENCODE_PROJECT_CONFIG_ALIASES, OpencodeRunnerConfig, ResolvedRunnerProfile,
    RunnerProfilesConfig, TrustedMountDescriptor, canonicalize_resolved_runner_profile,
    digest_profile, directory_content_digests, trusted_opencode_runtime_env,
    validate_untrusted_opencode_env, validate_untrusted_opencode_env_name,
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
