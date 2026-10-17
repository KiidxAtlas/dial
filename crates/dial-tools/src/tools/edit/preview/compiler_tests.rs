use super::*;
#[test]
fn compiler_preview_reports_type_failure_and_preserves_project() {
    if !crate::tools::exec::sandbox::require_for_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='preview_fixture'\nversion='0.0.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(
        root.path().join("src/lib.rs"),
        "pub fn value() -> u8 { 1 }\n",
    )
    .unwrap();
    // Compiler previews need the operator's installed toolchain. Give only this
    // disposable test an explicit Dev policy and its Cargo/Rustup directories;
    // production previews retain their workspace's configured sandbox policy.
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap();
    let mut toolchains = Vec::new();
    for (key, fallback) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
        let path = std::env::var_os(key)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(fallback));
        if path.is_dir() {
            toolchains.push(path);
        }
    }
    let ws = Workspace::new(root.path()).unwrap().with_policy(
        crate::tools::exec::sandbox::Policy::Dev {
            deny_read: vec![],
            extra_write: toolchains,
            allow_network: false,
        },
    );
    let output = run(
        &ws,
        &json!({"path":"src/lib.rs","content":"pub fn value() -> u8 { \"bad\" }\n","check":"type"}),
        &AtomicBool::new(false),
    );
    assert!(!output.is_error, "{}", output.text);
    let result: Value = serde_json::from_str(&output.text).unwrap();
    assert_eq!(result["compiler_failed"], true);
    assert!(
        result["compiler_output"]
            .as_str()
            .unwrap()
            .contains("mismatched types"),
        "{}",
        output.text
    );
    assert_eq!(
        fs::read_to_string(root.path().join("src/lib.rs")).unwrap(),
        "pub fn value() -> u8 { 1 }\n"
    );
    assert!(!root.path().join("target").exists());
    assert!(!root.path().join("Cargo.lock").exists());
}
