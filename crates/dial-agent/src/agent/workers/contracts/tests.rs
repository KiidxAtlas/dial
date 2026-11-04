use super::*;
use crate::agent::verify::worker_acceptance::Acceptance;

fn contract(id: &str, depends_on: &[&str]) -> Contract {
    Contract {
        id: id.into(),
        depends_on: depends_on.iter().map(|dep| (*dep).into()).collect(),
    }
}

fn request() -> Value {
    json!({"task":"inspect","acceptance_checks":["behavior"],"max_steps":4})
}

fn state_of(ledger: &Ledger, id: &str) -> Value {
    ledger.node(id).expect("node exists")["state"].clone()
}

#[test]
fn invalid_ids_and_self_dependencies_are_rejected_before_any_intent_is_recorded() {
    let mut ledger = Ledger::default();
    for bad in [
        contract("", &[]),
        contract("bad id", &[]),
        contract("n1", &["n1"]),
    ] {
        assert!(ledger.begin(&bad, request()).is_err());
    }
    assert!(ledger.snapshot().is_null());
}

#[test]
fn unknown_or_duplicate_dependencies_are_rejected() {
    let mut ledger = Ledger::default();
    assert!(
        ledger
            .begin(&contract("n1", &["ghost"]), request())
            .is_err()
    );
    assert!(
        ledger
            .begin(&contract("n1", &["a", "a"]), request())
            .is_err()
    );
    assert!(ledger.snapshot().is_null());
}

#[test]
fn attempts_are_bounded_and_the_assignment_is_immutable() {
    let mut ledger = Ledger::default();
    let node = contract("n1", &[]);
    ledger.begin(&node, request()).unwrap();
    // A changed acceptance target cannot reuse the durable node.
    ledger
        .settle(
            "n1",
            "never_started",
            Acceptance::unverified("no start"),
            None,
        )
        .unwrap();
    assert!(ledger.begin(&node, json!({"task":"other"})).is_err());
    for attempt in 2..=MAX_ATTEMPTS {
        ledger.begin(&node, request()).unwrap();
        assert_eq!(ledger.node("n1").unwrap()["attempts"], attempt);
        ledger
            .settle(
                "n1",
                "never_started",
                Acceptance::unverified("no start"),
                None,
            )
            .unwrap();
    }
    assert!(
        ledger.begin(&node, request()).is_err(),
        "fourth attempt must be refused"
    );
}

#[test]
fn a_worker_attaches_once_and_a_running_node_cannot_be_replayed() {
    let mut ledger = Ledger::default();
    ledger.begin(&contract("n1", &[]), request()).unwrap();
    ledger.attach("n1", "task-1").unwrap();
    assert!(ledger.attach("n1", "task-2").is_err());
    assert!(ledger.begin(&contract("n1", &[]), request()).is_err());
    assert_eq!(
        ledger.running(),
        vec![("n1".to_string(), "task-1".to_string())]
    );
}

#[test]
fn unverified_or_rejected_settlement_never_grants_acceptance() {
    let mut ledger = Ledger::default();
    ledger.begin(&contract("n1", &[]), request()).unwrap();
    ledger.attach("n1", "task-1").unwrap();
    let artifact = Some("artifact://out".to_string());
    ledger
        .settle(
            "n1",
            "done",
            Acceptance::unverified("no receipt"),
            artifact.clone(),
        )
        .unwrap();
    assert_ne!(state_of(&ledger, "n1"), json!("accepted"));
    assert!(ledger.blocker().is_some());

    let mut rejected = Acceptance::unverified("check failed");
    rejected.reject("configured acceptance check failed");
    ledger.begin(&contract("n2", &[]), request()).unwrap();
    ledger.attach("n2", "task-2").unwrap();
    ledger.settle("n2", "done", rejected, artifact).unwrap();
    assert_eq!(state_of(&ledger, "n2"), json!("retryable"));
    assert!(
        ledger.blocker().is_some(),
        "a retryable node still blocks completion"
    );
    assert_eq!(state_of(&ledger, "n1"), json!("interrupted"));
}

#[test]
fn restored_running_work_is_interrupted_without_a_replay_path() {
    let mut ledger = Ledger::default();
    ledger.begin(&contract("n1", &[]), request()).unwrap();
    ledger.attach("n1", "task-1").unwrap();

    let restored = Ledger::restore(&ledger.snapshot());
    assert_eq!(state_of(&restored, "n1"), json!("interrupted"));
    assert!(
        restored.running().is_empty(),
        "no worker may be reattached after restart"
    );
    assert!(
        restored.checks("n1").is_err(),
        "interrupted work is never reverified as accepted"
    );
    assert!(restored.blocker().unwrap().contains("interrupted"));
}

#[test]
fn absent_journal_restores_to_an_empty_unblocked_ledger() {
    let ledger = Ledger::restore(&Value::Null);
    assert!(ledger.snapshot().is_null());
    assert!(ledger.blocker().is_none());
}

#[test]
fn malformed_or_unsupported_journals_fail_closed() {
    for saved in [
        json!({"version":1,"nodes":"not-a-map","fault":null}),
        json!({"version":2,"nodes":{},"fault":null}),
        json!({"version":1,"nodes":{},"fault":null,"extra":true}),
    ] {
        let mut ledger = Ledger::restore(&saved);
        assert!(ledger.blocker().unwrap().contains("workflow journal"));
        assert!(ledger.begin(&contract("n2", &[]), request()).is_err());
    }
}

#[test]
fn journal_with_an_invalid_id_or_missing_dependency_fails_closed() {
    let mut ledger = Ledger::default();
    ledger.begin(&contract("n1", &[]), request()).unwrap();
    let mut saved = ledger.snapshot();
    saved["nodes"]["n1"]["depends_on"]["ghost"] = json!({"output":"artifact://x","binding":"b"});
    assert!(
        Ledger::restore(&saved)
            .blocker()
            .unwrap()
            .contains("missing dependency")
    );

    let mut saved = ledger.snapshot();
    let node = saved["nodes"]["n1"].clone();
    saved["nodes"] = json!({"bad id": node});
    assert!(
        Ledger::restore(&saved)
            .blocker()
            .unwrap()
            .contains("invalid node")
    );
}

#[test]
fn replacing_the_controller_revokes_retained_promotion_authority() {
    let mut ledger = Ledger::default();
    ledger.begin(&contract("child", &[]), request()).unwrap();
    let promotion = ledger.promotion("child");
    drop(ledger);
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(root.path()).unwrap();
    let error = promotion.lock()(&workspace, &VerificationConfig::default()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("workflow authority is unavailable"),
        "{error:#}"
    );
}
