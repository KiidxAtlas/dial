use super::*;
#[test]
fn syntax_preview_never_changes_live_source() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let out = run(
        &ws,
        &json!({"path":"a.rs","content":"fn a( {","check":"syntax"}),
        &AtomicBool::new(false),
    );
    assert!(!out.is_error);
    assert_eq!(
        serde_json::from_str::<Value>(&out.text).unwrap()["syntax"]["valid"],
        false
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn a() {}"
    );
}
#[test]
fn private_and_escaping_paths_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    for path in ["../a.rs", ".env", "private/key.rs"] {
        assert!(
            run(
                &ws,
                &json!({"path":path,"content":""}),
                &AtomicBool::new(false)
            )
            .is_error
        );
    }
}
