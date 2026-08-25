use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use agent_hub_executor::{
    ArtifactPaths, Cancellation, CleanupStatus, ContainmentStatus, Deadline, DockerCli,
    DockerExecutionPlan, DockerMount, DockerOwnership, ExecutionRequest, ExecutionStatus,
    NetworkMode, cleanup_temporary_files, execute_process, verify_image_identity,
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
        .remove_owned("owned-container", &expected, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(!state.exists());

    std::fs::write(&state, r#"{"a.example/owner":"other"}"#).unwrap();
    assert!(
        docker
            .remove_owned("owned-container", &expected, Duration::from_secs(2))
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
            .remove_owned("owned-container", &expected, Duration::from_secs(2))
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
