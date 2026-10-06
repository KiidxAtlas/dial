use dial::{
    config::types::{Admission, Config, Harness, Outcome, SuiteScore},
    learn::improve,
    storage::store::Store,
};

fn verdict(store: &Store, version: i64, score: f64, source: &str) {
    let id = store
        .begin_trajectory(version, "live-project", "coding", "implement the task")
        .unwrap();
    store.finish_trajectory(id, Some("finished"), None).unwrap();
    store
        .outcome(&Outcome {
            trajectory: id,
            score,
            source: source.into(),
            reference: None,
        })
        .unwrap();
}

#[test]
fn monitor_waits_for_evidence_then_rolls_back_the_admitted_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "project", &Harness::default()).unwrap();
    let base = store.active().unwrap();
    let mut h = base.harness.clone();
    h.system_prompt = "Different strategy".into();
    let candidate = store.candidate(base.id, "harness", &h, None).unwrap();
    store
        .admit(&Admission {
            candidate: candidate.id,
            baseline: base.id,
            passed: true,
            reason: "verified".into(),
            scores: vec![
                SuiteScore {
                    name: "target".into(),
                    primary: true,
                    baseline: 50.0,
                    candidate: 80.0,
                    cases: 10,
                    candidate_only: 3,
                    baseline_only: 0,
                },
                SuiteScore {
                    name: "guard".into(),
                    primary: false,
                    baseline: 100.0,
                    candidate: 100.0,
                    cases: 10,
                    candidate_only: 0,
                    baseline_only: 0,
                },
            ],
        })
        .unwrap();
    let mut cfg = Config::default();
    cfg.improve.monitor_min_turns = 2;
    for _ in 0..2 {
        verdict(&store, base.id, 100.0, "user");
    }
    verdict(&store, candidate.id, 0.0, "user:undo");
    assert!(improve::monitor(&store, &cfg).unwrap().is_none());
    // Legacy shell-derived rewards must not satisfy the minimum evidence requirement.
    verdict(&store, candidate.id, 0.0, "check:failed");
    assert!(improve::monitor(&store, &cfg).unwrap().is_none());
    verdict(&store, candidate.id, 0.0, "user");
    assert!(improve::monitor(&store, &cfg).unwrap().is_some());
    assert_eq!(store.active().unwrap().id, base.id);
    assert!(improve::monitor(&store, &cfg).unwrap().is_none());
}
