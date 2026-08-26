use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use agent_hub_executor::{
    ArtifactPaths, Cancellation, CleanupStatus, ContainmentStatus, Deadline, DockerCli,
    DockerExecutionPlan, DockerMount, DockerOwnership, ExecutionCancellationContext,
    ExecutionRequest, ExecutionStatus, JournalledEffect, NetworkMode, OutputLimits,
    cleanup_temporary_files, execute_output_cancellable, execute_output_journalled,
    execute_process, verify_image_identity,
};

#[test]
fn deadline_is_shared_and_monotonically_decreases() {
    let start = Instant::now();
    let deadline = Deadline::from_start(start, SystemTime::now(), Some(10)).unwrap();
    assert_eq!(deadline.remaining_at(start), Some(Duration::from_secs(10)));
    assert_eq!(
        deadline.remaining_at(start + Duration::from_secs(4)),
        Some(Duration::from_secs(6))
    );
    assert_eq!(
        deadline.remaining_at(start + Duration::from_secs(11)),
        Some(Duration::ZERO)
    );
}

#[test]
fn deadline_supports_immediate_expiry_and_rejects_overflow() {
    assert_eq!(
        Deadline::from_start(Instant::now(), SystemTime::now(), Some(0))
            .unwrap()
            .remaining_at(Instant::now()),
        Some(Duration::ZERO)
    );
    assert!(Deadline::new(Some(u64::MAX)).is_err());
    assert!(Deadline::new(None).is_ok());
}

#[test]
fn docker_plan_materializes_mandatory_containment_controls() {
    let plan = DockerExecutionPlan {
        image_identity: format!("sha256:{}", "a".repeat(64)),
        container_name: "executor-test".to_string(),
        entrypoint: "/bin/true".to_string(),
        command_args: Vec::new(),
        mounts: Vec::new(),
        environment: BTreeMap::new(),
        network: NetworkMode::None,
        dns: Vec::new(),
        extra_hosts: Vec::new(),
        ownership: DockerOwnership::new(BTreeMap::from([(
            "agent-hub.example/owner".to_string(),
            "test".to_string(),
        )]))
        .unwrap(),
        user: "1000:1000".to_string(),
        workdir: Some("/workspace".to_string()),
        memory_limit: "8g".to_string(),
        cpu_limit: "4".to_string(),
        pids_limit: 512,
        read_only: true,
        tmpfs: vec!["/tmp:rw,size=1g".to_string()],
    };
    let args = plan.materialize_args().unwrap();
    for required in [
        "--network=none",
        "--security-opt=no-new-privileges",
        "--cap-drop=ALL",
        "--memory=8g",
        "--cpus=4",
        "--pids-limit=512",
        "--read-only",
    ] {
        assert!(args.iter().any(|argument| argument == required));
    }
    assert_eq!(
        args.iter()
            .position(|argument| argument == "--entrypoint")
            .unwrap()
            + 3,
        args.len()
    );
}

#[test]
fn docker_mount_and_ownership_arguments_are_deterministic() {
    let ownership = DockerOwnership::new(BTreeMap::from([
        ("z.example/owner".to_string(), "second".to_string()),
        ("a.example/owner".to_string(), "first".to_string()),
    ]))
    .unwrap();
    assert_eq!(
        ownership.label_args(),
        vec![
            "--label",
            "a.example/owner=first",
            "--label",
            "z.example/owner=second"
        ]
    );
    assert_eq!(
        DockerMount::new("/host/path", "/container/path", true)
            .unwrap()
            .bind_argument(),
        "type=bind,src=/host/path,dst=/container/path,readonly"
    );
    assert!(DockerMount::new("/host/path", "/container/../escape", true).is_err());
    assert!(DockerMount::new("/host/path,attack", "/container/path", true).is_err());
    assert!(DockerMount::new("/host/path", "/container/path,attack", true).is_err());
}

#[test]
fn image_identity_recheck_rejects_tag_movement() {
    let first = format!("sha256:{}", "a".repeat(64));
    let second = format!("sha256:{}", "b".repeat(64));
    verify_image_identity(&first, &first).unwrap();
    assert!(verify_image_identity(&first, &second).is_err());
}

