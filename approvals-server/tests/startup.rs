#[cfg(unix)]
#[test]
fn starts_without_orchestration_configuration_and_saves_on_sigint() {
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "agent-hub-approvals-startup-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&directory).unwrap();
    let state_path = directory.join("server_data.json");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen_addr = listener.local_addr().unwrap();
    drop(listener);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_agent-hub-approvals-server"))
        .args([
            "serve",
            "noop",
            "local-file",
            "--path",
            state_path.to_str().unwrap(),
        ])
        .env("AUTH_MODE", "none")
        .env("APPROVAL_MODE", "readwrite")
        .env("BASE_URL", "http://localhost:8080")
        .env("LISTEN_ADDR", listen_addr.to_string())
        .env_remove("TASK_STATE_DIR")
        .env_remove("CONFIG_ROOT")
        .env_remove("WORKFLOWS_DIR")
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        assert!(
            child.try_wait().unwrap().is_none(),
            "server exited during startup"
        );
        if std::net::TcpStream::connect_timeout(&listen_addr, std::time::Duration::from_millis(50))
            .is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server did not become ready before timeout"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    assert!(child.wait().unwrap().success());

    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert!(state["sessions"].is_object());
    assert!(state["pending_approvals"].is_array());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn systemd_unit_preserves_legacy_install_contract() {
    let package =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../deploy/approvals-server");
    let unit = std::fs::read_to_string(package.join("agent-hub-approvals.service")).unwrap();
    assert!(unit.contains("/agent-hub-server serve noop local-file"));
    assert!(unit.contains("KillSignal=SIGINT"));
    assert!(unit.contains("server_data.json"));

    let deploy = std::fs::read_to_string(package.join("deploy.sh")).unwrap();
    assert!(deploy.contains("cargo build --locked --release -p agent-hub-approvals-server"));
    assert!(deploy.contains("release/agent-hub-approvals-server"));
    assert!(deploy.contains("/agent-hub-server\""));
    assert!(deploy.contains("--validate-legacy-state"));
    assert!(deploy.contains("/approvals/queue"));
    assert!(deploy.contains("/api/v1/sessions"));
}
