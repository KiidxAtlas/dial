use super::*;
fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Workspace, Experience) {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().join("dial")).unwrap();
    let ws = Workspace::new(root.path()).unwrap();
    let store = Experience::open(&layout).unwrap();
    #[cfg(windows)]
    assert!(
        platform_fs::is_owner_only(
            &fs::File::open(layout.state_dir.join("experience.sqlite3")).unwrap()
        )
        .unwrap()
    );
    (root, home, ws, store)
}
fn recipe(ws: &Workspace) -> Recipe {
    Recipe {
        objective: "repair arithmetic".into(),
        model: "model-a".into(),
        session: "session".into(),
        steps: vec![Step {
            tool: "write".into(),
            args: json!({"path":"a.rs","content":"fn a() {}"}),
            sources: preconditions(ws, "write", &json!({"path":"a.rs"})),
        }],
        checks: vec!["cargo test".into()],
        lesson: None,
    }
}
#[test]
fn changed_sources_refuse_reuse_and_failures_expire() {
    let (root, _home, ws, store) = fixture();
    fs::write(root.path().join("a.rs"), "fn old() {}").unwrap();
    let r = recipe(&ws);
    let id = store.recipe(&r).unwrap();
    assert!(applicable(
        &ws,
        &store.get_recipe(&id).unwrap().steps[0].sources
    ));
    store
        .failure(
            "model-a",
            "read",
            &json!({"path":"a.rs"}),
            &r.steps[0].sources,
            "missing_source",
        )
        .unwrap();
    assert!(
        store
            .recall("missing_source", "model-a", &ws, 8192)
            .unwrap()
            .0
            .contains("Failure condition")
    );
    fs::write(root.path().join("a.rs"), "fn changed() {}").unwrap();
    assert!(!applicable(&ws, &r.steps[0].sources));
    assert!(
        !store
            .recall("missing_source", "model-a", &ws, 8192)
            .unwrap()
            .0
            .contains("Failure condition")
    );
    assert!(
        store
            .recall("arithmetic", "model-b", &ws, 8192)
            .unwrap()
            .0
            .contains("analogous")
    );
}
#[test]
fn recipes_require_complete_conditions_and_passing_checks() {
    let (_root, _home, ws, store) = fixture();
    let mut r = recipe(&ws);
    r.checks.clear();
    assert!(store.recipe(&r).is_err());
    r.checks.push("cargo test".into());
    r.steps[0].sources.clear();
    assert!(store.recipe(&r).is_err());
    r.steps[0].sources.insert("other.rs".into(), None);
    assert!(store.recipe(&r).is_err());
}
#[test]
fn failures_never_store_raw_arguments_and_private_sources_are_excluded() {
    let (_root, _home, ws, store) = fixture();
    let conditions = preconditions(&ws, "read", &json!({"path":"a.rs"}));
    store
        .failure(
            "model-a",
            "read",
            &json!({"path":"a.rs","input":"secret raw message"}),
            &conditions,
            "missing_source",
        )
        .unwrap();
    assert!(
        !store
            .inspect()
            .unwrap()
            .to_string()
            .contains("secret raw message")
    );
    assert!(preconditions(&ws, "write", &json!({"path":".env"})).is_empty());
    assert!(source_hash(&ws, "../escape").is_err());
}
#[test]
fn utility_is_only_counted_on_explicit_observed_use() {
    let (_root, _home, ws, store) = fixture();
    let id = store.recipe(&recipe(&ws)).unwrap();
    store.recall("arithmetic", "model-a", &ws, 8192).unwrap();
    let report = store.inspect().unwrap();
    assert_eq!(report["entries"][0]["uses"], 0);
    store.credit(&id, true).unwrap();
    let report = store.inspect().unwrap();
    assert_eq!(report["entries"][0]["verified_uses"], 1);
    store.observe_tool("model-a", "write", false).unwrap();
    store.observe_tool("model-b", "write", true).unwrap();
    assert!(
        store.tool_scores("model-a").unwrap()["write"]
            < store.tool_scores("model-b").unwrap()["write"]
    );
}
