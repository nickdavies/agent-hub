#[cfg(unix)]
#[test]
fn catalog_process_uses_only_pinned_config_and_never_invokes_runtime_helpers() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::path::Path;

    let root = std::env::temp_dir().join(format!(
        "agent-hub-catalog-read-only-{}",
        uuid::Uuid::new_v4()
    ));
    let config_root = root.join("config");
    let environment_names = [
        "code-auto",
        "code-supervised",
        "infra-supervised",
        "research-auto",
        "research-web-auto",
    ];
    let image = config_root.join("images/job-code/image.toml");
    let system = config_root.join("system.toml");
    let helper = root.join("cat-command");
    let marker = root.join("runtime-helper-was-invoked");
    let host_config = root.join("host-run.json");

    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    std::fs::write(
        &system,
        format!(
            "[system]\noperator='test'\nbot_identity='test'\n[system.environments]\ninstalled=[{}]\ntriage_environment='code-auto'\n[system.images]\ninstalled=['job-code']\n",
            environment_names.map(|name| format!("'{name}'")).join(",")
        ),
    )
    .unwrap();
    let environments = environment_names.map(|name| {
        let path = config_root.join(format!("environments/{name}/environment.toml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("[environment]\nname='{name}'\ndescription='Purpose-specific {name} work'\nimage='agent-hub/job-code:2026-09-09'\nnetwork_policy='none'\napproval_mode='auto'\n[environment.discovery]\nrecommended_for=['Use {name} for its configured workflow']\nselection_notes=['Choose only when the task matches this environment']\n"),
        )
        .unwrap();
        path
    });
    std::fs::write(
        &image,
        "[image]\nname='job-code'\ntag='agent-hub/job-code:2026-09-09'\ndescription='Pinned tools'\n",
    )
    .unwrap();
    std::fs::write(
        &helper,
        format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&helper, permissions).unwrap();

    let pin = |path: &Path, relative: &str| {
        let bytes = std::fs::read(path).unwrap();
        let metadata = std::fs::metadata(path).unwrap();
        let digest = protocol::sha256_hex(&bytes);
        serde_json::json!({
            "path": path,
            "sha256": digest,
            "source_identity": format!("config:{relative}:sha256:{digest}"),
            "device": metadata.dev(),
            "inode": metadata.ino(),
        })
    };
    let config_pins = environments
        .iter()
        .zip(environment_names)
        .map(|(path, name)| pin(path, &format!("environments/{name}/environment.toml")))
        .chain([
            pin(&image, "images/job-code/image.toml"),
            pin(&system, "system.toml"),
        ])
        .collect::<Vec<_>>();
    let config_metadata = std::fs::metadata(&config_root).unwrap();
    std::fs::write(
        &host_config,
        serde_json::to_vec(&serde_json::json!({
            "schema": "agent_hub_run_host_v1",
            "config_root": config_root,
            "config_root_identity": {
                "canonical_path": config_root,
                "device": config_metadata.dev(),
                "inode": config_metadata.ino(),
            },
            "config_artifacts": config_pins,
            "self_binary": {"path": helper, "sha256": "0".repeat(64), "source_identity": "unused", "device": 0, "inode": 0},
            "gateway": {"path": helper, "sha256": "0".repeat(64), "source_identity": "unused", "device": 0, "inode": 0},
            "policy": {"path": helper, "sha256": "0".repeat(64), "source_identity": "unused", "device": 0, "inode": 0},
            "image_digests": environment_names.into_iter().map(|name| (name, format!("sha256:{}", "a".repeat(64)))).collect::<std::collections::BTreeMap<_, _>>(),
        }))
        .unwrap(),
    )
    .unwrap();

    let mut elapsed = Vec::new();
    let mut outputs = Vec::new();
    for _ in 0..2 {
        let started = std::time::Instant::now();
        outputs.push(
            std::process::Command::new(env!("CARGO_BIN_EXE_agent-hub"))
                .args(["local", "catalog", "--host-config"])
                .arg(&host_config)
                .env("HOME", root.join("empty-home"))
                .output()
                .unwrap(),
        );
        elapsed.push(started.elapsed());
    }
    let output = &outputs[0];
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let catalog: protocol::AgentHubCatalogV1 = serde_json::from_slice(&output.stdout).unwrap();
    catalog.validate().unwrap();
    assert_eq!(catalog.environments.len(), 5);
    assert_eq!(catalog.environments[0].name, "code-auto");
    println!(
        "agent-hub-catalog-process-measurement command='target/debug/agent-hub local catalog --host-config <synthetic>' profile=debug iterations=2 cold_ms={:.3} repeat_ms={:.3} payload_bytes={} environments=5",
        elapsed[0].as_secs_f64() * 1000.0,
        elapsed[1].as_secs_f64() * 1000.0,
        output.stdout.len(),
    );
    assert!(!marker.exists(), "catalog invoked a runtime helper");
    assert!(
        !root.join("empty-home/.agent-hub").exists(),
        "catalog wrote runtime state"
    );

    std::fs::remove_dir_all(root).unwrap();
}
