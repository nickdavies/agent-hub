use std::collections::{BTreeMap, BTreeSet};
use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, de};

/// Deserialize an `Option<u64>` that also accepts floating-point numbers (e.g. `900.0`).
/// This is needed because protobuf `google.protobuf.Value` only has `double` for numbers,
/// so integers become floats after a JSON→protobuf→JSON roundtrip.
fn deserialize_optional_u64_lenient<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    let v: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => {
            if let Some(i) = n.as_u64() {
                Ok(Some(i))
            } else if let Some(f) = n.as_f64() {
                if f >= 0.0 && f <= u64::MAX as f64 && f.fract() == 0.0 {
                    Ok(Some(f as u64))
                } else {
                    Err(de::Error::custom(format!("cannot convert {f} to u64")))
                }
            } else {
                Err(de::Error::custom("invalid number"))
            }
        }
        Some(other) => Err(de::Error::custom(format!(
            "expected number or null, got {other}"
        ))),
    }
}

fn deserialize_u32_lenient<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_u64()
                && let Ok(value) = u32::try_from(value)
            {
                return Ok(value);
            }
            if let Some(value) = number.as_f64()
                && value >= 0.0
                && value <= f64::from(u32::MAX)
                && value.fract() == 0.0
            {
                return Ok(value as u32);
            }
            Err(de::Error::custom("expected an unsigned 32-bit integer"))
        }
        other => Err(de::Error::custom(format!("expected a number, got {other}"))),
    }
}

#[cfg(test)]
mod immutable_authorization_tests {
    use super::*;

    fn request() -> ResolvedAuthorizationRequest {
        let environment = canonical_environment_for_execution(
            ExecutionMode::Docker,
            Some("image@sha256:abc".to_string()),
            Some(60),
        );
        let permissions = canonical_permissions_for_environment(&environment);
        ResolvedAuthorizationRequest {
            version: AUTHORIZATION_REQUEST_VERSION.to_string(),
            invocation_id: Uuid::nil(),
            work_unit_id: Uuid::nil(),
            root_work_item_id: Uuid::nil(),
            workflow_id: "workflow".to_string(),
            workflow_snapshot: serde_json::json!({"b":2,"a":1}),
            state_id: "build".to_string(),
            state_kind: ReactiveStateKind::Command,
            agent_output_events: Vec::new(),
            behavior: ResolvedAuthorizationBehavior {
                execution_kind: "command".to_string(),
                runner_profile: serde_json::json!({"config_digest":"command_v1","profile_name":"command"}),
                command: "cargo".to_string(),
                args: vec!["test".to_string()],
                env: BTreeMap::from([("RUST_BACKTRACE".to_string(), "1".to_string())]),
                prompt: None,
                primary_repo: None,
                primary_repo_branch: None,
                execution_context: serde_json::json!({
                    "state_invocation": {"state_id":"build", "id":Uuid::nil()}
                }),
                triage: false,
                allow_remote_credentials: true,
            },
            permission_envelope: CanonicalRuntimeSpec {
                runtime_version: "v2".to_string(),
                environment,
                permissions,
                approval_rules: Some(ApprovalRuleSet {
                    auto_approve: vec!["git".to_string(), "file_read".to_string()],
                    escalate: vec!["shell_unknown".to_string()],
                    deny: Vec::new(),
                }),
                repos: vec![PlannedRepoClaim {
                    repo_url: "https://example/repo.git".to_string(),
                    base_ref: "origin/main".to_string(),
                    read_only: false,
                    target_branch: Some("task/change".to_string()),
                    claim_type: "workspace".to_string(),
                    branch_name: Some("task/change".to_string()),
                }],
                github_capabilities: Vec::new(),
            },
            resolved_environment: serde_json::json!({"network":"none"}),
            provider: ResolvedAuthorizationProvider {
                provider: Some("test".to_string()),
                task_id: Some("task".to_string()),
            },
            resource_constraints: ResolvedAuthorizationResources {
                timeout_secs: Some(60),
                max_retries: 3,
                attempt: 0,
            },
        }
    }

    #[test]
    fn unattended_v4_exposes_the_shared_approved_execution_projection() {
        let request = request().normalize();
        let projection = request.approved_execution_spec();
        assert_eq!(projection.runtime, request.permission_envelope);
        assert_eq!(projection.behavior, request.behavior);
        assert!(projection.behavior_matches(&projection));
        assert!(projection.permissions_are_subset_of(&projection));
        let artifact = request.canonical_artifact_v4().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        assert_eq!(
            json["approved_execution"]["pids_limit"],
            crate::APPROVED_EXECUTION_PIDS_LIMIT
        );
    }

    #[test]
    fn digest_normalizes_set_like_permission_lists() {
        let first = request();
        let mut second = request();
        second
            .permission_envelope
            .approval_rules
            .as_mut()
            .unwrap()
            .auto_approve
            .reverse();
        assert_eq!(
            first.request_digest().unwrap(),
            second.request_digest().unwrap()
        );
    }

    #[test]
    fn read_only_repo_is_narrower_only_for_same_identity_and_base_ref() {
        let baseline = request();
        let mut narrower = request();
        narrower.permission_envelope.repos[0].read_only = true;
        narrower.permission_envelope.permissions.can_write_workspace = false;
        assert!(narrower.permissions_are_subset_of(&baseline));
        narrower.permission_envelope.repos[0].base_ref = "origin/other".to_string();
        assert!(!narrower.permissions_are_subset_of(&baseline));
    }

    #[test]
    fn behavior_changes_never_inherit_baseline() {
        let baseline = request();
        let mut changed = request();
        changed.behavior.args.push("--release".to_string());
        assert!(!changed.behavior_matches(&baseline));
    }

    #[test]
    fn every_permission_field_fails_closed_when_widened_or_unknown() {
        let baseline = request();

        let mut changed = baseline.clone();
        changed
            .permission_envelope
            .permissions
            .auto_approve_tools
            .push("shell".to_string());
        assert!(!changed.permissions_are_subset_of(&baseline));

        let mut changed = baseline.clone();
        changed
            .permission_envelope
            .permissions
            .escalate_tools
            .push("network".to_string());
        assert!(!changed.permissions_are_subset_of(&baseline));

        let mut changed = baseline.clone();
        changed.permission_envelope.permissions.aws_access = Some("unknown".to_string());
        assert!(!changed.permissions_are_subset_of(&baseline));

        let mut changed = baseline.clone();
        changed.permission_envelope.permissions.kubernetes_access = Some("read_write".to_string());
        assert!(!changed.permissions_are_subset_of(&baseline));

        let mut changed = baseline.clone();
        changed.permission_envelope.repos[0].claim_type = "unknown".to_string();
        assert!(!changed.permissions_are_subset_of(&baseline));
    }

    #[test]
    fn github_api_base_is_digest_bound_and_requires_exact_subset_identity() {
        let mut baseline = request();
        baseline.permission_envelope.github_capabilities = vec![GithubCapability {
            host: "github.com".to_string(),
            api_base_url: "https://api.github.com".to_string(),
            repository: "reddit/agent-hub".to_string(),
            operations: vec![GithubOperation::PushBranch],
            branch: Some("agent-hub/task".to_string()),
            head_branch: Some("agent-hub/task".to_string()),
            base_branch: Some("main".to_string()),
            pull_request: None,
            expected_head_sha: None,
            credential_provider: "github:github.com:read_write".to_string(),
        }];
        let mut changed = baseline.clone();
        changed.permission_envelope.github_capabilities[0].api_base_url =
            "https://github.com/api/v3".to_string();
        assert_ne!(
            baseline.request_digest().unwrap(),
            changed.request_digest().unwrap()
        );
        assert!(!changed.permissions_are_subset_of(&baseline));
    }

    #[test]
    fn identical_payload_in_new_invocation_has_distinct_authorization_identity() {
        let first = request();
        let mut second = first.clone();
        second.invocation_id = Uuid::new_v4();
        second.behavior.execution_context["state_invocation"]["id"] =
            serde_json::Value::String(second.invocation_id.to_string());
        assert_ne!(
            first.request_digest().unwrap(),
            second.request_digest().unwrap()
        );
        assert!(!second.behavior_matches(&first));
    }

    #[test]
    fn floating_transport_integer_is_not_normalized_for_authorization() {
        let artifact = request().canonical_artifact().unwrap();
        let mut transported: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        transported["resource_constraints"]["max_retries"] = serde_json::json!(3.0);
        let bytes = canonical_json_bytes(transported).unwrap();
        let transported = CanonicalAuthorizationArtifact {
            version: 1,
            digest: canonical_authorization_digest(&bytes),
            canonical_bytes: bytes,
        };
        assert!(transported.verify_and_parse().is_err());
    }

    #[test]
    fn canonical_authorization_v1_is_strict_and_preserves_json_numbers() {
        let mut request = request();
        request.resource_constraints.max_retries = 0;
        request.resource_constraints.timeout_secs = Some(120);
        request.workflow_snapshot = serde_json::from_str(
            r#"{"z":{"fraction":1.25,"negative":-7,"large":9007199254740993},"runner":{"uid":1000,"gid":1000,"ports":[8080,3000],"attempts":0},"ordered":["second","first"],"empty":[],"explicit_null":null,"timestamp":"2026-08-20T19:10:51Z","a":{}}"#,
        )
        .unwrap();
        request.behavior.runner_profile =
            serde_json::json!({"profile_name":"command", "config_digest":"command_v1"});

        let artifact = request.canonical_artifact().unwrap();
        let text = std::str::from_utf8(&artifact.canonical_bytes).unwrap();
        assert_eq!(
            text,
            r#"{"behavior":{"allow_remote_credentials":true,"args":["test"],"command":"cargo","env":{"RUST_BACKTRACE":"1"},"execution_context":{"state_invocation":{"id":"00000000-0000-0000-0000-000000000000","state_id":"build"}},"execution_kind":"command","primary_repo":null,"primary_repo_branch":null,"prompt":null,"runner_profile":{"config_digest":"command_v1","profile_name":"command"},"triage":false},"invocation_id":"00000000-0000-0000-0000-000000000000","permission_envelope":{"approval_rules":{"auto_approve":["file_read","git"],"deny":[],"escalate":["shell_unknown"]},"environment":{"container_image":"image@sha256:abc","cpu_limit":null,"execution_mode":"docker","file_mounts":[],"filesystem":null,"memory_limit":null,"network_resources":null,"tier":"docker","timeout_secs":60,"toolchains":[]},"github_capabilities":[],"permissions":{"auto_approve_tools":[],"aws_access":"none","can_access_network":false,"can_read_workspace":true,"can_run_shell":true,"can_write_workspace":true,"escalate_tools":[],"kubernetes_access":"none","tier":"docker_default"},"repos":[{"base_ref":"origin/main","branch_name":"task/change","claim_type":"workspace","read_only":false,"repo_url":"https://example/repo.git","target_branch":"task/change"}],"runtime_version":"v2"},"provider":{"provider":"test","task_id":"task"},"resolved_environment":{"network":"none"},"resource_constraints":{"attempt":0,"max_retries":0,"timeout_secs":120},"root_work_item_id":"00000000-0000-0000-0000-000000000000","state_id":"build","state_kind":"command","version":"resolved_authorization_request_v1","work_unit_id":"00000000-0000-0000-0000-000000000000","workflow_id":"workflow","workflow_snapshot":{"a":{},"empty":[],"explicit_null":null,"ordered":["second","first"],"runner":{"attempts":0,"gid":1000,"ports":[8080,3000],"uid":1000},"timestamp":"2026-08-20T19:10:51Z","z":{"fraction":1.25,"large":9007199254740993,"negative":-7}}}"#
        );
        assert_eq!(
            artifact.digest,
            "5084d6159dd3859a67b3f997fdc33f20aa2a67baf374486be552d58fab056b0a"
        );
        assert!(text.contains("9007199254740993"));
        assert!(text.contains("\"negative\":-7"));
        assert!(text.contains("\"fraction\":1.25"));
        assert_eq!(artifact.version, 1);
        assert_eq!(artifact.digest.len(), 64);
        assert_eq!(artifact.verify_and_parse().unwrap(), request.normalize());

        let mut unknown: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        unknown["unknown"] = serde_json::json!(true);
        let bytes = canonical_json_bytes(unknown).unwrap();
        let unknown = CanonicalAuthorizationArtifact {
            version: 1,
            digest: canonical_authorization_digest(&bytes),
            canonical_bytes: bytes,
        };
        assert!(unknown.verify_and_parse().is_err());

        let mut invalid: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        invalid["resolver"]["upstream"] = serde_json::json!("https://resolver.invalid");
        let invalid_bytes = canonical_json_bytes(invalid).unwrap();
        let invalid = CanonicalAuthorizationArtifact {
            version: CANONICAL_AUTHORIZATION_VERSION_V4,
            digest: canonical_authorization_digest_for_version(
                CANONICAL_AUTHORIZATION_VERSION_V4,
                &invalid_bytes,
            )
            .unwrap(),
            canonical_bytes: invalid_bytes,
        };
        assert!(invalid.verify_and_parse().is_err());

        let mut unknown_nested: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        unknown_nested["behavior"]["runner_profile"]["future_field"] = serde_json::json!(true);
        let bytes = canonical_json_bytes(unknown_nested).unwrap();
        let unknown_nested = CanonicalAuthorizationArtifact {
            version: 1,
            digest: canonical_authorization_digest(&bytes),
            canonical_bytes: bytes,
        };
        assert!(unknown_nested.verify_and_parse().is_err());

        let mut unknown_variant: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        unknown_variant["state_kind"] = serde_json::json!("future_kind");
        let bytes = canonical_json_bytes(unknown_variant).unwrap();
        let unknown_variant = CanonicalAuthorizationArtifact {
            version: 1,
            digest: canonical_authorization_digest(&bytes),
            canonical_bytes: bytes,
        };
        assert!(unknown_variant.verify_and_parse().is_err());

        let mut absent: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        absent["behavior"].as_object_mut().unwrap().remove("prompt");
        let bytes = canonical_json_bytes(absent).unwrap();
        let absent = CanonicalAuthorizationArtifact {
            version: 1,
            digest: canonical_authorization_digest(&bytes),
            canonical_bytes: bytes,
        };
        assert!(absent.verify_and_parse().is_err());
    }

    #[test]
    fn canonical_authorization_v2_binds_trusted_opencode_environment() {
        let mut request = request();
        request.behavior.runner_profile = serde_json::json!({
            "profile_name":"agent",
            "kind":"opencode",
            "command":"opencode",
            "args":["run"],
            "container_image":"image@sha256:abc",
            "file_mounts":[],
            "credential_providers":[],
            "trusted_mounts":[],
            "mcp_bridges":[],
            "egress":{
                "llm":{"default_provider":null,"providers":{}},
                "build":{"github_hosts":[],"go_registry_hosts":[],"crates_registry_hosts":[]},
                "proxy":{"image":"proxy@sha256:abc","listen_port":3128,"runtime_uid":13,"runtime_gid":13,"writable_tmpfs_mode":"0755","declared_volumes":["/var/log/squid","/var/spool/squid"]},
                "preflight_image":null
            },
            "github_hosts":[],
            "opencode":{
                "max_validation_attempts":3,
                "trusted_env":{"OPENCODE_PURE":"1"}
            },
            "config_digest":"fixture"
        });
        let artifact = request.canonical_artifact().unwrap();
        assert_eq!(artifact.version, CANONICAL_AUTHORIZATION_VERSION_V2);
        assert_eq!(
            artifact.digest,
            "d20e4db6eeee997f8615acf4218462b086cd57aaa30cbbd18672b3d35206a229"
        );
        assert_eq!(artifact.verify_and_parse().unwrap(), request.normalize());

        let v1 = CanonicalAuthorizationArtifact {
            version: 1,
            digest: canonical_authorization_digest(&artifact.canonical_bytes),
            canonical_bytes: artifact.canonical_bytes,
        };
        assert!(v1.verify_and_parse().is_err());
    }

    #[test]
    fn canonical_authorization_capability_matrix_is_version_exact() {
        let v1 = [WorkerProtocolCapability::CanonicalAuthorizationV1];
        let v2 = [WorkerProtocolCapability::CanonicalAuthorizationV2];
        let v3 = [WorkerProtocolCapability::CanonicalAuthorizationV3];
        let v4 = [WorkerProtocolCapability::CanonicalAuthorizationV4];
        let both = [
            WorkerProtocolCapability::CanonicalAuthorizationV1,
            WorkerProtocolCapability::CanonicalAuthorizationV2,
        ];
        assert!(worker_supports_canonical_authorization(&v1, 1));
        assert!(!worker_supports_canonical_authorization(&v1, 2));
        assert!(!worker_supports_canonical_authorization(&v2, 1));
        assert!(worker_supports_canonical_authorization(&v2, 2));
        assert!(worker_supports_canonical_authorization(&both, 1));
        assert!(worker_supports_canonical_authorization(&both, 2));
        assert!(!worker_supports_canonical_authorization(&v2, 3));
        assert!(worker_supports_canonical_authorization(&v3, 3));
        assert!(!worker_supports_canonical_authorization(&v3, 4));
        assert!(worker_supports_canonical_authorization(&v4, 4));
        assert!(!worker_supports_canonical_authorization(&v4, 3));
        assert!(!worker_supports_canonical_authorization(&both, 0));
        assert!(!worker_supports_canonical_authorization(&both, 3));
    }

    #[test]
    fn canonical_authorization_v3_requires_and_normalizes_agent_output_events() {
        let mut first = request();
        first.agent_output_events =
            vec![AgentOutputEvent::JobFailed, AgentOutputEvent::JobSucceeded];
        let mut reordered = first.clone();
        reordered.agent_output_events.reverse();

        let artifact = first.canonical_artifact().unwrap();
        assert_eq!(artifact.version, 3);
        assert_eq!(artifact, reordered.canonical_artifact().unwrap());
        assert_eq!(artifact.verify_and_parse().unwrap(), first.normalize());

        let mut missing: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .remove("agent_output_events");
        let bytes = canonical_json_bytes(missing).unwrap();
        let missing = CanonicalAuthorizationArtifact {
            version: 3,
            digest: canonical_authorization_digest_for_version(3, &bytes).unwrap(),
            canonical_bytes: bytes,
        };
        assert!(missing.verify_and_parse().is_err());

        let empty = request().canonical_artifact_v3().unwrap();
        assert_eq!(empty.version, CANONICAL_AUTHORIZATION_VERSION_V3);
        assert!(
            empty
                .verify_and_parse()
                .unwrap()
                .agent_output_events
                .is_empty()
        );
        let explicit = AgentOutput {
            next_event: Some("job_succeeded".to_string()),
            ..AgentOutput::default()
        };
        assert!(validate_agent_output_event(&explicit, &[]).is_err());
    }

    #[test]
    fn canonical_authorization_v3_rejects_human_and_external_output_events() {
        let mut request = request();
        request.agent_output_events = vec![AgentOutputEvent::JobSucceeded];
        let artifact = request.canonical_artifact().unwrap();
        let mut hostile: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        hostile["agent_output_events"] = serde_json::json!(["human_approved"]);
        let bytes = canonical_json_bytes(hostile).unwrap();
        let hostile = CanonicalAuthorizationArtifact {
            version: 3,
            digest: canonical_authorization_digest_for_version(3, &bytes).unwrap(),
            canonical_bytes: bytes,
        };
        assert!(hostile.verify_and_parse().is_err());

        let mut cancellation: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        cancellation["agent_output_events"] = serde_json::json!(["cancel"]);
        let bytes = canonical_json_bytes(cancellation).unwrap();
        let cancellation = CanonicalAuthorizationArtifact {
            version: 3,
            digest: canonical_authorization_digest_for_version(3, &bytes).unwrap(),
            canonical_bytes: bytes,
        };
        assert!(cancellation.verify_and_parse().is_err());

        let mut stale_projection = request;
        stale_projection.behavior.execution_context["workflow"] = serde_json::json!({
            "inputs": {"valid_next_events": ["human_approved", "cancel"]}
        });
        assert!(stale_projection.canonical_artifact().is_err());
    }

    #[test]
    fn canonical_authorization_v4_binds_resolver_and_rejects_tampering() {
        let request = resolver_authorization_request();
        let artifact = request.canonical_artifact_v4().unwrap();
        assert_eq!(artifact.version, CANONICAL_AUTHORIZATION_VERSION_V4);
        assert_eq!(artifact.verify_and_parse().unwrap(), request.normalize());

        let mut tampered: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        tampered["resolver"]["upstream"] = serde_json::json!("8.8.8.8@53");
        let tampered_bytes = canonical_json_bytes(tampered).unwrap();
        let stale_digest = CanonicalAuthorizationArtifact {
            version: CANONICAL_AUTHORIZATION_VERSION_V4,
            digest: artifact.digest.clone(),
            canonical_bytes: tampered_bytes.clone(),
        };
        assert!(stale_digest.verify_and_parse().is_err());

        let mut unknown: serde_json::Value = serde_json::from_slice(&tampered_bytes).unwrap();
        unknown["resolver"]["unknown"] = serde_json::json!(true);
        let unknown_bytes = canonical_json_bytes(unknown).unwrap();
        let unknown = CanonicalAuthorizationArtifact {
            version: CANONICAL_AUTHORIZATION_VERSION_V4,
            digest: canonical_authorization_digest_for_version(
                CANONICAL_AUTHORIZATION_VERSION_V4,
                &unknown_bytes,
            )
            .unwrap(),
            canonical_bytes: unknown_bytes,
        };
        assert!(unknown.verify_and_parse().is_err());

        let mut missing = resolver_authorization_request();
        missing.behavior.runner_profile["egress"]
            .as_object_mut()
            .unwrap()
            .remove("resolver");
        assert!(missing.canonical_artifact_v4().is_err());
    }

    #[test]
    fn canonical_authorization_cross_version_and_domain_replay_is_rejected() {
        let v4 = resolver_authorization_request()
            .canonical_artifact_v4()
            .unwrap();
        for version in [1, CANONICAL_AUTHORIZATION_VERSION_V2, 3] {
            let replay = CanonicalAuthorizationArtifact {
                version,
                digest: canonical_authorization_digest_for_version(version, &v4.canonical_bytes)
                    .unwrap(),
                canonical_bytes: v4.canonical_bytes.clone(),
            };
            assert!(replay.verify_and_parse().is_err());
        }

        assert!(
            resolver_authorization_request()
                .canonical_artifact_v3()
                .is_err()
        );
    }

    fn resolver_authorization_request() -> ResolvedAuthorizationRequest {
        let mut request = request();
        request.agent_output_events = vec![AgentOutputEvent::JobSucceeded];
        request.permission_envelope.environment.network_resources =
            Some(NetworkResourceRequest::Resources(vec![
                NetworkResourceKind::Llm,
            ]));
        request.behavior.runner_profile = serde_json::json!({
            "profile_name":"agent",
            "kind":"opencode",
            "command":"opencode",
            "args":["run"],
            "container_image":"image@sha256:abc",
            "file_mounts":[],
            "credential_providers":[],
            "trusted_mounts":[],
            "mcp_bridges":[],
            "egress":{
                "llm":{"default_provider":null,"providers":{}},
                "build":{"github_hosts":[],"go_registry_hosts":[],"crates_registry_hosts":[]},
                "proxy":{"image":"proxy@sha256:abc","listen_port":3128,"runtime_uid":13,"runtime_gid":13,"writable_tmpfs_mode":"0755","declared_volumes":["/var/log/squid","/var/spool/squid"]},
                "resolver":{"upstream":"127.0.0.11@53","readiness_host":"example.com"},
                "preflight_image":"image@sha256:abc"
            },
            "github_hosts":[],
            "opencode":{"max_validation_attempts":3,"trusted_env":{"OPENCODE_PURE":"1"}},
            "config_digest":"fixture"
        });
        request
    }

