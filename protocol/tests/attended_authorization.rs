use std::collections::BTreeMap;

use protocol::{
    ATTENDED_AUTHORIZATION_REQUEST_VERSION_V2, ATTENDED_AUTHORIZATION_VERSION_V2,
    ApprovedExecutionSpec, AttendedAuthorizationArtifact, AttendedAuthorizationRequestV1,
    AttendedAuthorizationRequestV2, AttendedExecutionSpec, AttendedLifecycleContract,
    AttendedOutputContract, AttendedProposal, CanonicalRuntimeSpec, ContextRootSpec, ExecutionMode,
    PlannedEnvironmentSpec, PlannedEnvironmentTier, PlannedPermissionSet, PlannedPermissionTier,
    RepositoryMountSpec, ResolvedAuthorizationBehavior, ToolApprovalMode, WorkspaceAccess,
    WorkspaceSpec,
};

fn request() -> AttendedAuthorizationRequestV1 {
    AttendedAuthorizationRequestV1 {
        version: "attended_authorization_request_v1".into(),
        proposal: AttendedProposal {
            environment: "development".into(),
            image_family: "job-code".into(),
            image_tag: "registry.example/job-code:locked".into(),
            image_id: format!("sha256:{}", "a".repeat(64)),
            network_class: "build".into(),
            network_resources: vec![
                protocol::NetworkResourceKind::PypiRegistry,
                protocol::NetworkResourceKind::NpmRegistry,
            ],
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
            config_digest: "config-digest".into(),
            config_provenance: BTreeMap::from([("system.toml".into(), "digest".into())]),
            environment_bundle_digest: "env-digest".into(),
            approved: ApprovedExecutionSpec {
                runtime: CanonicalRuntimeSpec {
                    runtime_version: "v2".into(),
                    environment: PlannedEnvironmentSpec {
                        tier: PlannedEnvironmentTier::Docker,
                        execution_mode: ExecutionMode::Docker,
                        container_image: Some("registry.example/job-code:locked".into()),
                        timeout_secs: Some(3600),
                        memory_limit: None,
                        cpu_limit: None,
                        network_resources: Some(protocol::NetworkResourceRequest::Resources(vec![
                            protocol::NetworkResourceKind::NpmRegistry,
                            protocol::NetworkResourceKind::PypiRegistry,
                        ])),
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
                        kubernetes_access: None,
                        aws_access: None,
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
            workspaces: vec![
                WorkspaceSpec {
                    alias: "zeta".into(),
                    host_path: "/work/zeta".into(),
                    container_path: "/workspace/zeta".into(),
                    access: WorkspaceAccess::ReadWrite,
                },
                WorkspaceSpec {
                    alias: "alpha".into(),
                    host_path: "/work/alpha".into(),
                    container_path: "/workspace/alpha".into(),
                    access: WorkspaceAccess::ReadOnly,
                },
            ],
            context_roots: vec![ContextRootSpec {
                alias: "code".into(),
                host_path: "/home/nick.davies/code".into(),
                container_path: "/context/code".into(),
            }],
            repositories: vec![
                RepositoryMountSpec {
                    workspace_alias: "zeta".into(),
                    repository_root: "/work/zeta".into(),
                    mount_path: "/workspace/zeta".into(),
                    remote: Some("git@github.com:example/zeta.git".into()),
                    access: WorkspaceAccess::ReadWrite,
                    starting: protocol::RepositoryState {
                        head: Some("0123456789abcdef".into()),
                        reference: Some("refs/heads/main".into()),
                        branch: Some("main".into()),
                        detached: false,
                        dirty_paths: vec![],
                    },
                },
                RepositoryMountSpec {
                    workspace_alias: "alpha".into(),
                    repository_root: "/work/alpha".into(),
                    mount_path: "/workspace/alpha".into(),
                    remote: None,
                    access: WorkspaceAccess::ReadOnly,
                    starting: protocol::RepositoryState {
                        head: None,
                        reference: None,
                        branch: None,
                        detached: false,
                        dirty_paths: vec![],
                    },
                },
            ],
            primary_workspace: "zeta".into(),
        },
        output: AttendedOutputContract::default(),
        lifecycle: AttendedLifecycleContract::default(),
    }
}

fn request_v2() -> AttendedAuthorizationRequestV2 {
    let request = request();
    AttendedAuthorizationRequestV2 {
        version: ATTENDED_AUTHORIZATION_REQUEST_VERSION_V2.into(),
        proposal: request.proposal,
        execution: request.execution,
        approval_bundle_digest: "a".repeat(64),
        approval_bundle: None,
        output: request.output,
        lifecycle: request.lifecycle,
    }
}

fn request_v3() -> protocol::AttendedAuthorizationRequestV3 {
    let mut request = request_v2();
    let git_views = request
        .execution
        .workspaces
        .iter()
        .map(|workspace| protocol::GitViewSpec {
            workspace_alias: workspace.alias.clone(),
            repository_type: protocol::GitRepositoryType::Standalone,
            workspace_path: workspace.host_path.clone(),
            workspace_container_path: workspace.container_path.clone(),
            git_dir: protocol::HostDirectoryIdentity {
                canonical_path: format!("{}/.git", workspace.host_path),
                device: 1,
                inode: 2,
            },
            common_dir: protocol::HostDirectoryIdentity {
                canonical_path: format!("{}/.git", workspace.host_path),
                device: 1,
                inode: 2,
            },
            object_stores: vec![protocol::GitObjectStoreSpec {
                identity: protocol::HostDirectoryIdentity {
                    canonical_path: format!("{}/.git/objects", workspace.host_path),
                    device: 1,
                    inode: 3,
                },
                container_path: format!("/agent-hub/git/views/{}/objects/0", workspace.alias),
            }],
            head: "1".repeat(40),
            reference: Some("refs/heads/main".into()),
            index_sha256: Some("2".repeat(64)),
            remote: None,
            access: workspace.access,
            pointer_container_path: format!("{}/.git", workspace.container_path),
            git_dir_container_path: format!("/agent-hub/git/views/{}/git", workspace.alias),
        })
        .collect::<Vec<_>>();
    for repository in &mut request.execution.repositories {
        let view = git_views
            .iter()
            .find(|view| view.workspace_alias == repository.workspace_alias)
            .unwrap();
        repository.remote = view.remote.clone();
        repository.starting.head = Some(view.head.clone());
        repository.starting.reference = view.reference.clone();
        repository.starting.branch = view
            .reference
            .as_deref()
            .and_then(|reference| reference.strip_prefix("refs/heads/"))
            .map(str::to_string);
        repository.starting.detached = view.reference.is_none();
    }
    protocol::AttendedAuthorizationRequestV3 {
        version: protocol::ATTENDED_AUTHORIZATION_REQUEST_VERSION_V3.into(),
        proposal: request.proposal,
        execution: request.execution,
        git_views,
        approval_bundle_digest: request.approval_bundle_digest,
        approval_bundle: request.approval_bundle,
        output: request.output,
        lifecycle: request.lifecycle,
    }
}

#[test]
fn attended_authorization_requires_a_positive_bounded_deadline() {
    for timeout in [None, Some(0), Some(86_401)] {
        let mut invalid = request();
        invalid.execution.runtime.environment.timeout_secs = timeout;
        assert!(invalid.canonical_artifact().is_err());
    }
}

#[test]
fn canonicalization_normalizes_set_order_and_has_attended_domain() {
    let first = request().canonical_artifact().unwrap();
    let mut reordered = request();
    reordered.execution.workspaces.reverse();
    reordered.proposal.network_resources.reverse();
    let second = reordered.canonical_artifact().unwrap();
    assert_eq!(first, second);
    assert_eq!(first.digest.len(), 64);
    assert!(first.verify().is_ok());
}

#[test]
fn reviewed_retry_constraints_are_canonical_capability_bytes() {
    let artifact = request_v2().canonical_artifact().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&artifact.canonical_bytes).unwrap();
    assert_eq!(value["lifecycle"]["retry"]["max_attempts"], 3);
    assert_eq!(value["lifecycle"]["retry"]["max_prelaunch_retries"], 2);
    assert_eq!(
        value["lifecycle"]["retry"]["max_provider_unavailable_retries"],
        1
    );

    let mut changed = request_v2();
    changed.lifecycle.retry.max_attempts = 4;
    assert!(changed.canonical_artifact().is_err());
}

#[test]
fn strict_dto_and_cross_domain_artifacts_are_rejected() {
    let artifact = request().canonical_artifact().unwrap();
    let mut json: serde_json::Value = serde_json::from_slice(&artifact.canonical_bytes).unwrap();
    json.as_object_mut()
        .unwrap()
        .insert("unexpected".into(), true.into());
    assert!(serde_json::from_value::<AttendedAuthorizationRequestV1>(json).is_err());

    let crossed = AttendedAuthorizationArtifact {
        version: artifact.version,
        digest: protocol::sha256_hex_with_domain(
            b"agent-hub:resolved-authorization:v3\0",
            &artifact.canonical_bytes,
        ),
        canonical_bytes: artifact.canonical_bytes,
    };
    assert!(crossed.verify().is_err());
}

#[test]
fn attended_v1_binds_config_and_environment_bundle_digests() {
    let artifact = request().canonical_artifact().unwrap();
    for field in ["config_digest", "environment_bundle_digest"] {
        let mut bytes: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        bytes["execution"][field] = serde_json::json!("tampered");
        let tampered = AttendedAuthorizationArtifact {
            version: artifact.version,
            digest: artifact.digest.clone(),
            canonical_bytes: serde_json::to_vec(&bytes).unwrap(),
        };
        assert!(tampered.verify().is_err(), "accepted tampered {field}");
    }
    let mut bytes: serde_json::Value = serde_json::from_slice(&artifact.canonical_bytes).unwrap();
    bytes["execution"]["config_provenance"]["system.toml"] = serde_json::json!("tampered");
    let tampered = AttendedAuthorizationArtifact {
        version: artifact.version,
        digest: artifact.digest,
        canonical_bytes: serde_json::to_vec(&bytes).unwrap(),
    };
    assert!(tampered.verify().is_err());
}

#[test]
fn attended_v2_binds_approval_bundle_without_redefining_v1() {
    let v1 = request().canonical_artifact().unwrap();
    assert_eq!(v1.version, 1);
    assert!(v1.verify().is_ok());
    assert!(v1.verify_v2().is_err());

    let v2 = request_v2().canonical_artifact().unwrap();
    assert_eq!(v2.version, ATTENDED_AUTHORIZATION_VERSION_V2);
    assert!(v2.verify().is_err());
    assert!(v2.verify_v2().is_ok());
    let mut bytes: serde_json::Value = serde_json::from_slice(&v2.canonical_bytes).unwrap();
    bytes["approval_bundle_digest"] = serde_json::json!("b".repeat(64));
    let changed = AttendedAuthorizationArtifact {
        canonical_bytes: serde_json::to_vec(&bytes).unwrap(),
        ..v2
    };
    assert!(changed.verify_v2().is_err());
}

#[test]
fn attended_v2_constructs_a_canonical_supervised_artifact_and_rejects_tampering() {
    let mut request = request_v2();
    request.proposal.tool_approval = ToolApprovalMode::Dippy;
    request.approval_bundle = Some(protocol::AttendedApprovalBundleIdentity {
        name: "code-supervised".into(),
        digest: "a".repeat(64),
        protocol: protocol::APPROVAL_BUNDLE_PROTOCOL.into(),
        manifest_sha256: "2".repeat(64),
        gateway_commit: "b".repeat(40),
        gateway_archive_sha256: "c".repeat(64),
        plugin_sha256: "d".repeat(64),
        dippy_commit: "e".repeat(40),
        dippy_archive_sha256: "f".repeat(64),
        dippy_wheel_sha256: "1".repeat(64),
    });
    let artifact = request.canonical_artifact().unwrap();
    assert_eq!(
        artifact.verify_v2().unwrap(),
        request.clone().normalize().unwrap()
    );
    request.approval_bundle.as_mut().unwrap().plugin_sha256 = "tampered".into();
    assert!(request.canonical_artifact().is_err());
    request.approval_bundle = None;
    assert!(request.canonical_artifact().is_err());
}

#[test]
fn attended_v3_binds_isolated_git_views_without_accepting_v1_or_v2_domains() {
    let v1 = request().canonical_artifact().unwrap();
    let v2 = request_v2().canonical_artifact().unwrap();
    let v3 = request_v3().canonical_artifact().unwrap();
    assert_eq!(v3.version, protocol::ATTENDED_AUTHORIZATION_VERSION_V3);
    assert!(v3.verify().is_err());
    assert!(v3.verify_v2().is_err());
    assert_eq!(v3.verify_v3().unwrap(), request_v3().normalize().unwrap());
    assert_eq!(v1, request().canonical_artifact().unwrap());
    assert_eq!(v2, request_v2().canonical_artifact().unwrap());

    let mut changed = request_v3();
    changed.git_views[0].index_sha256 = Some("3".repeat(64));
    assert_ne!(changed.canonical_artifact().unwrap().digest, v3.digest);
}

#[test]
fn attended_v1_binds_prompt_bytes_tag_and_local_image_identity() {
    let mut changed_prompt = request();
    changed_prompt.proposal.prompt.push('!');
    changed_prompt
        .execution
        .behavior
        .prompt
        .as_mut()
        .unwrap()
        .user
        .push('!');
    assert!(changed_prompt.canonical_artifact().is_err());

    let mut moved_tag = request();
    moved_tag.proposal.image_tag = "registry.example/job-code:moved".into();
    assert!(moved_tag.canonical_artifact().is_err());

    let mut invalid_id = request();
    invalid_id.proposal.image_id = "registry.example/job-code@sha256:abc".into();
    assert!(invalid_id.canonical_artifact().is_err());
}

#[test]
fn attended_network_resources_must_fit_the_explicit_class() {
    let mut invalid = request();
    invalid.proposal.network_class = "llm-only".into();
    assert!(invalid.canonical_artifact().is_err());

    let mut no_authority = request();
    no_authority.proposal.network_resources.clear();
    no_authority.execution.runtime.environment.network_resources =
        Some(protocol::NetworkResourceRequest::None);
    assert!(no_authority.canonical_artifact().is_ok());
}
