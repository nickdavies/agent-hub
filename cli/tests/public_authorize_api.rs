use std::path::PathBuf;

#[test]
fn dependent_crate_cannot_call_authorization_persistence() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/public-authorize-api");
    let target =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/public-authorize-api-negative");
    let lock = fixture.join("Cargo.lock");
    let _ = std::fs::remove_dir_all(&target);
    let _ = std::fs::remove_file(&lock);
    let output = std::process::Command::new(env!("CARGO"))
        .args(["check", "--quiet", "--manifest-path"])
        .arg(fixture.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("run dependent-crate compile check");
    let _ = std::fs::remove_dir_all(target);
    let _ = std::fs::remove_file(lock);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("StateStore"), "{stderr}");
    assert!(stderr.contains("AuthorizationRecord"), "{stderr}");
    assert!(
        stderr.contains("could not find") || stderr.contains("private module"),
        "{stderr}"
    );
}