    #[test]
    fn agent_output_validation_rejects_human_and_cancel_actions() {
        let allowed = vec!["job_succeeded".to_string(), "job_failed".to_string()];
        for next_event in ["human_approved", "cancel"] {
            let output = AgentOutput {
                next_event: Some(next_event.to_string()),
                ..AgentOutput::default()
            };
            assert!(validate_agent_output_event(&output, &allowed).is_err());
        }
        let output = AgentOutput {
            next_event: Some("job_succeeded".to_string()),
            ..AgentOutput::default()
        };
        assert!(validate_agent_output_event(&output, &allowed).is_ok());
    }

    #[test]
    fn canonical_authorization_v1_ordering_contract() {
        let first = request();
        let mut map_reordered = request();
        map_reordered.workflow_snapshot = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        assert_eq!(
            first.canonical_artifact().unwrap(),
            map_reordered.canonical_artifact().unwrap()
        );

        let mut ordered = request();
        ordered.behavior.args.push("--workspace".to_string());
        let mut ordered_reordered = ordered.clone();
        ordered_reordered.behavior.args.reverse();
        assert_ne!(
            ordered.request_digest().unwrap(),
            ordered_reordered.request_digest().unwrap()
        );

        let mut set_reordered = request();
        set_reordered
            .permission_envelope
            .approval_rules
            .as_mut()
            .unwrap()
            .auto_approve
            .reverse();
        assert_eq!(
            first.request_digest().unwrap(),
            set_reordered.request_digest().unwrap()
        );
    }

    #[test]
    fn canonical_authorization_v1_tampering_fails_before_parsing() {
        let artifact = request().canonical_artifact().unwrap();
        let bundle = ImmutableAuthorizationBundle::create(&request()).unwrap();
        assert!(
            ImmutableAuthorizationBundle::verify(&bundle.artifact(), &bundle.authorization_id)
                .is_ok()
        );
        assert!(ImmutableAuthorizationBundle::verify(&bundle.artifact(), "tampered-id").is_err());
        let mut payload_tamper = artifact.clone();
        payload_tamper.canonical_bytes[0] ^= 1;
        assert!(payload_tamper.verify_and_parse().is_err());

        let mut digest_tamper = artifact.clone();
        digest_tamper.digest.push('0');
        assert!(digest_tamper.verify_and_parse().is_err());

        let mut version_tamper = artifact;
        version_tamper.version += 1;
        assert!(version_tamper.verify_and_parse().is_err());
    }

    #[test]
    fn canonical_authorization_v1_normalizes_every_set_with_total_keys() {
        let mut first = request();
        first.permission_envelope.environment.file_mounts = vec![
            PlannedFileMount {
                host_path: "/b".to_string(),
                container_path: "/same".to_string(),
                read_only: false,
            },
            PlannedFileMount {
                host_path: "/a".to_string(),
                container_path: "/same".to_string(),
                read_only: true,
            },
        ];
        first.permission_envelope.environment.toolchains =
            vec!["rust".to_string(), "go".to_string()];
        first.permission_envelope.environment.network_resources =
            Some(NetworkResourceRequest::Resources(vec![
                NetworkResourceKind::Github,
                NetworkResourceKind::Llm,
            ]));
        first.permission_envelope.permissions.auto_approve_tools =
            vec!["write".to_string(), "read".to_string()];
        first.permission_envelope.permissions.escalate_tools =
            vec!["network".to_string(), "shell".to_string()];
        first.permission_envelope.repos = vec![
            PlannedRepoClaim {
                repo_url: "https://example/same.git".to_string(),
                base_ref: "origin/main".to_string(),
                read_only: false,
                target_branch: None,
                claim_type: "workspace".to_string(),
                branch_name: None,
            },
            PlannedRepoClaim {
                repo_url: "https://example/same.git".to_string(),
                base_ref: "origin/main".to_string(),
                read_only: false,
                target_branch: None,
                claim_type: "existing_branch".to_string(),
                branch_name: None,
            },
        ];
        first.permission_envelope.github_capabilities = vec![
            GithubCapability {
                host: "github.com".to_string(),
                api_base_url: "https://api.github.com".to_string(),
                repository: "example/same".to_string(),
                operations: vec![GithubOperation::PushBranch, GithubOperation::ReadRepository],
                branch: None,
                head_branch: None,
                base_branch: None,
                pull_request: None,
                expected_head_sha: None,
                credential_provider: "github:github.com:read_write".to_string(),
            },
            GithubCapability {
                host: "github.com".to_string(),
                api_base_url: "https://api.github.com".to_string(),
                repository: "example/same".to_string(),
                operations: vec![GithubOperation::ReadPullRequest],
                branch: None,
                head_branch: None,
                base_branch: None,
                pull_request: None,
                expected_head_sha: None,
                credential_provider: "github:github.com:read_write".to_string(),
            },
        ];
        first.behavior.runner_profile = serde_json::json!({
            "profile_name":"agent",
            "kind":"generic",
            "command":"agent",
            "args":["run"],
            "container_image":"image@sha256:abc",
            "file_mounts":[
                {"host_path":"/b","container_path":"/b","read_only":true},
                {"host_path":"/a","container_path":"/a","read_only":false}
            ],
            "credential_providers":[
                {"name":"b","provider_type":"oauth","targets":["/b","/a"]},
                {"name":"a","provider_type":"api","targets":["/z","/y"]}
            ],
            "trusted_mounts":[
                {"purpose":"state","container_path":"/state","read_only":false},
                {"purpose":"config","container_path":"/config","read_only":true}
            ],
            "mcp_bridges":[
                {"name":"b","endpoint_type":"unix","target":"b","container_socket_path":"/b.sock","args":["second","first"],"env_keys":["Z","A"]},
                {"name":"a","endpoint_type":"unix","target":"a","container_socket_path":"/a.sock","args":[],"env_keys":["Y","B"]}
            ],
            "egress":{
                "llm":{"default_provider":null,"providers":{"openai":["api.openai.com","chatgpt.com"]}},
                "build":{"github_hosts":["github.com"],"go_registry_hosts":["proxy.golang.org"],"crates_registry_hosts":["crates.io"],"pypi_registry_hosts":["pypi.org"],"npm_registry_hosts":["registry.npmjs.org"]},
                "proxy":{"image":"proxy@sha256:abc","listen_port":3128,"runtime_uid":13,"runtime_gid":13,"writable_tmpfs_mode":"0755","declared_volumes":["/var/spool/squid","/var/log/squid"]},
                "preflight_image":null
            },
            "github_hosts":["git.example.com","github.com"],
            "opencode":{"max_validation_attempts":0},
            "config_digest":"fixture"
        });

        let mut reversed = first.clone();
        reversed
            .permission_envelope
            .environment
            .file_mounts
            .reverse();
        reversed
            .permission_envelope
            .environment
            .toolchains
            .reverse();
        if let Some(NetworkResourceRequest::Resources(resources)) = reversed
            .permission_envelope
            .environment
            .network_resources
            .as_mut()
        {
            resources.reverse();
        }
        reversed
            .permission_envelope
            .permissions
            .auto_approve_tools
            .reverse();
        reversed
            .permission_envelope
            .permissions
            .escalate_tools
            .reverse();
        reversed.permission_envelope.repos.reverse();
        reversed.permission_envelope.github_capabilities.reverse();
        for capability in &mut reversed.permission_envelope.github_capabilities {
            capability.operations.reverse();
        }
        for key in [
            "file_mounts",
            "credential_providers",
            "trusted_mounts",
            "mcp_bridges",
            "github_hosts",
        ] {
            reversed.behavior.runner_profile[key]
                .as_array_mut()
                .unwrap()
                .reverse();
        }
        for provider in reversed.behavior.runner_profile["credential_providers"]
            .as_array_mut()
            .unwrap()
        {
            provider["targets"].as_array_mut().unwrap().reverse();
        }
        for bridge in reversed.behavior.runner_profile["mcp_bridges"]
            .as_array_mut()
            .unwrap()
        {
            bridge["env_keys"].as_array_mut().unwrap().reverse();
        }
        reversed.behavior.runner_profile["egress"]["llm"]["providers"]["openai"]
            .as_array_mut()
            .unwrap()
            .reverse();
        reversed.behavior.runner_profile["egress"]["proxy"]["declared_volumes"]
            .as_array_mut()
            .unwrap()
            .reverse();

        assert_eq!(
            first.canonical_artifact().unwrap(),
            reversed.canonical_artifact().unwrap()
        );

        let artifact = first.canonical_artifact().unwrap();
        let mut unknown_egress: serde_json::Value =
            serde_json::from_slice(&artifact.canonical_bytes).unwrap();
        unknown_egress["behavior"]["runner_profile"]["egress"]["future_field"] =
            serde_json::json!(true);
        let bytes = canonical_json_bytes(unknown_egress).unwrap();
        let unknown_egress = CanonicalAuthorizationArtifact {
            version: artifact.version,
            digest: canonical_authorization_digest_for_version(artifact.version, &bytes).unwrap(),
            canonical_bytes: bytes,
        };
        assert!(unknown_egress.verify_and_parse().is_err());
    }

    #[test]
    fn canonical_authorization_v1_json_number_policy_is_frozen() {
        let mut request = request();
        request.workflow_snapshot = serde_json::from_str(
            r#"{"i64_min":-9223372036854775808,"u64_max":18446744073709551615,"above_js_safe":9007199254740993,"integer_float":1.0,"negative_zero":-0.0,"smallest_positive":5e-324,"largest_finite":1.7976931348623157e308}"#,
        )
        .unwrap();
        let artifact = request.canonical_artifact().unwrap();
        let text = std::str::from_utf8(&artifact.canonical_bytes).unwrap();
        for expected in [
            "-9223372036854775808",
            "18446744073709551615",
            "9007199254740993",
            "\"integer_float\":1.0",
            "\"negative_zero\":-0.0",
            "5e-324",
            "1.7976931348623157e+308",
        ] {
            assert!(
                text.contains(expected),
                "missing canonical number {expected}"
            );
        }

        let noncanonical_bytes = text
            .replace("\"integer_float\":1.0", "\"integer_float\":1e0")
            .into_bytes();
        let noncanonical = CanonicalAuthorizationArtifact {
            version: artifact.version,
            digest: canonical_authorization_digest_for_version(
                artifact.version,
                &noncanonical_bytes,
            )
            .unwrap(),
            canonical_bytes: noncanonical_bytes,
        };
        assert!(noncanonical.verify_and_parse().is_err());
    }
}
use sha2::{Digest, Sha256};
use strum::{Display, EnumString};
use uuid::Uuid;

use crate::sessions::SessionId;

