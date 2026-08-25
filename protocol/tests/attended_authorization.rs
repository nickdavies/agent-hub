use std::collections::BTreeMap;

use protocol::{
    ApprovedExecutionSpec, AttendedAuthorizationArtifact, AttendedAuthorizationRequestV1,
    AttendedLifecycleContract, AttendedOutputContract, AttendedProposal, CanonicalRuntimeSpec,
    ContextRootSpec, ExecutionMode, PlannedEnvironmentSpec, PlannedEnvironmentTier,
    PlannedPermissionSet, PlannedPermissionTier, RepositoryMountSpec,
    ResolvedAuthorizationBehavior, ToolApprovalMode, WorkspaceAccess, WorkspaceSpec,
};

fn request() -> AttendedAuthorizationRequestV1 {
    AttendedAuthorizationRequestV1 {
        version: "attended_authorization_request_v1".into(),
        proposal: AttendedProposal {
            environment: "development".into(),
            image_family: "job-code".into(),
            network_class: "build".into(),
            network_resources: vec![
                protocol::NetworkResourceKind::PypiRegistry,
                protocol::NetworkResourceKind::NpmRegistry,
            ],
            tool_approval: ToolApprovalMode::Auto,
            prompt: "Fix the test".into(),
        },
        execution: ApprovedExecutionSpec {
            config_root: "/config".into(),
            config_digest: "config-digest".into(),
            environment_bundle_digest: "env-digest".into(),
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
                    head: "0123456789abcdef".into(),
                },
                RepositoryMountSpec {
                    workspace_alias: "alpha".into(),
                    repository_root: "/work/alpha".into(),
                    mount_path: "/workspace/alpha".into(),
                    remote: None,
                    head: String::new(),
                },
            ],
            primary_workspace: "zeta".into(),
        },
        output: AttendedOutputContract::default(),
        lifecycle: AttendedLifecycleContract::default(),
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
}
