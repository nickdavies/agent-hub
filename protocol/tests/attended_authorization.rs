use std::collections::BTreeMap;

use protocol::{
    ATTENDED_AUTHORIZATION_REQUEST_VERSION_V2, ATTENDED_AUTHORIZATION_VERSION_V2,
    ApprovedExecutionSpec, AttendedAuthorizationArtifact, AttendedAuthorizationRequestV1,
    AttendedAuthorizationRequestV2, AttendedExecutionSpec, AttendedLifecycleContract,
    AttendedOutputContract, AttendedProposal, CanonicalRuntimeSpec, ContextRootSpec, ExecutionMode,
    PlannedEnvironmentSpec, PlannedEnvironmentTier, PlannedPermissionSet, PlannedPermissionTier,
    RepositoryMountSpec, ResolvedAuthorizationBehavior, ResolvedRoot, RootRole, ToolApprovalMode,
    WorkspaceAccess, WorkspaceSpec,
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
            effective_network_grants: vec![
                protocol::AttendedNetworkGrantV1 {
                    resource: protocol::NetworkResourceKind::NpmRegistry,
                    mode: protocol::AttendedNetworkGrantModeV1::Hosts,
                    hosts: vec!["registry.npmjs.org".into()],
                    provider: None,
                },
                protocol::AttendedNetworkGrantV1 {
                    resource: protocol::NetworkResourceKind::PypiRegistry,
                    mode: protocol::AttendedNetworkGrantModeV1::Hosts,
                    hosts: vec!["pypi.org".into()],
                    provider: None,
                },
            ],
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

fn with_github_overlay(
    mut request: AttendedAuthorizationRequestV1,
) -> AttendedAuthorizationRequestV1 {
    let name = "github:github.com:read_write".to_string();
    request.proposal.overlays = vec![name.clone()];
    request.execution.overlays = vec![protocol::AuthorizedOverlay {
        name: name.clone(),
        provider: "github".into(),
        config: serde_json::to_string(&protocol::GithubOverlayBinding {
            host: "github.com".into(),
            api_base_url: "https://api.github.com".into(),
            access: "read_write".into(),
            network_resource: "github".into(),
            credential_provider: name.clone(),
            credential_target: "/agent-hub/github/tokens/github.com".into(),
            authenticated: true,
            private_repositories: true,
            repository_remotes: vec!["https://github.com/example/zeta.git".into()],
        })
        .unwrap(),
    }];
    request.execution.credential_providers = vec![protocol::AuthorizedCredentialProvider {
        name,
        provider_type: "github-token".into(),
        source: "gh".into(),
        command: Some("gh".into()),
        args: vec!["auth".into(), "token".into()],
        validity: None,
        targets: vec!["/agent-hub/github/tokens/github.com".into()],
        access: "read_write".into(),
        constraints: BTreeMap::from([
            ("host".into(), "github.com".into()),
            ("api_base_url".into(), "https://api.github.com".into()),
            (
                "target".into(),
                "/agent-hub/github/tokens/github.com".into(),
            ),
        ]),
    }];
    request.execution.behavior.allow_remote_credentials = true;
    request.execution.repositories[1].remote = Some("https://github.com/example/zeta.git".into());
    request
}

#[test]
fn github_credentials_require_matching_network_and_host() {
    let github = with_github_overlay(request());
    let mut without_network = request_v4();
    without_network.proposal = github.proposal.clone();
    without_network.execution = github.execution.clone();
    assert!(without_network.clone().normalize().is_err());

    let mut host_mismatch = without_network;
    host_mismatch
        .proposal
        .network_resources
        .push(protocol::NetworkResourceKind::Github);
    host_mismatch
        .execution
        .effective_network_grants
        .push(protocol::AttendedNetworkGrantV1 {
            resource: protocol::NetworkResourceKind::Github,
            mode: protocol::AttendedNetworkGrantModeV1::Hosts,
            hosts: vec!["api.github.com".into(), "github.com".into()],
            provider: None,
        });
    host_mismatch
        .execution
        .runtime
        .environment
        .network_resources = Some(protocol::NetworkResourceRequest::Resources(
        host_mismatch.proposal.network_resources.clone(),
    ));
    let mut binding: protocol::GithubOverlayBinding =
        serde_json::from_str(&host_mismatch.execution.overlays[0].config).unwrap();
    binding.repository_remotes = vec!["https://github.snooguts.net/example/zeta.git".into()];
    host_mismatch.execution.overlays[0].config = serde_json::to_string(&binding).unwrap();
    assert!(host_mismatch.normalize().is_err());

    let mut valid = request_v4();
    valid.proposal = github.proposal;
    valid.execution = github.execution;
    valid
        .proposal
        .network_resources
        .push(protocol::NetworkResourceKind::Github);
    valid
        .execution
        .effective_network_grants
        .push(protocol::AttendedNetworkGrantV1 {
            resource: protocol::NetworkResourceKind::Github,
            mode: protocol::AttendedNetworkGrantModeV1::Hosts,
            hosts: vec!["api.github.com".into(), "github.com".into()],
            provider: None,
        });
    valid.execution.runtime.environment.network_resources = Some(
        protocol::NetworkResourceRequest::Resources(valid.proposal.network_resources.clone()),
    );
    for view in &mut valid.git_views {
        let repository = valid
            .execution
            .repositories
            .iter_mut()
            .find(|repository| repository.workspace_alias == view.workspace_alias)
            .unwrap();
        repository.starting.head = Some(view.head.clone());
        repository.starting.reference = view.reference.clone();
        repository.starting.branch = view
            .reference
            .as_deref()
            .and_then(|reference| reference.strip_prefix("refs/heads/"))
            .map(str::to_string);
        repository.starting.detached = view.reference.is_none();
        view.remote = repository.remote.clone();
    }
    let normalized = valid.clone().normalize();
    assert!(normalized.is_ok(), "{normalized:?}");
    valid.proposal.overlays[0] = "github:github.com:read".into();
    valid.execution.overlays[0].name = "github:github.com:read".into();
    let mut binding: protocol::GithubOverlayBinding =
        serde_json::from_str(&valid.execution.overlays[0].config).unwrap();
    binding.access = "read".into();
    binding.credential_provider = "github:github.com:read".into();
    valid.execution.overlays[0].config = serde_json::to_string(&binding).unwrap();
    valid.execution.credential_providers[0].name = "github:github.com:read".into();
    valid.execution.credential_providers[0].access = "read".into();
    assert!(valid.normalize().is_err());
}

#[test]
fn semantic_image_family_accepts_an_opaque_managed_reference() {
    let reference = "registry.example:5443/platform/job-code-qualification@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let mut request = request();
    request.proposal.image_tag = reference.into();
    request.execution.runtime.environment.container_image = Some(reference.into());

    assert_eq!(
        request.normalize().unwrap().proposal.image_family,
        "job-code"
    );
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
        roots: vec![
            ResolvedRoot {
                identity: "work".into(),
                host_path: "/work".into(),
                roles: vec![
                    RootRole::WorkspaceRo,
                    RootRole::WorkspaceRw,
                    RootRole::GitMetadata,
                ],
            },
            ResolvedRoot {
                identity: "code".into(),
                host_path: "/home/nick.davies/code".into(),
                roles: vec![RootRole::ContextRo],
            },
        ],
        workspace_root_identities: BTreeMap::from([
            ("alpha".into(), "work".into()),
            ("zeta".into(), "work".into()),
        ]),
        context_root_identities: BTreeMap::from([("code".into(), "code".into())]),
        git_views,
        approval_bundle_digest: request.approval_bundle_digest,
        approval_bundle: request.approval_bundle,
        output: request.output,
        workspace_export: None,
        lifecycle: request.lifecycle,
        retry: protocol::AttendedRetryConstraints {
            max_attempts: 3,
            max_prelaunch_retries: 1,
            max_provider_unavailable_retries: 1,
        },
    }
}