#[derive(
    Debug,
    Clone,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
    PartialEq,
    Eq,
    Hash,
    Display,
    EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    Docker,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WorkItemState {
    Pending,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderIdentity {
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderOwnership {
    #[default]
    Unassigned,
    Bot,
    Human {
        owners: Vec<ProviderIdentity>,
    },
    BotAndHuman {
        owners: Vec<ProviderIdentity>,
    },
}

impl ProviderOwnership {
    pub fn is_bot_only(&self) -> bool {
        matches!(self, Self::Bot)
    }

    pub fn human_owners(&self) -> &[ProviderIdentity] {
        match self {
            Self::Human { owners } | Self::BotAndHuman { owners } => owners,
            Self::Unassigned | Self::Bot => &[],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum BlockReason {
    HumanReview,
    Dependency,
    Clarification,
    ExternalSystem,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum JobState {
    Pending,
    Assigned,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WorkerState {
    Idle,
    Busy,
    Offline,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WorkflowEventKind {
    Start,
    Submit,
    TaskCreated,
    TaskUpdated,
    CommentAdded,
    LabelChanged,
    JobSucceeded,
    JobFailed,
    AllChildrenCompleted,
    ChildFailed,
    HumanApproved,
    HumanChangesRequested,
    HumanUnblocked,
    RetryRequested,
    NeedsTriage,
    WorkflowChosen,
    PrCreated,
    PrApproved,
    PrChangesRequested,
    OwnerPrApproved,
    OwnerChangesRequested,
    CoworkerPrApproved,
    CoworkerChangesRequested,
    PrMerged,
    ChecksPassed,
    ChecksFailed,
    /// Emitted by a CI-failure triage step (today: a stub command, soon:
    /// an agent step that reads CI logs) when it determines the failure
    /// is trivial (lint/format/obvious unit-test breakage in code we
    /// authored) and the workflow should auto-loop back to `implement`.
    CiFailureTrivial,
    /// Emitted by the same triage step when the failure cause is unclear
    /// or requires human judgment (flaky CI, infrastructure issues,
    /// cross-PR dependencies). The workflow escalates to a human gate
    /// rather than thrashing the implement loop.
    CiFailureEscalate,
    Timeout,
    Cancel,
}

/// Workflow transitions that an executable agent is permitted to emit.
/// Human/provider actions and external orchestration events are deliberately
/// not representable by this type.
#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    JsonSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Display,
    EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AgentOutputEvent {
    JobSucceeded,
    JobFailed,
    CiFailureTrivial,
    CiFailureEscalate,
}

impl TryFrom<WorkflowEventKind> for AgentOutputEvent {
    type Error = String;

    fn try_from(event: WorkflowEventKind) -> Result<Self, Self::Error> {
        match event {
            WorkflowEventKind::JobSucceeded => Ok(Self::JobSucceeded),
            WorkflowEventKind::JobFailed => Ok(Self::JobFailed),
            WorkflowEventKind::CiFailureTrivial => Ok(Self::CiFailureTrivial),
            WorkflowEventKind::CiFailureEscalate => Ok(Self::CiFailureEscalate),
            other => Err(format!(
                "workflow event '{}' is not an agent output event",
                other
            )),
        }
    }
}

impl From<AgentOutputEvent> for WorkflowEventKind {
    fn from(event: AgentOutputEvent) -> Self {
        match event {
            AgentOutputEvent::JobSucceeded => Self::JobSucceeded,
            AgentOutputEvent::JobFailed => Self::JobFailed,
            AgentOutputEvent::CiFailureTrivial => Self::CiFailureTrivial,
            AgentOutputEvent::CiFailureEscalate => Self::CiFailureEscalate,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum WorkflowEventPayload {
    None,
    Comment {
        body: String,
    },
    AgentResult {
        exit_code: i32,
        summary: Option<String>,
    },
    PullRequest {
        number: u64,
        url: String,
        action: String,
    },
    Labels {
        added: Vec<String>,
        removed: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventSource {
    Webhook,
    ScmPoller,
    Agent,
    System,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowEvent {
    pub work_item_id: String,
    pub event_type: WorkflowEventKind,
    pub payload: WorkflowEventPayload,
    pub source: EventSource,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

pub fn workflow_event_is_externally_fireable(event: &WorkflowEventKind) -> bool {
    matches!(
        event,
        WorkflowEventKind::Submit
            | WorkflowEventKind::TaskCreated
            | WorkflowEventKind::TaskUpdated
            | WorkflowEventKind::CommentAdded
            | WorkflowEventKind::LabelChanged
            | WorkflowEventKind::HumanApproved
            | WorkflowEventKind::HumanChangesRequested
            | WorkflowEventKind::HumanUnblocked
            | WorkflowEventKind::RetryRequested
            | WorkflowEventKind::PrCreated
            | WorkflowEventKind::PrApproved
            | WorkflowEventKind::PrChangesRequested
            | WorkflowEventKind::OwnerPrApproved
            | WorkflowEventKind::OwnerChangesRequested
            | WorkflowEventKind::CoworkerPrApproved
            | WorkflowEventKind::CoworkerChangesRequested
            | WorkflowEventKind::ChecksPassed
            | WorkflowEventKind::ChecksFailed
            | WorkflowEventKind::Cancel
    )
}

pub fn workflow_event_is_internal_system_only(event: &WorkflowEventKind) -> bool {
    !workflow_event_is_externally_fireable(event)
}

// TODO(reactive-events): consider a dedicated newtype/enum wrapper if reactive events diverge
// from the shared workflow event taxonomy. For now they intentionally collapse to the same set.
pub type ReactiveEventKind = WorkflowEventKind;

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct ReactiveTransitionEvent(pub ReactiveEventKind);

impl ReactiveTransitionEvent {
    pub fn kind(&self) -> ReactiveEventKind {
        self.0.clone()
    }

    pub fn is_kind(&self, expected: ReactiveEventKind) -> bool {
        self.0 == expected
    }
}

impl From<ReactiveEventKind> for ReactiveTransitionEvent {
    fn from(value: ReactiveEventKind) -> Self {
        Self(value)
    }
}

impl<'de> Deserialize<'de> for ReactiveTransitionEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        let kind = reactive_event_kind_from_str(&raw).ok_or_else(|| {
            de::Error::custom(format!(
                "unknown reactive transition event '{raw}' (use canonical workflow event names like 'start', 'job_succeeded', 'human_approved', etc.)"
            ))
        })?;
        Ok(Self(kind))
    }
}

pub fn reactive_event_kind_from_str(event: &str) -> Option<ReactiveEventKind> {
    event.parse().ok()
}

pub fn reactive_event_kind_for_workflow_event(event: WorkflowEventKind) -> ReactiveEventKind {
    event
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    ExitCode,
    Timeout,
    Cancelled,
    WorkerError,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionTiming {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StructuredTranscriptArtifact {
    pub key: String,
    pub path: String,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub record_count: Option<u64>,
    #[serde(default)]
    pub inline_json: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactSummary {
    pub stdout_path: String,
    pub stderr_path: String,
    #[serde(default)]
    pub output_path: Option<String>,
    #[serde(default)]
    pub transcript_dir: Option<String>,
    #[serde(default)]
    pub stdout_bytes: Option<u64>,
    #[serde(default)]
    pub stderr_bytes: Option<u64>,
    #[serde(default)]
    pub output_bytes: Option<u64>,
    #[serde(default)]
    pub structured_transcript_artifacts: Vec<StructuredTranscriptArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepoExecutionMetadata {
    #[serde(default)]
    pub base_ref: Option<String>,
    #[serde(default)]
    pub target_branch: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub head_sha: Option<String>,
    pub is_dirty: bool,
    pub changed_files: u32,
    pub untracked_files: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GithubPullRequestMetadata {
    pub number: u64,
    pub url: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub is_draft: Option<bool>,
    #[serde(default)]
    pub head_ref_name: Option<String>,
    #[serde(default)]
    pub base_ref_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PromptRef {
    pub system: Option<String>,
    pub user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RepoSource {
    pub repo_url: String,
    /// Git ref to check out in worktree (e.g. origin/main).
    pub base_ref: String,
    /// Desired branch name for job/work item lifecycle.
    #[serde(default)]
    pub branch_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RepoAccessMode {
    ReadOnly,
    #[default]
    ReadWrite,
}

#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    JsonSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Display,
    EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum GithubOperation {
    ReadRepository,
    PushBranch,
    ReadPullRequest,
    CreatePullRequest,
    MarkPullRequestReady,
    RequestReview,
    MergePullRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum GithubAction {
    PushHead,
    ReadOrCreatePullRequest,
    MarkPullRequestReady,
    RequestReview,
    MergePullRequest,
}

impl GithubAction {
    pub fn required_operations(&self) -> &'static [GithubOperation] {
        match self {
            Self::PushHead => &[GithubOperation::PushBranch],
            Self::ReadOrCreatePullRequest => &[
                GithubOperation::ReadPullRequest,
                GithubOperation::CreatePullRequest,
            ],
            Self::MarkPullRequestReady => &[
                GithubOperation::ReadPullRequest,
                GithubOperation::MarkPullRequestReady,
            ],
            Self::RequestReview => &[
                GithubOperation::ReadPullRequest,
                GithubOperation::RequestReview,
            ],
            Self::MergePullRequest => &[
                GithubOperation::ReadPullRequest,
                GithubOperation::MergePullRequest,
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GithubCapabilityRequest {
    pub operations: Vec<GithubOperation>,
    pub access: GithubAccessClass,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GithubAccessClass {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GithubCapability {
    pub host: String,
    pub api_base_url: String,
    pub repository: String,
    pub operations: Vec<GithubOperation>,
    pub branch: Option<String>,
    pub head_branch: Option<String>,
    pub base_branch: Option<String>,
    pub pull_request: Option<u64>,
    pub expected_head_sha: Option<String>,
    pub credential_provider: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRepositoryIdentity {
    pub host: String,
    pub owner: String,
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubPullRequestIdentity {
    pub repository: GithubRepositoryIdentity,
    pub number: u64,
}

pub fn parse_github_repository_url(value: &str) -> Option<GithubRepositoryIdentity> {
    let url = parse_strict_https_url(value)?;
    let segments = strict_path_segments(&url)?;
    if segments.len() != 2 {
        return None;
    }
    let owner = validated_github_path_segment(segments[0])?;
    let repository =
        validated_github_path_segment(segments[1].strip_suffix(".git").unwrap_or(segments[1]))?;
    Some(GithubRepositoryIdentity {
        host: url.host_str()?.to_string(),
        owner: owner.to_string(),
        repository: repository.to_string(),
    })
}

pub fn canonical_github_host(value: &str) -> Option<String> {
    if value.is_empty() || value != value.trim() || value.contains(['/', '@', ':', '?', '#']) {
        return None;
    }
    let url = url::Url::parse(&format!("https://{value}/")).ok()?;
    if url.port().is_some() {
        return None;
    }
    Some(url.host_str()?.to_ascii_lowercase())
}

pub fn canonical_github_api_base_url(value: &str) -> Option<String> {
    if value.is_empty() || value != value.trim() || value.contains('%') {
        return None;
    }
    let authority = value.strip_prefix("https://")?.split('/').next()?;
    if authority.contains(':') {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_some()
        || url.host_str().is_none()
    {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    if path.split('/').any(|segment| matches!(segment, "." | "..")) {
        return None;
    }
    let mut canonical = format!("https://{}", url.host_str()?);
    canonical.push_str(path);
    Some(canonical)
}

pub fn canonical_github_api_base_for_host(host: &str, value: &str) -> Option<String> {
    let host = canonical_github_host(host)?;
    let api_base = canonical_github_api_base_url(value)?;
    let api_url = url::Url::parse(&api_base).ok()?;
    let api_host = api_url.host_str()?;
    if host == "github.com" {
        if api_host != "api.github.com" || api_url.path() != "/" {
            return None;
        }
    } else if api_host != host {
        return None;
    }
    Some(api_base)
}

pub fn parse_github_pull_request_url(value: &str) -> Option<GithubPullRequestIdentity> {
    let url = parse_strict_https_url(value)?;
    let segments = strict_path_segments(&url)?;
    if segments.len() != 4 || segments[2] != "pull" {
        return None;
    }
    let owner = validated_github_path_segment(segments[0])?;
    let repository =
        validated_github_path_segment(segments[1].strip_suffix(".git").unwrap_or(segments[1]))?;
    let number = segments[3].parse::<u64>().ok()?;
    if number == 0 {
        return None;
    }
    Some(GithubPullRequestIdentity {
        repository: GithubRepositoryIdentity {
            host: url.host_str()?.to_string(),
            owner: owner.to_string(),
            repository: repository.to_string(),
        },
        number,
    })
}

fn parse_strict_https_url(value: &str) -> Option<url::Url> {
    if value.is_empty() || value != value.trim() || value.contains('%') {
        return None;
    }
    let authority = value.strip_prefix("https://")?.split('/').next()?;
    if authority.contains(':') {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_some()
    {
        return None;
    }
    url.host_str()?;
    Some(url)
}

fn strict_path_segments(url: &url::Url) -> Option<Vec<&str>> {
    let segments = url.path_segments()?.collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    Some(segments)
}

fn validated_github_path_segment(value: &str) -> Option<&str> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some(value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentDefinition {
    pub name: String,
    pub image: String,
    pub network_class: String,
    pub approval_mode: String,
    pub allowed_overlays: Vec<String>,
    pub description: Option<String>,
    /// Optional runtime preference for this environment. Names a managed-runner
    /// id known to the local-agent (e.g. `opencode-default`),
    /// or `auto`/`None` to defer to the local-agent's configured default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentOverlay {
    pub name: String,
    pub provider: String,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentSelection {
    pub base: String,
    pub overlays: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ReactiveStateKind {
    Auto,
    Agent,
    Command,
    Managed,
    Human,
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ReactiveExecutionConfig {
    pub executor: ExecutionMode,
    #[serde(default)]
    pub container_image: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub overlays: Vec<String>,
    #[serde(default)]
    pub network_resources: Option<NetworkResourceRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_rules: Option<ApprovalRuleSet>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub toolchains: Option<Vec<String>>,
    #[serde(default)]
    pub candidate_repos: Vec<RepoSource>,
    pub repo_access: RepoAccessMode,
    #[serde(default)]
    pub github_capability: Option<GithubCapabilityRequest>,
    #[serde(default)]
    pub max_retries: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReactiveAgentStateConfig {
    #[serde(flatten)]
    pub execution: ReactiveExecutionConfig,
    #[serde(default)]
    pub prompt: Option<PromptRef>,
}

impl<'de> Deserialize<'de> for ReactiveAgentStateConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawReactiveAgentStateConfig {
            executor: ExecutionMode,
            #[serde(default)]
            container_image: Option<String>,
            #[serde(default)]
            environment: Option<String>,
            #[serde(default)]
            overlays: Vec<String>,
            #[serde(default)]
            network_resources: Option<NetworkResourceRequest>,
            #[serde(default)]
            approval_rules: Option<ApprovalRuleSet>,
            #[serde(default)]
            timeout_secs: Option<u64>,
            #[serde(default)]
            toolchains: Option<Vec<String>>,
            #[serde(default)]
            candidate_repos: Vec<RepoSource>,
            repo_access: RepoAccessMode,
            #[serde(default)]
            github_capability: Option<GithubCapabilityRequest>,
            #[serde(default)]
            max_retries: u32,
            #[serde(default)]
            prompt: Option<PromptRef>,
        }

        let raw = RawReactiveAgentStateConfig::deserialize(deserializer)?;
        Ok(Self {
            execution: ReactiveExecutionConfig {
                executor: raw.executor,
                container_image: raw.container_image,
                environment: raw.environment,
                overlays: raw.overlays,
                network_resources: raw.network_resources,
                approval_rules: raw.approval_rules,
                timeout_secs: raw.timeout_secs,
                toolchains: raw.toolchains,
                candidate_repos: raw.candidate_repos,
                repo_access: raw.repo_access,
                github_capability: raw.github_capability,
                max_retries: raw.max_retries,
            },
            prompt: raw.prompt,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReactiveCommandStateConfig {
    #[serde(flatten)]
    pub execution: ReactiveExecutionConfig,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub prompt: Option<PromptRef>,
}

impl<'de> Deserialize<'de> for ReactiveCommandStateConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawReactiveCommandStateConfig {
            executor: ExecutionMode,
            #[serde(default)]
            container_image: Option<String>,
            #[serde(default)]
            environment: Option<String>,
            #[serde(default)]
            overlays: Vec<String>,
            #[serde(default)]
            network_resources: Option<NetworkResourceRequest>,
            #[serde(default)]
            approval_rules: Option<ApprovalRuleSet>,
            #[serde(default)]
            timeout_secs: Option<u64>,
            #[serde(default)]
            toolchains: Option<Vec<String>>,
            #[serde(default)]
            candidate_repos: Vec<RepoSource>,
            repo_access: RepoAccessMode,
            #[serde(default)]
            github_capability: Option<GithubCapabilityRequest>,
            #[serde(default)]
            max_retries: u32,
            command: String,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            env: HashMap<String, String>,
            #[serde(default)]
            prompt: Option<PromptRef>,
        }

        let raw = RawReactiveCommandStateConfig::deserialize(deserializer)?;
        Ok(Self {
            execution: ReactiveExecutionConfig {
                executor: raw.executor,
                container_image: raw.container_image,
                environment: raw.environment,
                overlays: raw.overlays,
                network_resources: raw.network_resources,
                approval_rules: raw.approval_rules,
                timeout_secs: raw.timeout_secs,
                toolchains: raw.toolchains,
                candidate_repos: raw.candidate_repos,
                repo_access: raw.repo_access,
                github_capability: raw.github_capability,
                max_retries: raw.max_retries,
            },
            command: raw.command,
            args: raw.args,
            env: raw.env,
            prompt: raw.prompt,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReactiveManagedStateConfig {
    #[serde(flatten)]
    pub execution: ReactiveExecutionConfig,
    pub operation: GithubAction,
    #[serde(default)]
    pub prompt: Option<PromptRef>,
}

impl<'de> Deserialize<'de> for ReactiveManagedStateConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawReactiveManagedStateConfig {
            executor: ExecutionMode,
            #[serde(default)]
            container_image: Option<String>,
            #[serde(default)]
            environment: Option<String>,
            #[serde(default)]
            overlays: Vec<String>,
            #[serde(default)]
            network_resources: Option<NetworkResourceRequest>,
            #[serde(default)]
            approval_rules: Option<ApprovalRuleSet>,
            #[serde(default)]
            timeout_secs: Option<u64>,
            #[serde(default)]
            toolchains: Option<Vec<String>>,
            #[serde(default)]
            candidate_repos: Vec<RepoSource>,
            repo_access: RepoAccessMode,
            #[serde(default)]
            github_capability: Option<GithubCapabilityRequest>,
            #[serde(default)]
            max_retries: u32,
            operation: GithubAction,
            #[serde(default)]
            prompt: Option<PromptRef>,
        }

        let raw = RawReactiveManagedStateConfig::deserialize(deserializer)?;
        Ok(Self {
            execution: ReactiveExecutionConfig {
                executor: raw.executor,
                container_image: raw.container_image,
                environment: raw.environment,
                overlays: raw.overlays,
                network_resources: raw.network_resources,
                approval_rules: raw.approval_rules,
                timeout_secs: raw.timeout_secs,
                toolchains: raw.toolchains,
                candidate_repos: raw.candidate_repos,
                repo_access: raw.repo_access,
                github_capability: raw.github_capability,
                max_retries: raw.max_retries,
            },
            operation: raw.operation,
            prompt: raw.prompt,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ReactiveHumanStateConfig {
    /// Optional escalation timeout. When the work item has been parked
    /// at this human state for longer than `timeout_secs` (counted from
    /// the most recent meaningful state change — entry, resumption, or
    /// poller-observed PR head_sha bump), the orchestrator fires a
    /// `WorkflowEventKind::Timeout` event so the workflow can transition
    /// to a stuck/escalation state. Workflows without a `timeout`
    /// transition simply ignore the fired event.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ReactiveAutoStateConfig {
    #[serde(default)]
    pub child_workflow: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ReactiveTerminalStateConfig {
    #[serde(default)]
    pub terminal_outcome: Option<ReactiveTerminalOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReactiveStateConfig {
    Agent(ReactiveAgentStateConfig),
    Command(ReactiveCommandStateConfig),
    Managed(ReactiveManagedStateConfig),
    Human(ReactiveHumanStateConfig),
    Auto(ReactiveAutoStateConfig),
    Terminal(ReactiveTerminalStateConfig),
}

impl ReactiveStateConfig {
    pub fn kind(&self) -> ReactiveStateKind {
        match self {
            Self::Agent(_) => ReactiveStateKind::Agent,
            Self::Command(_) => ReactiveStateKind::Command,
            Self::Managed(_) => ReactiveStateKind::Managed,
            Self::Human(_) => ReactiveStateKind::Human,
            Self::Auto(_) => ReactiveStateKind::Auto,
            Self::Terminal(_) => ReactiveStateKind::Terminal,
        }
    }

    pub fn execution(&self) -> Option<&ReactiveExecutionConfig> {
        match self {
            Self::Agent(config) => Some(&config.execution),
            Self::Command(config) => Some(&config.execution),
            Self::Managed(config) => Some(&config.execution),
            Self::Human(_) | Self::Auto(_) | Self::Terminal(_) => None,
        }
    }

    pub fn prompt(&self) -> Option<&PromptRef> {
        match self {
            Self::Agent(config) => config.prompt.as_ref(),
            Self::Command(config) => config.prompt.as_ref(),
            Self::Managed(config) => config.prompt.as_ref(),
            Self::Human(_) | Self::Auto(_) | Self::Terminal(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ReactiveTerminalOutcome {
    // TODO(reactive-terminal-outcomes): extend if we need richer terminal semantics like timed_out
    // without overloading generic failure.
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReactiveWorkflowTransition {
    pub event: ReactiveTransitionEvent,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReactiveWorkflowState {
    pub id: String,
    #[serde(flatten)]
    pub config: ReactiveStateConfig,
    #[serde(default)]
    pub on: Vec<ReactiveWorkflowTransition>,
}

impl ReactiveWorkflowState {
    pub fn kind(&self) -> ReactiveStateKind {
        self.config.kind()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRuleSet {
    /// Tools that are auto-approved for this job
    #[serde(default)]
    pub auto_approve: Vec<String>,
    /// Tools that always escalate to human for this job
    #[serde(default)]
    pub escalate: Vec<String>,
    /// Tools that are denied entirely
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MergePolicy {
    /// Required status check names that must pass
    #[serde(default)]
    pub required_checks: Vec<String>,
    /// Minimum number of approving reviews required
    #[serde(default)]
    pub required_approvals: u32,
    /// Whether dismissal of stale reviews is enforced
    #[serde(default)]
    pub dismiss_stale_reviews: bool,
    /// Whether the branch must be up-to-date with base
    #[serde(default)]
    pub require_up_to_date: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReactiveWorkflowDefinition {
    pub id: String,
    pub schema: String,
    pub initial_state: String,
    #[serde(default)]
    pub states: Vec<ReactiveWorkflowState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WorkflowValidationError {
    pub workflow_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct WorkflowValidationReport {
    #[serde(default)]
    pub errors: Vec<WorkflowValidationError>,
}

impl WorkflowValidationReport {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactiveWorkflowTomlFile {
    pub workflow: ReactiveWorkflowDefinition,
}

pub fn parse_workflow_toml(input: &str) -> Result<ReactiveWorkflowDefinition, toml::de::Error> {
    let value: toml::Value = toml::from_str(input)?;
    let schema = value
        .get("workflow")
        .and_then(|w| w.get("schema"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| {
            serde::de::Error::custom("workflow.schema is required and must be 'reactive_v2'")
        })?;
    if schema != "reactive_v2" {
        return Err(serde::de::Error::custom(format!(
            "workflow schema '{schema}' is not supported (expected reactive_v2)"
        )));
    }
    let wrapped: ReactiveWorkflowTomlFile = value.try_into()?;
    Ok(wrapped.workflow)
}

pub fn validate_reactive_workflow(def: &ReactiveWorkflowDefinition) -> WorkflowValidationReport {
    let mut errors = Vec::new();
    if def.id.trim().is_empty() {
        errors.push(WorkflowValidationError {
            workflow_id: def.id.clone(),
            message: "workflow id must not be empty".to_string(),
        });
    }
    if def.schema != "reactive_v2" {
        errors.push(WorkflowValidationError {
            workflow_id: def.id.clone(),
            message: format!(
                "reactive workflow schema '{}' is not supported (expected reactive_v2)",
                def.schema
            ),
        });
    }

    let mut ids = HashSet::new();
    for state in &def.states {
        let execution = state.config.execution();
        let effective_network_request = execution.and_then(|cfg| cfg.network_resources.clone());
        if matches!(
            effective_network_request,
            Some(NetworkResourceRequest::None)
        ) {
            let network_required_toolchains = ["kubectl", "aws-cli", "gh"];
            if let Some(toolchains) = execution.and_then(|cfg| cfg.toolchains.as_ref()) {
                let blocked_toolchains = toolchains
                    .iter()
                    .filter(|tool| network_required_toolchains.contains(&tool.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                if !blocked_toolchains.is_empty() {
                    errors.push(WorkflowValidationError {
                        workflow_id: def.id.clone(),
                        message: format!(
                            "warning: state '{}' sets network_resources='none' but declares network-requiring toolchains: {}",
                            state.id,
                            blocked_toolchains.join(", ")
                        ),
                    });
                }
            }
        }

        if state.id.trim().is_empty() {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: "state id must not be empty".to_string(),
            });
            continue;
        }
        if !ids.insert(state.id.clone()) {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!("duplicate state id '{}'", state.id),
            });
        }
        if state.id.contains('.') {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!(
                    "state '{}' must not contain '.' (reserved by dot-path interpolation aliases)",
                    state.id
                ),
            });
        }
        if execution.is_some_and(|config| config.executor != ExecutionMode::Docker) {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!("executable state '{}' must use Docker", state.id),
            });
        }
        if let Some(capability) = execution.and_then(|config| config.github_capability.as_ref()) {
            if capability.operations.is_empty() {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' GitHub capability must declare operations",
                        state.id
                    ),
                });
            }
            if !matches!(state.config, ReactiveStateConfig::Managed(_)) {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' may declare github_capability only for a managed GitHub action",
                        state.id
                    ),
                });
            }
            if !matches!(execution.and_then(|config| config.network_resources.as_ref()), Some(NetworkResourceRequest::Resources(resources)) if resources == &[NetworkResourceKind::Github])
            {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' GitHub capability requires the GitHub-only network policy",
                        state.id
                    ),
                });
            }
        }
        if let ReactiveStateConfig::Managed(config) = &state.config {
            if execution
                .and_then(|config| config.timeout_secs)
                .is_none_or(|value| value == 0)
            {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "managed GitHub state '{}' requires positive timeout_secs",
                        state.id
                    ),
                });
            }
            let valid_capability = execution
                .and_then(|execution| execution.github_capability.as_ref())
                .is_some_and(|capability| {
                    let mut actual = capability.operations.clone();
                    actual.sort();
                    actual.dedup();
                    let mut required = config.operation.required_operations().to_vec();
                    required.sort();
                    capability.access == GithubAccessClass::ReadWrite
                        && actual == required
                        && capability.operations.len() == required.len()
                });
            if !valid_capability {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "managed GitHub state '{}' requires the exact read_write operation set for '{}'",
                        state.id, config.operation
                    ),
                });
            }
        }
        if matches!(state.kind(), ReactiveStateKind::Agent) {
            if let ReactiveStateConfig::Agent(_) = &state.config {
            } else {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' kind '{}' has invalid config variant",
                        state.id,
                        state.kind()
                    ),
                });
            }
            if let Some(timeout_secs) = execution.and_then(|cfg| cfg.timeout_secs)
                && timeout_secs == 0
            {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!("agent state '{}' timeout_secs must be > 0", state.id),
                });
            }
        }
        if let ReactiveStateConfig::Command(config) = &state.config
            && config.command.trim().is_empty()
        {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!("command state '{}' must declare command", state.id),
            });
        }
        if let Some(timeout_secs) = execution.and_then(|cfg| cfg.timeout_secs)
            && timeout_secs == 0
        {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!("state '{}' timeout_secs must be > 0", state.id),
            });
        }
        let child_workflow = match &state.config {
            ReactiveStateConfig::Auto(config) => config
                .child_workflow
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty()),
            _ => None,
        };
        let has_child_workflow = child_workflow.is_some();

        if matches!(&state.config, ReactiveStateConfig::Auto(config) if config.child_workflow.is_some())
            && !has_child_workflow
        {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!("state '{}' child_workflow must not be empty", state.id),
            });
        }

        if has_child_workflow && !matches!(state.kind(), ReactiveStateKind::Auto) {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!(
                    "state '{}' child_workflow is only supported for auto states",
                    state.id
                ),
            });
        }

        if matches!(state.kind(), ReactiveStateKind::Auto)
            && !has_child_workflow
            && !state
                .on
                .iter()
                .any(|tr| tr.event.is_kind(ReactiveEventKind::Start))
        {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!("auto state '{}' must define a 'start' transition", state.id),
            });
        }
        if has_child_workflow
            && !state.on.iter().any(|tr| {
                tr.event.is_kind(ReactiveEventKind::AllChildrenCompleted)
                    || tr.event.is_kind(ReactiveEventKind::ChildFailed)
            })
        {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!(
                    "state '{}' with child_workflow must define 'all_children_completed' and/or 'child_failed' transitions",
                    state.id
                ),
            });
        }
        if matches!(state.kind(), ReactiveStateKind::Auto)
            && !has_child_workflow
            && state
                .on
                .iter()
                .any(|tr| !tr.event.is_kind(ReactiveEventKind::Start))
        {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!(
                    "auto state '{}' may only define 'start' transitions",
                    state.id
                ),
            });
        }
        if matches!(state.kind(), ReactiveStateKind::Terminal) && !state.on.is_empty() {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!("terminal state '{}' must not define transitions", state.id),
            });
        }
        let terminal_outcome = match &state.config {
            ReactiveStateConfig::Terminal(config) => config.terminal_outcome.as_ref(),
            _ => None,
        };
        if !matches!(state.kind(), ReactiveStateKind::Terminal) && terminal_outcome.is_some() {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!(
                    "state '{}' kind '{}' must not define terminal_outcome",
                    state.id,
                    state.kind()
                ),
            });
        }

        let mut seen_events = HashSet::new();
        for tr in &state.on {
            let event_kind = tr.event.kind();
            if !seen_events.insert(event_kind.clone()) {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' defines duplicate transition event '{}'",
                        state.id, event_kind
                    ),
                });
            }

            if event_kind == ReactiveEventKind::Cancel {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' must not define explicit 'cancel' transition (cancel is implicit)",
                        state.id
                    ),
                });
                continue;
            }

            let event_allowed_for_state_kind = match state.kind() {
                ReactiveStateKind::Auto => {
                    if has_child_workflow {
                        event_kind == ReactiveEventKind::AllChildrenCompleted
                            || event_kind == ReactiveEventKind::ChildFailed
                    } else {
                        event_kind == ReactiveEventKind::Start
                    }
                }
                ReactiveStateKind::Agent => {
                    event_kind == ReactiveEventKind::JobSucceeded
                        || event_kind == ReactiveEventKind::JobFailed
                        || event_kind == ReactiveEventKind::Timeout
                        // CI-triage outcome events: the triage step (whether
                        // command-stub or future agent) selects one of these
                        // in its `next_event` to route the workflow.
                        || event_kind == ReactiveEventKind::CiFailureTrivial
                        || event_kind == ReactiveEventKind::CiFailureEscalate
                }
                ReactiveStateKind::Command | ReactiveStateKind::Managed => {
                    event_kind == ReactiveEventKind::JobSucceeded
                        || event_kind == ReactiveEventKind::JobFailed
                        || event_kind == ReactiveEventKind::Timeout
                        || event_kind == ReactiveEventKind::CiFailureTrivial
                        || event_kind == ReactiveEventKind::CiFailureEscalate
                }
                ReactiveStateKind::Human => {
                    event_kind == ReactiveEventKind::Submit
                        || event_kind == ReactiveEventKind::TaskCreated
                        || event_kind == ReactiveEventKind::TaskUpdated
                        || event_kind == ReactiveEventKind::CommentAdded
                        || event_kind == ReactiveEventKind::LabelChanged
                        || event_kind == ReactiveEventKind::HumanApproved
                        || event_kind == ReactiveEventKind::HumanChangesRequested
                        || event_kind == ReactiveEventKind::HumanUnblocked
                        || event_kind == ReactiveEventKind::RetryRequested
                        || event_kind == ReactiveEventKind::PrCreated
                        || event_kind == ReactiveEventKind::PrApproved
                        || event_kind == ReactiveEventKind::PrChangesRequested
                        || event_kind == ReactiveEventKind::OwnerPrApproved
                        || event_kind == ReactiveEventKind::OwnerChangesRequested
                        || event_kind == ReactiveEventKind::CoworkerPrApproved
                        || event_kind == ReactiveEventKind::CoworkerChangesRequested
                        || event_kind == ReactiveEventKind::PrMerged
                        || event_kind == ReactiveEventKind::ChecksPassed
                        || event_kind == ReactiveEventKind::ChecksFailed
                        // Allow human states to declare their own
                        // escalation transition. The state-level
                        // `timeout_secs` field on `ReactiveHumanStateConfig`
                        // is the producer; without a `timeout` transition
                        // the orchestrator simply doesn't fire it.
                        || event_kind == ReactiveEventKind::Timeout
                }
                ReactiveStateKind::Terminal => false,
            };
            if !event_allowed_for_state_kind {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' kind '{}' does not allow transition event '{}'",
                        state.id,
                        state.kind(),
                        event_kind
                    ),
                });
            }
        }
        for tr in &state.on {
            if tr.target.trim().is_empty() {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!("state '{}' has empty transition target", state.id),
                });
            }
        }
    }

    if !ids.contains(&def.initial_state) {
        errors.push(WorkflowValidationError {
            workflow_id: def.id.clone(),
            message: format!("initial_state '{}' does not exist", def.initial_state),
        });
    }

    for state in &def.states {
        for tr in &state.on {
            if !ids.contains(&tr.target) {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' transition target '{}' does not exist",
                        state.id, tr.target
                    ),
                });
            }
        }
    }

    // Semantic reachability checks.
    let state_map = def
        .states
        .iter()
        .map(|state| (state.id.as_str(), state))
        .collect::<HashMap<_, _>>();
    let mut transition_graph = HashMap::<&str, Vec<&str>>::new();
    for state in &def.states {
        let mut next_states = Vec::new();
        for tr in &state.on {
            if ids.contains(&tr.target) {
                next_states.push(tr.target.as_str());
            }
        }
        transition_graph.insert(state.id.as_str(), next_states);
    }

    let mut reachable = HashSet::<&str>::new();
    if ids.contains(&def.initial_state) {
        let mut queue = VecDeque::from([def.initial_state.as_str()]);
        while let Some(current) = queue.pop_front() {
            if !reachable.insert(current) {
                continue;
            }
            if let Some(next_states) = transition_graph.get(current) {
                for next in next_states {
                    if !reachable.contains(next) {
                        queue.push_back(next);
                    }
                }
            }
        }

        let has_reachable_terminal = reachable.iter().any(|state_id| {
            state_map
                .get(state_id)
                .is_some_and(|state| matches!(state.kind(), ReactiveStateKind::Terminal))
        });
        if !has_reachable_terminal {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!(
                    "no terminal state reachable from initial state '{}'",
                    def.initial_state
                ),
            });
        }

        for state in &def.states {
            if state.id != def.initial_state && !reachable.contains(state.id.as_str()) {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "warning: state '{}' is unreachable from initial state '{}'",
                        state.id, def.initial_state
                    ),
                });
            }
        }
    }

    WorkflowValidationReport { errors }
}

