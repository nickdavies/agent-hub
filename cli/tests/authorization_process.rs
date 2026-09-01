use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const IMAGE_TAG: &str = "agent-hub/job-code:2026-08-27";

#[test]
fn authorization_retries_preflight_and_fences_concurrent_attempts() {
    let root = temp_root();
    let config = root.join("config");
    let workspace = root.join("workspace");
    let state = root.join("state");
    let authorization = root.join("authorization.json");
    let docker_marker = root.join("docker.calls");
    let fake_bin = root.join("bin");
    std::fs::create_dir(&workspace).unwrap();
    initialize_repository(&workspace);
    std::fs::create_dir(&fake_bin).unwrap();
    write_config(&config, &workspace);
    write_fake_docker(&fake_bin.join("docker"), &docker_marker, &workspace, &root);

    let proposal = invoke(
        &root,
        &fake_bin,
        &state,
        &[
            "local",
            "propose",
            "--config-root",
            config.to_str().unwrap(),
            "--environment",
            "test",
            "--image-family",
            "job-code",
            "--network-class",
            "none",
            "--tool-approval",
            "auto",
            "--workspace",
            &format!("primary={}:rw", workspace.display()),
            "--primary-workspace",
            "primary",
            "--timeout-secs",
            "30",
            "--prompt-stdin",
            "--output",
            authorization.to_str().unwrap(),
        ],
        Some("Return success"),
    );
    assert_success(&proposal);
    let stdout = String::from_utf8(proposal.stdout).unwrap();
    let digest = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Authorization digest: "))
        .unwrap();
    std::fs::remove_file(&docker_marker).unwrap();

    let run_args = [
        "local",
        "run",
        "--environment",
        "test",
        "--image-family",
        "job-code",
        "--network-class",
        "none",
        "--tool-approval",
        "auto",
        "--timeout-secs",
        "30",
        "--authorization",
        authorization.to_str().unwrap(),
        "--digest",
        digest,
    ];
    let unauthorized = invoke(&root, &fake_bin, &state, &run_args, None);
    assert!(!unauthorized.status.success());
    assert!(!docker_marker.exists(), "unauthorized run invoked Docker");
    assert!(!state.exists(), "unauthorized run created attended state");

    let authorize = invoke(
        &root,
        &fake_bin,
        &state,
        &[
            "local",
            "authorize",
            "--environment",
            "test",
            "--image-family",
            "job-code",
            "--network-class",
            "none",
            "--tool-approval",
            "auto",
            "--timeout-secs",
            "30",
            "--authorization",
            authorization.to_str().unwrap(),
            "--digest",
            digest,
        ],
        None,
    );
    assert_success(&authorize);
    let receipt: serde_json::Value = serde_json::from_slice(&authorize.stdout).unwrap();
    assert_eq!(receipt["schema"], "attended_authorization_receipt_v1");
    assert_eq!(receipt["digest"], digest);
    assert_eq!(receipt["retry_eligible"], true);
    assert!(
        receipt["authorization_id"]
            .as_str()
            .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok())
    );
    assert!(
        receipt["canonical_run_command"]
            .as_str()
            .is_some_and(|command| command == format!("agent-hub {}", run_args.join(" ")))
    );
    std::fs::remove_file(&docker_marker).unwrap();

    std::fs::write(root.join("fail-preflight-once"), "fail").unwrap();
    let preflight_failure = invoke(&root, &fake_bin, &state, &run_args, None);
    assert!(!preflight_failure.status.success());
    let first: serde_json::Value = serde_json::from_slice(&preflight_failure.stdout).unwrap();
    assert_eq!(first["launch_state"], "terminal");
    assert_eq!(first["retry_class"], "prelaunch");
    assert_eq!(first["retry_eligible"], true);

    std::fs::write(root.join("slow-launch"), "slow").unwrap();
    let (executed, duplicate) = std::thread::scope(|scope| {
        let first = scope.spawn(|| invoke(&root, &fake_bin, &state, &run_args, None));
        std::thread::sleep(std::time::Duration::from_millis(150));
        let duplicate = invoke(&root, &fake_bin, &state, &run_args, None);
        (first.join().unwrap(), duplicate)
    });
    assert_success(&executed);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already consumed"));
    let second: serde_json::Value = serde_json::from_slice(&executed.stdout).unwrap();
    assert_eq!(first["authorization_id"], second["authorization_id"]);
    assert_eq!(first["receipt_id"], second["receipt_id"]);
    assert_ne!(first["attempt_id"], second["attempt_id"]);
    assert_ne!(first["run_id"], second["run_id"]);
    assert_eq!(first["attempt_number"], 1);
    assert_eq!(second["attempt_number"], 2);
    assert_eq!(second["parent_attempt_id"], first["attempt_id"]);
    assert_eq!(second["attempt_ancestry"][0], first["attempt_id"]);
    assert_eq!(
        first["authorization_digest"],
        second["authorization_digest"]
    );
    assert!(docker_marker.exists());
    let replay = invoke(&root, &fake_bin, &state, &run_args, None);
    assert!(!replay.status.success());
    assert!(String::from_utf8_lossy(&replay.stderr).contains("already consumed"));
    std::fs::remove_dir_all(root).unwrap();
}

