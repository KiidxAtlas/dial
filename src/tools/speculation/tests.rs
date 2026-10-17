use super::*;
use serde_json::json;
fn setup() -> (
    tempfile::TempDir,
    Workspace,
    crate::config::types::Config,
    Registry,
) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "fn main() { old(x); }\n").unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let mut config = crate::config::types::Config::default();
    config.extensions.enabled = false;
    config.integrations = vec!["ast_search".into()];
    let registry = Registry::standard(&config, Vec::new());
    (root, workspace, config, registry)
}
fn ready(pool: &Pool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while pool.stats().completed == 0 {
        assert!(Instant::now() < deadline, "preparation failed to settle");
        std::thread::yield_now();
    }
}
#[test]
fn batch_exit_discards_unconsumed_results_without_cancelling_the_owner() {
    let (_root, workspace, config, registry) = setup();
    let pool = Arc::new(Pool::default());
    let session = crate::runtime::cancellation::SessionCancellation::default();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let args = json!({"path":"a.rs","pattern":"old($A)"});
    {
        let _batch = pool.batch();
        assert!(pool.start(
            &workspace,
            config,
            &registry,
            args.clone(),
            "owner".into(),
            session.turn(stop.clone())
        ));
        ready(&pool);
        assert!(pool.pending("owner", &args));
    }
    assert!(!pool.pending("owner", &args));
    assert!(pool.take(&workspace, &args, "owner").is_none());
    assert_eq!(pool.stats().discarded, 1);
    assert!(!stop.load(Ordering::SeqCst));
}

#[test]
fn unchanged_captured_source_reuses_without_granting_edit_witnesses_or_writing() {
    let (_root, workspace, config, registry) = setup();
    let pool = Pool::default();
    let session = crate::runtime::cancellation::SessionCancellation::default();
    let args = json!({"path":"a.rs","pattern":"old($A)"});
    assert!(pool.start(
        &workspace,
        config,
        &registry,
        args.clone(),
        "owner".into(),
        session.turn(Arc::new(std::sync::atomic::AtomicBool::new(false)))
    ));
    ready(&pool);
    let result = pool.take(&workspace, &args, "owner").unwrap();
    assert!(!result.is_error);
    assert!(result.text.contains("old(x)"));
    assert_eq!(
        result.execution.effects,
        super::super::outcome::Effects::None
    );
    assert_eq!(
        std::fs::read_to_string(workspace.root().join("a.rs")).unwrap(),
        "fn main() { old(x); }\n"
    );
    assert!(
        workspace
            .store()
            .check_ranges(
                &workspace.root().join("a.rs"),
                "fn main() { old(x); }\n",
                &[(1, 1)]
            )
            .is_err()
    );
    assert_eq!(pool.stats().reused, 1);
}
#[test]
fn source_changes_and_policy_changes_cannot_reuse_prepared_results() {
    let (_root, workspace, config, registry) = setup();
    let pool = Pool::default();
    let session = crate::runtime::cancellation::SessionCancellation::default();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let args = json!({"path":"a.rs","pattern":"old($A)"});
    assert!(pool.start(
        &workspace,
        config,
        &registry,
        args.clone(),
        "original-policy".into(),
        session.turn(stop.clone())
    ));
    ready(&pool);
    assert!(pool.take(&workspace, &args, "changed-policy").is_none());
    std::fs::write(workspace.root().join("a.rs"), "fn main() { new(x); }\n").unwrap();
    assert!(pool.take(&workspace, &args, "original-policy").is_none());
    assert!(!stop.load(Ordering::SeqCst));
    assert_eq!(pool.stats().discarded, 1);
}
#[test]
fn third_party_names_and_cancelled_owners_do_not_supply_cached_authority() {
    let (_root, workspace, config, registry) = setup();
    let pool = Pool::default();
    let session = crate::runtime::cancellation::SessionCancellation::default();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let args = json!({"path":"a.rs","pattern":"old($A)"});
    assert!(!pool.start(
        &workspace,
        config.clone(),
        &Registry::new(),
        args.clone(),
        "owner".into(),
        session.turn(stop.clone())
    ));
    assert!(pool.start(
        &workspace,
        config,
        &registry,
        args.clone(),
        "owner".into(),
        session.turn(stop.clone())
    ));
    ready(&pool);
    session.cancel();
    assert!(pool.take(&workspace, &args, "owner").is_none());
    assert!(stop.load(Ordering::SeqCst));
}
