use crate::attended::read_artifact_bounded;
use crate::state::StateStore;

#[test]
fn state_rejects_non_private_existing_root() {
    let root = temp_root("permissions").join("state");
    std::fs::create_dir(&root).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(StateStore::open(root.clone()).is_err());
    }
    std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
}

#[test]
fn fresh_state_creates_runs_parent_and_bounds_artifact_reads() {
    let root = temp_root("bounded-read");
    let state = StateStore::open(root.join("state")).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    state
        .ensure_directory(std::path::Path::new("runs"))
        .unwrap();
    state
        .ensure_directory(&std::path::PathBuf::from("runs").join(&id))
        .unwrap();
    state
        .create_json(
            &std::path::PathBuf::from("runs")
                .join(&id)
                .join("result.json"),
            &serde_json::json!({"result":"abcdefghijklmnopqrstuvwxyz"}),
        )
        .unwrap();
    let bounded = read_artifact_bounded(&state, &id, "result.json", 8).unwrap();
    assert_eq!(bounded.returned_bytes, 8);
    assert!(bounded.truncated);
    assert!(bounded.total_bytes > bounded.returned_bytes);
    std::fs::remove_dir_all(root).unwrap();
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agent-hub-attended-{name}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&root).unwrap();
    root
}
