use super::*;
use crate::config::types::{Admission, Harness, SuiteScore};
#[test]
fn profiles_require_acceptance_and_expire_on_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "coding", &Harness::default()).unwrap();
    let base = store.active().unwrap();
    let candidate = store
        .candidate(base.id, "harness", &base.harness, None)
        .unwrap();
    let pairs = (0..8)
        .map(|_| {
            (
                Kind::Debugging,
                CaseMeasurement {
                    passed: true,
                    metrics: Metrics {
                        tool_calls: 1,
                        ..Default::default()
                    },
                },
                CaseMeasurement {
                    passed: true,
                    metrics: Metrics {
                        tool_calls: 3,
                        ..Default::default()
                    },
                },
            )
        })
        .collect::<Vec<_>>();
    measure(&store, &base, &candidate, &pairs).unwrap();
    assert_eq!(select(&store, base.clone(), "fix bug").unwrap().id, base.id);
    store
        .admit(&Admission {
            candidate: candidate.id,
            baseline: base.id,
            passed: true,
            reason: "passed".into(),
            scores: vec![
                SuiteScore {
                    name: "primary".into(),
                    primary: true,
                    baseline: 50.0,
                    candidate: 100.0,
                    cases: 8,
                    baseline_only: 0,
                    candidate_only: 4,
                    tier: None,
                },
                SuiteScore {
                    name: "guard".into(),
                    primary: false,
                    baseline: 100.0,
                    candidate: 100.0,
                    cases: 8,
                    baseline_only: 0,
                    candidate_only: 0,
                    tier: None,
                },
            ],
        })
        .unwrap();
    assert_eq!(
        select(&store, store.active().unwrap(), "fix bug")
            .unwrap()
            .id,
        base.id
    );
    assert_eq!(
        select(&store, store.active().unwrap(), "explain code")
            .unwrap()
            .id,
        candidate.id
    );
    store.rollback().unwrap();
    assert_eq!(
        select(&store, store.active().unwrap(), "fix bug")
            .unwrap()
            .id,
        base.id
    );
}
