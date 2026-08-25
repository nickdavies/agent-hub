use std::collections::BTreeMap;

use agent_hub::attended::{
    AuthorizationSelectors, DIPPY_SELECTOR_PREFIX, authorize, render_proposal,
};
use agent_hub::state::StateStore;
use protocol::{
    ApprovedExecutionSpec, AttendedAuthorizationRequestV1, AttendedLifecycleContract,
    AttendedOutputContract, AttendedProposal, CanonicalRuntimeSpec, ExecutionMode,
    PlannedEnvironmentSpec, PlannedEnvironmentTier, PlannedPermissionSet, PlannedPermissionTier,
    ResolvedAuthorizationBehavior, ToolApprovalMode,
};

#[test]
fn selector_render_is_deterministic_and_authorization_replay_is_rejected() {
    let root = temp_root("state");
    let state = StateStore::open(root.join("state")).unwrap();
    let artifact = request().canonical_artifact().unwrap();
    let path = root.join("authorization.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&artifact).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let first = render_proposal(&path, &artifact).unwrap();
    let second = render_proposal(&path, &artifact).unwrap();
    assert_eq!(first, second);
    assert!(first.contains(DIPPY_SELECTOR_PREFIX));
    assert!(first.contains("--environment development --image-family job-code --network-class none --tool-approval auto --authorization"));
    assert!(!first.contains("gateway"));
    assert!(!first.contains(" --resource "));

    authorize(&state, &path, selectors(&artifact.digest, "development")).unwrap();
    assert!(authorize(&state, &path, selectors(&artifact.digest, "development"),).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn state_rejects_non_private_existing_root() {
    let root = temp_root("permissions").join("state");
    std::fs::create_dir(&root).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(StateStore::open(root.clone()).is_err());
    }
    std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
}

#[test]
fn authorization_is_no_follow_private_and_selector_failures_do_not_create_state() {
    let root = temp_root("authorization-file");
    let artifact = request().canonical_artifact().unwrap();
    let path = root.join("authorization.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&artifact).unwrap()).unwrap();
    let state_root = root.join("state");
    let state = StateStore::open(state_root.clone()).unwrap();
    assert!(!state_root.exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    assert!(
        authorize(
            &state,
            &path,
            selectors(&artifact.digest, "wrong-environment"),
        )
        .is_err()
    );
    assert!(!state_root.exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(authorize(&state, &path, selectors(&artifact.digest, "development"),).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = root.join("authorization-link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(authorize(&state, &link, selectors(&artifact.digest, "development"),).is_err());
    }
    assert!(!state_root.exists());
    std::fs::remove_dir_all(root).unwrap();
}

fn selectors<'a>(digest: &'a str, environment: &'a str) -> AuthorizationSelectors<'a> {
    AuthorizationSelectors {
        digest,
        environment,
        image_family: "job-code",
        network_class: "none",
        network_resources: &[],
        tool_approval: "auto",
    }
}

fn request() -> AttendedAuthorizationRequestV1 {
    AttendedAuthorizationRequestV1 {
        version: "attended_authorization_request_v1".into(),
        proposal: AttendedProposal {
            environment: "development".into(),
            image_family: "job-code".into(),
            network_class: "none".into(),
            network_resources: vec![],
            tool_approval: ToolApprovalMode::Auto,
            prompt: "Fix the test".into(),
        },
        execution: ApprovedExecutionSpec {
            config_root: "/config".into(),
            config_digest: "config".into(),
            environment_bundle_digest: "bundle".into(),
            runtime: CanonicalRuntimeSpec {
                runtime_version: "v2".into(),
                environment: PlannedEnvironmentSpec {
                    tier: PlannedEnvironmentTier::Docker,
                    execution_mode: ExecutionMode::Docker,
                    container_image: Some("agent-hub/job-code:locked".into()),
                    timeout_secs: Some(3600),
                    memory_limit: None,
                    cpu_limit: None,
                    network_resources: Some(protocol::NetworkResourceRequest::None),
                    filesystem: None,
                    file_mounts: vec![],
                    toolchains: vec![],
                },
                permissions: PlannedPermissionSet {
                    tier: PlannedPermissionTier::DockerDefault,
                    can_read_workspace: true,
                    can_write_workspace: true,
                    can_run_shell: true,
                    can_access_network: false,
                    auto_approve_tools: vec![],
                    escalate_tools: vec![],
                    kubernetes_access: Some("none".into()),
                    aws_access: Some("none".into()),
                },
                approval_rules: None,
                repos: vec![],
                github_capabilities: vec![],
            },
            behavior: ResolvedAuthorizationBehavior {
                execution_kind: "agent".into(),
                runner_profile: serde_json::json!({
                    "command": "/usr/local/bin/opencode",
                    "args": ["run"]
                }),
                command: "/usr/local/bin/opencode".into(),
                args: vec!["run".into()],
                env: BTreeMap::new(),
                prompt: Some(protocol::PromptRef {
                    system: None,
                    user: "Fix the test".into(),
                }),
                primary_repo: None,
                primary_repo_branch: None,
                execution_context: serde_json::json!({}),
                triage: false,
                allow_remote_credentials: false,
            },
            workspaces: vec![protocol::WorkspaceSpec {
                alias: "primary".into(),
                host_path: "/work/repo".into(),
                container_path: "/workspace/primary".into(),
                access: protocol::WorkspaceAccess::ReadWrite,
            }],
            context_roots: vec![],
            repositories: vec![protocol::RepositoryMountSpec {
                workspace_alias: "primary".into(),
                repository_root: "/work/repo".into(),
                mount_path: "/workspace/primary".into(),
                remote: None,
                head: String::new(),
            }],
            primary_workspace: "primary".into(),
        },
        output: AttendedOutputContract::default(),
        lifecycle: AttendedLifecycleContract::default(),
    }
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agent-hub-attended-{name}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&root).unwrap();
    root
}
