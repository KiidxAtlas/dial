//! Live monitor contract: matched evidence only, observational only (no automatic rollback), alpha spent
//! per new look, and retries never counted as independent samples.
use super::*;
use crate::config::types::Outcome;

fn pairs(parent_better: usize, candidate_better: usize, ties: usize) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    out.extend(std::iter::repeat((90.0, 10.0)).take(parent_better));
    out.extend(std::iter::repeat((10.0, 90.0)).take(candidate_better));
    out.extend(std::iter::repeat((50.0, 50.0)).take(ties));
    out
}

#[test]
fn alpha_spending_sums_to_the_total_over_any_number_of_looks() {
    assert!((look_level(0.1, 1) - 0.1 * 6.0 / std::f64::consts::PI.powi(2)).abs() < 1e-12);
    let spent: f64 = (1..=100_000).map(|k| look_level(0.1, k)).sum();
    assert!(spent <= 0.1 + 1e-12 && spent > 0.0999, "{spent}");
}

#[test]
fn drop_bounds_are_zero_without_discordance_and_grow_with_evidence() {
    assert_eq!(drop_lower_bound(0, 0, 20, 0.05), 0.0);
    assert_eq!(drop_lower_bound(0, 0, 0, 0.05), 0.0);
    // Improvements (candidate better on every discordant prompt) are never a drop.
    assert_eq!(drop_lower_bound(0, 20, 20, 0.05), 0.0);
    let level: f64 = 0.0304;
    let eight = drop_lower_bound(8, 0, 8, level);
    let sixteen = drop_lower_bound(16, 0, 16, level);
    // For all-worse pairs each exact lower bound has the closed form (level/2)^(1/n).
    let lower = (level / 2.0).powf(1.0 / 8.0);
    assert!((eight - 100.0 * lower * (2.0 * lower - 1.0)).abs() < 1e-10);
    assert!(sixteen > eight && sixteen > 15.0, "{eight} {sixteen}");
}

#[test]
fn an_observed_drop_needs_its_lower_bound_above_the_limit() {
    // Eight matched prompts, all worse for the candidate: lower bound about 18.9 points at the first look.
    match judge_live(&pairs(8, 0, 0), 1, 8, 15.0, 0.1) {
        LiveVerdict::Observed {
            pairs,
            look,
            drop_lower,
            ..
        } => {
            assert_eq!((pairs, look), (8, 1));
            assert!((15.0..25.0).contains(&drop_lower), "{drop_lower}");
        }
        other => panic!("expected an observed drop, got {other:?}"),
    }
    // Seven matched prompts: the same evidence stays below the limit (about 13 points).
    assert!(matches!(
        judge_live(&pairs(7, 0, 0), 1, 7, 15.0, 0.1),
        LiveVerdict::Holds { .. }
    ));
    // A later look spends less alpha, so the same eight prompts no longer clear the limit.
    assert!(matches!(
        judge_live(&pairs(8, 0, 0), 2, 8, 15.0, 0.1),
        LiveVerdict::Holds { .. }
    ));
}

#[test]
fn noise_ties_and_improvements_never_look_like_a_drop() {
    assert!(matches!(
        judge_live(&pairs(4, 4, 0), 1, 8, 15.0, 0.1),
        LiveVerdict::Holds { .. }
    ));
    assert!(matches!(
        judge_live(&pairs(0, 0, 20), 1, 8, 15.0, 0.1),
        LiveVerdict::Holds { .. }
    ));
    assert!(matches!(
        judge_live(&pairs(0, 20, 0), 1, 8, 15.0, 0.1),
        LiveVerdict::Holds { .. }
    ));
}

#[test]
fn too_few_matched_prompts_is_insufficient_evidence() {
    assert_eq!(
        judge_live(&pairs(3, 0, 0), 1, 8, 15.0, 0.1),
        LiveVerdict::Insufficient {
            pairs: 3,
            needed: 8
        }
    );
    assert_eq!(
        judge_live(&[], 1, 0, 15.0, 0.1),
        LiveVerdict::Insufficient {
            pairs: 0,
            needed: 1
        }
    );
}

/// A live turn recorded with an explicit score. Each call is one distinct trajectory.
fn live(store: &Store, version: i64, prompt: &str, score: f64, source: &str) {
    let id = store
        .begin_trajectory(version, "coding", "session:1", prompt)
        .unwrap();
    store.finish_trajectory(id, Some("done"), None).unwrap();
    store
        .outcome(&Outcome {
            trajectory: id,
            score,
            source: source.into(),
            reference: None,
        })
        .unwrap();
}

