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
    materialize_environment_bundle, materialize_environment_bundle_with_snapshot,
    materialize_environment_bundle_with_system, validate_durable_opencode_config,
};
pub use image::{ImageConfig, resolve_build_order};
pub use network_policy::{FlattenedPolicy, NetworkPolicyConfig, PolicyAllowRule, flatten_policy};
pub use rules::{
    ConfigAction, ConfigDecision, DefaultAction, RuleAction, RuleSummary, ToolConfig,
    default_to_resolved, load_tool_config, resolve_action, validate_tool_config,
};
pub use runner::{
    CredentialProviderDescriptor, ManagedRunnerFileMount, ManagedRunnerKind, ManagedRunnerSpec,
    McpBridgeDescriptor, OpencodeRunnerConfig, ResolvedRunnerProfile, RunnerProfilesConfig,
    TrustedMountDescriptor, digest_profile, directory_content_digests,
};
pub use system::SystemConfig;
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
