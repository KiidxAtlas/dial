use super::{FAILURE_CAUSE_CHARS, TurnSteps, failure_cause};

#[test]
fn a_failed_turn_saves_its_cause_without_the_generic_recovery_advice() {
    let error = anyhow::anyhow!("model server returned HTTP 529: overloaded")
        .context("reading chat stream")
        .context(crate::agent::run::failure_guidance::PROVIDER_RECOVERY);
    assert_eq!(
        failure_cause(&error),
        "reading chat stream: model server returned HTTP 529: overloaded"
    );
    let long = anyhow::anyhow!("x".repeat(5_000));
    assert_eq!(
        failure_cause(&long).chars().count(),
        FAILURE_CAUSE_CHARS + 1
    );
}

#[test]
fn initial_repair_and_stop_hook_rounds_share_one_finite_budget() {
    let mut steps = TurnSteps::new(10);
    assert_eq!(steps.charge(4, 4), 4);
    assert_eq!(steps.remaining(), Some(6));
    // A repair round reports the cumulative turn count, not only its own steps.
    assert_eq!(steps.charge(5, 5), 9);
    assert_eq!(steps.remaining(), Some(1));
    // The stop-hook continuation gets only what is left.
    assert_eq!(steps.charge(1, 1), 10);
    assert_eq!(steps.remaining(), None);
}

#[test]
fn zero_budget_stays_unlimited_but_still_counts_steps() {
    let mut steps = TurnSteps::new(0);
    assert_eq!(steps.charge(1_000, 1_000), 1_000);
    assert_eq!(steps.remaining(), Some(0));
}

#[test]
fn refunded_attempts_leave_real_allowance_for_stop_hook_continuations() {
    let mut steps = TurnSteps::new(3);
    // Two unusable replies were refunded before the first completed candidate.
    assert_eq!(steps.charge(3, 1), 3);
    assert_eq!(steps.remaining(), Some(2));
    assert_eq!(steps.charge(2, 2), 5);
    assert_eq!(steps.remaining(), None);
}

#[test]
fn completion_persistence_failure_keeps_the_original_error_and_backend() {
    use crate::agent::*;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    let overlay =
        toml::from_str("model='test'\n[improve]\nenabled=false\n[budget]\nmax_turn_cost_usd=1.0")
            .unwrap();
    let config = Config::load(&layout, Some(overlay)).unwrap();
    let store = Store::open(
        &config.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )
    .unwrap();
    let db = rusqlite::Connection::open(config.state_dir.join("state.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_completion BEFORE INSERT ON events
         WHEN NEW.kind = 'completion_evidence'
         BEGIN SELECT RAISE(FAIL, 'completion persistence regression'); END;",
    )
    .unwrap();
    let session = Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: config.model.clone(),
        },
    )
    .unwrap();
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        Default::default(),
        5,
    );
    let mut agent = Agent::interactive(
        &config,
        backend,
        layout,
        session,
        None,
        Some(&store),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    agent.usage_scope = agent.usage_scope.next_turn(0).unwrap();
    let mut receipt = agent.usage_scope.reserve_external("test".into()).unwrap();
    receipt.mark_started().unwrap();
    receipt.account(&json!({"cost":1.0}), true, false).unwrap();

    // Stop locally at the observed cost boundary: no provider request is needed to settle a turn.
    let error = agent
        .turn_inner("Report the current status", &mut NullUi, true, None)
        .err()
        .expect("completion persistence must fail");
    assert!(format!("{error:#}").contains("completion persistence regression"));
    assert!(
        agent.backend.is_some(),
        "persistence must not consume the backend"
    );

    db.execute_batch("DROP TRIGGER reject_completion").unwrap();
    let report = agent
        .turn_inner("Report again", &mut NullUi, true, None)
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert!(report.text.contains("this turn is $1.0000"));
    assert!(
        agent.backend.is_some(),
        "the next turn can reuse the backend"
    );
}