/// A store with the parent active, then a harness candidate admitted (so it is the active version).
fn admitted_candidate() -> (tempfile::TempDir, Store, Config, Version, Version) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        use_case: "test".into(),
        ..Config::default()
    };
    let store = Store::open(dir.path(), "test-model", &config.use_case, &config.harness).unwrap();
    let parent = store.active().unwrap();
    let candidate = store
        .candidate(parent.id, "harness", &config.harness, None)
        .unwrap();
    store
        .admit(&Admission {
            candidate: candidate.id,
            baseline: parent.id,
            passed: true,
            reason: "test".into(),
            scores: vec![
                crate::config::types::SuiteScore {
                    name: "primary".into(),
                    primary: true,
                    baseline: 50.0,
                    candidate: 60.0,
                    cases: 10,
                    candidate_only: 1,
                    baseline_only: 0,
                    tier: None,
                },
                crate::config::types::SuiteScore {
                    name: "guard".into(),
                    primary: false,
                    baseline: 100.0,
                    candidate: 100.0,
                    cases: 10,
                    candidate_only: 0,
                    baseline_only: 0,
                    tier: None,
                },
            ],
        })
        .unwrap();
    assert_eq!(store.active().unwrap().id, candidate.id);
    (dir, store, config, parent, candidate)
}

#[test]
fn unpaired_live_scores_never_trigger_or_roll_back_anything() {
    let (_dir, store, config, _parent, candidate) = admitted_candidate();
    // Twenty candidate prompts scored 0 and twenty parent prompts scored 100, none shared: no matched evidence.
    for i in 0..20 {
        live(
            &store,
            candidate.id,
            &format!("candidate task {i}"),
            0.0,
            "user:rating",
        );
    }
    for i in 0..20 {
        live(
            &store,
            store.version(candidate.parent.unwrap()).unwrap().id,
            &format!("parent task {i}"),
            100.0,
            "user:rating",
        );
    }
    assert_eq!(monitor(&store, &config).unwrap(), None);
    assert_eq!(store.active().unwrap().id, candidate.id, "no rollback");
    let last: Value =
        serde_json::from_str(&store.get_meta("last_monitor").unwrap().unwrap()).unwrap();
    assert_eq!(last["monitor"]["status"], "insufficient_evidence");
}

#[test]
fn matched_worse_prompts_are_recorded_as_observed_and_never_rolled_back() {
    let (_dir, store, config, parent, candidate) = admitted_candidate();
    for i in 0..8 {
        let prompt = format!("shared task {i}");
        live(&store, parent.id, &prompt, 90.0, "user:rating");
        live(&store, candidate.id, &prompt, 10.0, "user:rating");
    }
    assert_eq!(monitor(&store, &config).unwrap(), None);
    assert_eq!(
        store.active().unwrap().id,
        candidate.id,
        "observational only"
    );
    let last: Value =
        serde_json::from_str(&store.get_meta("last_monitor").unwrap().unwrap()).unwrap();
    assert_eq!(last["monitor"]["status"], "observed_unconfirmed");
    assert_eq!(last["monitor"]["look"], 1);
    assert!(store.get_meta("last_rollback").unwrap().is_none());
    let _ = parent;
}

#[test]
fn unchanged_matched_data_is_not_a_new_look_and_growth_is() {
    let (_dir, store, config, parent, candidate) = admitted_candidate();
    for i in 0..8 {
        let prompt = format!("shared task {i}");
        let (p, c) = if i % 2 == 0 {
            (90.0, 10.0)
        } else {
            (10.0, 90.0)
        };
        live(&store, parent.id, &prompt, p, "user:rating");
        live(&store, candidate.id, &prompt, c, "user:rating");
    }
    let key = format!("monitor_look:{}", candidate.id);
    monitor(&store, &config).unwrap();
    assert_eq!(store.get_meta(&key).unwrap().as_deref(), Some("1:8"));
    monitor(&store, &config).unwrap();
    assert_eq!(
        store.get_meta(&key).unwrap().as_deref(),
        Some("1:8"),
        "no new evidence, no new look"
    );
    let last: Value =
        serde_json::from_str(&store.get_meta("last_monitor").unwrap().unwrap()).unwrap();
    assert_eq!(last["monitor"]["status"], "no_new_evidence");
    live(&store, parent.id, "shared task 8", 90.0, "user:rating");
    live(&store, candidate.id, "shared task 8", 10.0, "user:rating");
    monitor(&store, &config).unwrap();
    assert_eq!(store.get_meta(&key).unwrap().as_deref(), Some("2:9"));
}

#[test]
fn retries_and_checks_do_not_add_independent_samples() {
    let (_dir, store, config, parent, candidate) = admitted_candidate();
    // Four retried attempts at one prompt on the candidate average into a single matched prompt.
    for score in [0.0, 0.0, 0.0, 0.0] {
        live(&store, candidate.id, "retried task", score, "user:rating");
    }
    live(&store, parent.id, "retried task", 90.0, "user:rating");
    // A behavioural check and a user verdict on the same prompt are different classes: no mixed pair.
    live(
        &store,
        parent.id,
        "checked task",
        100.0,
        "checks:behavioral",
    );
    live(&store, candidate.id, "checked task", 0.0, "user:rating");
    let pairs = store.matched_live_scores(candidate.id, parent.id).unwrap();
    assert_eq!(pairs, vec![(90.0, 0.0)]);
    assert_eq!(monitor(&store, &config).unwrap(), None);
    let last: Value =
        serde_json::from_str(&store.get_meta("last_monitor").unwrap().unwrap()).unwrap();
    assert_eq!(last["monitor"]["status"], "insufficient_evidence");
}