fn initialize_repository(path: &Path) {
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(path.join("tracked.txt"), "tracked\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&[
        "-c",
        "user.name=Agent Hub Test",
        "-c",
        "user.email=agent-hub@example.com",
        "commit",
        "-q",
        "-m",
        "initial",
    ]);
}

fn invoke(
    root: &Path,
    fake_bin: &Path,
    state: &Path,
    args: &[&str],
    stdin: Option<&str>,
) -> Output {
    use std::io::Write as _;
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-hub"));
    command
        .args(args)
        .current_dir(root)
        .env("AGENT_HUB_ATTENDED_STATE", state)
        .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(stdin) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_config(root: &Path, workspace: &Path) {
    std::fs::create_dir_all(root.join("network_policies/none")).unwrap();
    std::fs::create_dir_all(root.join("environments/test")).unwrap();
    std::fs::create_dir_all(root.join("images/job-code")).unwrap();
    std::fs::write(
        root.join("system.toml"),
        format!(
            "[system]\noperator='test'\nbot_identity='test'\n[system.repo_discovery]\nenabled=true\n[[system.repo_discovery.strategies]]\ntype='directory'\npath='{}'\ninclude=[]\nexclude=[]\n[system.environments]\ninstalled=['test']\ntriage_environment='test'\n[system.network_policies]\ninstalled=['none']\n[system.images]\ninstalled=['job-code']\n",
            workspace.display()
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("local-agent.json"),
        serde_json::to_vec(&serde_json::json!({
            "default_agent":{"kind":"generic","command":"true"},
            "triage_repos":[workspace],
            "approved_repository_roots":[workspace.parent().unwrap()],
            "triage_mounts":[{"host_path":workspace,"container_path":"/repos/workspace","read_only":true}],
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("network_policies/none/policy.toml"),
        "[policy]\nname='none'\ndescription='none'\ndocker_network='none'\n",
    )
    .unwrap();
    std::fs::write(
        root.join("environments/test/environment.toml"),
        format!("[environment]\nname='test'\ndescription='test'\nimage='{IMAGE_TAG}'\nnetwork_policy='none'\napproval_mode='auto'\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("environments/test/agent-hub-permissions.json"),
        "{}",
    )
    .unwrap();
    std::fs::write(root.join("environments/test/dippy.conf"), "allow true\n").unwrap();
    std::fs::write(
        root.join("environments/test/opencode.json"),
        r#"{"model":"openai/gpt-5.6-sol","small_model":"openai/gpt-5.6-sol","enabled_providers":["openai"],"permission":{"*":"allow"}}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("images/job-code/image.toml"),
        format!("[image]\nname='job-code'\ntag='{IMAGE_TAG}'\n"),
    )
    .unwrap();
}

fn write_fake_docker(path: &Path, marker: &Path, workspace: &Path, root: &Path) {
    use std::os::unix::fs::MetadataExt as _;
    let report = preflight_report(
        std::fs::metadata(workspace).unwrap().uid(),
        std::fs::metadata(workspace).unwrap().gid(),
    );
    let report_path = root.join("preflight.json");
    std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
    let result_event = serde_json::json!({
        "type": "text",
        "part": {
            "text": serde_json::json!({
                "schema": "agent_result_v1",
                "complete": true,
                "summary": "executed",
                "acceptance_results": [],
                "blockers": [],
                "artifact_references": []
            }).to_string()
        }
    })
    .to_string();
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1 $2\" = 'image inspect' ]; then printf '%s\\n' 'sha256:{}'; exit 0; fi\nif [ \"$1 $2\" = 'container inspect' ]; then printf '%s\\n' 'No such container' >&2; exit 1; fi\ncase \"$*\" in *agent-hub-job-code-preflight*) if [ -f '{}' ]; then rm '{}'; printf 'injected preflight failure\\n' >&2; exit 75; fi; cat '{}' ;; *) if [ -f '{}' ]; then sleep 1; fi; printf '%s\\n' '{}' ;; esac\n",
            marker.display(),
            "a".repeat(64),
            root.join("fail-preflight-once").display(),
            root.join("fail-preflight-once").display(),
            report_path.display(),
            root.join("slow-launch").display(),
            result_event,
        ),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn preflight_report(uid: u32, gid: u32) -> serde_json::Value {
    let tools = [
        "agent_hub_gateway",
        "cargo",
        "cc",
        "cmake",
        "corepack",
        "curl",
        "cxx",
        "git",
        "gh",
        "go",
        "jq",
        "make",
        "ninja",
        "node",
        "npm",
        "opencode",
        "pip",
        "pkg_config",
        "pnpm",
        "python",
        "rg",
        "rustc",
        "unbound",
        "unbound_checkconf",
        "uv",
        "yarn",
    ]
    .into_iter()
    .map(|name| (name.to_string(), serde_json::json!(format!("{name} 1.0"))))
    .collect::<serde_json::Map<_, _>>();
    let sources = [
        "agent_hub",
        "gh",
        "go",
        "node",
        "opencode",
        "pnpm",
        "rust_cargo",
        "rust_channel_manifest",
        "rust_std",
        "rustc",
        "uv",
        "yarn",
    ]
    .into_iter()
    .map(|name| {
        (
            name.to_string(),
            serde_json::json!({
                "url":format!("https://example.com/{name}-1.0.tar.xz"),
                "digest":{"algorithm":"sha256","value":"b".repeat(64)}
            }),
        )
    })
    .collect::<serde_json::Map<_, _>>();
    let manifest = serde_json::json!({
        "schema":"agent-hub.job-code-tools.v3",
        "image":IMAGE_TAG,
        "debian_snapshot":"20260824T000000Z",
        "runtime":{"rust_home":"/opt/rust","rust_bin":"/opt/rust/bin","cargo_home":"$HOME/.cargo"},
        "tools":tools,
        "provenance":{"packages":{"unbound":{"version":"1.17.1-2+deb12u4","snapshot":"20260824T000000Z","uid":100,"gid":101}},"sources":sources},
    });
    let contents = serde_json::to_string(&manifest).unwrap();
    serde_json::json!({
        "schema":"agent-hub.job-code-preflight.v3",
        "status":"ok",
        "image":{"resolved":format!("sha256:{}", "a".repeat(64))},
        "requested":{"uid":uid,"gid":gid},
        "runtime":{"uid":uid,"gid":gid},
        "manifest":{"path":config::JOB_CODE_TOOL_MANIFEST_PATH,"sha256":config::job_code_tool_manifest_sha256(&contents),"contents":contents},
    })
}

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("authorization-process-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    root
}
