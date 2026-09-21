#[cfg(unix)]
#[test]
fn catalog_process_uses_only_published_config_and_never_invokes_runtime_helpers() {
    use std::os::unix::fs::PermissionsExt as _;

    const REFERENCE: &str = "agent-hub/job-code-qualification:4d596913-2cc2-4068-a842-3db326aa6f1c";
    const REVISION: &str = "test-revision-1";
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/test-scratch/2026-09-18")
        .join(format!("catalog-read-only-{}", uuid::Uuid::new_v4()));
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
    for name in environment_names {
        let path = config_root.join(format!("environments/{name}/environment.toml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("[environment]\nname='{name}'\ndescription='Purpose-specific {name} work'\nimage='{REFERENCE}'\nnetwork_policy='none'\napproval_mode='auto'\n[environment.discovery]\nrecommended_for=['Use {name} for its configured workflow']\nselection_notes=['Choose only when the task matches this environment']\n"),
        )
        .unwrap();
    }
    let identity = format!("sha256:{}", "a".repeat(64));
    let facts = serde_json::to_vec(&serde_json::json!({
        "schema": "agent_hub_image_tool_facts_v1",
        "image_reference": REFERENCE,
        "backend_metadata_revision": REVISION,
        "image_identity": identity,
        "tools": [{"name": "cargo", "version": "1.89.0"}],
    }))
    .unwrap();
    let facts_path = config_root.join("images/job-code/tool-facts.json");
    std::fs::write(&facts_path, &facts).unwrap();
    std::fs::write(
        &image,
        format!(
            "[image]\nname='job-code'\ntag='{REFERENCE}'\ndescription='Pinned tools'\n[image.tool_facts]\npath='images/job-code/tool-facts.json'\nsha256='{}'\nimage_reference='{REFERENCE}'\nbackend_metadata_revision='{REVISION}'\nimage_identity='{identity}'\n",
            protocol::sha256_hex(&facts)
        ),
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

    std::fs::write(
        &host_config,
        serde_json::to_vec(&serde_json::json!({
            "schema": "agent_hub_run_host_v2",
            "config_root": config_root,
            "gateway": {"path": helper},
            "policy": {"path": helper},
            "managed_images": {"job-code": REFERENCE},
            "backend_metadata_revision": REVISION,
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
    assert_eq!(catalog.environments[0].image_family.as_str(), "job-code");
    assert!(matches!(
        &catalog.images[0].tool_facts,
        protocol::AgentHubCatalogToolFactsV1::Available { tools }
            if tools.len() == 1 && tools[0].name == "cargo"
    ));
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
