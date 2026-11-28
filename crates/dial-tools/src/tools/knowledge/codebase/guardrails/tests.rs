use super::*;
use tempfile::tempdir;

fn setup() -> (tempfile::TempDir, Codebase) {
    let dir = tempdir().unwrap();
    let project = dir.path().join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("src/auth.rs"),
        "pub fn refresh_token() -> bool { true }\n",
    )
    .unwrap();
    fs::write(
        project.join("src/cache.rs"),
        "pub fn lookup(key: &str) -> Option<String> { None }\n",
    )
    .unwrap();
    let layout = Layout::with_user_dir(&project, dir.path().join("user")).unwrap();
    let cache = Codebase::open(&layout).unwrap();
    (dir, cache)
}

#[test]
fn invariants_attach_lazily_dedupe_and_self_expire_on_change() {
    let (dir, cache) = setup();
    let saved = cache
        .remember_invariant(
            "src/auth.rs",
            "Never cache bearer tokens across tenant switches.\nKeep it single-line.",
            "s1",
        )
        .unwrap();
    let id = saved["saved"].as_str().unwrap().to_owned();
    assert!(id.starts_with("inv-"));
    assert_eq!(
        saved["summary"],
        "Never cache bearer tokens across tenant switches. Keep it single-line."
    );

    let baselines = BTreeMap::new();
    let mut shown = BTreeSet::new();

    // Unrelated file gets zero invariants and zero filesystem reads.
    SOURCE_READS.with(|reads| reads.borrow_mut().clear());
    let unrelated = cache
        .invariants_for_paths(&["src/cache.rs".into()], &baselines, &mut shown, 700)
        .unwrap();
    assert!(unrelated.is_none());
    SOURCE_READS.with(|reads| assert!(reads.borrow().is_empty()));

    // First touch of src/auth.rs returns the fresh 1-line invariant.
    let first = cache
        .invariants_for_paths(&["src/auth.rs".into()], &baselines, &mut shown, 700)
        .unwrap()
        .expect("fresh invariant should attach");
    assert!(first.contains("INVARIANT [src/auth.rs]"));
    assert!(first.contains("Never cache bearer tokens across tenant switches."));
    assert!(shown.contains(&id));

    // Second touch in the same generation is deduplicated (0 tokens).
    let second = cache
        .invariants_for_paths(&["src/auth.rs".into()], &baselines, &mut shown, 700)
        .unwrap();
    assert!(second.is_none());

    // After compaction/reset (empty shown set), mutating src/auth.rs self-expires the invariant.
    shown.clear();
    fs::write(
        dir.path().join("project/src/auth.rs"),
        "pub fn refresh_token() -> bool { false }\n",
    )
    .unwrap();
    let stale = cache
        .invariants_for_paths(&["src/auth.rs".into()], &baselines, &mut shown, 700)
        .unwrap();
    assert!(stale.is_none());

    // list_guardrails still shows it as stale so it can be pruned or replaced.
    let list = cache.list_guardrails(None, None).unwrap();
    assert_eq!(list["invariants"][0]["status"], "stale");

    cache.forget_invariant(&id).unwrap();
    let after = cache.list_guardrails(None, None).unwrap();
    assert!(after["invariants"].as_array().unwrap().is_empty());
}

