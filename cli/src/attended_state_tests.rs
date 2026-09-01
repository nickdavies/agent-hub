use std::collections::BTreeMap;

use crate::attended::{DIPPY_SELECTOR_PREFIX, read_artifact_bounded, render_proposal};
use crate::state::StateStore;
use protocol::{
    ATTENDED_AUTHORIZATION_REQUEST_VERSION_V3, ApprovedExecutionSpec,
    AttendedAuthorizationRequestV3, AttendedExecutionSpec, AttendedLifecycleContract,
    AttendedOutputContract, AttendedProposal, CanonicalRuntimeSpec, ExecutionMode,
    PlannedEnvironmentSpec, PlannedEnvironmentTier, PlannedPermissionSet, PlannedPermissionTier,
    ResolvedAuthorizationBehavior, ToolApprovalMode,
};

#[test]
fn selector_render_is_deterministic() {
    let root = temp_root("state");
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
    assert!(first.contains("--environment development --image-family job-code --network-class none --tool-approval auto --timeout-secs 3600 --authorization"));
    assert!(!first.contains("gateway"));
    assert!(!first.contains(" --resource "));

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
fn fresh_state_creates_runs_parent_and_bounds_artifact_reads() {
    let root = temp_root("bounded-read");
    let state = StateStore::open(root.join("state")).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    state
        .ensure_directory(std::path::Path::new("runs"))
        .unwrap();
    state
        .ensure_directory(&std::path::PathBuf::from("runs").join(&id))
        .unwrap();
    state
        .create_json(
            &std::path::PathBuf::from("runs")
                .join(&id)
                .join("result.json"),
            &serde_json::json!({"result":"abcdefghijklmnopqrstuvwxyz"}),
        )
        .unwrap();
    let bounded = read_artifact_bounded(&state, &id, "result.json", 8).unwrap();
    assert_eq!(bounded.returned_bytes, 8);
    assert!(bounded.truncated);
    assert!(bounded.total_bytes > bounded.returned_bytes);
    std::fs::remove_dir_all(root).unwrap();
}

fn request() -> AttendedAuthorizationRequestV3 {
    AttendedAuthorizationRequestV3 {
        version: ATTENDED_AUTHORIZATION_REQUEST_VERSION_V3.into(),
        proposal: AttendedProposal {
            environment: "development".into(),
            image_family: "job-code".into(),
            image_tag: "agent-hub/job-code:locked".into(),
            image_id: format!("sha256:{}", "a".repeat(64)),
            network_class: "none".into(),
            network_resources: vec![],
            tool_approval: ToolApprovalMode::Auto,
            overlays: vec![],
            timeout_secs: 3600,
            prompt_digest: protocol::sha256_hex_with_domain(
                b"agent-hub:attended-prompt:v1\0",
                b"Fix the test",
            ),
            prompt: "Fix the test".into(),
        },
        execution: AttendedExecutionSpec {
            config_root: "/config".into(),
            config_digest: "config".into(),
            config_provenance: BTreeMap::from([("system.toml".into(), "digest".into())]),
            environment_bundle_digest: "bundle".into(),
            approved: ApprovedExecutionSpec {
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
                pids_limit: protocol::APPROVED_EXECUTION_PIDS_LIMIT,
            },
            credential_providers: vec![],
            overlays: vec![],
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
                access: protocol::WorkspaceAccess::ReadWrite,
                starting: protocol::RepositoryState {
                    head: Some("1".repeat(40)),
                    reference: Some("refs/heads/main".into()),
                    branch: Some("main".into()),
                    detached: false,
                    dirty_paths: vec![],
                },
            }],
            primary_workspace: "primary".into(),
        },
        roots: vec![protocol::ResolvedRoot {
            identity: "work".into(),
            host_path: "/work".into(),
            roles: vec![
                protocol::RootRole::WorkspaceRo,
                protocol::RootRole::WorkspaceRw,
                protocol::RootRole::GitMetadata,
            ],
        }],
        workspace_root_identities: BTreeMap::from([("primary".into(), "work".into())]),
        context_root_identities: BTreeMap::new(),
        git_views: vec![protocol::GitViewSpec {
            workspace_alias: "primary".into(),
            repository_type: protocol::GitRepositoryType::Standalone,
            workspace_path: "/work/repo".into(),
            workspace_container_path: "/workspace/primary".into(),
            git_dir: protocol::HostDirectoryIdentity {
                canonical_path: "/work/repo/.git".into(),
                device: 1,
                inode: 2,
            },
            common_dir: protocol::HostDirectoryIdentity {
                canonical_path: "/work/repo/.git".into(),
                device: 1,
                inode: 2,
            },
            object_stores: vec![protocol::GitObjectStoreSpec {
                identity: protocol::HostDirectoryIdentity {
                    canonical_path: "/work/repo/.git/objects".into(),
                    device: 1,
                    inode: 3,
                },
                container_path: "/agent-hub/git/views/primary/objects/0".into(),
            }],
            head: "1".repeat(40),
            reference: Some("refs/heads/main".into()),
            index_sha256: Some("2".repeat(64)),
            remote: None,
            access: protocol::WorkspaceAccess::ReadWrite,
            pointer_container_path: "/workspace/primary/.git".into(),
            git_dir_container_path: "/agent-hub/git/views/primary/git".into(),
        }],
        approval_bundle_digest: "a".repeat(64),
        approval_bundle: None,
        output: AttendedOutputContract::default(),
        lifecycle: AttendedLifecycleContract::default(),
        retry: protocol::AttendedRetryConstraints {
            max_attempts: 3,
            max_prelaunch_retries: 2,
            max_provider_unavailable_retries: 1,
        },
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
