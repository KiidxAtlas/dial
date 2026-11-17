use super::*;

#[test]
fn research_changes_require_exact_typed_consent() {
    let config = Config::default();
    for params in [
        json!({"action":"evaluate","candidate":1}),
        json!({"action":"evaluate","candidate":1,"confirm":false}),
        json!({"action":"evaluate","candidate":0,"confirm":true}),
        json!({"action":"evaluate","candidate":1,"confirm":"true"}),
        json!({"action":"evaluate","candidate":1,"confirm":true,"passed":true}),
        json!({"action":"promote_steering","name":"../outside","confirm":true}),
    ] {
        let error = request(&config, &params).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<super::super::RequestError>()
                .unwrap()
                .code,
            ErrorCode::InvalidParams,
            "{params}"
        );
    }
    assert!(
        request(
            &config,
            &json!({"action":"evaluate","candidate":1,"confirm":true})
        )
        .is_ok()
    );
}

#[test]
fn inspection_does_not_initialize_learning_and_cancelled_admission_does_not_write() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        state_dir: root.path().join("absent"),
        model: "fixture".into(),
        ..Config::default()
    };
    assert_eq!(status(&config).unwrap()["learning"]["initialized"], false);
    assert!(!config.state_dir.exists());
    let error = action(
        &config,
        &json!({"action":"evaluate","candidate":1,"confirm":true}),
        Arc::new(AtomicBool::new(true)),
        &mut |_| panic!("cancelled work ran"),
    )
    .unwrap_err();
    assert!(error.is::<crate::llm::Interrupted>());
    assert!(!config.state_dir.exists());
}

#[test]
fn steering_admission_keeps_owner_gate_and_never_trusts_client_verdict() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        state_dir: root.path().into(),
        model: "fixture".into(),
        ..Config::default()
    };
    let pending = crate::steering::pending_path(&config, "candidate").unwrap();
    std::fs::create_dir_all(pending.parent().unwrap()).unwrap();
    let bytes = br#"{"model_name":"fixture","evaluation":{"passed":true}}"#;
    std::fs::write(&pending, bytes).unwrap();
    assert!(
        action(
            &config,
            &json!({"action":"promote_steering","name":"candidate","confirm":true}),
            Arc::new(AtomicBool::new(false)),
            &mut |_| {}
        )
        .is_err()
    );
    assert_eq!(std::fs::read(pending).unwrap(), bytes);
    assert!(!crate::llm::steering::vector_path(&config).exists());
}

#[test]
fn cancelled_held_out_evaluation_leaves_candidate_pending_and_active_unchanged() {
    use crate::config::types::{GateConfig, SuiteConfig};
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let cases = root.path().join("cases.jsonl");
    std::fs::write(
        &cases,
        "{\"id\":\"held-out\",\"prompt\":\"test\",\"expected\":\"ok\"}\n",
    )
    .unwrap();
    let guard = root.path().join("guard.jsonl");
    std::fs::write(
        &guard,
        "{\"id\":\"guard\",\"prompt\":\"guard test\",\"expected\":\"ok\"}\n",
    )
    .unwrap();
    let config = Config {
        model: "fixture".into(),
        use_case: "test".into(),
        state_dir: root.path().join("state"),
        user_dir: root.path().join("user"),
        project_root: root.path().join("project"),
        compute_locks: false,
        gate: Some(GateConfig {
            suites: vec![
                SuiteConfig {
                    name: "sealed".into(),
                    path: cases,
                    primary: true,
                },
                SuiteConfig {
                    name: "guard".into(),
                    path: guard,
                    primary: false,
                },
            ],
            max_regression: 0.0,
            min_improvement: 1.0,
        }),
        ..Config::default()
    };
    let store = Store::open(
        &config.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )
    .unwrap();
    let active = store.active().unwrap();
    let candidate = store
        .candidate(active.id, "harness", &config.harness, None)
        .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let error = action(
        &config,
        &json!({"action":"evaluate","candidate":candidate.id,"confirm":true}),
        cancel.clone(),
        &mut |_| cancel.store(true, Ordering::SeqCst),
    )
    .unwrap_err();
    assert!(error.is::<crate::llm::Interrupted>(), "{error:#}");
    assert_eq!(store.active().unwrap().id, active.id);
    assert_eq!(store.version(candidate.id).unwrap().status, "candidate");
    // Both the version-store lock and background lease are released on cancellation.
    let control = auto::lock_file(&config.state_dir.join("control.lock")).unwrap();
    control.try_lock().unwrap();
    assert!(!compute::background_active());
    assert!(!compute::can_yield());
}