#[test]
fn dead_ends_match_signature_and_respect_turn_baseline_digests() {
    let (dir, cache) = setup();
    let saved = cache
        .remember_dead_end(
            "src/cache.rs",
            "test_cache_invalidation",
            "Caching by mtime missed same-second edits; use SHA-256 content digests instead.",
            "s1",
        )
        .unwrap();
    let id = saved["saved"].as_str().unwrap().to_owned();
    let original_digest = saved["digest"].as_str().unwrap().to_owned();
    assert!(id.starts_with("dead-"));

    let mut shown = BTreeSet::new();
    let baselines = BTreeMap::new();

    // Unrelated failure output does not trigger the dead end.
    let miss = cache
        .dead_ends_for_failure(
            &["src/auth.rs".into()],
            "test auth::test_login ... FAILED\nassertion failed",
            &baselines,
            &mut shown,
            700,
        )
        .unwrap();
    assert!(miss.is_none());

    // Matching failing test triggers the dead end even when src/cache.rs was edited during this turn,
    // as long as the pre-turn baseline digest matches the recorded digest.
    fs::write(
        dir.path().join("project/src/cache.rs"),
        "pub fn lookup(key: &str) -> Option<String> { Some(key.into()) }\n",
    )
    .unwrap();
    let turn_baselines = BTreeMap::from([("src/cache.rs".into(), original_digest)]);
    let hit = cache
        .dead_ends_for_failure(
            &["src/cache.rs".into()],
            "test cache::test_cache_invalidation ... FAILED\nexpected fresh entry",
            &turn_baselines,
            &mut shown,
            700,
        )
        .unwrap()
        .expect("matching dead end should attach on failure");
    assert!(hit.contains("DEAD_END [src/cache.rs]"));
    assert!(hit.contains("sig=\"test_cache_invalidation\""));
    assert!(hit.contains("use SHA-256 content digests instead."));
    assert!(shown.contains(&id));

    // Same dead end does not repeat within the same history generation.
    let repeat = cache
        .dead_ends_for_failure(
            &["src/cache.rs".into()],
            "test cache::test_cache_invalidation ... FAILED",
            &turn_baselines,
            &mut shown,
            700,
        )
        .unwrap();
    assert!(repeat.is_none());

    // In a later session where src/cache.rs was refactored (neither current nor baseline matches),
    // the dead end self-expires.
    shown.clear();
    let expired = cache
        .dead_ends_for_failure(
            &["src/cache.rs".into()],
            "test cache::test_cache_invalidation ... FAILED",
            &BTreeMap::new(),
            &mut shown,
            700,
        )
        .unwrap();
    assert!(expired.is_none());
}

#[test]
fn guardrails_reject_secrets_and_bound_attachment_count() {
    let (_dir, cache) = setup();
    assert!(
        cache
            .remember_invariant(
                "src/auth.rs",
                "API key is sk-proj-1234567890abcdefghij",
                "s1"
            )
            .is_err()
    );
    for i in 0..5 {
        cache
            .remember_invariant(
                "src/auth.rs",
                &format!("Invariant number {i} keeps token validation deterministic."),
                "s1",
            )
            .unwrap();
    }
    let mut shown = BTreeSet::new();
    let note = cache
        .invariants_for_paths(&["src/auth.rs".into()], &BTreeMap::new(), &mut shown, 700)
        .unwrap()
        .unwrap();
    let lines = note
        .lines()
        .filter(|line| line.starts_with("- INVARIANT "))
        .count();
    assert!(lines <= MAX_ATTACHED_INVARIANTS);
    assert!(note.len() <= 700);
}

#[test]
fn codebase_tool_dispatches_guardrail_actions_end_to_end() {
    let (dir, cache) = setup();
    let workspace = Workspace::new(&dir.path().join("project")).unwrap();
    let config = crate::config::types::Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let tool = tool(Arc::new(cache), "s-tool".into());

    let inv = tool.call(
        &ctx,
        &json!({
            "action": "remember_invariant",
            "sources": ["src/auth.rs"],
            "summary": "Validate tenant scope before issuing tokens."
        }),
    );
    assert!(!inv.is_error, "{}", inv.text);
    let inv_json: Value = serde_json::from_str(&inv.text).unwrap();
    let inv_id = inv_json["saved"].as_str().unwrap().to_owned();

    let dead = tool.call(
        &ctx,
        &json!({
            "action": "remember_dead_end",
            "sources": ["src/cache.rs"],
            "signature": "E0502",
            "summary": "Holding Conn lock while hashing files deadlocks worker threads."
        }),
    );
    assert!(!dead.is_error, "{}", dead.text);
    let dead_json: Value = serde_json::from_str(&dead.text).unwrap();
    let dead_id = dead_json["saved"].as_str().unwrap().to_owned();

    let listed = tool.call(&ctx, &json!({"action": "list_guardrails"}));
    assert!(!listed.is_error, "{}", listed.text);
    let listed_json: Value = serde_json::from_str(&listed.text).unwrap();
    assert_eq!(listed_json["invariants"].as_array().unwrap().len(), 1);
    assert_eq!(listed_json["dead_ends"].as_array().unwrap().len(), 1);

    let f1 = tool.call(
        &ctx,
        &json!({"action": "forget_invariant", "guardrail_id": inv_id}),
    );
    assert!(!f1.is_error, "{}", f1.text);
    let f2 = tool.call(
        &ctx,
        &json!({"action": "forget_dead_end", "guardrail_id": dead_id}),
    );
    assert!(!f2.is_error, "{}", f2.text);
}