fn request_v4() -> protocol::AttendedAuthorizationRequestV4 {
    request_v3().into()
}

#[test]
fn agent_hub_run_receipts_bind_decision_digest_invocation_and_expiry() {
    let artifact = request_v4().canonical_artifact().unwrap();
    let now = chrono::Utc::now();
    let invocation = uuid::Uuid::new_v4().to_string();
    let request = protocol::AgentHubRunResolvedV1::new(
        invocation.clone(),
        artifact.clone(),
        now,
        now + chrono::Duration::minutes(5),
    )
    .unwrap();

    for (decision, authority) in [
        (
            protocol::AgentHubRunDecision::Allow,
            protocol::AgentHubRunDecisionAuthority::Automatic,
        ),
        (
            protocol::AgentHubRunDecision::Allow,
            protocol::AgentHubRunDecisionAuthority::Human,
        ),
        (
            protocol::AgentHubRunDecision::Deny,
            protocol::AgentHubRunDecisionAuthority::Automatic,
        ),
        (
            protocol::AgentHubRunDecision::Deny,
            protocol::AgentHubRunDecisionAuthority::Human,
        ),
    ] {
        let decision_record = protocol::AgentHubRunDecisionV1 {
            schema: protocol::AGENT_HUB_RUN_DECISION_SCHEMA_V1.into(),
            invocation_id: invocation.clone(),
            canonical_digest: artifact.digest.clone(),
            request_digest: request.request_digest.clone(),
            decision,
            authority,
            approval_id: if authority == protocol::AgentHubRunDecisionAuthority::Automatic {
                invocation.clone()
            } else {
                uuid::Uuid::new_v4().to_string()
            },
            reason: None,
            decided_at: now,
            expires_at: request.expires_at,
        };
        let receipt =
            protocol::AgentHubRunReceiptV1::issue(&request, decision_record, now).unwrap();
        assert!(receipt.verify(&request, now).is_ok());
        let mut wrong_invocation = request.clone();
        wrong_invocation.invocation_id = uuid::Uuid::new_v4().to_string();
        assert!(receipt.verify(&wrong_invocation, now).is_err());
        let mut wrong_digest = request.clone();
        wrong_digest.canonical_digest = "0".repeat(64);
        assert!(receipt.verify(&wrong_digest, now).is_err());
        assert!(
            receipt
                .verify(&request, now + chrono::Duration::minutes(6))
                .is_err()
        );

        let mut tampered = serde_json::to_value(&receipt).unwrap();
        tampered["canonical_digest"] = serde_json::Value::String("f".repeat(64));
        let tampered: protocol::AgentHubRunReceiptV1 = serde_json::from_value(tampered).unwrap();
        assert!(tampered.verify(&request, now).is_err());
    }
}