#[tokio::test]
async fn docker_removal_requires_exact_ownership_and_proves_absence() {
    let root = temp_root("docker-ownership");
    let command = root.join("docker");
    let state = root.join("state");
    std::fs::write(&state, r#"{"a.example/owner":"first"}"#).unwrap();
    std::fs::write(
        &command,
        format!(
            "#!/bin/sh\nstate='{}'\nif [ \"$1\" = container ]; then [ -f \"$state\" ] || {{ echo 'No such container' >&2; exit 1; }}; cat \"$state\"; exit 0; fi\nif [ \"$1\" = rm ]; then rm -f \"$state\"; exit 0; fi\nexit 2\n",
            state.display()
        ),
    )
    .unwrap();
    make_executable(&command);
    let docker = DockerCli::new(&command);
    let expected = DockerOwnership::new(BTreeMap::from([(
        "a.example/owner".to_string(),
        "first".to_string(),
    )]))
    .unwrap();
    docker
        .remove_owned("owned-container", &expected, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(!state.exists());

    std::fs::write(&state, r#"{"a.example/owner":"other"}"#).unwrap();
    assert!(
        docker
            .remove_owned("owned-container", &expected, Duration::from_secs(3))
            .await
            .is_err()
    );
    assert!(state.exists());

    std::fs::write(
        &state,
        r#"{"a.example/owner":"first","a.example/extra":"unexpected"}"#,
    )
    .unwrap();
    assert!(
        docker
            .remove_owned("owned-container", &expected, Duration::from_secs(3))
            .await
            .is_err()
    );
    assert!(state.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn process_execution_persists_streams_and_reports_containment() {
    let root = temp_root("process-output");
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let request = ExecutionRequest {
        command: PathBuf::from("sh"),
        args: vec![
            "-lc".into(),
            "printf 'out secret\\n'; printf 'err secret\\n' >&2".into(),
        ],
        current_dir: root.clone(),
        environment: BTreeMap::new(),
        artifacts: ArtifactPaths::in_directory(&root),
        deadline: Deadline::new(Some(5)).unwrap(),
        cancellation: Cancellation::new(cancel_rx),
        append: false,
        redaction_values: vec!["secret".into()],
        output_limits: OutputLimits {
            stdout_bytes: 1024,
            stderr_bytes: 1024,
            transcript_bytes: 4096,
        },
        effects: None,
        operation_identity: None,
    };
    drop(cancel_tx);
    let result = execute_process(request).await.unwrap();
    assert_eq!(result.status, ExecutionStatus::Exited(0));
    assert_eq!(
        result.containment.process,
        ContainmentStatus::VerifiedAbsent
    );
    assert_eq!(
        std::fs::read_to_string(root.join("stdout.log")).unwrap(),
        "out [REDACTED]\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("stderr.log")).unwrap(),
        "err [REDACTED]\n"
    );
    let transcript = std::fs::read_to_string(root.join("transcript.jsonl")).unwrap();
    assert!(transcript.contains(r#""stream":"stdout""#));
    assert!(transcript.contains(r#""stream":"stderr""#));
    assert!(!transcript.contains("secret"));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cancellation_contains_the_entire_process_group() {
    let root = temp_root("process-cancellation");
    let pid_path = root.join("descendant.pid");
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let request = ExecutionRequest {
        command: PathBuf::from("sh"),
        args: vec![
            "-lc".into(),
            format!("sleep 30 & echo $! > '{}'; wait", pid_path.display()),
        ],
        current_dir: root.clone(),
        environment: BTreeMap::new(),
        artifacts: ArtifactPaths::in_directory(&root),
        deadline: Deadline::new(Some(5)).unwrap(),
        cancellation: Cancellation::new(cancel_rx),
        append: false,
        redaction_values: Vec::new(),
        output_limits: OutputLimits {
            stdout_bytes: 1024,
            stderr_bytes: 1024,
            transcript_bytes: 4096,
        },
        effects: None,
        operation_identity: None,
    };
    let execution = tokio::spawn(execute_process(request));
    for _ in 0..100 {
        if pid_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancel_tx.send(true).unwrap();
    let result = execution.await.unwrap().unwrap();
    assert_eq!(result.status, ExecutionStatus::Cancelled);
    assert_eq!(
        result.containment.process,
        ContainmentStatus::VerifiedAbsent
    );
    let descendant = std::fs::read_to_string(&pid_path).unwrap();
    assert!(!PathBuf::from(format!("/proc/{}", descendant.trim())).exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn output_overflow_kills_execution_and_keeps_files_bounded() {
    let root = temp_root("process-output-limit");
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let result = execute_process(ExecutionRequest {
        command: PathBuf::from("sh"),
        args: vec![
            "-lc".into(),
            "while :; do printf '1234567890\\n'; done".into(),
        ],
        current_dir: root.clone(),
        environment: BTreeMap::new(),
        artifacts: ArtifactPaths::in_directory(&root),
        deadline: Deadline::new(Some(5)).unwrap(),
        cancellation: Cancellation::new(cancel_rx),
        append: false,
        redaction_values: Vec::new(),
        output_limits: OutputLimits {
            stdout_bytes: 64,
            stderr_bytes: 64,
            transcript_bytes: 4096,
        },
        effects: None,
        operation_identity: None,
    })
    .await
    .unwrap();
    assert_eq!(result.status, ExecutionStatus::OutputLimitExceeded);
    assert!(std::fs::metadata(root.join("stdout.log")).unwrap().len() <= 64);
    assert!(
        std::fs::metadata(root.join("transcript.jsonl"))
            .unwrap()
            .len()
            <= 4096
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn huge_newline_free_output_is_killed_with_bounded_valid_jsonl() {
    let root = temp_root("process-newline-free-limit");
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let result = execute_process(ExecutionRequest {
        command: PathBuf::from("sh"),
        args: vec![
            "-lc".into(),
            "while :; do printf '0123456789abcdef'; done".into(),
        ],
        current_dir: root.clone(),
        environment: BTreeMap::new(),
        artifacts: ArtifactPaths::in_directory(&root),
        deadline: Deadline::new(Some(5)).unwrap(),
        cancellation: Cancellation::new(cancel_rx),
        append: false,
        redaction_values: Vec::new(),
        output_limits: OutputLimits {
            stdout_bytes: 32 * 1024,
            stderr_bytes: 1024,
            transcript_bytes: 128 * 1024,
        },
        effects: None,
        operation_identity: None,
    })
    .await
    .unwrap();
    assert_eq!(result.status, ExecutionStatus::OutputLimitExceeded);
    assert!(std::fs::metadata(root.join("stdout.log")).unwrap().len() <= 32 * 1024);
    let transcript = std::fs::read(root.join("transcript.jsonl")).unwrap();
    assert!(transcript.len() <= 128 * 1024);
    for record in transcript
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: serde_json::Value = serde_json::from_slice(record).unwrap();
        assert_eq!(record["partial"], true);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn huge_newline_free_subprocess_output_is_rejected_before_growth() {
    let result = execute_output_cancellable(
        std::path::Path::new("sh"),
        &[
            "-c".to_string(),
            "dd if=/dev/zero bs=2097152 count=1 2>/dev/null | tr '\\000' x".to_string(),
        ],
        &Deadline::new(Some(5)).unwrap(),
        &Cancellation::never(),
        "huge bounded subprocess",
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn cancelled_primary_late_create_is_recovered_after_restart_and_quiescence() {
    let root = temp_root("primary-late-create");
    let command = root.join("docker");
    let state = root.join("container-state");
    let submitted = root.join("submitted");
    std::fs::write(
        &command,
        format!(
            "#!/bin/sh\nstate='{}'\nif [ \"$1\" = run ]; then touch '{}'; setsid sh -c \"sleep 2.5; printf '%s' '{{\\\"owner\\\":\\\"test\\\"}}' > '$state'\" >/dev/null 2>&1 & sleep 30; fi\nif [ \"$1\" = container ]; then [ -f \"$state\" ] || {{ echo 'No such container' >&2; exit 1; }}; cat \"$state\"; exit 0; fi\nif [ \"$1\" = rm ]; then rm -f \"$state\"; exit 0; fi\nexit 2\n",
            state.display(),
            submitted.display(),
        ),
    )
    .unwrap();
    make_executable(&command);
    let effects_root = root.join("effects");
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let effects =
        ExecutionCancellationContext::new(&effects_root, Cancellation::new(cancel_rx.clone()))
            .unwrap();
    drop(
        effects
            .begin_external_operation(
                "primary_docker_mutation",
                "container=late-primary;owner=test",
            )
            .unwrap(),
    );
    let execution = tokio::spawn(execute_process(ExecutionRequest {
        command: command.clone(),
        args: vec!["run".into()],
        current_dir: root.clone(),
        environment: BTreeMap::new(),
        artifacts: ArtifactPaths::in_directory(&root),
        deadline: Deadline::new(Some(5)).unwrap(),
        cancellation: Cancellation::new(cancel_rx),
        append: false,
        redaction_values: Vec::new(),
        output_limits: OutputLimits {
            stdout_bytes: 1024,
            stderr_bytes: 1024,
            transcript_bytes: 4096,
        },
        effects: Some(effects.clone()),
        operation_identity: Some("container=late-primary;owner=test".into()),
    }));
    wait_for_path(&submitted).await;
    cancel_tx.send(true).unwrap();
    assert_eq!(
        execution.await.unwrap().unwrap().status,
        ExecutionStatus::Cancelled
    );
    drop(effects);
    let recovered =
        ExecutionCancellationContext::new(&effects_root, Cancellation::never()).unwrap();
    let reconciliation_started = std::time::Instant::now();
    DockerCli::new(&command)
        .with_effects(recovered.clone())
        .with_absence_quiescence(Duration::from_secs(2))
        .with_ambiguous_quiescence(Duration::from_secs(3))
        .remove_owned(
            "late-primary",
            &DockerOwnership::new(BTreeMap::from([("owner".into(), "test".into())])).unwrap(),
            Duration::from_secs(8),
        )
        .await
        .unwrap();
    assert!(reconciliation_started.elapsed() >= Duration::from_secs(5));
    recovered.finalize().unwrap();
    assert!(!state.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cancelled_egress_late_creates_recover_all_resource_kinds_after_restart() {
    let root = temp_root("egress-late-create");
    let command = root.join("docker");
    let namespace = "late-egress";
    let names = [
        format!("agent-hub-executor-{namespace}-proxy"),
        format!("agent-hub-executor-{namespace}-resolver"),
        format!("agent-hub-executor-{namespace}-internal"),
        format!("agent-hub-executor-{namespace}-external"),
    ];
    let submitted = root.join("submitted");
    std::fs::write(
        &command,
        format!(
            r#"#!/bin/sh
root='{}'
namespace='{namespace}'
if [ "$1" = late-create ]; then
  touch '{}'
  setsid sh -c 'sleep 0.2; for name in {}; do touch "$1/$name"; done' sh "$root" >/dev/null 2>&1 &
  sleep 30
fi
kind="$1"
action="$2"
name="$3"
state="$root/$name"
if [ "$action" = inspect ]; then
  [ -f "$state" ] || {{ echo 'not found' >&2; exit 1; }}
  if [ "$kind" = network ]; then resource_kind=network; else case "$name" in *-proxy) resource_kind=proxy;; *) resource_kind=resolver;; esac; fi
  if [ "$kind" = container ]; then
    printf '[{{"Config":{{"Labels":{{"agent-hub.reddit.com/executor-egress":"true","agent-hub.reddit.com/executor-namespace":"%s","agent-hub.reddit.com/executor-kind":"%s"}}}}}}]' "$namespace" "$resource_kind"
  else
    printf '[{{"Labels":{{"agent-hub.reddit.com/executor-egress":"true","agent-hub.reddit.com/executor-namespace":"%s","agent-hub.reddit.com/executor-kind":"%s"}}}}]' "$namespace" "$resource_kind"
  fi
  exit 0
fi
if [ "$1" = rm ]; then rm -f "$root/$4"; exit 0; fi
if [ "$1" = network ] && [ "$2" = rm ]; then rm -f "$root/$3"; exit 0; fi
exit 2
"#,
            root.display(),
            submitted.display(),
            names.join(" "),
        ),
    )
    .unwrap();
    make_executable(&command);
    let effects_root = root.join("effects").join(namespace);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let effects =
        ExecutionCancellationContext::new(&effects_root, Cancellation::new(cancel_rx)).unwrap();
    for (kind, name) in [
        ("egress_resolver_create_request", names[1].as_str()),
        ("egress_network_create_request", names[2].as_str()),
        ("egress_network_create_request", names[3].as_str()),
    ] {
        effects
            .begin_external_operation(kind, name)
            .unwrap()
            .completed()
            .unwrap();
    }
    let command_task = command.clone();
    let effects_task = effects.clone();
    let operation = tokio::spawn(async move {
        execute_output_journalled(
            &command_task,
            &["late-create".to_string()],
            &Deadline::new(Some(5)).unwrap(),
            &effects_task,
            "egress_proxy_create_request",
            &format!("agent-hub-executor-{namespace}-proxy"),
            JournalledEffect::Mutation,
            "submit late egress creates",
        )
        .await
    });
    wait_for_path(&submitted).await;
    cancel_tx.send(true).unwrap();
    assert!(operation.await.unwrap().is_err());
    drop(effects);
    agent_hub_executor::egress::recover_restricted_egress(
        &command,
        &root,
        namespace,
        &Deadline::new(Some(45)).unwrap(),
    )
    .await
    .unwrap();
    assert!(names.iter().all(|name| !root.join(name).exists()));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn unknown_effect_cannot_be_blanket_reconciled_or_finalized() {
    let root = temp_root("unknown-effect");
    let effects = ExecutionCancellationContext::new(&root, Cancellation::never()).unwrap();
    effects
        .begin_external_operation("unknown_external_mutation", "unknown-resource")
        .unwrap()
        .completed()
        .unwrap();
    effects.reconcile_owned_effects(&root).await.unwrap();
    assert!(effects.finalize().is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn unknown_helper_name_is_not_recovered_by_suffix() {
    let root = temp_root("unknown-helper-effect");
    let effects = ExecutionCancellationContext::new(&root, Cancellation::never()).unwrap();
    effects
        .begin_external_operation("unknown_helper_process", "not-a-process-identity")
        .unwrap()
        .completed()
        .unwrap();
    effects.settle_process_helpers().await.unwrap();
    assert!(effects.finalize().is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn persisted_active_helper_is_terminated_before_reconciliation() {
    let root = temp_root("active-helper-effect");
    let effects = ExecutionCancellationContext::new(&root, Cancellation::never()).unwrap();
    let mut command = tokio::process::Command::new("sleep");
    command.arg("30");
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().unwrap();
    let identity =
        agent_hub_executor::read_process_identity(i32::try_from(child.id().unwrap()).unwrap())
            .unwrap();
    effects
        .begin_external_operation(
            "docker_mutation_helper_process",
            &format!(
                "pid={};pgid={};boot={};start={}",
                identity.pid, identity.pgid, identity.linux_boot_id, identity.proc_start_time_ticks
            ),
        )
        .unwrap()
        .completed()
        .unwrap();
    effects.settle_process_helpers().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    effects.finalize().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn helper_settlement_continues_after_malformed_identity() {
    let root = temp_root("aggregate-helper-settlement");
    let effects = ExecutionCancellationContext::new(&root, Cancellation::never()).unwrap();
    effects
        .begin_external_operation("docker_mutation_helper_process", "malformed-first-helper")
        .unwrap()
        .completed()
        .unwrap();
    let mut command = tokio::process::Command::new("sleep");
    command.arg("30");
    command.as_std_mut().process_group(0);
    let mut child = command.spawn().unwrap();
    let identity =
        agent_hub_executor::read_process_identity(i32::try_from(child.id().unwrap()).unwrap())
            .unwrap();
    effects
        .begin_external_operation(
            "journalled_subprocess_helper_process",
            &format!(
                "pid={};pgid={};boot={};start={}",
                identity.pid, identity.pgid, identity.linux_boot_id, identity.proc_start_time_ticks
            ),
        )
        .unwrap()
        .completed()
        .unwrap();
    let error = effects.settle_process_helpers().await.unwrap_err();
    assert!(error.to_string().contains("docker_mutation_helper_process"));
    tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(effects.finalize().is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn retried_docker_inspect_reconciles_crashed_request_effect() {
    let root = temp_root("retry-inspect-effect");
    let docker = root.join("docker");
    std::fs::write(
        &docker,
        "#!/bin/sh\nif [ \"$1\" = container ] && [ \"$2\" = inspect ]; then echo 'No such container' >&2; exit 1; fi\nexit 2\n",
    )
    .unwrap();
    make_executable(&docker);
    let effects =
        ExecutionCancellationContext::new(&root.join("effects"), Cancellation::never()).unwrap();
    drop(
        effects
            .begin_external_operation("docker_container_inspect", "owned-container")
            .unwrap(),
    );
    DockerCli::new(&docker)
        .with_effects(effects.clone())
        .with_absence_quiescence(Duration::from_millis(100))
        .remove_owned(
            "owned-container",
            &DockerOwnership::new(BTreeMap::from([("owner".into(), "test".into())])).unwrap(),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    effects.finalize().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn descendant_held_pipes_are_bounded_by_operation_deadline() {
    let root = temp_root("descendant-held-pipes");
    let pid_file = root.join("descendant.pid");
    let effects = ExecutionCancellationContext::new(&root, Cancellation::never()).unwrap();
    let started = Instant::now();
    let result = execute_output_journalled(
        std::path::Path::new("/bin/sh"),
        &[
            "-c".to_string(),
            "setsid sh -c 'echo $$ > \"$1\"; sleep 30' descendant \"$1\" & exit 0".to_string(),
            "outer".to_string(),
            pid_file.to_string_lossy().into_owned(),
        ],
        &Deadline::new(Some(1)).unwrap(),
        &effects,
        "egress_proxy_create_request",
        "pipe-holder",
        JournalledEffect::Mutation,
        "descendant-held pipe fixture",
    )
    .await;
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(
        effects.required_resource_quiescence(
            "pipe-holder",
            Duration::from_secs(2),
            Duration::from_secs(30),
        ),
        Duration::from_secs(30)
    );
    assert!(effects.finalize().is_err());
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn effect_recovery_removes_exact_credential_file_before_reconciling() {
    let root = temp_root("effect-credential-recovery");
    let credentials = root.join("credentials");
    std::fs::create_dir(&credentials).unwrap();
    let credential = credentials.join("owned.secret");
    std::fs::write(&credential, "secret").unwrap();
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let effects =
        ExecutionCancellationContext::new(&root.join("effects"), Cancellation::new(cancel_rx))
            .unwrap();
    let unresolved = effects
        .begin_external_operation("credential_secret_write", &credential.to_string_lossy())
        .unwrap();
    drop(unresolved);
    effects.reconcile_owned_effects(&credentials).await.unwrap();
    effects.finalize().unwrap();
    assert!(!credential.exists());
    assert!(root.join("effects/executor-effects.jsonl").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn effect_recovery_removes_owned_credential_directories_deepest_first() {
    let root = temp_root("effect-credential-directory-recovery");
    let credentials = root.join("data");
    let auth_dir = credentials.join("opencode");
    let nested_dir = auth_dir.join("nested");
    std::fs::create_dir_all(&nested_dir).unwrap();
    let credential = nested_dir.join("auth.json");
    std::fs::write(&credential, "secret").unwrap();
    let effects =
        ExecutionCancellationContext::new(&root.join("effects"), Cancellation::never()).unwrap();
    effects
        .begin_external_operation("credential_directory_create", &auth_dir.to_string_lossy())
        .unwrap()
        .completed()
        .unwrap();
    effects
        .begin_external_operation("credential_directory_create", &nested_dir.to_string_lossy())
        .unwrap()
        .completed()
        .unwrap();
    effects
        .begin_external_operation("credential_secret_write", &credential.to_string_lossy())
        .unwrap()
        .completed()
        .unwrap();

    effects.reconcile_owned_effects(&credentials).await.unwrap();
    effects.finalize().unwrap();
    assert!(!auth_dir.exists());
    assert!(credentials.is_dir());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn effect_recovery_hard_fails_on_unknown_credential_material_then_retries() {
    let root = temp_root("effect-credential-directory-retry");
    let credentials = root.join("data");
    let auth_dir = credentials.join("opencode");
    std::fs::create_dir_all(&auth_dir).unwrap();
    let credential = auth_dir.join("auth.json");
    let unknown = auth_dir.join("unknown");
    std::fs::write(&credential, "secret").unwrap();
    std::fs::write(&unknown, "not-owned").unwrap();
    let effects =
        ExecutionCancellationContext::new(&root.join("effects"), Cancellation::never()).unwrap();
    effects
        .begin_external_operation("credential_directory_create", &auth_dir.to_string_lossy())
        .unwrap()
        .completed()
        .unwrap();
    effects
        .begin_external_operation("credential_secret_write", &credential.to_string_lossy())
        .unwrap()
        .completed()
        .unwrap();

    let error = effects
        .reconcile_owned_effects(&credentials)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown material"));
    assert!(!credential.exists());
    assert!(auth_dir.is_dir());
    assert!(effects.finalize().is_err());

    std::fs::remove_file(unknown).unwrap();
    effects.reconcile_owned_effects(&credentials).await.unwrap();
    effects.finalize().unwrap();
    assert!(!auth_dir.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn effect_recovery_removes_owned_opencode_runtime_data_without_following_symlinks() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = temp_root("effect-opencode-runtime-data-recovery");
    let data = root.join("data");
    let runtime_data = data.join("opencode");
    let caller_target = root.join("caller-target");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&runtime_data).unwrap();
    std::fs::create_dir(&caller_target).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&runtime_data, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(caller_target.join("untouched"), "caller-owned").unwrap();
    for relative in [
        "opencode.db",
        "opencode.db-wal",
        "log/opencode.log",
        "snapshot/state.json",
        "repos/repository.json",
    ] {
        let path = runtime_data.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "runtime-state").unwrap();
    }
    symlink(&caller_target, runtime_data.join("repos/caller-link")).unwrap();
    let auth = runtime_data.join("auth.json");
    std::fs::write(&auth, "secret").unwrap();

    let effects =
        ExecutionCancellationContext::new(&root.join("effects"), Cancellation::never()).unwrap();
    effects
        .begin_external_operation(
            "opencode_runtime_data_directory_create",
            &runtime_data.to_string_lossy(),
        )
        .unwrap()
        .completed()
        .unwrap();
    effects
        .begin_external_operation("credential_secret_write", &auth.to_string_lossy())
        .unwrap()
        .completed()
        .unwrap();

    effects
        .reconcile_owned_credentials(&data, Some(&runtime_data))
        .unwrap();
    assert!(!auth.exists());
    assert!(runtime_data.join("opencode.db").is_file());
    effects
        .reconcile_owned_runtime_data(&data, &runtime_data)
        .unwrap();
    effects.finalize().unwrap();

    assert!(!runtime_data.exists());
    assert_eq!(
        std::fs::read_to_string(caller_target.join("untouched")).unwrap(),
        "caller-owned"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn effect_recovery_reclassifies_only_the_exact_legacy_opencode_runtime_root() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = temp_root("effect-legacy-opencode-runtime-data-recovery");
    let data = root.join("data");
    let runtime_data = data.join("opencode");
    let unrelated = data.join("other");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&runtime_data).unwrap();
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&runtime_data, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(runtime_data.join("opencode.db"), "runtime-state").unwrap();
    std::fs::write(unrelated.join("unknown"), "not-runtime-data").unwrap();
    let effects =
        ExecutionCancellationContext::new(&root.join("effects"), Cancellation::never()).unwrap();
    for path in [&runtime_data, &unrelated] {
        effects
            .begin_external_operation("credential_directory_create", &path.to_string_lossy())
            .unwrap()
            .completed()
            .unwrap();
    }

    let error = effects
        .reconcile_owned_credentials(&data, Some(&runtime_data))
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown material"));
    assert!(runtime_data.is_dir());
    assert!(unrelated.is_dir());

    std::fs::remove_file(unrelated.join("unknown")).unwrap();
    effects
        .reconcile_owned_credentials(&data, Some(&runtime_data))
        .unwrap();
    effects
        .reconcile_owned_runtime_data(&data, &runtime_data)
        .unwrap();
    effects.finalize().unwrap();
    assert!(!runtime_data.exists());
    assert!(!unrelated.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn effect_recovery_rejects_runtime_data_effect_outside_the_exact_authorized_root() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = temp_root("effect-opencode-runtime-data-containment");
    let data = root.join("data");
    let runtime_data = data.join("opencode");
    let caller_target = root.join("caller-target");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&runtime_data).unwrap();
    std::fs::create_dir(&caller_target).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&runtime_data, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(caller_target.join("untouched"), "caller-owned").unwrap();
    let effects =
        ExecutionCancellationContext::new(&root.join("effects"), Cancellation::never()).unwrap();
    effects
        .begin_external_operation(
            "opencode_runtime_data_directory_create",
            &caller_target.to_string_lossy(),
        )
        .unwrap()
        .completed()
        .unwrap();

    let error = effects
        .reconcile_owned_runtime_data(&data, &runtime_data)
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match its authorized root"));
    assert_eq!(
        std::fs::read_to_string(caller_target.join("untouched")).unwrap(),
        "caller-owned"
    );
    assert!(runtime_data.is_dir());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cleanup_failure_is_explicit() {
    let root = temp_root("cleanup-status");
    let not_a_file = root.join("directory");
    std::fs::create_dir(&not_a_file).unwrap();
    assert!(matches!(
        cleanup_temporary_files(&[not_a_file]),
        CleanupStatus::Failed(_)
    ));
    std::fs::remove_dir_all(root).unwrap();
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn make_executable(path: &std::path::Path) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

async fn wait_for_path(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {}", path.display());
}
