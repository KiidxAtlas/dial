use std::process::Command;

#[test]
fn build_identity_is_available_without_project_config_and_does_not_change_at_runtime() {
    let binary = env!("CARGO_BIN_EXE_dial");
    let dir = tempfile::tempdir().unwrap();
    let run = |override_revision: &str| {
        let output = Command::new(binary)
            .arg("--build-info")
            .current_dir(dir.path())
            .env("DIAL_HOME", dir.path().join("unused-home"))
            .env("DIAL_BUILD_REVISION", override_revision)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let first = run("aaaaaaaaaaaa");
    let second = run("bbbbbbbbbbbb");
    assert_eq!(
        first, second,
        "runtime variables must not replace the executable's compiled identity"
    );
    assert_eq!(first["version"], env!("CARGO_PKG_VERSION"));
    assert!(!first["revision"].as_str().unwrap().is_empty());
    assert_eq!(
        std::path::Path::new(first["executable"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        std::path::Path::new(binary).canonicalize().unwrap()
    );
    assert!(
        !dir.path().join("unused-home").exists(),
        "build inspection must not load or create runtime configuration"
    );
    let version = Command::new(binary).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(
        String::from_utf8(version.stdout)
            .unwrap()
            .contains(first["revision"].as_str().unwrap())
    );
}