#[test]
fn agent_hub_run_public_intent_excludes_host_owned_fields_and_enforces_bounds() {
    let base = serde_json::json!({
        "schema":"agent_hub_run_intent_v1",
        "environment":"code-auto",
        "image_family":"job-code",
        "network_class":"build",
        "network_resources":["llm"],
        "tool_approval":"auto",
        "overlays":[],
        "timeout_secs":60,
        "prompt":"test",
        "workspaces":[{"alias":"source","path":"/workspace/source","access":"read_write"}],
        "context_roots":[],
        "primary_workspace":"source"
    });
    let intent: protocol::AgentHubRunIntentV1 = serde_json::from_value(base.clone()).unwrap();
    intent.validate_shape().unwrap();
    assert!(
        !serde_json::to_value(&intent)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("workspace_export_root")
    );
    let mut with_export = intent.clone();
    with_export.workspace_export_root = Some("/workspace/source/.agent-hub/runs".into());
    with_export.validate_shape().unwrap();
    let mut invalid_export = with_export.clone();
    invalid_export.workspace_export_root = Some("/workspace/source/output".into());
    assert!(invalid_export.validate_shape().is_err());
    let mut read_only_export = with_export;
    read_only_export.workspaces[0].access = protocol::WorkspaceAccess::ReadOnly;
    assert!(read_only_export.validate_shape().is_err());
    for field in [
        "config_root",
        "image_digest",
        "provider_command",
        "gateway_path",
    ] {
        let mut value = base.clone();
        value
            .as_object_mut()
            .unwrap()
            .insert(field.into(), serde_json::json!("untrusted"));
        assert!(serde_json::from_value::<protocol::AgentHubRunIntentV1>(value).is_err());
    }
    let mut zero_timeout = intent.clone();
    zero_timeout.timeout_secs = 0;
    assert!(zero_timeout.validate_shape().is_err());
    let mut duplicate = intent;
    duplicate
        .network_resources
        .push(protocol::NetworkResourceKind::Llm);
    assert!(duplicate.validate_shape().is_err());
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
fn pre_v3_canonical_artifacts_are_frozen_literal_fixtures() {
    const V1: &[u8] = br#"{"execution":{"behavior":{"allow_remote_credentials":false,"args":["run"],"command":"/usr/local/bin/opencode","env":{},"execution_context":{},"execution_kind":"agent","primary_repo":null,"primary_repo_branch":null,"prompt":{"system":null,"user":"Fix the test"},"runner_profile":{"args":["run"],"command":"/usr/local/bin/opencode"},"triage":false},"config_digest":"config-digest","config_provenance":{"system.toml":"digest"},"config_root":"/config","context_roots":[{"alias":"code","container_path":"/context/code","host_path":"/home/nick.davies/code"}],"credential_providers":[],"environment_bundle_digest":"env-digest","overlays":[],"pids_limit":512,"primary_workspace":"zeta","repositories":[{"access":"read_only","mount_path":"/workspace/alpha","remote":null,"repository_root":"/work/alpha","starting":{"branch":null,"detached":false,"dirty_paths":[],"head":null,"reference":null},"workspace_alias":"alpha"},{"access":"read_write","mount_path":"/workspace/zeta","remote":"git@github.com:example/zeta.git","repository_root":"/work/zeta","starting":{"branch":"main","detached":false,"dirty_paths":[],"head":"0123456789abcdef","reference":"refs/heads/main"},"workspace_alias":"zeta"}],"runtime":{"approval_rules":null,"environment":{"container_image":"registry.example/job-code:locked","cpu_limit":null,"execution_mode":"docker","file_mounts":[],"filesystem":null,"memory_limit":null,"network_resources":{"resources":["npm_registry","pypi_registry"]},"tier":"docker","timeout_secs":3600,"toolchains":[]},"github_capabilities":[],"permissions":{"auto_approve_tools":[],"aws_access":null,"can_access_network":false,"can_read_workspace":true,"can_run_shell":true,"can_write_workspace":true,"escalate_tools":[],"kubernetes_access":null,"tier":"docker_default"},"repos":[],"runtime_version":"v2"},"workspaces":[{"access":"read_only","alias":"alpha","container_path":"/workspace/alpha","host_path":"/work/alpha"},{"access":"read_write","alias":"zeta","container_path":"/workspace/zeta","host_path":"/work/zeta"}]},"lifecycle":{"cancellation_requires_owner":true,"cleanup_is_explicit":true,"preserve_workspace_on_failure":true},"output":{"artifacts":"artifacts.json","cleanup":"cleanup.json","limits":{"artifact_bytes":4194304,"stderr_bytes":16777216,"stdout_bytes":16777216,"transcript_bytes":33554432},"repositories":"repositories.json","result":"result.json","transcript":"transcript.jsonl"},"proposal":{"environment":"development","image_family":"job-code","image_id":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","image_tag":"registry.example/job-code:locked","network_class":"build","network_resources":["npm_registry","pypi_registry"],"overlays":[],"prompt":"Fix the test","prompt_digest":"67c477ce630a643bd64fd6f54fba454140b5c8e0e4c572fafd111b123daa4217","timeout_secs":3600,"tool_approval":"auto"},"version":"attended_authorization_request_v1"}"#;
    const V2: &[u8] = br#"{"approval_bundle":null,"approval_bundle_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","execution":{"behavior":{"allow_remote_credentials":false,"args":["run"],"command":"/usr/local/bin/opencode","env":{},"execution_context":{},"execution_kind":"agent","primary_repo":null,"primary_repo_branch":null,"prompt":{"system":null,"user":"Fix the test"},"runner_profile":{"args":["run"],"command":"/usr/local/bin/opencode"},"triage":false},"config_digest":"config-digest","config_provenance":{"system.toml":"digest"},"config_root":"/config","context_roots":[{"alias":"code","container_path":"/context/code","host_path":"/home/nick.davies/code"}],"credential_providers":[],"environment_bundle_digest":"env-digest","overlays":[],"pids_limit":512,"primary_workspace":"zeta","repositories":[{"access":"read_only","mount_path":"/workspace/alpha","remote":null,"repository_root":"/work/alpha","starting":{"branch":null,"detached":false,"dirty_paths":[],"head":null,"reference":null},"workspace_alias":"alpha"},{"access":"read_write","mount_path":"/workspace/zeta","remote":"git@github.com:example/zeta.git","repository_root":"/work/zeta","starting":{"branch":"main","detached":false,"dirty_paths":[],"head":"0123456789abcdef","reference":"refs/heads/main"},"workspace_alias":"zeta"}],"runtime":{"approval_rules":null,"environment":{"container_image":"registry.example/job-code:locked","cpu_limit":null,"execution_mode":"docker","file_mounts":[],"filesystem":null,"memory_limit":null,"network_resources":{"resources":["npm_registry","pypi_registry"]},"tier":"docker","timeout_secs":3600,"toolchains":[]},"github_capabilities":[],"permissions":{"auto_approve_tools":[],"aws_access":null,"can_access_network":false,"can_read_workspace":true,"can_run_shell":true,"can_write_workspace":true,"escalate_tools":[],"kubernetes_access":null,"tier":"docker_default"},"repos":[],"runtime_version":"v2"},"workspaces":[{"access":"read_only","alias":"alpha","container_path":"/workspace/alpha","host_path":"/work/alpha"},{"access":"read_write","alias":"zeta","container_path":"/workspace/zeta","host_path":"/work/zeta"}]},"lifecycle":{"cancellation_requires_owner":true,"cleanup_is_explicit":true,"preserve_workspace_on_failure":true},"output":{"artifacts":"artifacts.json","cleanup":"cleanup.json","limits":{"artifact_bytes":4194304,"stderr_bytes":16777216,"stdout_bytes":16777216,"transcript_bytes":33554432},"repositories":"repositories.json","result":"result.json","transcript":"transcript.jsonl"},"proposal":{"environment":"development","image_family":"job-code","image_id":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","image_tag":"registry.example/job-code:locked","network_class":"build","network_resources":["npm_registry","pypi_registry"],"overlays":[],"prompt":"Fix the test","prompt_digest":"67c477ce630a643bd64fd6f54fba454140b5c8e0e4c572fafd111b123daa4217","timeout_secs":3600,"tool_approval":"auto"},"version":"attended_authorization_request_v2"}"#;

    let v1 = request().canonical_artifact().unwrap();
    assert_eq!(v1.canonical_bytes, V1);
    assert_eq!(
        v1.digest,
        "5fc477638a33d59107ad81c3c5439c256ca649e9f2549a401302fb0ca3c9beb0"
    );
    let v2 = request_v2().canonical_artifact().unwrap();
    assert_eq!(v2.canonical_bytes, V2);
    assert_eq!(
        v2.digest,
        "3a5024ecdc219770bf830bbcfed7c493f383481383216deabea702aef37c24fa"
    );
}

#[test]
fn reviewed_retry_constraints_are_canonical_capability_bytes() {
    let artifact = request_v3().canonical_artifact().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&artifact.canonical_bytes).unwrap();
    assert_eq!(value["retry"]["max_attempts"], 3);
    assert_eq!(value["retry"]["max_prelaunch_retries"], 1);
    assert_eq!(value["retry"]["max_provider_unavailable_retries"], 1);

    let mut changed = request_v3();
    changed.retry.max_attempts = 4;
    assert!(changed.canonical_artifact().is_err());

    let mut supervised = request_v3();
    supervised.proposal.tool_approval = ToolApprovalMode::Dippy;
    supervised.approval_bundle = Some(protocol::AttendedApprovalBundleIdentity {
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
    supervised.retry = protocol::AttendedRetryConstraints::for_mode(ToolApprovalMode::Dippy);
    let value: serde_json::Value =
        serde_json::from_slice(&supervised.canonical_artifact().unwrap().canonical_bytes).unwrap();
    assert_eq!(value["retry"]["max_attempts"], 4);
    assert_eq!(value["retry"]["max_prelaunch_retries"], 2);
}

#[test]
fn attended_v3_binds_only_the_reserved_read_write_workspace_export_root() {
    let mut request = request_v3();
    request.workspace_export = Some(protocol::AttendedWorkspaceExportContract {
        root: "/work/zeta/.agent-hub/runs".into(),
        workspace_alias: "zeta".into(),
        workspace_identity: protocol::HostDirectoryIdentity {
            canonical_path: "/work/zeta".into(),
            device: 1,
            inode: 2,
        },
    });
    let artifact = request.canonical_artifact().unwrap();
    assert_eq!(
        artifact.verify_v3().unwrap().workspace_export,
        request.workspace_export
    );

    for root in [
        "/work/alpha/output",
        "/work/zeta/.agent-hub/runs/extra",
        "/work/alpha/.agent-hub/runs",
        "/work/zeta/.agent-hub/../runs",
    ] {
        let mut invalid = request_v3();
        invalid.workspace_export = Some(protocol::AttendedWorkspaceExportContract {
            root: root.into(),
            workspace_alias: "zeta".into(),
            workspace_identity: protocol::HostDirectoryIdentity {
                canonical_path: "/work/zeta".into(),
                device: 1,
                inode: 2,
            },
        });
        assert!(
            invalid.canonical_artifact().is_err(),
            "accepted export root {root}"
        );
    }
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

    let mut registry_context = request_v3();
    registry_context.execution.context_roots[0].host_path = "/reference/code".into();
    registry_context
        .roots
        .iter_mut()
        .find(|root| root.identity == "code")
        .unwrap()
        .host_path = "/reference".into();
    assert!(registry_context.canonical_artifact().is_ok());

    let mut legacy_context = request_v2();
    legacy_context.execution.context_roots[0].host_path = "/reference/code".into();
    assert!(legacy_context.canonical_artifact().is_err());
}

#[test]
fn attended_v3_digest_snapshot_is_stable() {
    let artifact = request_v3().canonical_artifact().unwrap();
    let frozen_file = include_bytes!("fixtures/attended_authorization_v3.json");
    let mut frozen: serde_json::Value = serde_json::from_slice(frozen_file).unwrap();
    frozen["execution"]
        .as_object_mut()
        .unwrap()
        .remove("effective_network_grants");
    let frozen_bytes = serde_json::to_vec(&frozen).unwrap();
    assert_eq!(artifact.canonical_bytes, frozen_bytes);
    assert_eq!(
        artifact.digest,
        "1c908dc42dd9f5f50530d3ab0fa2bda0b9753e656528b8c9d43fe0a24c3f9c06"
    );
    let frozen = AttendedAuthorizationArtifact {
        version: protocol::ATTENDED_AUTHORIZATION_VERSION_V3,
        digest: artifact.digest,
        canonical_bytes: frozen_bytes,
    };
    assert_eq!(
        frozen.verify_v3().unwrap(),
        request_v3().normalize().unwrap()
    );
}

#[test]
fn private_v4_requires_and_binds_effective_network_grants() {
    let artifact = request_v4().canonical_artifact().unwrap();
    assert_eq!(
        artifact.version,
        protocol::ATTENDED_AUTHORIZATION_VERSION_V4
    );
    assert!(artifact.verify_v3().is_err());
    assert!(artifact.verify_v4().is_ok());

    let mut missing = request_v4();
    missing.execution.effective_network_grants.clear();
    assert!(missing.canonical_artifact().is_err());

    let mut broader = request_v4();
    broader.execution.effective_network_grants[0]
        .hosts
        .push("registry.yarnpkg.com".into());
    let broader = broader.canonical_artifact().unwrap();
    assert_ne!(artifact.digest, broader.digest);
}

#[test]
fn structured_agent_result_validation_is_actionable() {
    let valid = protocol::AgentResultV1 {
        schema: protocol::AGENT_RESULT_V1_SCHEMA.into(),
        complete: true,
        summary: "All requested checks passed".into(),
        acceptance_results: vec![protocol::AgentAcceptanceResult {
            criterion: "tests pass".into(),
            passed: true,
            evidence: "cargo test".into(),
        }],
        blockers: vec![],
        artifact_references: vec![protocol::AgentArtifactReference {
            path: "target/report.json".into(),
            description: "machine-readable report".into(),
        }],
    };
    assert!(valid.validate().is_ok());

    let mut invalid = valid.clone();
    invalid.summary = " ".into();
    assert_eq!(
        invalid.validate().unwrap_err(),
        "agent result summary must not be empty"
    );
    let mut invalid = valid.clone();
    invalid.acceptance_results[0].evidence.clear();
    assert_eq!(
        invalid.validate().unwrap_err(),
        "agent acceptance results require criterion and evidence"
    );
    let mut invalid = valid.clone();
    invalid.blockers.push("blocked".into());
    assert_eq!(
        invalid.validate().unwrap_err(),
        "complete agent result contradicts blockers or failed acceptance results"
    );
    let mut invalid = valid;
    invalid.artifact_references[0].description.clear();
    assert_eq!(
        invalid.validate().unwrap_err(),
        "agent artifact references require path and description"
    );
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
    no_authority.execution.effective_network_grants.clear();
    no_authority.execution.runtime.environment.network_resources =
        Some(protocol::NetworkResourceRequest::None);
    assert!(no_authority.canonical_artifact().is_ok());
}