pub fn validate_reactive_workflows(
    defs: &[ReactiveWorkflowDefinition],
) -> WorkflowValidationReport {
    let mut errors = Vec::new();
    let mut ids = HashSet::new();
    let all_catalog_ids = defs.iter().map(|d| d.id.as_str()).collect::<HashSet<_>>();
    for def in defs {
        if !ids.insert(def.id.clone()) {
            errors.push(WorkflowValidationError {
                workflow_id: def.id.clone(),
                message: format!("duplicate reactive workflow id '{}'", def.id),
            });
        }
        errors.extend(validate_reactive_workflow(def).errors);
        for state in &def.states {
            let child_workflow_ref = match &state.config {
                ReactiveStateConfig::Auto(config) => config.child_workflow.as_deref(),
                _ => None,
            };
            let Some(child_workflow) = child_workflow_ref.map(str::trim).filter(|v| !v.is_empty())
            else {
                continue;
            };
            if !all_catalog_ids.contains(child_workflow) {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' references unknown child workflow '{}'",
                        state.id, child_workflow
                    ),
                });
            }
            if child_workflow == def.id {
                errors.push(WorkflowValidationError {
                    workflow_id: def.id.clone(),
                    message: format!(
                        "state '{}' child workflow must not reference itself",
                        state.id
                    ),
                });
            }
        }
    }
    let child_graph = defs
        .iter()
        .map(|def| {
            let children = def
                .states
                .iter()
                .filter_map(|state| match &state.config {
                    ReactiveStateConfig::Auto(config) => config.child_workflow.clone(),
                    _ => None,
                })
                .filter(|child| defs.iter().any(|candidate| candidate.id == *child))
                .collect::<Vec<_>>();
            (def.id.clone(), children)
        })
        .collect::<HashMap<_, _>>();
    fn visit_child_graph(
        node: &str,
        graph: &HashMap<String, Vec<String>>,
        visiting: &mut Vec<String>,
        visited: &mut HashSet<String>,
        errors: &mut Vec<WorkflowValidationError>,
    ) {
        if let Some(index) = visiting.iter().position(|candidate| candidate == node) {
            let mut cycle = visiting[index..].to_vec();
            cycle.push(node.to_string());
            errors.push(WorkflowValidationError {
                workflow_id: node.to_string(),
                message: format!(
                    "reactive child-workflow cycle detected: {}",
                    cycle.join(" -> ")
                ),
            });
            return;
        }
        if !visited.insert(node.to_string()) {
            return;
        }
        visiting.push(node.to_string());
        for child in graph.get(node).into_iter().flatten() {
            visit_child_graph(child, graph, visiting, visited, errors);
        }
        visiting.pop();
    }
    let mut visited = HashSet::new();
    for workflow_id in child_graph.keys() {
        visit_child_graph(
            workflow_id,
            &child_graph,
            &mut Vec::new(),
            &mut visited,
            &mut errors,
        );
    }
    WorkflowValidationReport { errors }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkItem {
    pub id: Uuid,
    pub workflow_id: String,
    #[serde(default)]
    pub external_task_id: Option<String>,
    #[serde(default)]
    pub external_task_source: Option<String>,
    #[serde(default)]
    pub provider_ownership: ProviderOwnership,
    #[serde(default)]
    pub remembered_human_owners: Vec<ProviderIdentity>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub priority: Option<String>,
    pub session_id: Option<SessionId>,
    pub current_step: String,
    #[serde(default)]
    pub projected_state: Option<String>,
    #[serde(default)]
    pub last_failure_reason: Option<String>,
    #[serde(default)]
    pub required_action_description: Option<String>,
    #[serde(default)]
    pub triage_status: Option<String>,
    #[serde(default)]
    pub human_gate: bool,
    #[serde(default)]
    pub human_gate_state: Option<String>,
    #[serde(default)]
    pub blocked_on: Option<BlockReason>,
    pub state: WorkItemState,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub repo: Option<RepoSource>,
    #[serde(default)]
    pub parent_work_item_id: Option<Uuid>,
    #[serde(default)]
    pub parent_step_id: Option<String>,
    #[serde(default)]
    pub child_work_item_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_written_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub write_back_failures: u32,
    #[serde(default)]
    pub write_back_retry_after: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status_comment_id: Option<i64>,
    #[serde(default)]
    pub checkpoint_comment_id: Option<i64>,
    #[serde(default)]
    pub job_result_comment_ids: HashMap<String, i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Job {
    pub id: Uuid,
    pub work_item_id: Uuid,
    pub step_id: String,
    pub state: JobState,
    pub assigned_worker_id: Option<String>,
    #[serde(default)]
    pub assigned_worker_session_id: Option<Uuid>,
    #[serde(default)]
    pub assigned_worker_session_generation: Option<u64>,
    pub execution_mode: ExecutionMode,
    #[serde(default)]
    pub container_image: Option<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub prompt: Option<PromptRef>,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub canonical_authorization: Option<CanonicalAuthorizationArtifact>,
    #[serde(default)]
    pub repo: Option<RepoSource>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub max_retries: u32,
    pub attempt: u32,
    pub transcript_dir: Option<String>,
    #[serde(default)]
    pub result: Option<JobResult>,
    #[serde(default)]
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkerInfo {
    pub id: String,
    pub hostname: String,
    pub state: WorkerState,
    #[serde(default)]
    pub supported_modes: Vec<ExecutionMode>,
    #[serde(default)]
    pub protocol_capabilities: Vec<WorkerProtocolCapability>,
    pub session_id: Option<Uuid>,
    pub session_generation: u64,
    pub last_heartbeat_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateWorkItemRequest {
    pub workflow_id: String,
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub repo: Option<RepoSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FireEventRequest {
    pub event: WorkflowEventKind,
    pub job_id: Option<Uuid>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CancellationOutcome {
    Accepted,
    AlreadyRequested,
    TerminalNoop,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CancellationLifecycleState {
    Requested,
    RecoveryPending,
    ContainmentFailed,
    Completed,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CancellationDeliveryState {
    Pending,
    Delivered,
    RecoveryPending,
    ContainmentFailed,
    Acknowledged,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CancellationStatus {
    pub outcome: CancellationOutcome,
    #[serde(default)]
    pub cancellation_id: Option<Uuid>,
    #[serde(default)]
    pub generation: Option<u64>,
    pub terminal_jobs: u32,
    pub running_jobs: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FireEventResponse {
    pub work_item: WorkItem,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation: Option<CancellationStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct CancellationCommand {
    pub cancellation_id: Uuid,
    pub generation: u64,
    pub job_id: Uuid,
    pub invocation_id: Uuid,
    pub attempt: u32,
    pub assignment_id: Uuid,
    pub runtime_assignment_id: Uuid,
    pub worker_id: String,
    pub worker_session_id: Uuid,
    pub worker_session_generation: u64,
}

pub fn cancellation_assignment_id(
    job_id: Uuid,
    invocation_id: Uuid,
    attempt: u32,
    worker_id: &str,
    worker_session_id: Uuid,
    worker_session_generation: u64,
) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(b"agent-hub:cancellation-assignment:v1\0");
    hasher.update(job_id.as_bytes());
    hasher.update(invocation_id.as_bytes());
    hasher.update(attempt.to_be_bytes());
    hasher.update((worker_id.len() as u64).to_be_bytes());
    hasher.update(worker_id.as_bytes());
    hasher.update(worker_session_id.as_bytes());
    hasher.update(worker_session_generation.to_be_bytes());
    let digest = hasher.finalize();
    Uuid::from_slice(&digest[..16]).expect("SHA-256 prefix has UUID length")
}

pub fn verify_cancellation_assignment_id(command: &CancellationCommand) -> Result<(), String> {
    let expected = cancellation_assignment_id(
        command.job_id,
        command.invocation_id,
        command.attempt,
        &command.worker_id,
        command.worker_session_id,
        command.worker_session_generation,
    );
    if command.assignment_id != expected {
        return Err(
            "cancellation assignment identity does not match its session fence".to_string(),
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ContainmentAbsenceEvidence {
    pub checked_at: DateTime<Utc>,
    pub process: ContainmentStatus,
    pub container: ContainmentStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContainmentStatus {
    NotApplicable,
    VerifiedAbsent,
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct CancellationAcknowledgement {
    pub command: CancellationCommand,
    pub containment: ContainmentAbsenceEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RegisterWorkerRequest {
    pub id: String,
    pub hostname: String,
    #[serde(default)]
    pub supported_modes: Vec<ExecutionMode>,
    #[serde(default)]
    pub protocol_capabilities: Vec<WorkerProtocolCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterWorkerResponse {
    pub worker: WorkerInfo,
    pub session_id: Uuid,
    pub session_token: crate::Secret,
    pub session_generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub server_protocol_capabilities: Vec<WorkerProtocolCapability>,
}

/// Credential for one registered worker process generation. The token is
/// transported only in worker-authentication headers/metadata over the
/// deployment's loopback or bearer-protected channel; it must not enter job
/// context, logs, artifacts, or persisted cleartext state.
#[derive(Clone)]
pub struct WorkerSessionCredential {
    pub session_id: Uuid,
    pub session_token: crate::Secret,
}

#[derive(Clone)]
pub struct AuthenticatedWorkerSession {
    pub worker_id: String,
    pub session_id: Uuid,
    pub session_token: crate::Secret,
    pub generation: u64,
}

impl AuthenticatedWorkerSession {
    pub fn credential(&self) -> WorkerSessionCredential {
        WorkerSessionCredential {
            session_id: self.session_id,
            session_token: self.session_token.clone(),
        }
    }
}

#[derive(
    Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkerProtocolCapability {
    /// The worker requires lossless context bytes and canonical authorization
    /// bytes. Servers must not assign canonical jobs when this is absent, and
    /// workers advertising it never fall back to legacy context evidence.
    CanonicalAuthorizationV1,
    /// The worker verifies canonical authorization v2, including trusted
    /// OpenCode runtime environment fields and its distinct digest domain.
    CanonicalAuthorizationV2,
    /// The worker verifies canonical authorization v3 and derives executable
    /// agent transitions only from its signed agent_output_events set.
    CanonicalAuthorizationV3,
    /// The worker verifies canonical authorization v4, including the exact
    /// resolver enforcement contract used by restricted egress.
    CanonicalAuthorizationV4,
}

pub fn worker_capability_for_canonical_authorization_version(
    version: u32,
) -> Option<WorkerProtocolCapability> {
    match version {
        1 => Some(WorkerProtocolCapability::CanonicalAuthorizationV1),
        CANONICAL_AUTHORIZATION_VERSION_V2 => {
            Some(WorkerProtocolCapability::CanonicalAuthorizationV2)
        }
        CANONICAL_AUTHORIZATION_VERSION_V3 => {
            Some(WorkerProtocolCapability::CanonicalAuthorizationV3)
        }
        CANONICAL_AUTHORIZATION_VERSION_V4 => {
            Some(WorkerProtocolCapability::CanonicalAuthorizationV4)
        }
        _ => None,
    }
}

pub fn worker_supports_canonical_authorization(
    capabilities: &[WorkerProtocolCapability],
    version: u32,
) -> bool {
    worker_capability_for_canonical_authorization_version(version)
        .is_some_and(|required| capabilities.contains(&required))
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HeartbeatRequest {
    pub state: WorkerState,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HeartbeatResponse {
    pub worker: WorkerInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation: Option<CancellationCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PollJobRequest {
    pub worker_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PollJobResponse {
    pub job: Option<Job>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation: Option<CancellationCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobUpdateRequest {
    pub worker_id: String,
    pub state: JobState,
    pub transcript_dir: Option<String>,
    #[serde(default)]
    pub result: Option<JobResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobResult {
    pub outcome: ExecutionOutcome,
    pub termination_reason: TerminationReason,
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub timing: ExecutionTiming,
    pub artifacts: ArtifactSummary,
    #[serde(default)]
    pub output_json: Option<serde_json::Value>,
    #[serde(default)]
    pub repo: Option<RepoExecutionMetadata>,
    #[serde(default)]
    pub github_pr: Option<GithubPullRequestMetadata>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_acknowledgement: Option<CancellationAcknowledgement>,
}

/// Typed contract for agent output (output.json).
/// Agents MAY include any of these fields. All are optional for backward compat.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentOutput {
    /// The workflow event the agent wants to fire (must be in agent_output_events)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_event: Option<String>,

    /// Human-readable summary of what the agent did
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

    /// Structured key-value outputs for downstream steps
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<serde_json::Map<String, serde_json::Value>>,

    /// GitHub PR metadata if a PR was created/updated
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_pr: Option<AgentOutputGitHubPr>,

    /// Repository selection/discovery for downstream workflow steps
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoSource>,

    /// Environment selection/discovery for downstream workflow steps
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentSelection>,

    /// Error details if the agent encountered an error
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentOutputError>,

    /// Catch-all for additional untyped fields (forward compat)
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutputGitHubPr {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutputError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PromptBundle {
    pub system_path: Option<String>,
    pub user_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PlannedPermissionSet {
    pub tier: PlannedPermissionTier,
    #[serde(default)]
    pub can_read_workspace: bool,
    #[serde(default)]
    pub can_write_workspace: bool,
    #[serde(default)]
    pub can_run_shell: bool,
    #[serde(default)]
    pub can_access_network: bool,
    #[serde(default)]
    pub auto_approve_tools: Vec<String>,
    #[serde(default)]
    pub escalate_tools: Vec<String>,
    #[serde(default)]
    pub kubernetes_access: Option<String>,
    #[serde(default)]
    pub aws_access: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlannedPermissionTier {
    DockerDefault,
}

#[derive(
    Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum NetworkResourceKind {
    Llm,
    Github,
    GoRegistry,
    CratesRegistry,
    PypiRegistry,
    NpmRegistry,
    GuardedWeb,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkResourceRequest {
    None,
    Full,
    Resources(Vec<NetworkResourceKind>),
}

impl NetworkResourceRequest {
    pub fn resources(&self) -> Vec<NetworkResourceKind> {
        match self {
            Self::None | Self::Full => Vec::new(),
            Self::Resources(resources) => {
                let mut deduped = resources.clone();
                deduped.sort_by_key(|a| a.to_string());
                deduped.dedup();
                deduped
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PlannedEnvironmentSpec {
    pub tier: PlannedEnvironmentTier,
    pub execution_mode: ExecutionMode,
    #[serde(default)]
    pub container_image: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u64_lenient")]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub memory_limit: Option<String>,
    #[serde(default)]
    pub cpu_limit: Option<String>,
    #[serde(default)]
    pub network_resources: Option<NetworkResourceRequest>,
    #[serde(default)]
    pub filesystem: Option<String>,
    #[serde(default)]
    pub file_mounts: Vec<PlannedFileMount>,
    #[serde(default)]
    pub toolchains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PlannedFileMount {
    pub host_path: String,
    pub container_path: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlannedEnvironmentTier {
    Docker,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct PlannedRepoClaim {
    pub repo_url: String,
    pub base_ref: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub target_branch: Option<String>,
    #[serde(default)]
    pub claim_type: String,
    #[serde(default)]
    pub branch_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlannedWorkItemView {
    pub id: Uuid,
    pub workflow_id: String,
    pub current_state_key: String,
    pub prototype_state: WorkItemState,
    #[serde(default)]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub repo_claim: Option<PlannedRepoClaim>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PlannedAgentJobView {
    pub id: Uuid,
    pub work_item_id: Uuid,
    pub state_key: String,
    pub prototype_state: JobState,
    #[serde(default)]
    pub assigned_worker_id: Option<String>,
    #[serde(default)]
    pub assigned_worker_session_id: Option<Uuid>,
    #[serde(default)]
    pub assigned_worker_session_generation: Option<u64>,
    pub environment: PlannedEnvironmentSpec,
    pub permission_set: PlannedPermissionSet,
    #[serde(default)]
    pub repo_claim: Option<PlannedRepoClaim>,
    pub attempt: u32,
    pub max_retries: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct CanonicalRuntimeSpec {
    #[serde(default = "default_runtime_version")]
    pub runtime_version: String,
    pub environment: PlannedEnvironmentSpec,
    pub permissions: PlannedPermissionSet,
    #[serde(default)]
    pub approval_rules: Option<ApprovalRuleSet>,
    #[serde(default)]
    pub repos: Vec<PlannedRepoClaim>,
    #[serde(default)]
    pub github_capabilities: Vec<GithubCapability>,
}

fn default_runtime_version() -> String {
    "v2".to_string()
}

pub const CONTEXT_RUNTIME_SPEC_KEY: &str = "canonical_runtime_spec";
pub const CONTEXT_AUTHORIZATION_REQUEST_KEY: &str = "resolved_authorization_request";
pub const CONTEXT_AUTHORIZATION_ID_KEY: &str = "authorization_id";
pub const CONTEXT_REQUEST_DIGEST_KEY: &str = "request_digest";
pub const AUTHORIZATION_REQUEST_VERSION: &str = "resolved_authorization_request_v1";
/// Identifies the exact DTO, canonicalization, and digest domain contract below.
/// Any change to that contract requires a distinct version and domain separator.
pub const CANONICAL_AUTHORIZATION_VERSION_V2: u32 = 2;
pub const CANONICAL_AUTHORIZATION_VERSION_V3: u32 = 3;
pub const CANONICAL_AUTHORIZATION_VERSION_V4: u32 = 4;
pub const CANONICAL_AUTHORIZATION_VERSION: u32 = CANONICAL_AUTHORIZATION_VERSION_V4;
const CANONICAL_AUTHORIZATION_DOMAIN_V1: &[u8] = b"agent-hub:resolved-authorization:v1\0";
const CANONICAL_AUTHORIZATION_DOMAIN_V2: &[u8] = b"agent-hub:resolved-authorization:v2\0";
const CANONICAL_AUTHORIZATION_DOMAIN_V3: &[u8] = b"agent-hub:resolved-authorization:v3\0";
const CANONICAL_AUTHORIZATION_DOMAIN_V4: &[u8] = b"agent-hub:resolved-authorization:v4\0";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalAuthorizationArtifact {
    pub version: u32,
    pub digest: String,
    #[serde(with = "hex_bytes")]
    #[schemars(with = "String")]
    pub canonical_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableAuthorizationBundle {
    pub authorization_id: String,
    pub digest: String,
    pub version: u32,
    pub canonical_bytes: Vec<u8>,
    pub request: ResolvedAuthorizationRequest,
}

impl ImmutableAuthorizationBundle {
    pub fn create(request: &ResolvedAuthorizationRequest) -> Result<Self, serde_json::Error> {
        let artifact = request.canonical_artifact_v4()?;
        let request = artifact.verify_and_parse().map_err(|error| {
            serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })?;
        let authorization_id = authorization_id_for_request(&request, &artifact.digest);
        Ok(Self {
            authorization_id,
            digest: artifact.digest,
            version: artifact.version,
            canonical_bytes: artifact.canonical_bytes,
            request,
        })
    }

    pub fn verify(
        artifact: &CanonicalAuthorizationArtifact,
        authorization_id: &str,
    ) -> Result<Self, String> {
        let request = artifact.verify_and_parse()?;
        let expected_id = authorization_id_for_request(&request, &artifact.digest);
        if authorization_id != expected_id {
            return Err(format!(
                "canonical authorization identity mismatch for version {} digest {}",
                artifact.version, artifact.digest
            ));
        }
        Ok(Self {
            authorization_id: expected_id,
            digest: artifact.digest.clone(),
            version: artifact.version,
            canonical_bytes: artifact.canonical_bytes.clone(),
            request,
        })
    }

    pub fn artifact(&self) -> CanonicalAuthorizationArtifact {
        CanonicalAuthorizationArtifact {
            version: self.version,
            digest: self.digest.clone(),
            canonical_bytes: self.canonical_bytes.clone(),
        }
    }
}

pub fn authorization_id_for_request(
    request: &ResolvedAuthorizationRequest,
    verified_digest: &str,
) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        request.root_work_item_id,
        request.work_unit_id,
        request.state_id,
        request.invocation_id,
        verified_digest
    )
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        hex::decode(encoded).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedAuthorizationRequestV1 {
    version: String,
    invocation_id: Uuid,
    work_unit_id: Uuid,
    root_work_item_id: Uuid,
    workflow_id: String,
    workflow_snapshot: serde_json::Value,
    state_id: String,
    state_kind: ReactiveStateKindV1,
    behavior: ResolvedAuthorizationBehaviorV1,
    permission_envelope: CanonicalRuntimeSpecV1,
    resolved_environment: serde_json::Value,
    provider: ResolvedAuthorizationProviderV1,
    resource_constraints: ResolvedAuthorizationResourcesV1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedAuthorizationRequestV4 {
    request: ResolvedAuthorizationRequestV1,
    approved_execution: ApprovedExecutionSpecV4,
    agent_output_events: Vec<AgentOutputEvent>,
    resolver: ResolverEnforcementV4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovedExecutionSpecV4 {
    runtime: CanonicalRuntimeSpecV1,
    behavior: ResolvedAuthorizationBehaviorV1,
    pids_limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum ResolverEnforcementV4 {
    Disabled,
    Unbound {
        upstream: String,
        readiness_host: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerResolverV4 {
    upstream: String,
    readiness_host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedAuthorizationBehaviorV1 {
    execution_kind: String,
    runner_profile: RunnerProfileV1,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    prompt: Option<PromptRefV1>,
    primary_repo: Option<RepoSourceV1>,
    primary_repo_branch: Option<String>,
    execution_context: AuthorizationExecutionContextV1,
    triage: bool,
    allow_remote_credentials: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptRefV1 {
    system: Option<String>,
    user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoSourceV1 {
    repo_url: String,
    base_ref: String,
    branch_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalRuntimeSpecV1 {
    runtime_version: String,
    environment: PlannedEnvironmentSpecV1,
    permissions: PlannedPermissionSetV1,
    approval_rules: Option<ApprovalRuleSetV1>,
    repos: Vec<PlannedRepoClaimV1>,
    github_capabilities: Vec<GithubCapabilityV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedEnvironmentSpecV1 {
    tier: PlannedEnvironmentTierV1,
    execution_mode: ExecutionModeV1,
    container_image: Option<String>,
    timeout_secs: Option<u64>,
    memory_limit: Option<String>,
    cpu_limit: Option<String>,
    network_resources: Option<NetworkResourceRequestV1>,
    filesystem: Option<String>,
    file_mounts: Vec<PlannedFileMountV1>,
    toolchains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedFileMountV1 {
    host_path: String,
    container_path: String,
    read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedPermissionSetV1 {
    tier: PlannedPermissionTierV1,
    can_read_workspace: bool,
    can_write_workspace: bool,
    can_run_shell: bool,
    can_access_network: bool,
    auto_approve_tools: Vec<String>,
    escalate_tools: Vec<String>,
    kubernetes_access: Option<String>,
    aws_access: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalRuleSetV1 {
    auto_approve: Vec<String>,
    escalate: Vec<String>,
    deny: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedRepoClaimV1 {
    repo_url: String,
    base_ref: String,
    read_only: bool,
    target_branch: Option<String>,
    claim_type: String,
    branch_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedAuthorizationProviderV1 {
    provider: Option<String>,
    task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedAuthorizationResourcesV1 {
    timeout_secs: Option<u64>,
    max_retries: u32,
    attempt: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReactiveStateKindV1 {
    Auto,
    Agent,
    Command,
    Managed,
    Human,
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExecutionModeV1 {
    Docker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PlannedEnvironmentTierV1 {
    Docker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PlannedPermissionTierV1 {
    DockerDefault,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum NetworkResourceKindV1 {
    Llm,
    Github,
    GoRegistry,
    CratesRegistry,
    PypiRegistry,
    NpmRegistry,
    GuardedWeb,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NetworkResourceRequestV1 {
    None,
    Full,
    Resources(Vec<NetworkResourceKindV1>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum GithubOperationV1 {
    ReadRepository,
    PushBranch,
    ReadPullRequest,
    CreatePullRequest,
    MarkPullRequestReady,
    RequestReview,
    MergePullRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GithubCapabilityV1 {
    host: String,
    api_base_url: String,
    repository: String,
    operations: Vec<GithubOperationV1>,
    branch: Option<String>,
    head_branch: Option<String>,
    base_branch: Option<String>,
    pull_request: Option<u64>,
    expected_head_sha: Option<String>,
    credential_provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum RunnerProfileV1 {
    Builtin(BuiltinRunnerProfileV1),
    Resolved(Box<ResolvedRunnerProfileV1>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuiltinRunnerProfileV1 {
    profile_name: String,
    config_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedRunnerProfileV1 {
    profile_name: String,
    kind: ManagedRunnerKindV1,
    command: String,
    args: Vec<String>,
    container_image: Option<String>,
    file_mounts: Vec<RunnerFileMountV1>,
    credential_providers: Vec<CredentialProviderV1>,
    trusted_mounts: Vec<TrustedMountV1>,
    mcp_bridges: Vec<McpBridgeV1>,
    egress: RunnerEgressV1,
    github_hosts: Vec<String>,
    opencode: OpencodeRunnerV1,
    config_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManagedRunnerKindV1 {
    Generic,
    Opencode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerFileMountV1 {
    host_path: String,
    container_path: String,
    read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialProviderV1 {
    name: String,
    provider_type: String,
    targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedMountV1 {
    purpose: String,
    container_path: String,
    read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpBridgeV1 {
    name: String,
    endpoint_type: String,
    target: String,
    container_socket_path: String,
    args: Vec<String>,
    env_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpencodeRunnerV1 {
    max_validation_attempts: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    trusted_env: BTreeMap<String, String>,
}

impl RunnerProfileV1 {
    fn has_trusted_opencode_environment(&self) -> bool {
        matches!(
            self,
            Self::Resolved(profile) if !profile.opencode.trusted_env.is_empty()
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerEgressV1 {
    llm: RunnerEgressLlmV1,
    build: RunnerEgressBuildV1,
    proxy: RunnerEgressProxyV1,
    preflight_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerEgressLlmV1 {
    default_provider: Option<String>,
    providers: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerEgressBuildV1 {
    github_hosts: Vec<String>,
    go_registry_hosts: Vec<String>,
    crates_registry_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pypi_registry_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    npm_registry_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerEgressProxyV1 {
    image: String,
    listen_port: u16,
    runtime_uid: u32,
    runtime_gid: u32,
    writable_tmpfs_mode: String,
    declared_volumes: Vec<String>,
}

impl ResolvedAuthorizationRequestV1 {
    fn from_semantic(request: ResolvedAuthorizationRequest) -> Result<Self, serde_json::Error> {
        Ok(Self {
            version: request.version,
            invocation_id: request.invocation_id,
            work_unit_id: request.work_unit_id,
            root_work_item_id: request.root_work_item_id,
            workflow_id: request.workflow_id,
            workflow_snapshot: request.workflow_snapshot,
            state_id: request.state_id,
            state_kind: match request.state_kind {
                ReactiveStateKind::Auto => ReactiveStateKindV1::Auto,
                ReactiveStateKind::Agent => ReactiveStateKindV1::Agent,
                ReactiveStateKind::Command => ReactiveStateKindV1::Command,
                ReactiveStateKind::Managed => ReactiveStateKindV1::Managed,
                ReactiveStateKind::Human => ReactiveStateKindV1::Human,
                ReactiveStateKind::Terminal => ReactiveStateKindV1::Terminal,
            },
            behavior: ResolvedAuthorizationBehaviorV1 {
                execution_kind: request.behavior.execution_kind,
                runner_profile: serde_json::from_value(request.behavior.runner_profile)?,
                command: request.behavior.command,
                args: request.behavior.args,
                env: request.behavior.env,
                prompt: request.behavior.prompt.map(|prompt| PromptRefV1 {
                    system: prompt.system,
                    user: prompt.user,
                }),
                primary_repo: request.behavior.primary_repo.map(|repo| RepoSourceV1 {
                    repo_url: repo.repo_url,
                    base_ref: repo.base_ref,
                    branch_name: repo.branch_name,
                }),
                primary_repo_branch: request.behavior.primary_repo_branch,
                execution_context: serde_json::from_value(request.behavior.execution_context)?,
                triage: request.behavior.triage,
                allow_remote_credentials: request.behavior.allow_remote_credentials,
            },
            permission_envelope: CanonicalRuntimeSpecV1 {
                runtime_version: request.permission_envelope.runtime_version,
                environment: PlannedEnvironmentSpecV1 {
                    tier: match request.permission_envelope.environment.tier {
                        PlannedEnvironmentTier::Docker => PlannedEnvironmentTierV1::Docker,
                    },
                    execution_mode: match request.permission_envelope.environment.execution_mode {
                        ExecutionMode::Docker => ExecutionModeV1::Docker,
                    },
                    container_image: request.permission_envelope.environment.container_image,
                    timeout_secs: request.permission_envelope.environment.timeout_secs,
                    memory_limit: request.permission_envelope.environment.memory_limit,
                    cpu_limit: request.permission_envelope.environment.cpu_limit,
                    network_resources: request
                        .permission_envelope
                        .environment
                        .network_resources
                        .map(network_request_v1_from_semantic),
                    filesystem: request.permission_envelope.environment.filesystem,
                    file_mounts: request
                        .permission_envelope
                        .environment
                        .file_mounts
                        .into_iter()
                        .map(|mount| PlannedFileMountV1 {
                            host_path: mount.host_path,
                            container_path: mount.container_path,
                            read_only: mount.read_only,
                        })
                        .collect(),
                    toolchains: request.permission_envelope.environment.toolchains,
                },
                permissions: PlannedPermissionSetV1 {
                    tier: match request.permission_envelope.permissions.tier {
                        PlannedPermissionTier::DockerDefault => {
                            PlannedPermissionTierV1::DockerDefault
                        }
                    },
                    can_read_workspace: request.permission_envelope.permissions.can_read_workspace,
                    can_write_workspace: request
                        .permission_envelope
                        .permissions
                        .can_write_workspace,
                    can_run_shell: request.permission_envelope.permissions.can_run_shell,
                    can_access_network: request.permission_envelope.permissions.can_access_network,
                    auto_approve_tools: request.permission_envelope.permissions.auto_approve_tools,
                    escalate_tools: request.permission_envelope.permissions.escalate_tools,
                    kubernetes_access: request.permission_envelope.permissions.kubernetes_access,
                    aws_access: request.permission_envelope.permissions.aws_access,
                },
                approval_rules: request.permission_envelope.approval_rules.map(|rules| {
                    ApprovalRuleSetV1 {
                        auto_approve: rules.auto_approve,
                        escalate: rules.escalate,
                        deny: rules.deny,
                    }
                }),
                repos: request
                    .permission_envelope
                    .repos
                    .into_iter()
                    .map(|repo| PlannedRepoClaimV1 {
                        repo_url: repo.repo_url,
                        base_ref: repo.base_ref,
                        read_only: repo.read_only,
                        target_branch: repo.target_branch,
                        claim_type: repo.claim_type,
                        branch_name: repo.branch_name,
                    })
                    .collect(),
                github_capabilities: request
                    .permission_envelope
                    .github_capabilities
                    .into_iter()
                    .map(github_capability_v1_from_semantic)
                    .collect(),
            },
            resolved_environment: request.resolved_environment,
            provider: ResolvedAuthorizationProviderV1 {
                provider: request.provider.provider,
                task_id: request.provider.task_id,
            },
            resource_constraints: ResolvedAuthorizationResourcesV1 {
                timeout_secs: request.resource_constraints.timeout_secs,
                max_retries: request.resource_constraints.max_retries,
                attempt: request.resource_constraints.attempt,
            },
        })
    }

    fn into_semantic(self) -> Result<ResolvedAuthorizationRequest, serde_json::Error> {
        Ok(ResolvedAuthorizationRequest {
            version: self.version,
            invocation_id: self.invocation_id,
            work_unit_id: self.work_unit_id,
            root_work_item_id: self.root_work_item_id,
            workflow_id: self.workflow_id,
            workflow_snapshot: self.workflow_snapshot,
            state_id: self.state_id,
            state_kind: match self.state_kind {
                ReactiveStateKindV1::Auto => ReactiveStateKind::Auto,
                ReactiveStateKindV1::Agent => ReactiveStateKind::Agent,
                ReactiveStateKindV1::Command => ReactiveStateKind::Command,
                ReactiveStateKindV1::Managed => ReactiveStateKind::Managed,
                ReactiveStateKindV1::Human => ReactiveStateKind::Human,
                ReactiveStateKindV1::Terminal => ReactiveStateKind::Terminal,
            },
            agent_output_events: Vec::new(),
            behavior: ResolvedAuthorizationBehavior {
                execution_kind: self.behavior.execution_kind,
                runner_profile: serde_json::to_value(self.behavior.runner_profile)?,
                command: self.behavior.command,
                args: self.behavior.args,
                env: self.behavior.env,
                prompt: self.behavior.prompt.map(|prompt| PromptRef {
                    system: prompt.system,
                    user: prompt.user,
                }),
                primary_repo: self.behavior.primary_repo.map(|repo| RepoSource {
                    repo_url: repo.repo_url,
                    base_ref: repo.base_ref,
                    branch_name: repo.branch_name,
                }),
                primary_repo_branch: self.behavior.primary_repo_branch,
                execution_context: serde_json::to_value(self.behavior.execution_context)?,
                triage: self.behavior.triage,
                allow_remote_credentials: self.behavior.allow_remote_credentials,
            },
            permission_envelope: CanonicalRuntimeSpec {
                runtime_version: self.permission_envelope.runtime_version,
                environment: PlannedEnvironmentSpec {
                    tier: match self.permission_envelope.environment.tier {
                        PlannedEnvironmentTierV1::Docker => PlannedEnvironmentTier::Docker,
                    },
                    execution_mode: match self.permission_envelope.environment.execution_mode {
                        ExecutionModeV1::Docker => ExecutionMode::Docker,
                    },
                    container_image: self.permission_envelope.environment.container_image,
                    timeout_secs: self.permission_envelope.environment.timeout_secs,
                    memory_limit: self.permission_envelope.environment.memory_limit,
                    cpu_limit: self.permission_envelope.environment.cpu_limit,
                    network_resources: self
                        .permission_envelope
                        .environment
                        .network_resources
                        .map(network_request_v1_into_semantic),
                    filesystem: self.permission_envelope.environment.filesystem,
                    file_mounts: self
                        .permission_envelope
                        .environment
                        .file_mounts
                        .into_iter()
                        .map(|mount| PlannedFileMount {
                            host_path: mount.host_path,
                            container_path: mount.container_path,
                            read_only: mount.read_only,
                        })
                        .collect(),
                    toolchains: self.permission_envelope.environment.toolchains,
                },
                permissions: PlannedPermissionSet {
                    tier: match self.permission_envelope.permissions.tier {
                        PlannedPermissionTierV1::DockerDefault => {
                            PlannedPermissionTier::DockerDefault
                        }
                    },
                    can_read_workspace: self.permission_envelope.permissions.can_read_workspace,
                    can_write_workspace: self.permission_envelope.permissions.can_write_workspace,
                    can_run_shell: self.permission_envelope.permissions.can_run_shell,
                    can_access_network: self.permission_envelope.permissions.can_access_network,
                    auto_approve_tools: self.permission_envelope.permissions.auto_approve_tools,
                    escalate_tools: self.permission_envelope.permissions.escalate_tools,
                    kubernetes_access: self.permission_envelope.permissions.kubernetes_access,
                    aws_access: self.permission_envelope.permissions.aws_access,
                },
                approval_rules: self.permission_envelope.approval_rules.map(|rules| {
                    ApprovalRuleSet {
                        auto_approve: rules.auto_approve,
                        escalate: rules.escalate,
                        deny: rules.deny,
                    }
                }),
                repos: self
                    .permission_envelope
                    .repos
                    .into_iter()
                    .map(|repo| PlannedRepoClaim {
                        repo_url: repo.repo_url,
                        base_ref: repo.base_ref,
                        read_only: repo.read_only,
                        target_branch: repo.target_branch,
                        claim_type: repo.claim_type,
                        branch_name: repo.branch_name,
                    })
                    .collect(),
                github_capabilities: self
                    .permission_envelope
                    .github_capabilities
                    .into_iter()
                    .map(github_capability_v1_into_semantic)
                    .collect(),
            },
            resolved_environment: self.resolved_environment,
            provider: ResolvedAuthorizationProvider {
                provider: self.provider.provider,
                task_id: self.provider.task_id,
            },
            resource_constraints: ResolvedAuthorizationResources {
                timeout_secs: self.resource_constraints.timeout_secs,
                max_retries: self.resource_constraints.max_retries,
                attempt: self.resource_constraints.attempt,
            },
        })
    }

    fn normalize_sets(&mut self) {
        let environment = &mut self.permission_envelope.environment;
        environment.file_mounts.sort_by(|left, right| {
            (&left.host_path, &left.container_path, left.read_only).cmp(&(
                &right.host_path,
                &right.container_path,
                right.read_only,
            ))
        });
        environment.file_mounts.dedup_by(|left, right| {
            left.host_path == right.host_path
                && left.container_path == right.container_path
                && left.read_only == right.read_only
        });
        normalize_string_set(&mut environment.toolchains);
        if let Some(NetworkResourceRequestV1::Resources(resources)) =
            environment.network_resources.as_mut()
        {
            resources.sort();
            resources.dedup();
        }
        let permissions = &mut self.permission_envelope.permissions;
        normalize_string_set(&mut permissions.auto_approve_tools);
        normalize_string_set(&mut permissions.escalate_tools);
        if let Some(rules) = self.permission_envelope.approval_rules.as_mut() {
            normalize_string_set(&mut rules.auto_approve);
            normalize_string_set(&mut rules.escalate);
            normalize_string_set(&mut rules.deny);
        }
        self.permission_envelope.repos.sort_by(|left, right| {
            (
                &left.repo_url,
                &left.base_ref,
                left.read_only,
                &left.target_branch,
                &left.claim_type,
                &left.branch_name,
            )
                .cmp(&(
                    &right.repo_url,
                    &right.base_ref,
                    right.read_only,
                    &right.target_branch,
                    &right.claim_type,
                    &right.branch_name,
                ))
        });
        self.permission_envelope.repos.dedup_by(|left, right| {
            left.repo_url == right.repo_url
                && left.base_ref == right.base_ref
                && left.read_only == right.read_only
                && left.target_branch == right.target_branch
                && left.claim_type == right.claim_type
                && left.branch_name == right.branch_name
        });
        if let RunnerProfileV1::Resolved(profile) = &mut self.behavior.runner_profile {
            profile.file_mounts.sort_by(|left, right| {
                (&left.host_path, &left.container_path, left.read_only).cmp(&(
                    &right.host_path,
                    &right.container_path,
                    right.read_only,
                ))
            });
            profile.file_mounts.dedup_by(|left, right| {
                left.host_path == right.host_path
                    && left.container_path == right.container_path
                    && left.read_only == right.read_only
            });
            for provider in &mut profile.credential_providers {
                normalize_string_set(&mut provider.targets);
            }
            profile.credential_providers.sort_by(|left, right| {
                (&left.name, &left.provider_type, &left.targets).cmp(&(
                    &right.name,
                    &right.provider_type,
                    &right.targets,
                ))
            });
            profile.credential_providers.dedup_by(|left, right| {
                left.name == right.name
                    && left.provider_type == right.provider_type
                    && left.targets == right.targets
            });
            profile.trusted_mounts.sort_by(|left, right| {
                (&left.purpose, &left.container_path, left.read_only).cmp(&(
                    &right.purpose,
                    &right.container_path,
                    right.read_only,
                ))
            });
            profile.trusted_mounts.dedup_by(|left, right| {
                left.purpose == right.purpose
                    && left.container_path == right.container_path
                    && left.read_only == right.read_only
            });
            for bridge in &mut profile.mcp_bridges {
                normalize_string_set(&mut bridge.env_keys);
            }
            profile.mcp_bridges.sort_by(|left, right| {
                (
                    &left.name,
                    &left.endpoint_type,
                    &left.target,
                    &left.container_socket_path,
                    &left.args,
                    &left.env_keys,
                )
                    .cmp(&(
                        &right.name,
                        &right.endpoint_type,
                        &right.target,
                        &right.container_socket_path,
                        &right.args,
                        &right.env_keys,
                    ))
            });
            profile.mcp_bridges.dedup_by(|left, right| {
                left.name == right.name
                    && left.endpoint_type == right.endpoint_type
                    && left.target == right.target
                    && left.container_socket_path == right.container_socket_path
                    && left.args == right.args
                    && left.env_keys == right.env_keys
            });
            normalize_lowercase_string_set(&mut profile.github_hosts);
            let mut providers = BTreeMap::<String, Vec<String>>::new();
            for (provider, mut hosts) in std::mem::take(&mut profile.egress.llm.providers) {
                normalize_lowercase_string_set(&mut hosts);
                providers
                    .entry(provider.trim().to_ascii_lowercase())
                    .or_default()
                    .extend(hosts);
            }
            for hosts in providers.values_mut() {
                normalize_lowercase_string_set(hosts);
            }
            profile.egress.llm.providers = providers;
            profile.egress.llm.default_provider = profile
                .egress
                .llm
                .default_provider
                .take()
                .map(|provider| provider.trim().to_ascii_lowercase());
            normalize_lowercase_string_set(&mut profile.egress.build.github_hosts);
            normalize_lowercase_string_set(&mut profile.egress.build.go_registry_hosts);
            normalize_lowercase_string_set(&mut profile.egress.build.crates_registry_hosts);
            normalize_lowercase_string_set(&mut profile.egress.build.pypi_registry_hosts);
            normalize_lowercase_string_set(&mut profile.egress.build.npm_registry_hosts);
            normalize_string_set(&mut profile.egress.proxy.declared_volumes);
        }
        for capability in &mut self.permission_envelope.github_capabilities {
            capability.operations.sort();
            capability.operations.dedup();
        }
        self.permission_envelope
            .github_capabilities
            .sort_by(|left, right| {
                (
                    &left.host,
                    &left.api_base_url,
                    &left.repository,
                    &left.operations,
                    &left.branch,
                    &left.head_branch,
                    &left.base_branch,
                    left.pull_request,
                    &left.expected_head_sha,
                    &left.credential_provider,
                )
                    .cmp(&(
                        &right.host,
                        &right.api_base_url,
                        &right.repository,
                        &right.operations,
                        &right.branch,
                        &right.head_branch,
                        &right.base_branch,
                        right.pull_request,
                        &right.expected_head_sha,
                        &right.credential_provider,
                    ))
            });
        self.permission_envelope
            .github_capabilities
            .dedup_by(|left, right| {
                left.host == right.host
                    && left.api_base_url == right.api_base_url
                    && left.repository == right.repository
                    && left.operations == right.operations
                    && left.branch == right.branch
                    && left.head_branch == right.head_branch
                    && left.base_branch == right.base_branch
                    && left.pull_request == right.pull_request
                    && left.expected_head_sha == right.expected_head_sha
                    && left.credential_provider == right.credential_provider
            });
    }
}

impl ResolvedAuthorizationRequestV4 {
    fn from_semantic(mut request: ResolvedAuthorizationRequest) -> Result<Self, String> {
        let resolver = take_resolver_enforcement(&mut request)?;
        validate_resolver_enforcement(&resolver)?;
        let agent_output_events = request.agent_output_events.clone();
        let mut request = ResolvedAuthorizationRequestV1::from_semantic(request)
            .map_err(|error| error.to_string())?;
        request.normalize_sets();
        if request_requires_resolver(&request)
            && !matches!(resolver, ResolverEnforcementV4::Unbound { .. })
        {
            return Err("restricted authorization requires resolver enforcement".to_string());
        }
        let approved_execution = ApprovedExecutionSpecV4 {
            runtime: request.permission_envelope.clone(),
            behavior: request.behavior.clone(),
            pids_limit: crate::APPROVED_EXECUTION_PIDS_LIMIT,
        };
        Ok(Self {
            request,
            approved_execution,
            agent_output_events,
            resolver,
        })
    }

    fn normalize_sets(&mut self) {
        self.request.normalize_sets();
        self.agent_output_events.sort();
        self.agent_output_events.dedup();
    }

    fn into_semantic(self) -> Result<ResolvedAuthorizationRequest, String> {
        validate_resolver_enforcement(&self.resolver)?;
        if request_requires_resolver(&self.request)
            && !matches!(self.resolver, ResolverEnforcementV4::Unbound { .. })
        {
            return Err("restricted authorization requires resolver enforcement".to_string());
        }
        if self.approved_execution.pids_limit == 0
            || serde_json::to_value(&self.approved_execution.runtime).map_err(|e| e.to_string())?
                != serde_json::to_value(&self.request.permission_envelope)
                    .map_err(|e| e.to_string())?
            || serde_json::to_value(&self.approved_execution.behavior).map_err(|e| e.to_string())?
                != serde_json::to_value(&self.request.behavior).map_err(|e| e.to_string())?
        {
            return Err(
                "canonical authorization V4 approved execution projection differs from its request"
                    .to_string(),
            );
        }
        let mut request = self
            .request
            .into_semantic()
            .map_err(|error| error.to_string())?;
        request.agent_output_events = self.agent_output_events;
        restore_resolver_enforcement(&mut request, self.resolver)?;
        Ok(request)
    }
}

fn validate_resolver_enforcement(resolver: &ResolverEnforcementV4) -> Result<(), String> {
    let ResolverEnforcementV4::Unbound {
        upstream,
        readiness_host,
    } = resolver
    else {
        return Ok(());
    };
    let (address, port) = upstream
        .rsplit_once('@')
        .ok_or_else(|| "resolver upstream must include @port".to_string())?;
    address
        .parse::<std::net::IpAddr>()
        .map_err(|_| "resolver upstream address must be an IP literal".to_string())?;
    if port.parse::<u16>().ok().filter(|port| *port != 0).is_none() {
        return Err("resolver upstream port is invalid".to_string());
    }
    if !readiness_host.contains('.')
        || readiness_host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err("resolver readiness host is invalid".to_string());
    }
    Ok(())
}

fn request_requires_resolver(request: &ResolvedAuthorizationRequestV1) -> bool {
    matches!(
        request.permission_envelope.environment.network_resources.as_ref(),
        Some(NetworkResourceRequestV1::Resources(resources)) if !resources.is_empty()
    )
}

fn take_resolver_enforcement(
    request: &mut ResolvedAuthorizationRequest,
) -> Result<ResolverEnforcementV4, String> {
    let Some(profile) = request.behavior.runner_profile.as_object_mut() else {
        return Err("runner profile must be an object".to_string());
    };
    if let Some(resolver) = profile.remove("resolver_enforcement") {
        let resolver: RunnerResolverV4 = serde_json::from_value(resolver)
            .map_err(|error| format!("invalid resolver enforcement: {error}"))?;
        return Ok(ResolverEnforcementV4::Unbound {
            upstream: resolver.upstream,
            readiness_host: resolver.readiness_host,
        });
    }
    let Some(egress) = profile.get_mut("egress") else {
        return Ok(ResolverEnforcementV4::Disabled);
    };
    let egress = egress
        .as_object_mut()
        .ok_or_else(|| "runner egress must be an object".to_string())?;
    let Some(resolver) = egress.remove("resolver") else {
        return Ok(ResolverEnforcementV4::Disabled);
    };
    let resolver: RunnerResolverV4 =
        serde_json::from_value(resolver).map_err(|error| format!("invalid resolver: {error}"))?;
    Ok(ResolverEnforcementV4::Unbound {
        upstream: resolver.upstream,
        readiness_host: resolver.readiness_host,
    })
}

fn restore_resolver_enforcement(
    request: &mut ResolvedAuthorizationRequest,
    resolver: ResolverEnforcementV4,
) -> Result<(), String> {
    let ResolverEnforcementV4::Unbound {
        upstream,
        readiness_host,
    } = resolver
    else {
        return Ok(());
    };
    let profile = request
        .behavior
        .runner_profile
        .as_object_mut()
        .ok_or_else(|| "runner profile must be an object".to_string())?;
    let resolver = serde_json::json!({
        "upstream": upstream,
        "readiness_host": readiness_host,
    });
    if let Some(egress) = profile
        .get_mut("egress")
        .and_then(serde_json::Value::as_object_mut)
    {
        egress.insert("resolver".to_string(), resolver);
    } else {
        profile.insert("resolver_enforcement".to_string(), resolver);
    }
    Ok(())
}

fn normalize_lowercase_string_set(values: &mut Vec<String>) {
    for value in values.iter_mut() {
        *value = value.trim().to_ascii_lowercase();
    }
    normalize_string_set(values);
}

fn network_kind_v1_from_semantic(kind: NetworkResourceKind) -> NetworkResourceKindV1 {
    match kind {
        NetworkResourceKind::Llm => NetworkResourceKindV1::Llm,
        NetworkResourceKind::Github => NetworkResourceKindV1::Github,
        NetworkResourceKind::GoRegistry => NetworkResourceKindV1::GoRegistry,
        NetworkResourceKind::CratesRegistry => NetworkResourceKindV1::CratesRegistry,
        NetworkResourceKind::PypiRegistry => NetworkResourceKindV1::PypiRegistry,
        NetworkResourceKind::NpmRegistry => NetworkResourceKindV1::NpmRegistry,
        NetworkResourceKind::GuardedWeb => NetworkResourceKindV1::GuardedWeb,
    }
}

fn network_kind_v1_into_semantic(kind: NetworkResourceKindV1) -> NetworkResourceKind {
    match kind {
        NetworkResourceKindV1::Llm => NetworkResourceKind::Llm,
        NetworkResourceKindV1::Github => NetworkResourceKind::Github,
        NetworkResourceKindV1::GoRegistry => NetworkResourceKind::GoRegistry,
        NetworkResourceKindV1::CratesRegistry => NetworkResourceKind::CratesRegistry,
        NetworkResourceKindV1::PypiRegistry => NetworkResourceKind::PypiRegistry,
        NetworkResourceKindV1::NpmRegistry => NetworkResourceKind::NpmRegistry,
        NetworkResourceKindV1::GuardedWeb => NetworkResourceKind::GuardedWeb,
    }
}

fn network_request_v1_from_semantic(request: NetworkResourceRequest) -> NetworkResourceRequestV1 {
    match request {
        NetworkResourceRequest::None => NetworkResourceRequestV1::None,
        NetworkResourceRequest::Full => NetworkResourceRequestV1::Full,
        NetworkResourceRequest::Resources(resources) => NetworkResourceRequestV1::Resources(
            resources
                .into_iter()
                .map(network_kind_v1_from_semantic)
                .collect(),
        ),
    }
}

fn network_request_v1_into_semantic(request: NetworkResourceRequestV1) -> NetworkResourceRequest {
    match request {
        NetworkResourceRequestV1::None => NetworkResourceRequest::None,
        NetworkResourceRequestV1::Full => NetworkResourceRequest::Full,
        NetworkResourceRequestV1::Resources(resources) => NetworkResourceRequest::Resources(
            resources
                .into_iter()
                .map(network_kind_v1_into_semantic)
                .collect(),
        ),
    }
}

fn github_operation_v1_from_semantic(operation: GithubOperation) -> GithubOperationV1 {
    match operation {
        GithubOperation::ReadRepository => GithubOperationV1::ReadRepository,
        GithubOperation::PushBranch => GithubOperationV1::PushBranch,
        GithubOperation::ReadPullRequest => GithubOperationV1::ReadPullRequest,
        GithubOperation::CreatePullRequest => GithubOperationV1::CreatePullRequest,
        GithubOperation::MarkPullRequestReady => GithubOperationV1::MarkPullRequestReady,
        GithubOperation::RequestReview => GithubOperationV1::RequestReview,
        GithubOperation::MergePullRequest => GithubOperationV1::MergePullRequest,
    }
}

fn github_operation_v1_into_semantic(operation: GithubOperationV1) -> GithubOperation {
    match operation {
        GithubOperationV1::ReadRepository => GithubOperation::ReadRepository,
        GithubOperationV1::PushBranch => GithubOperation::PushBranch,
        GithubOperationV1::ReadPullRequest => GithubOperation::ReadPullRequest,
        GithubOperationV1::CreatePullRequest => GithubOperation::CreatePullRequest,
        GithubOperationV1::MarkPullRequestReady => GithubOperation::MarkPullRequestReady,
        GithubOperationV1::RequestReview => GithubOperation::RequestReview,
        GithubOperationV1::MergePullRequest => GithubOperation::MergePullRequest,
    }
}

fn github_capability_v1_from_semantic(capability: GithubCapability) -> GithubCapabilityV1 {
    GithubCapabilityV1 {
        host: capability.host,
        api_base_url: capability.api_base_url,
        repository: capability.repository,
        operations: capability
            .operations
            .into_iter()
            .map(github_operation_v1_from_semantic)
            .collect(),
        branch: capability.branch,
        head_branch: capability.head_branch,
        base_branch: capability.base_branch,
        pull_request: capability.pull_request,
        expected_head_sha: capability.expected_head_sha,
        credential_provider: capability.credential_provider,
    }
}

fn github_capability_v1_into_semantic(capability: GithubCapabilityV1) -> GithubCapability {
    GithubCapability {
        host: capability.host,
        api_base_url: capability.api_base_url,
        repository: capability.repository,
        operations: capability
            .operations
            .into_iter()
            .map(github_operation_v1_into_semantic)
            .collect(),
        branch: capability.branch,
        head_branch: capability.head_branch,
        base_branch: capability.base_branch,
        pull_request: capability.pull_request,
        expected_head_sha: capability.expected_head_sha,
        credential_provider: capability.credential_provider,
    }
}

pub fn authorized_default_branch(work_item_id: Uuid, invocation_id: Uuid) -> String {
    format!(
        "agent-hub/{}--i{}",
        work_item_id.simple(),
        invocation_id.simple()
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ResolvedAuthorizationRequest {
    pub version: String,
    pub invocation_id: Uuid,
    pub work_unit_id: Uuid,
    pub root_work_item_id: Uuid,
    pub workflow_id: String,
    pub workflow_snapshot: serde_json::Value,
    pub state_id: String,
    pub state_kind: ReactiveStateKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_output_events: Vec<AgentOutputEvent>,
    pub behavior: ResolvedAuthorizationBehavior,
    pub permission_envelope: CanonicalRuntimeSpec,
    pub resolved_environment: serde_json::Value,
    pub provider: ResolvedAuthorizationProvider,
    pub resource_constraints: ResolvedAuthorizationResources,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ResolvedAuthorizationBehavior {
    pub execution_kind: String,
    pub runner_profile: serde_json::Value,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub prompt: Option<PromptRef>,
    pub primary_repo: Option<RepoSource>,
    pub primary_repo_branch: Option<String>,
    pub execution_context: serde_json::Value,
    pub triage: bool,
    pub allow_remote_credentials: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ResolvedAuthorizationProvider {
    pub provider: Option<String>,
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ResolvedAuthorizationResources {
    #[serde(default, deserialize_with = "deserialize_optional_u64_lenient")]
    pub timeout_secs: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_u32_lenient")]
    pub max_retries: u32,
    pub attempt: u32,
}

impl ResolvedAuthorizationRequest {
    pub fn normalize(mut self) -> Self {
        let approved = crate::ApprovedExecutionSpec {
            runtime: self.permission_envelope,
            behavior: self.behavior,
            pids_limit: crate::APPROVED_EXECUTION_PIDS_LIMIT,
        }
        .normalize();
        self.permission_envelope = approved.runtime;
        self.behavior = approved.behavior;
        self.agent_output_events.sort();
        self.agent_output_events.dedup();
        self
    }

    pub fn approved_execution_spec(&self) -> crate::ApprovedExecutionSpec {
        let normalized = self.clone().normalize();
        crate::ApprovedExecutionSpec {
            runtime: normalized.permission_envelope,
            behavior: normalized.behavior,
            pids_limit: crate::APPROVED_EXECUTION_PIDS_LIMIT,
        }
    }

    pub fn request_digest(&self) -> Result<String, serde_json::Error> {
        Ok(self.canonical_artifact_v4()?.digest)
    }

    pub fn canonical_artifact(&self) -> Result<CanonicalAuthorizationArtifact, serde_json::Error> {
        self.canonical_artifact_with_v3(!self.agent_output_events.is_empty())
    }

    pub fn canonical_artifact_v3(
        &self,
    ) -> Result<CanonicalAuthorizationArtifact, serde_json::Error> {
        self.canonical_artifact_with_v3(true)
    }

    pub fn canonical_artifact_v4(
        &self,
    ) -> Result<CanonicalAuthorizationArtifact, serde_json::Error> {
        if self.version != AUTHORIZATION_REQUEST_VERSION {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported authorization request schema {}", self.version),
            )));
        }
        let normalized = self.clone().normalize();
        let mut dto =
            ResolvedAuthorizationRequestV4::from_semantic(normalized).map_err(|error| {
                serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            })?;
        if authorization_execution_context_has_legacy_events(
            &dto.request.behavior.execution_context,
        ) {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "canonical authorization v4 execution context must not contain valid_next_events",
            )));
        }
        dto.normalize_sets();
        let canonical_bytes = canonical_json_bytes(serde_json::to_value(dto)?)?;
        let digest = canonical_authorization_digest_for_version(
            CANONICAL_AUTHORIZATION_VERSION_V4,
            &canonical_bytes,
        )
        .expect("canonical authorization v4 is supported");
        Ok(CanonicalAuthorizationArtifact {
            version: CANONICAL_AUTHORIZATION_VERSION_V4,
            digest,
            canonical_bytes,
        })
    }

    fn canonical_artifact_with_v3(
        &self,
        issue_v3: bool,
    ) -> Result<CanonicalAuthorizationArtifact, serde_json::Error> {
        if self.version != AUTHORIZATION_REQUEST_VERSION {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported authorization request schema {}", self.version),
            )));
        }
        let normalized = self.clone().normalize();
        let agent_output_events = normalized.agent_output_events.clone();
        let mut dto = ResolvedAuthorizationRequestV1::from_semantic(normalized)?;
        dto.normalize_sets();
        let legacy_version = if dto
            .behavior
            .runner_profile
            .has_trusted_opencode_environment()
        {
            CANONICAL_AUTHORIZATION_VERSION_V2
        } else {
            1
        };
        let (version, value) = if !issue_v3 {
            (legacy_version, serde_json::to_value(dto)?)
        } else {
            if authorization_execution_context_has_legacy_events(&dto.behavior.execution_context) {
                return Err(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "canonical authorization v3 execution context must not contain valid_next_events",
                )));
            }
            let mut value = serde_json::to_value(dto)?;
            value
                .as_object_mut()
                .expect("request DTO is an object")
                .insert(
                    "agent_output_events".to_string(),
                    serde_json::to_value(agent_output_events)?,
                );
            (CANONICAL_AUTHORIZATION_VERSION_V3, value)
        };
        let canonical_bytes = canonical_json_bytes(value)?;
        let digest = canonical_authorization_digest_for_version(version, &canonical_bytes)
            .expect("supported canonical authorization version");
        Ok(CanonicalAuthorizationArtifact {
            version,
            digest,
            canonical_bytes,
        })
    }

    pub fn behavior_matches(&self, baseline: &Self) -> bool {
        self.version == baseline.version
            && self.invocation_id == baseline.invocation_id
            && self.work_unit_id == baseline.work_unit_id
            && self.root_work_item_id == baseline.root_work_item_id
            && self.workflow_id == baseline.workflow_id
            && canonical_json(self.workflow_snapshot.clone())
                == canonical_json(baseline.workflow_snapshot.clone())
            && self.state_id == baseline.state_id
            && self.state_kind == baseline.state_kind
            && self.agent_output_events == baseline.agent_output_events
            && self.behavior == baseline.behavior
            && canonical_json(self.resolved_environment.clone())
                == canonical_json(baseline.resolved_environment.clone())
            && self.provider == baseline.provider
            && self.resource_constraints.max_retries == baseline.resource_constraints.max_retries
            && self.resource_constraints.attempt == baseline.resource_constraints.attempt
    }

    pub fn permissions_are_subset_of(&self, baseline: &Self) -> bool {
        runtime_is_subset(
            &self.clone().normalize().permission_envelope,
            &baseline.clone().normalize().permission_envelope,
        ) && optional_limit_is_subset(
            self.resource_constraints.timeout_secs,
            baseline.resource_constraints.timeout_secs,
        )
    }

    pub fn matches_job_execution(&self, job: &Job) -> bool {
        self.work_unit_id == job.work_item_id
            && self.state_id == job.step_id
            && self.behavior.command == job.command
            && self.behavior.args == job.args
            && self.behavior.env == job.env.clone().into_iter().collect()
            && self.behavior.prompt == job.prompt
            && self.behavior.primary_repo == job.repo
            && self.permission_envelope.environment.execution_mode == job.execution_mode
            && self.permission_envelope.environment.container_image == job.container_image
            && self.resource_constraints.timeout_secs == job.timeout_secs
            && self.resource_constraints.max_retries == job.max_retries
            && self.resource_constraints.attempt == job.attempt
            && self.behavior.execution_context == authorization_execution_context(&job.context)
            && self
                .behavior
                .execution_context
                .get("state_invocation")
                .and_then(|value| value.get("id"))
                .and_then(serde_json::Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                == Some(self.invocation_id)
    }
}

impl crate::ApprovedExecutionSpec {
    pub fn normalize(mut self) -> Self {
        normalize_runtime_for_authorization(&mut self.runtime);
        self
    }

    pub fn behavior_matches(&self, baseline: &Self) -> bool {
        self.clone().normalize().behavior == baseline.clone().normalize().behavior
    }

    pub fn permissions_are_subset_of(&self, baseline: &Self) -> bool {
        self.pids_limit > 0
            && self.pids_limit <= baseline.pids_limit
            && runtime_is_subset(
                &self.clone().normalize().runtime,
                &baseline.clone().normalize().runtime,
            )
    }
}

impl CanonicalAuthorizationArtifact {
    pub fn verify_and_parse(&self) -> Result<ResolvedAuthorizationRequest, String> {
        let actual_digest =
            canonical_authorization_digest_for_version(self.version, &self.canonical_bytes)?;
        if actual_digest != self.digest {
            return Err(format!(
                "canonical authorization digest mismatch for version {}",
                self.version
            ));
        }
        if self.version == CANONICAL_AUTHORIZATION_VERSION_V4 {
            let mut dto: ResolvedAuthorizationRequestV4 =
                serde_json::from_slice(&self.canonical_bytes).map_err(|error| {
                    format!("canonical authorization version 4 is invalid: {error}")
                })?;
            if authorization_execution_context_has_legacy_events(
                &dto.request.behavior.execution_context,
            ) {
                return Err(
                    "canonical authorization version 4 execution context contains valid_next_events"
                        .to_string(),
                );
            }
            if dto.request.version != AUTHORIZATION_REQUEST_VERSION {
                return Err(
                    "canonical authorization version 4 has incompatible request schema".to_string(),
                );
            }
            dto.normalize_sets();
            let canonical = canonical_json_bytes(
                serde_json::to_value(&dto).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            if canonical != self.canonical_bytes {
                return Err(
                    "canonical authorization payload is not canonical for version 4".to_string(),
                );
            }
            return dto.into_semantic().map_err(|error| {
                format!("canonical authorization version 4 is semantically invalid: {error}")
            });
        }
        let (mut dto, mut agent_output_events) = if self.version
            == CANONICAL_AUTHORIZATION_VERSION_V3
        {
            let mut value: serde_json::Value = serde_json::from_slice(&self.canonical_bytes)
                .map_err(|error| {
                    format!(
                        "canonical authorization version {} is invalid: {error}",
                        self.version
                    )
                })?;
            let events = value
                .as_object_mut()
                .and_then(|object| object.remove("agent_output_events"))
                .ok_or_else(|| {
                    "canonical authorization version 3 requires agent_output_events".to_string()
                })?;
            let events: Vec<AgentOutputEvent> =
                serde_json::from_value(events).map_err(|error| {
                    format!(
                        "canonical authorization version 3 has invalid agent_output_events: {error}"
                    )
                })?;
            let dto: ResolvedAuthorizationRequestV1 =
                serde_json::from_value(value).map_err(|error| {
                    format!(
                        "canonical authorization version {} is invalid: {error}",
                        self.version
                    )
                })?;
            if authorization_execution_context_has_legacy_events(&dto.behavior.execution_context) {
                return Err(
                    "canonical authorization version 3 execution context contains valid_next_events"
                        .to_string(),
                );
            }
            (dto, events)
        } else {
            let dto: ResolvedAuthorizationRequestV1 = serde_json::from_slice(&self.canonical_bytes)
                .map_err(|error| {
                    format!(
                        "canonical authorization version {} is invalid: {error}",
                        self.version
                    )
                })?;
            (dto, Vec::new())
        };
        let has_trusted_environment = dto
            .behavior
            .runner_profile
            .has_trusted_opencode_environment();
        if (self.version == 1 && has_trusted_environment)
            || (self.version == CANONICAL_AUTHORIZATION_VERSION_V2 && !has_trusted_environment)
        {
            return Err(format!(
                "canonical authorization version {} has incompatible OpenCode runner controls",
                self.version
            ));
        }
        if dto.version != AUTHORIZATION_REQUEST_VERSION {
            return Err(format!(
                "canonical authorization version {} has incompatible request schema",
                self.version
            ));
        }
        dto.normalize_sets();
        agent_output_events.sort();
        agent_output_events.dedup();
        let mut value = serde_json::to_value(&dto).map_err(|error| error.to_string())?;
        if self.version == CANONICAL_AUTHORIZATION_VERSION_V3 {
            value
                .as_object_mut()
                .expect("request DTO is an object")
                .insert(
                    "agent_output_events".to_string(),
                    serde_json::to_value(&agent_output_events)
                        .map_err(|error| error.to_string())?,
                );
        }
        let canonical = canonical_json_bytes(value).map_err(|error| error.to_string())?;
        if canonical != self.canonical_bytes {
            return Err(format!(
                "canonical authorization payload is not canonical for version {}",
                self.version
            ));
        }
        let mut request = dto.into_semantic().map_err(|error| {
            format!(
                "canonical authorization version {} is semantically invalid: {error}",
                self.version
            )
        })?;
        request.agent_output_events = agent_output_events;
        Ok(request)
    }
}

fn authorization_execution_context_has_legacy_events(
    context: &AuthorizationExecutionContextV1,
) -> bool {
    context.valid_next_events.is_some()
        || context.workflow.as_ref().is_some_and(|workflow| {
            workflow.get("valid_next_events").is_some()
                || workflow
                    .get("inputs")
                    .is_some_and(|inputs| inputs.get("valid_next_events").is_some())
        })
}

#[cfg(test)]
fn canonical_authorization_digest(canonical_bytes: &[u8]) -> String {
    canonical_authorization_digest_for_version(1, canonical_bytes)
        .expect("canonical authorization version 1 is supported")
}

fn canonical_authorization_digest_for_version(
    version: u32,
    canonical_bytes: &[u8],
) -> Result<String, String> {
    let mut digest = Sha256::new();
    digest.update(match version {
        1 => CANONICAL_AUTHORIZATION_DOMAIN_V1,
        CANONICAL_AUTHORIZATION_VERSION_V2 => CANONICAL_AUTHORIZATION_DOMAIN_V2,
        CANONICAL_AUTHORIZATION_VERSION_V3 => CANONICAL_AUTHORIZATION_DOMAIN_V3,
        CANONICAL_AUTHORIZATION_VERSION_V4 => CANONICAL_AUTHORIZATION_DOMAIN_V4,
        _ => {
            return Err(format!(
                "unsupported canonical authorization version {version}"
            ));
        }
    });
    digest.update(canonical_bytes);
    Ok(hex::encode(digest.finalize()))
}

/// Canonical numbers are the compact forms emitted by `serde_json` for its
/// signed-integer, unsigned-integer, and finite-float number kinds. Integer
/// kinds use base-10 digits without exponent or decimal point. Float kinds use
/// the shortest round-trippable finite representation, retain a decimal point
/// when needed to preserve float kind, and retain negative zero. Immutable
/// parsing rejects any alternate spelling because reserialization must match
/// the received bytes exactly.
fn canonical_json_bytes(value: serde_json::Value) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&canonical_json(value))
}

pub fn project_authorization_bundle_into_job_context(
    context: &mut serde_json::Value,
    bundle: &ImmutableAuthorizationBundle,
) -> Result<(), serde_json::Error> {
    if !context.is_object() {
        *context = serde_json::json!({});
    }
    let object = context
        .as_object_mut()
        .expect("context normalized as object");
    object.insert(
        CONTEXT_AUTHORIZATION_ID_KEY.to_string(),
        serde_json::Value::String(bundle.authorization_id.clone()),
    );
    object.insert(
        CONTEXT_REQUEST_DIGEST_KEY.to_string(),
        serde_json::Value::String(bundle.digest.clone()),
    );
    object.insert(
        CONTEXT_AUTHORIZATION_REQUEST_KEY.to_string(),
        serde_json::to_value(&bundle.request)?,
    );
    Ok(())
}

pub fn authorization_request_from_job_context(
    context: &serde_json::Value,
) -> Option<ResolvedAuthorizationRequest> {
    context
        .get(CONTEXT_AUTHORIZATION_REQUEST_KEY)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizationExecutionContextV1 {
    #[serde(skip_serializing_if = "Option::is_none")]
    approval_mode: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_runtime_spec: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment_base: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment_runtime: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    github_pr_state: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inputs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_artifact_data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_artifact_metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_artifact_paths: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_outputs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_results: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    managed_operation: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    overlays: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_invocation_id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_catalog: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_catalog_hint: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_invocation: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_next_events: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    work_item: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<serde_json::Value>,
}

pub fn authorization_execution_context(context: &serde_json::Value) -> serde_json::Value {
    let field = |key: &str| context.get(key).cloned();
    let allowlisted = AuthorizationExecutionContextV1 {
        approval_mode: field("approval_mode"),
        canonical_runtime_spec: field(CONTEXT_RUNTIME_SPEC_KEY),
        environment: field("environment"),
        environment_base: field("environment_base"),
        environment_runtime: field("environment_runtime"),
        github_pr_state: field("github_pr_state"),
        inputs: field("inputs"),
        job_artifact_data: field("job_artifact_data"),
        job_artifact_metadata: field("job_artifact_metadata"),
        job_artifact_paths: field("job_artifact_paths"),
        job_outputs: field("job_outputs"),
        job_results: field("job_results"),
        managed_operation: field("managed_operation"),
        overlays: field("overlays"),
        parent_invocation_id: field("parent_invocation_id"),
        repo_catalog: field("repo_catalog"),
        repo_catalog_hint: field("repo_catalog_hint"),
        state_invocation: field("state_invocation"),
        valid_next_events: field("valid_next_events"),
        work_item: field("work_item"),
        workflow: field("workflow"),
    };
    canonical_json(serde_json::to_value(allowlisted).expect("allowlisted context is serializable"))
}

fn normalize_runtime_for_authorization(runtime: &mut CanonicalRuntimeSpec) {
    runtime.environment.file_mounts.sort_by(|left, right| {
        (&left.host_path, &left.container_path, left.read_only).cmp(&(
            &right.host_path,
            &right.container_path,
            right.read_only,
        ))
    });
    runtime.environment.file_mounts.dedup();
    normalize_string_set(&mut runtime.environment.toolchains);
    normalize_string_set(&mut runtime.permissions.auto_approve_tools);
    normalize_string_set(&mut runtime.permissions.escalate_tools);
    if let Some(NetworkResourceRequest::Resources(resources)) =
        runtime.environment.network_resources.as_mut()
    {
        resources.sort_by_key(ToString::to_string);
        resources.dedup();
    }
    if let Some(rules) = runtime.approval_rules.as_mut() {
        normalize_string_set(&mut rules.auto_approve);
        normalize_string_set(&mut rules.escalate);
        normalize_string_set(&mut rules.deny);
    }
    runtime.repos.sort_by(|left, right| {
        (
            &left.repo_url,
            &left.base_ref,
            left.read_only,
            &left.target_branch,
            &left.claim_type,
            &left.branch_name,
        )
            .cmp(&(
                &right.repo_url,
                &right.base_ref,
                right.read_only,
                &right.target_branch,
                &right.claim_type,
                &right.branch_name,
            ))
    });
    runtime.repos.dedup();
    for capability in &mut runtime.github_capabilities {
        capability.operations.sort();
        capability.operations.dedup();
    }
    runtime.github_capabilities.sort_by(|left, right| {
        (
            &left.host,
            &left.api_base_url,
            &left.repository,
            &left.operations,
            &left.branch,
            &left.head_branch,
            &left.base_branch,
            left.pull_request,
            &left.expected_head_sha,
            &left.credential_provider,
        )
            .cmp(&(
                &right.host,
                &right.api_base_url,
                &right.repository,
                &right.operations,
                &right.branch,
                &right.head_branch,
                &right.base_branch,
                right.pull_request,
                &right.expected_head_sha,
                &right.credential_provider,
            ))
    });
    runtime.github_capabilities.dedup();
}

fn normalize_string_set(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

fn runtime_is_subset(request: &CanonicalRuntimeSpec, baseline: &CanonicalRuntimeSpec) -> bool {
    request.runtime_version == baseline.runtime_version
        && request.environment.execution_mode == baseline.environment.execution_mode
        && request.environment.tier == baseline.environment.tier
        && request.permissions.tier == baseline.permissions.tier
        && request.environment.container_image == baseline.environment.container_image
        && request.environment.memory_limit == baseline.environment.memory_limit
        && request.environment.cpu_limit == baseline.environment.cpu_limit
        && request.environment.filesystem == baseline.environment.filesystem
        && optional_limit_is_subset(
            request.environment.timeout_secs,
            baseline.environment.timeout_secs,
        )
        && request.environment.file_mounts == baseline.environment.file_mounts
        && string_set_is_subset(
            &request.environment.toolchains,
            &baseline.environment.toolchains,
        )
        && network_is_subset(
            request.environment.network_resources.as_ref(),
            baseline.environment.network_resources.as_ref(),
        )
        && (!request.permissions.can_read_workspace || baseline.permissions.can_read_workspace)
        && (!request.permissions.can_write_workspace || baseline.permissions.can_write_workspace)
        && (!request.permissions.can_run_shell || baseline.permissions.can_run_shell)
        && (!request.permissions.can_access_network || baseline.permissions.can_access_network)
        && string_set_is_subset(
            &request.permissions.auto_approve_tools,
            &baseline.permissions.auto_approve_tools,
        )
        && string_set_is_subset(
            &request.permissions.escalate_tools,
            &baseline.permissions.escalate_tools,
        )
        && access_is_subset(
            request.permissions.kubernetes_access.as_deref(),
            baseline.permissions.kubernetes_access.as_deref(),
        )
        && access_is_subset(
            request.permissions.aws_access.as_deref(),
            baseline.permissions.aws_access.as_deref(),
        )
        && approval_is_subset(
            request.approval_rules.as_ref(),
            baseline.approval_rules.as_ref(),
        )
        && repos_are_subset(&request.repos, &baseline.repos)
        && github_capabilities_are_subset(
            &request.github_capabilities,
            &baseline.github_capabilities,
        )
}

fn github_capabilities_are_subset(
    request: &[GithubCapability],
    baseline: &[GithubCapability],
) -> bool {
    request.iter().all(|requested| {
        baseline.iter().any(|approved| {
            requested.host == approved.host
                && requested.api_base_url == approved.api_base_url
                && requested.repository == approved.repository
                && requested.branch == approved.branch
                && requested.head_branch == approved.head_branch
                && requested.base_branch == approved.base_branch
                && requested.pull_request == approved.pull_request
                && requested.expected_head_sha == approved.expected_head_sha
                && requested.credential_provider == approved.credential_provider
                && requested
                    .operations
                    .iter()
                    .all(|operation| approved.operations.contains(operation))
        })
    })
}

fn optional_limit_is_subset(request: Option<u64>, baseline: Option<u64>) -> bool {
    match (request, baseline) {
        (Some(request), Some(baseline)) => request <= baseline,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => true,
    }
}

fn string_set_is_subset(request: &[String], baseline: &[String]) -> bool {
    let baseline = baseline.iter().collect::<BTreeSet<_>>();
    request.iter().all(|value| baseline.contains(value))
}

fn network_is_subset(
    request: Option<&NetworkResourceRequest>,
    baseline: Option<&NetworkResourceRequest>,
) -> bool {
    match (request, baseline) {
        (None | Some(NetworkResourceRequest::None), _) => true,
        (Some(NetworkResourceRequest::Full), Some(NetworkResourceRequest::Full)) => true,
        (Some(NetworkResourceRequest::Resources(_)), Some(NetworkResourceRequest::Full)) => true,
        (
            Some(NetworkResourceRequest::Resources(request)),
            Some(NetworkResourceRequest::Resources(baseline)),
        ) => request.iter().all(|resource| baseline.contains(resource)),
        _ => false,
    }
}

fn access_is_subset(request: Option<&str>, baseline: Option<&str>) -> bool {
    let rank = |value: Option<&str>| match value.unwrap_or("none") {
        "none" => Some(0_u8),
        "read_only" => Some(1),
        "read_write" => Some(2),
        _ => None,
    };
    matches!((rank(request), rank(baseline)), (Some(request), Some(baseline)) if request <= baseline)
}

fn approval_is_subset(
    request: Option<&ApprovalRuleSet>,
    baseline: Option<&ApprovalRuleSet>,
) -> bool {
    let tools = request
        .into_iter()
        .flat_map(|rules| {
            rules
                .auto_approve
                .iter()
                .chain(&rules.escalate)
                .chain(&rules.deny)
        })
        .chain(baseline.into_iter().flat_map(|rules| {
            rules
                .auto_approve
                .iter()
                .chain(&rules.escalate)
                .chain(&rules.deny)
        }))
        .collect::<BTreeSet<_>>();
    tools.into_iter().all(|tool| {
        let preserves_explicit_deny = !baseline
            .is_some_and(|rules| rules.deny.iter().any(|candidate| candidate == tool))
            || request.is_some_and(|rules| rules.deny.iter().any(|candidate| candidate == tool));
        preserves_explicit_deny && approval_rank(request, tool) <= approval_rank(baseline, tool)
    })
}

fn approval_rank(rules: Option<&ApprovalRuleSet>, tool: &str) -> u8 {
    let Some(rules) = rules else { return 0 };
    if rules.auto_approve.iter().any(|candidate| candidate == tool) {
        2
    } else if rules.escalate.iter().any(|candidate| candidate == tool) {
        1
    } else {
        0
    }
}

fn repos_are_subset(request: &[PlannedRepoClaim], baseline: &[PlannedRepoClaim]) -> bool {
    request.iter().all(|requested| {
        baseline.iter().any(|approved| {
            requested.repo_url == approved.repo_url
                && requested.base_ref == approved.base_ref
                && (requested.read_only || !approved.read_only)
                && claim_type_is_subset(&requested.claim_type, &approved.claim_type)
                && (requested.read_only
                    || (requested.target_branch == approved.target_branch
                        && requested.branch_name == approved.branch_name))
        })
    })
}

fn claim_type_is_subset(request: &str, baseline: &str) -> bool {
    let rank = |value: &str| match value {
        "read_only" => Some(0_u8),
        "workspace" | "existing_branch" => Some(1),
        _ => None,
    };
    matches!((rank(request), rank(baseline)), (Some(request), Some(baseline)) if request <= baseline)
}

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        other => other,
    }
}
pub const AGENT_CONTEXT_SCHEMA_NAME: &str = "agent_context_v1";
pub const AGENT_CONTEXT_SCHEMA_VERSION: &str = "agent_context_v3";
pub const AGENT_OUTPUT_SCHEMA_NAME: &str = "agent_output_v1";
pub const JOB_RUN_SCHEMA_NAME: &str = "job_run_v1";

/// Validate an AgentOutput's next_event against authorized agent output events.
/// Returns Ok(()) if next_event is None or is in the valid set.
/// Returns Err with a message if next_event is set but not valid.
pub fn validate_agent_output_event(
    output: &AgentOutput,
    agent_output_events: &[String],
) -> Result<(), String> {
    if let Some(ref event) = output.next_event
        && !agent_output_events.contains(event)
    {
        return Err(format!(
            "next_event '{}' not in agent_output_events: {:?}",
            event, agent_output_events
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    pub schema: String,
    pub job: AgentContextJob,
    pub paths: AgentContextPaths,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item: Option<serde_json::Value>,
    pub workflow: AgentContextWorkflow,
    pub artifacts: AgentContextArtifacts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<serde_json::Value>,
    pub repos: AgentContextRepos,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PlannedPermissionSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_rules: Option<ApprovalRuleSet>,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub advisory: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextJob {
    pub id: Uuid,
    pub work_item_id: Option<Uuid>,
    pub step_id: String,
    pub execution_mode: String,
    pub timeout_secs: Option<u64>,
    pub container_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextPaths {
    pub workspace_dir: String,
    pub transcript_dir: String,
    pub context_json: String,
    pub agent_context_json: String,
    pub output_json: String,
    pub run_json: String,
    pub transcript_jsonl: String,
    pub job_events_jsonl: String,
    pub stdout_log: String,
    pub stderr_log: String,
    pub prompt_user: String,
    pub prompt_system: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextWorkflow {
    pub inputs: serde_json::Value,
    pub agent_output_events: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextArtifacts {
    pub paths: serde_json::Value,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextRepos {
    pub primary: Option<serde_json::Value>,
    #[serde(default)]
    pub additional: Vec<serde_json::Value>,
}

pub fn canonical_environment_for_execution(
    execution_mode: ExecutionMode,
    container_image: Option<String>,
    timeout_secs: Option<u64>,
) -> PlannedEnvironmentSpec {
    PlannedEnvironmentSpec {
        tier: PlannedEnvironmentTier::Docker,
        execution_mode,
        container_image,
        timeout_secs,
        memory_limit: None,
        cpu_limit: None,
        network_resources: None,
        filesystem: None,
        file_mounts: Vec::new(),
        toolchains: Vec::new(),
    }
}

pub fn canonical_permissions_for_environment(
    _environment: &PlannedEnvironmentSpec,
) -> PlannedPermissionSet {
    PlannedPermissionSet {
        tier: PlannedPermissionTier::DockerDefault,
        can_read_workspace: true,
        can_write_workspace: true,
        can_run_shell: true,
        can_access_network: false,
        auto_approve_tools: Vec::new(),
        escalate_tools: Vec::new(),
        kubernetes_access: Some("none".to_string()),
        aws_access: Some("none".to_string()),
    }
}

pub fn default_approval_rules_for_tier(
    tier: &PlannedEnvironmentTier,
    network: Option<&NetworkResourceRequest>,
) -> ApprovalRuleSet {
    match tier {
        PlannedEnvironmentTier::Docker => {
            let has_network = !matches!(network, None | Some(NetworkResourceRequest::None));
            if has_network {
                ApprovalRuleSet {
                    auto_approve: vec![
                        "file_read".to_string(),
                        "file_write".to_string(),
                        "shell_safe".to_string(),
                        "git".to_string(),
                        "network".to_string(),
                    ],
                    escalate: vec!["shell_unknown".to_string(), "k8s_read".to_string()],
                    deny: vec!["k8s_write".to_string(), "aws_write".to_string()],
                }
            } else {
                ApprovalRuleSet {
                    auto_approve: vec![
                        "file_read".to_string(),
                        "file_write".to_string(),
                        "shell_safe".to_string(),
                        "git".to_string(),
                    ],
                    escalate: vec!["shell_unknown".to_string()],
                    deny: Vec::new(),
                }
            }
        }
    }
}

pub fn canonical_runtime_spec_for_job(job: &Job) -> CanonicalRuntimeSpec {
    let mut runtime = canonical_runtime_spec_for_execution(
        job.execution_mode.clone(),
        job.container_image.clone(),
        job.timeout_secs,
    );
    if let Some(repo) = &job.repo {
        runtime.repos.push(PlannedRepoClaim::from(repo));
    }
    runtime
}

pub fn canonical_runtime_spec_for_execution(
    execution_mode: ExecutionMode,
    container_image: Option<String>,
    timeout_secs: Option<u64>,
) -> CanonicalRuntimeSpec {
    let environment =
        canonical_environment_for_execution(execution_mode, container_image, timeout_secs);
    let permissions = canonical_permissions_for_environment(&environment);
    CanonicalRuntimeSpec {
        runtime_version: default_runtime_version(),
        approval_rules: Some(default_approval_rules_for_tier(
            &environment.tier,
            environment.network_resources.as_ref(),
        )),
        environment,
        permissions,
        repos: Vec::new(),
        github_capabilities: Vec::new(),
    }
}

pub fn canonical_runtime_spec_for_job_effective(job: &Job) -> CanonicalRuntimeSpec {
    runtime_spec_from_job_context(&job.context)
        .unwrap_or_else(|| canonical_runtime_spec_for_job(job))
}

pub fn runtime_spec_from_job_context(context: &serde_json::Value) -> Option<CanonicalRuntimeSpec> {
    context
        .as_object()?
        .get(CONTEXT_RUNTIME_SPEC_KEY)
        .cloned()
        .and_then(|v| serde_json::from_value::<CanonicalRuntimeSpec>(v).ok())
}

pub fn project_runtime_spec_into_job_context(
    context: &mut serde_json::Value,
    runtime: &CanonicalRuntimeSpec,
) {
    if !context.is_object() {
        *context = serde_json::json!({});
    }
    if let Some(obj) = context.as_object_mut() {
        obj.insert(
            CONTEXT_RUNTIME_SPEC_KEY.to_string(),
            serde_json::to_value(runtime).unwrap_or(serde_json::Value::Null),
        );
    }
}

impl From<&RepoSource> for PlannedRepoClaim {
    fn from(value: &RepoSource) -> Self {
        Self {
            repo_url: value.repo_url.clone(),
            base_ref: value.base_ref.clone(),
            read_only: false,
            target_branch: value.branch_name.clone(),
            claim_type: if value.branch_name.is_some() {
                "existing_branch".to_string()
            } else {
                "read_only".to_string()
            },
            branch_name: value.branch_name.clone(),
        }
    }
}

pub fn planned_work_item_view_from_prototype(work_item: &WorkItem) -> PlannedWorkItemView {
    PlannedWorkItemView {
        id: work_item.id,
        workflow_id: work_item.workflow_id.clone(),
        current_state_key: work_item.current_step.clone(),
        prototype_state: work_item.state.clone(),
        session_id: work_item.session_id.clone(),
        context: work_item.context.clone(),
        repo_claim: work_item.repo.as_ref().map(PlannedRepoClaim::from),
        created_at: work_item.created_at,
        updated_at: work_item.updated_at,
    }
}

pub fn planned_agent_job_view_from_prototype(job: &Job) -> PlannedAgentJobView {
    let runtime = canonical_runtime_spec_for_job(job);
    PlannedAgentJobView {
        id: job.id,
        work_item_id: job.work_item_id,
        state_key: job.step_id.clone(),
        prototype_state: job.state.clone(),
        assigned_worker_id: job.assigned_worker_id.clone(),
        assigned_worker_session_id: job.assigned_worker_session_id,
        assigned_worker_session_generation: job.assigned_worker_session_generation,
        environment: runtime.environment,
        permission_set: runtime.permissions,
        repo_claim: job.repo.as_ref().map(PlannedRepoClaim::from),
        attempt: job.attempt,
        max_retries: job.max_retries,
        created_at: job.created_at,
        updated_at: job.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reactive_workflow_document() {
        let input = r#"
[workflow]
id = "reactive_demo"
schema = "reactive_v2"
initial_state = "draft"

[[workflow.states]]
id = "draft"
kind = "human"

[[workflow.states.on]]
event = "submit"
target = "running"

[[workflow.states]]
id = "running"
kind = "auto"

[[workflow.states.on]]
event = "start"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;

        let workflow = parse_workflow_toml(input).expect("parse");
        assert_eq!(workflow.id, "reactive_demo");
        assert!(validate_reactive_workflow(&workflow).is_valid());
    }

    #[test]
    fn reactive_transition_event_uses_only_canonical_names_on_deserialize() {
        let typed: ReactiveTransitionEvent =
            serde_json::from_str("\"start\"").expect("canonical event");
        assert_eq!(typed.kind(), ReactiveEventKind::Start);
        assert!(typed.is_kind(ReactiveEventKind::Start));

        let submit: ReactiveTransitionEvent =
            serde_json::from_str("\"submit\"").expect("submit event");
        assert_eq!(submit.kind(), ReactiveEventKind::Submit);

        let serialized = serde_json::to_string(&submit).expect("serialize canonical");
        assert_eq!(serialized, "\"submit\"");
    }

    #[test]
    fn reactive_transition_event_rejects_legacy_aliases_on_deserialize() {
        let err = serde_json::from_str::<ReactiveTransitionEvent>("\"retry\"")
            .expect_err("legacy alias should fail parse");
        assert!(
            err.to_string()
                .contains("unknown reactive transition event 'retry'")
        );
    }

    #[test]
    fn reactive_transition_event_rejects_unknown_strings_on_deserialize() {
        let err = serde_json::from_str::<ReactiveTransitionEvent>("\"totally_unknown\"")
            .expect_err("unknown transition event must fail at parse boundary");
        assert!(
            err.to_string()
                .contains("unknown reactive transition event")
        );
    }

    #[test]
    fn reactive_event_kind_is_unified_with_workflow_event_kind() {
        let workflow_event = WorkflowEventKind::ChildFailed;
        let reactive_event: ReactiveEventKind =
            reactive_event_kind_for_workflow_event(workflow_event.clone());
        assert_eq!(reactive_event, WorkflowEventKind::ChildFailed);
        assert_eq!(
            reactive_event_kind_from_str("child_failed"),
            Some(workflow_event)
        );
    }

    #[test]
    fn classifies_external_vs_internal_workflow_events() {
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::Submit
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::TaskCreated
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::TaskUpdated
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::CommentAdded
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::LabelChanged
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::HumanApproved
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::HumanChangesRequested
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::HumanUnblocked
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::RetryRequested
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::PrCreated
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::PrApproved
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::PrChangesRequested
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::OwnerPrApproved
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::OwnerChangesRequested
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::CoworkerPrApproved
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::CoworkerChangesRequested
        ));
        assert!(workflow_event_is_internal_system_only(
            &WorkflowEventKind::PrMerged
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::ChecksPassed
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::ChecksFailed
        ));
        assert!(workflow_event_is_externally_fireable(
            &WorkflowEventKind::Cancel
        ));

        assert!(workflow_event_is_internal_system_only(
            &WorkflowEventKind::WorkflowChosen
        ));
        assert!(workflow_event_is_internal_system_only(
            &WorkflowEventKind::JobSucceeded
        ));
        assert!(workflow_event_is_internal_system_only(
            &WorkflowEventKind::Start
        ));
        assert!(workflow_event_is_internal_system_only(
            &WorkflowEventKind::Timeout
        ));
    }

    #[test]
    fn runtime_spec_roundtrip_via_job_context_projection() {
        let runtime = CanonicalRuntimeSpec {
            runtime_version: default_runtime_version(),
            environment: PlannedEnvironmentSpec {
                tier: PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: Some("alpine:3.20".into()),
                timeout_secs: Some(45),
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
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
            github_capabilities: vec![],
            repos: vec![],
        };

        let mut ctx = serde_json::json!({"k": "v"});
        project_runtime_spec_into_job_context(&mut ctx, &runtime);
        let decoded = runtime_spec_from_job_context(&ctx).expect("runtime spec in context");
        assert_eq!(decoded, runtime);
    }

    #[test]
    fn reactive_human_states_allow_role_aware_pr_events() {
        let input = r#"
[workflow]
id = "reactive_role_review"
schema = "reactive_v2"
initial_state = "await_review"

[[workflow.states]]
id = "await_review"
kind = "human"

[[workflow.states.on]]
event = "owner_pr_approved"
target = "done"

[[workflow.states.on]]
event = "owner_changes_requested"
target = "done"

[[workflow.states.on]]
event = "coworker_pr_approved"
target = "done"

[[workflow.states.on]]
event = "coworker_changes_requested"
target = "done"

[[workflow.states.on]]
event = "pr_changes_requested"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;

        let workflow = parse_workflow_toml(input).expect("parse role-aware workflow");
        assert!(
            validate_reactive_workflow(&workflow).is_valid(),
            "role-aware review events should be valid for human states"
        );
    }

    #[test]
    fn canonical_runtime_spec_for_job_effective_prefers_context() {
        let now = Utc::now();
        let mut job = Job {
            id: Uuid::new_v4(),
            work_item_id: Uuid::new_v4(),
            step_id: "s1".into(),
            state: JobState::Pending,
            assigned_worker_id: None,
            assigned_worker_session_id: None,
            assigned_worker_session_generation: None,
            execution_mode: ExecutionMode::Docker,
            container_image: None,
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            canonical_authorization: None,
            repo: None,
            timeout_secs: Some(5),
            max_retries: 0,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };

        let from_context = CanonicalRuntimeSpec {
            runtime_version: default_runtime_version(),
            environment: PlannedEnvironmentSpec {
                tier: PlannedEnvironmentTier::Docker,
                execution_mode: ExecutionMode::Docker,
                container_image: Some("alpine:3.20".into()),
                timeout_secs: Some(42),
                memory_limit: None,
                cpu_limit: None,
                network_resources: None,
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
            github_capabilities: vec![],
            repos: vec![],
        };
        project_runtime_spec_into_job_context(&mut job.context, &from_context);
        let effective = canonical_runtime_spec_for_job_effective(&job);
        assert_eq!(effective, from_context);
    }

    fn exec_with_mode(mode: ExecutionMode) -> ReactiveExecutionConfig {
        ReactiveExecutionConfig {
            executor: mode,
            ..Default::default()
        }
    }

    fn auto_state(id: &str, target: &str) -> ReactiveWorkflowState {
        ReactiveWorkflowState {
            id: id.to_string(),
            config: ReactiveStateConfig::Auto(ReactiveAutoStateConfig::default()),
            on: vec![ReactiveWorkflowTransition {
                event: ReactiveEventKind::Start.into(),
                target: target.to_string(),
            }],
        }
    }

    fn agent_state(id: &str, next: &str) -> ReactiveWorkflowState {
        ReactiveWorkflowState {
            id: id.to_string(),
            config: ReactiveStateConfig::Agent(ReactiveAgentStateConfig {
                execution: exec_with_mode(ExecutionMode::Docker),
                prompt: None,
            }),
            on: vec![ReactiveWorkflowTransition {
                event: ReactiveEventKind::JobSucceeded.into(),
                target: next.to_string(),
            }],
        }
    }

    fn terminal_state(id: &str) -> ReactiveWorkflowState {
        ReactiveWorkflowState {
            id: id.to_string(),
            config: ReactiveStateConfig::Terminal(ReactiveTerminalStateConfig::default()),
            on: vec![],
        }
    }

    #[test]
    fn reactive_validation_accepts_agent_command_managed_kinds() {
        let def = ReactiveWorkflowDefinition {
            id: "kinds_ok".into(),
            schema: "reactive_v2".into(),
            initial_state: "auto".into(),
            states: vec![
                auto_state("auto", "agent"),
                ReactiveWorkflowState {
                    id: "agent".into(),
                    config: ReactiveStateConfig::Agent(ReactiveAgentStateConfig {
                        execution: exec_with_mode(ExecutionMode::Docker),
                        prompt: Some(PromptRef {
                            system: None,
                            user: "u".into(),
                        }),
                    }),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::JobSucceeded.into(),
                        target: "command".into(),
                    }],
                },
                ReactiveWorkflowState {
                    id: "command".into(),
                    config: ReactiveStateConfig::Command(ReactiveCommandStateConfig {
                        execution: exec_with_mode(ExecutionMode::Docker),
                        command: "echo".into(),
                        args: vec!["ok".into()],
                        env: HashMap::new(),
                        prompt: None,
                    }),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::JobSucceeded.into(),
                        target: "managed".into(),
                    }],
                },
                ReactiveWorkflowState {
                    id: "managed".into(),
                    config: ReactiveStateConfig::Managed(ReactiveManagedStateConfig {
                        execution: ReactiveExecutionConfig {
                            network_resources: Some(NetworkResourceRequest::Resources(vec![
                                NetworkResourceKind::Github,
                            ])),
                            repo_access: RepoAccessMode::ReadWrite,
                            github_capability: Some(GithubCapabilityRequest {
                                operations: vec![GithubOperation::PushBranch],
                                access: GithubAccessClass::ReadWrite,
                            }),
                            timeout_secs: Some(120),
                            ..exec_with_mode(ExecutionMode::Docker)
                        },
                        operation: GithubAction::PushHead,
                        prompt: None,
                    }),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::JobSucceeded.into(),
                        target: "done".into(),
                    }],
                },
                terminal_state("done"),
            ],
        };
        let report = validate_reactive_workflow(&def);
        assert!(report.is_valid(), "{:?}", report.errors);
    }

    #[test]
    fn reactive_validation_rejects_empty_command_or_operation() {
        let command_bad = ReactiveWorkflowDefinition {
            id: "command_bad".into(),
            schema: "reactive_v2".into(),
            initial_state: "run".into(),
            states: vec![
                ReactiveWorkflowState {
                    id: "run".into(),
                    config: ReactiveStateConfig::Command(ReactiveCommandStateConfig {
                        execution: exec_with_mode(ExecutionMode::Docker),
                        command: " ".into(),
                        args: vec![],
                        env: HashMap::new(),
                        prompt: None,
                    }),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::JobSucceeded.into(),
                        target: "done".into(),
                    }],
                },
                terminal_state("done"),
            ],
        };
        assert!(
            validate_reactive_workflow(&command_bad)
                .errors
                .iter()
                .any(|e| e.message.contains("must declare command"))
        );
    }

    #[test]
    fn reactive_validation_rejects_auto_state_without_start_transition() {
        let def = ReactiveWorkflowDefinition {
            id: "bad_auto".into(),
            schema: "reactive_v2".into(),
            initial_state: "auto1".into(),
            states: vec![ReactiveWorkflowState {
                id: "auto1".into(),
                config: ReactiveStateConfig::Auto(ReactiveAutoStateConfig::default()),
                on: vec![ReactiveWorkflowTransition {
                    event: ReactiveEventKind::WorkflowChosen.into(),
                    target: "auto1".into(),
                }],
            }],
        };
        assert!(!validate_reactive_workflow(&def).is_valid());
    }

    #[test]
    fn reactive_validation_enforces_event_compatibility_for_command_and_managed() {
        let def = ReactiveWorkflowDefinition {
            id: "bad_events".into(),
            schema: "reactive_v2".into(),
            initial_state: "cmd".into(),
            states: vec![
                ReactiveWorkflowState {
                    id: "cmd".into(),
                    config: ReactiveStateConfig::Command(ReactiveCommandStateConfig {
                        execution: exec_with_mode(ExecutionMode::Docker),
                        command: "echo".into(),
                        args: vec![],
                        env: HashMap::new(),
                        prompt: None,
                    }),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::HumanApproved.into(),
                        target: "managed".into(),
                    }],
                },
                ReactiveWorkflowState {
                    id: "managed".into(),
                    config: ReactiveStateConfig::Managed(ReactiveManagedStateConfig {
                        execution: exec_with_mode(ExecutionMode::Docker),
                        operation: GithubAction::PushHead,
                        prompt: None,
                    }),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::Submit.into(),
                        target: "done".into(),
                    }],
                },
                terminal_state("done"),
            ],
        };
        let report = validate_reactive_workflow(&def);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.message.contains("does not allow transition event"))
        );
    }

    #[test]
    fn reactive_validation_requires_reachable_terminal_from_initial_state() {
        let def = ReactiveWorkflowDefinition {
            id: "no_terminal".into(),
            schema: "reactive_v2".into(),
            initial_state: "a".into(),
            states: vec![auto_state("a", "b"), agent_state("b", "a")],
        };

        let report = validate_reactive_workflow(&def);
        assert!(report.errors.iter().any(|e| {
            e.message
                .contains("no terminal state reachable from initial state 'a'")
        }));
    }

    #[test]
    fn parse_reactive_workflow_rejects_unknown_transition_events_at_boundary() {
        let input = r#"
[workflow]
id = "bad_events"
schema = "reactive_v2"
initial_state = "draft"

[[workflow.states]]
id = "draft"
kind = "human"

[[workflow.states.on]]
event = "go_now"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;

        let err = parse_workflow_toml(input).expect_err("unknown event should fail parse");
        assert!(
            err.to_string()
                .contains("unknown reactive transition event 'go_now'")
        );
    }

    #[test]
    fn reactive_validation_rejects_terminal_transitions() {
        let def = ReactiveWorkflowDefinition {
            id: "bad_terminal".into(),
            schema: "reactive_v2".into(),
            initial_state: "done".into(),
            states: vec![ReactiveWorkflowState {
                id: "done".into(),
                config: ReactiveStateConfig::Terminal(ReactiveTerminalStateConfig::default()),
                on: vec![ReactiveWorkflowTransition {
                    event: ReactiveEventKind::Cancel.into(),
                    target: "done".into(),
                }],
            }],
        };

        assert!(!validate_reactive_workflow(&def).is_valid());
    }

    #[test]
    fn reactive_validation_rejects_duplicate_transition_events_in_state() {
        let def = ReactiveWorkflowDefinition {
            id: "dupe_events".into(),
            schema: "reactive_v2".into(),
            initial_state: "review".into(),
            states: vec![
                ReactiveWorkflowState {
                    id: "review".into(),
                    config: ReactiveStateConfig::Human(ReactiveHumanStateConfig::default()),
                    on: vec![
                        ReactiveWorkflowTransition {
                            event: ReactiveEventKind::HumanApproved.into(),
                            target: "done".into(),
                        },
                        ReactiveWorkflowTransition {
                            event: ReactiveEventKind::HumanApproved.into(),
                            target: "done_alt".into(),
                        },
                    ],
                },
                terminal_state("done"),
                terminal_state("done_alt"),
            ],
        };

        assert!(!validate_reactive_workflow(&def).is_valid());
    }

    #[test]
    fn reactive_validation_enforces_agent_state_event_compatibility() {
        let def = ReactiveWorkflowDefinition {
            id: "bad_agent_event".into(),
            schema: "reactive_v2".into(),
            initial_state: "run".into(),
            states: vec![
                ReactiveWorkflowState {
                    id: "run".into(),
                    config: ReactiveStateConfig::Agent(ReactiveAgentStateConfig {
                        execution: exec_with_mode(ExecutionMode::Docker),
                        prompt: None,
                    }),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::HumanApproved.into(),
                        target: "done".into(),
                    }],
                },
                terminal_state("done"),
            ],
        };

        assert!(!validate_reactive_workflow(&def).is_valid());
    }

    #[test]
    fn reactive_validation_enforces_human_state_event_compatibility() {
        let def = ReactiveWorkflowDefinition {
            id: "bad_human_event".into(),
            schema: "reactive_v2".into(),
            initial_state: "review".into(),
            states: vec![
                ReactiveWorkflowState {
                    id: "review".into(),
                    config: ReactiveStateConfig::Human(ReactiveHumanStateConfig::default()),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::JobSucceeded.into(),
                        target: "done".into(),
                    }],
                },
                terminal_state("done"),
            ],
        };

        assert!(!validate_reactive_workflow(&def).is_valid());
    }

    #[test]
    fn reactive_validation_rejects_explicit_cancel_transition() {
        let def = ReactiveWorkflowDefinition {
            id: "bad_cancel".into(),
            schema: "reactive_v2".into(),
            initial_state: "review".into(),
            states: vec![
                ReactiveWorkflowState {
                    id: "review".into(),
                    config: ReactiveStateConfig::Human(ReactiveHumanStateConfig::default()),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::Cancel.into(),
                        target: "done".into(),
                    }],
                },
                terminal_state("done"),
            ],
        };

        assert!(!validate_reactive_workflow(&def).is_valid());
    }

    #[test]
    fn reactive_validation_allows_terminal_outcome_on_terminal_state() {
        let def = ReactiveWorkflowDefinition {
            id: "terminal_outcome_ok".into(),
            schema: "reactive_v2".into(),
            initial_state: "done".into(),
            states: vec![ReactiveWorkflowState {
                id: "done".into(),
                config: ReactiveStateConfig::Terminal(ReactiveTerminalStateConfig {
                    terminal_outcome: Some(ReactiveTerminalOutcome::Failed),
                }),
                on: vec![],
            }],
        };

        assert!(validate_reactive_workflow(&def).is_valid());
    }

    #[test]
    fn parse_reactive_workflow_rejects_terminal_outcome_on_non_terminal_state() {
        let input = r#"
[workflow]
id = "terminal_outcome_bad"
schema = "reactive_v2"
initial_state = "draft"

[[workflow.states]]
id = "draft"
kind = "human"
terminal_outcome = "failed"

[[workflow.states.on]]
event = "submit"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;

        let err =
            parse_workflow_toml(input).expect_err("human state should reject terminal_outcome");
        assert!(err.to_string().contains("unknown field `terminal_outcome`"));
    }

    #[test]
    fn parse_reactive_workflow_rejects_child_workflow_on_non_auto_state() {
        let input = r#"
[workflow]
id = "bad_child_kind"
schema = "reactive_v2"
initial_state = "review"

[[workflow.states]]
id = "review"
kind = "human"
child_workflow = "child"

[[workflow.states.on]]
event = "submit"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        let err = parse_workflow_toml(input).expect_err("human state should reject child_workflow");
        assert!(err.to_string().contains("unknown field `child_workflow`"));
    }

    #[test]
    fn parse_reactive_workflow_allows_top_level_execution_fields_for_command_state() {
        let input = r#"
[workflow]
id = "command_top_level_exec"
schema = "reactive_v2"
initial_state = "run"

[[workflow.states]]
id = "run"
kind = "command"
command = "echo"
executor = "docker"
repo_access = "read_only"
timeout_secs = 30

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;

        let workflow = parse_workflow_toml(input).expect("parse command state");
        let run = workflow
            .states
            .iter()
            .find(|state| state.id == "run")
            .expect("run state");
        match &run.config {
            ReactiveStateConfig::Command(config) => {
                assert_eq!(config.command, "echo");
                assert_eq!(config.execution.executor, ExecutionMode::Docker);
                assert_eq!(config.execution.timeout_secs, Some(30));
            }
            _ => panic!("expected command state"),
        }
    }

    #[test]
    fn parse_reactive_workflow_rejects_unknown_fields_on_agent_command_and_managed_states() {
        let agent_input = r#"
[workflow]
id = "agent_unknown_field"
schema = "reactive_v2"
initial_state = "run"

[[workflow.states]]
id = "run"
kind = "agent"
prompt_typo = "nope"

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        let err =
            parse_workflow_toml(agent_input).expect_err("agent state should reject unknown field");
        assert!(err.to_string().contains("unknown field `prompt_typo`"));

        let command_input = r#"
[workflow]
id = "command_unknown_field"
schema = "reactive_v2"
initial_state = "run"

[[workflow.states]]
id = "run"
kind = "command"
command = "echo"
argz = ["oops"]

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        let err = parse_workflow_toml(command_input)
            .expect_err("command state should reject unknown field");
        assert!(err.to_string().contains("unknown field `argz`"));

        let managed_input = r#"
[workflow]
id = "managed_unknown_field"
schema = "reactive_v2"
initial_state = "run"

[[workflow.states]]
id = "run"
kind = "managed"
executor = "docker"
repo_access = "read_write"
network_resources = { resources = ["github"] }
operation = "push_head"
github_capability = { operations = ["push_branch"], access = "read_write" }
operation_typo = "oops"

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        let err = parse_workflow_toml(managed_input)
            .expect_err("managed state should reject unknown field");
        assert!(err.to_string().contains("unknown field `operation_typo`"));
    }

    #[test]
    fn reactive_schema_rejects_timeout_alias_and_unknown_nested_fields() {
        let base = r#"[workflow]
id = "strict"
schema = "reactive_v2"
initial_state = "run"

[[workflow.states]]
id = "run"
kind = "command"
executor = "docker"
repo_access = "read_only"
command = "true"
timeout_secs = 30

[[workflow.states.on]]
event = "job_succeeded"
target = "done"

[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        assert!(parse_workflow_toml(base).is_ok());
        for invalid in [
            base.replace("timeout_secs = 30", "timeout = 30"),
            base.replace("timeout_secs = 30", "timeout_sec = 30"),
            base.replace("target = \"done\"", "target = \"done\"\ntypo = true"),
            base.replace(
                "schema = \"reactive_v2\"",
                "schema = \"reactive_v2\"\nunknown = true",
            ),
            base.replace(
                "command = \"true\"",
                "command = \"true\"\napproval_rules = { auto_approve = [], typo = [] }",
            ),
            base.replace(
                "command = \"true\"",
                "command = \"true\"\nprompt = { user = \"run\", typo = true }",
            ),
            base.replace("repo_access = \"read_only\"\n", ""),
        ] {
            assert!(
                parse_workflow_toml(&invalid).is_err(),
                "accepted:\n{invalid}"
            );
        }
    }

    #[test]
    fn managed_github_capability_matrix_is_validated_at_catalog_boundary() {
        let cases = [
            ("push_head", "[\"push_branch\"]"),
            (
                "read_or_create_pull_request",
                "[\"read_pull_request\", \"create_pull_request\"]",
            ),
            (
                "mark_pull_request_ready",
                "[\"read_pull_request\", \"mark_pull_request_ready\"]",
            ),
            (
                "request_review",
                "[\"read_pull_request\", \"request_review\"]",
            ),
            (
                "merge_pull_request",
                "[\"read_pull_request\", \"merge_pull_request\"]",
            ),
        ];
        for (action, operations) in cases {
            let source = format!(
                "[workflow]\nid = \"managed\"\nschema = \"reactive_v2\"\ninitial_state = \"run\"\n[[workflow.states]]\nid = \"run\"\nkind = \"managed\"\nexecutor = \"docker\"\ntimeout_secs = 120\nrepo_access = \"read_only\"\nnetwork_resources = {{ resources = [\"github\"] }}\noperation = \"{action}\"\ngithub_capability = {{ operations = {operations}, access = \"read_write\" }}\n[[workflow.states.on]]\nevent = \"job_succeeded\"\ntarget = \"done\"\n[[workflow.states]]\nid = \"done\"\nkind = \"terminal\"\n"
            );
            let definition = parse_workflow_toml(&source).unwrap();
            assert!(
                validate_reactive_workflow(&definition).is_valid(),
                "{action}"
            );
            for invalid in [
                source.replace("timeout_secs = 120\n", ""),
                source.replace("access = \"read_write\"", "access = \"read_only\""),
                source.replace(
                    "network_resources = { resources = [\"github\"] }",
                    "network_resources = { resources = [\"github\", \"llm\"] }",
                ),
                source.replace(
                    &format!("github_capability = {{ operations = {operations}, access = \"read_write\" }}"),
                    "github_capability = { operations = [\"push_branch\", \"read_pull_request\"], access = \"read_write\" }",
                ),
            ] {
                let definition = parse_workflow_toml(&invalid).unwrap();
                assert!(!validate_reactive_workflow(&definition).is_valid());
            }
        }

        let command_with_capability = r#"[workflow]
id = "invalid"
schema = "reactive_v2"
initial_state = "run"
[[workflow.states]]
id = "run"
kind = "command"
executor = "docker"
repo_access = "read_write"
network_resources = { resources = ["github"] }
github_capability = { operations = ["push_branch"], access = "read_write" }
command = "true"
[[workflow.states.on]]
event = "job_succeeded"
target = "done"
[[workflow.states]]
id = "done"
kind = "terminal"
"#;
        assert!(
            !validate_reactive_workflow(&parse_workflow_toml(command_with_capability).unwrap())
                .is_valid()
        );
    }

    #[test]
    fn github_api_base_must_match_repository_host_identity() {
        assert_eq!(
            canonical_github_api_base_for_host("github.com", "https://api.github.com"),
            Some("https://api.github.com".to_string())
        );
        assert!(
            canonical_github_api_base_for_host("github.com", "https://github.com/api/v3").is_none()
        );
        assert_eq!(
            canonical_github_api_base_for_host(
                "ghe.example.com",
                "https://ghe.example.com/custom/api/v3"
            ),
            Some("https://ghe.example.com/custom/api/v3".to_string())
        );
        assert!(
            canonical_github_api_base_for_host(
                "ghe.example.com",
                "https://api.other.example/custom/api/v3"
            )
            .is_none()
        );
    }

    #[test]
    fn repository_and_provider_identity_records_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<RepoSource>(serde_json::json!({
                "repo_url":"https://github.com/reddit/agent-hub",
                "base_ref":"origin/main",
                "branch_name":null,
                "unknown":true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ProviderIdentity>(serde_json::json!({
                "provider_id":"42",
                "display_name":"alice",
                "unknown":true
            }))
            .is_err()
        );
    }

    #[test]
    fn reactive_validation_rejects_child_workflow_without_child_outcome_transition() {
        let def = ReactiveWorkflowDefinition {
            id: "bad_child_transitions".into(),
            schema: "reactive_v2".into(),
            initial_state: "spawn".into(),
            states: vec![
                ReactiveWorkflowState {
                    id: "spawn".into(),
                    config: ReactiveStateConfig::Auto(ReactiveAutoStateConfig {
                        child_workflow: Some("child".into()),
                        action: Some("spawn_children".into()),
                    }),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::Start.into(),
                        target: "done".into(),
                    }],
                },
                terminal_state("done"),
            ],
        };
        assert!(!validate_reactive_workflow(&def).is_valid());
    }

    #[test]
    fn reactive_validation_cross_checks_child_workflow_catalog_reference() {
        let reactive = ReactiveWorkflowDefinition {
            id: "parent".into(),
            schema: "reactive_v2".into(),
            initial_state: "spawn".into(),
            states: vec![
                ReactiveWorkflowState {
                    id: "spawn".into(),
                    config: ReactiveStateConfig::Auto(ReactiveAutoStateConfig {
                        child_workflow: Some("missing_child".into()),
                        action: Some("spawn_children".into()),
                    }),
                    on: vec![ReactiveWorkflowTransition {
                        event: ReactiveEventKind::AllChildrenCompleted.into(),
                        target: "done".into(),
                    }],
                },
                terminal_state("done"),
            ],
        };

        let report = validate_reactive_workflows(&[reactive]);
        assert!(!report.is_valid());
        assert!(report.errors.iter().any(|e| {
            e.message
                .contains("references unknown child workflow 'missing_child'")
        }));
    }

    #[test]
    fn reactive_validation_rejects_indirect_child_workflow_cycle() {
        fn workflow(id: &str, child: &str) -> ReactiveWorkflowDefinition {
            ReactiveWorkflowDefinition {
                id: id.to_string(),
                schema: "reactive_v2".to_string(),
                initial_state: "spawn".to_string(),
                states: vec![
                    ReactiveWorkflowState {
                        id: "spawn".to_string(),
                        config: ReactiveStateConfig::Auto(ReactiveAutoStateConfig {
                            action: None,
                            child_workflow: Some(child.to_string()),
                        }),
                        on: vec![ReactiveWorkflowTransition {
                            event: ReactiveEventKind::AllChildrenCompleted.into(),
                            target: "done".to_string(),
                        }],
                    },
                    ReactiveWorkflowState {
                        id: "done".to_string(),
                        config: ReactiveStateConfig::Terminal(ReactiveTerminalStateConfig {
                            terminal_outcome: Some(ReactiveTerminalOutcome::Succeeded),
                        }),
                        on: Vec::new(),
                    },
                ],
            }
        }
        let report = validate_reactive_workflows(&[workflow("a", "b"), workflow("b", "a")]);
        assert!(report.errors.iter().any(|error| {
            error
                .message
                .contains("reactive child-workflow cycle detected")
        }));
        let direct = validate_reactive_workflows(&[workflow("self", "self")]);
        assert!(direct.errors.iter().any(|error| {
            error.message.contains("must not reference itself")
                || error
                    .message
                    .contains("reactive child-workflow cycle detected")
        }));
    }

    #[test]
    fn maps_prototype_records_to_planned_views() {
        let now = Utc::now();
        let work_item = WorkItem {
            id: Uuid::new_v4(),
            workflow_id: "wf".into(),
            external_task_id: None,
            external_task_source: None,
            provider_ownership: ProviderOwnership::Unassigned,
            remembered_human_owners: vec![],
            title: None,
            description: None,
            labels: vec![],
            priority: None,
            session_id: None,
            current_step: "build".into(),
            projected_state: Some("running".into()),
            last_failure_reason: None,
            required_action_description: None,
            triage_status: None,
            human_gate: false,
            human_gate_state: None,
            blocked_on: None,
            state: WorkItemState::Running,
            context: serde_json::json!({"x": 1}),
            repo: Some(RepoSource {
                repo_url: "https://github.com/reddit/agent-hub.git".into(),
                base_ref: "origin/main".into(),
                branch_name: Some("agent-hub/demo".into()),
            }),
            parent_work_item_id: None,
            parent_step_id: None,
            child_work_item_id: None,
            created_at: now,
            updated_at: now,
            last_written_at: None,
            write_back_failures: 0,
            write_back_retry_after: None,
            status_comment_id: None,
            checkpoint_comment_id: None,
            job_result_comment_ids: HashMap::new(),
        };

        let planned_item = planned_work_item_view_from_prototype(&work_item);
        assert_eq!(planned_item.current_state_key, "build");

        let job = Job {
            id: Uuid::new_v4(),
            work_item_id: work_item.id,
            step_id: "build".into(),
            state: JobState::Running,
            assigned_worker_id: Some("w1".into()),
            assigned_worker_session_id: None,
            assigned_worker_session_generation: None,
            execution_mode: ExecutionMode::Docker,
            container_image: Some("alpine:3.20".into()),
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            prompt: None,
            context: serde_json::json!({}),
            canonical_authorization: None,
            repo: work_item.repo.clone(),
            timeout_secs: Some(30),
            max_retries: 1,
            attempt: 0,
            transcript_dir: None,
            result: None,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
        };
        let planned_job = planned_agent_job_view_from_prototype(&job);
        assert!(planned_job.permission_set.can_run_shell);
        assert_eq!(planned_job.state_key, "build");
    }

    #[test]
    fn docker_runtime_has_restricted_default_permissions() {
        let env = canonical_environment_for_execution(
            ExecutionMode::Docker,
            Some("alpine:3.20".into()),
            Some(30),
        );
        let permissions = canonical_permissions_for_environment(&env);
        assert_eq!(env.tier, PlannedEnvironmentTier::Docker);
        assert_eq!(permissions.tier, PlannedPermissionTier::DockerDefault);
        assert!(!permissions.can_access_network);
    }
}
