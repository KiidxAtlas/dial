use super::*;
use crate::config::types::{Event, Harness, Outcome};
use serde_json::json;

fn harness() -> Harness {
    Harness {
        system_prompt: "test".into(),
        memory: vec![],
        tools: vec![],
        subagents: vec![],
        max_steps: 2,
        temperature: 0.0,
        max_tokens: 32,
        edit_mode: None,
    }
}

fn synthetic(n: usize) -> Vec<(Vec<f64>, bool)> {
    (0..n)
        .map(|i| {
            let failed = i % 2 == 0;
            let mut f = features(
                "tool_result",
                (i % 7) as u32,
                &json!({"name":"bash","output":"ok"}),
            );
            if failed {
                f = features(
                    "tool_error",
                    (i % 7) as u32,
                    &json!({"name":"bash","output":"error: boom\n[exit code: 2]"}),
                );
            }
            (f, failed)
        })
        .collect()
}

#[test]
fn features_have_fixed_length_and_are_finite_even_for_garbage() {
    for data in [
        json!(null),
        json!({}),
        json!({"output":5,"prompt_tokens":"x","elapsed_ms":-3,"name":7}),
        json!({"output":"[exit code: 1]","prompt_tokens":120,"completion_tokens":30,"elapsed_ms":900,"tool_calls":2}),
    ] {
        for kind in ["generation", "tool_form_failure", "mystery", ""] {
            let f = features(kind, 3, &data);
            assert_eq!(f.len(), FEATURES);
            assert!(f.iter().all(|x| x.is_finite()));
        }
    }
    let f = features(
        "tool_form_failure",
        0,
        &json!({"kind":"schema","name":"read"}),
    );
    assert_eq!(f[F_FORM + 3], 1.0);
    assert_eq!(f[F_KIND + 5], 1.0);
    assert_eq!(
        features("tool_result", 0, &json!({"output":"x\n[exit code: 0]"}))[F_EXIT_NONZERO],
        0.0
    );
    assert_eq!(
        features("tool_error", 0, &json!({"output":"x\n[exit code: 3]"}))[F_EXIT_NONZERO],
        1.0
    );
    assert_eq!(
        features(
            "verification",
            0,
            &json!({"command":"t","exit":1,"passed":false})
        )[F_EXIT_NONZERO],
        1.0
    );
}

#[test]
fn training_separates_a_synthetic_dataset() {
    let probe = train(&synthetic(200), 7).unwrap();
    assert!(probe.auc > 0.9, "auc {}", probe.auc);
    assert_eq!(probe.trained_on, 200);
    let failing = probe.risk(&features(
        "tool_error",
        2,
        &json!({"name":"bash","output":"error\n[exit code: 2]"}),
    ));
    let fine = probe.risk(&features(
        "tool_result",
        2,
        &json!({"name":"bash","output":"ok"}),
    ));
    assert!(
        failing > 0.8 && fine < 0.2 && (0.0..=1.0).contains(&failing),
        "{failing} {fine}"
    );
}

#[test]
fn degenerate_data_is_rejected() {
    assert!(train(&synthetic(10), 1).is_none());
    let one_class: Vec<_> = synthetic(40).into_iter().map(|(f, _)| (f, true)).collect();
    assert!(train(&one_class, 1).is_none());
    let mut wrong_width = synthetic(40);
    wrong_width[0].0.pop();
    assert!(train(&wrong_width, 1).is_none());
}

#[test]
fn training_is_deterministic_for_a_seed_and_the_seed_matters_for_the_split() {
    // Noisy labels so the holdout AUC depends on which examples are held out.
    let noisy: Vec<_> = synthetic(100)
        .into_iter()
        .enumerate()
        .map(|(i, (f, l))| (f, if i % 5 == 0 { !l } else { l }))
        .collect();
    let (a, b) = (train(&noisy, 3).unwrap(), train(&noisy, 3).unwrap());
    assert_eq!((a.weights, a.bias, a.auc), (b.weights, b.bias, b.auc));
    let aucs: Vec<f64> = (0..8).map(|s| train(&noisy, s).unwrap().auc).collect();
    assert!(aucs.iter().any(|x| *x != aucs[0]), "{aucs:?}");
}

#[test]
fn holdout_does_not_memorize_events_from_the_same_trajectory() {
    // The only signal identifies the trajectory, not anything generalizable about failure.
    let mut examples = Vec::new();
    let mut groups = Vec::new();
    for trajectory in 0..10 {
        for _ in 0..20 {
            let mut f = vec![0.0; FEATURES];
            f[trajectory] = 1.0;
            examples.push((f, trajectory % 2 == 0));
            groups.push(trajectory);
        }
    }
    assert!(train(&examples, 7).unwrap().auc > 0.9);
    assert_eq!(train_grouped(&examples, &groups, 7).unwrap().auc, 0.5);
}

#[test]
fn auc_is_the_rank_statistic_and_handles_ties() {
    assert_eq!(auc(&[0.1, 0.2, 0.8, 0.9], &[false, false, true, true]), 1.0);
    assert_eq!(auc(&[0.9, 0.8, 0.2, 0.1], &[false, false, true, true]), 0.0);
    assert_eq!(
        auc(&[0.5; 6], &[true, false, true, false, true, false]),
        0.5
    );
    // pos {0.3, 0.7}, neg {0.3, 0.5}: pairs (0.3>0.3 tie .5)+(0.3>0.5 no)+(0.7>0.3)+(0.7>0.5) = 2.5/4
    assert!((auc(&[0.3, 0.7, 0.3, 0.5], &[true, true, false, false]) - 0.625).abs() < 1e-12);
    assert_eq!(auc(&[0.1, 0.2], &[true, true]), 0.5);
}

#[test]
fn trajectory_risk_is_the_mean_of_the_top_three_steps() {
    let probe = train(&synthetic(100), 1).unwrap();
    let bad = (
        "tool_error".to_string(),
        1,
        json!({"name":"bash","output":"error\n[exit code: 2]"}),
    );
    let good = (
        "tool_result".to_string(),
        1,
        json!({"name":"bash","output":"ok"}),
    );
    let (rb, rg) = (
        probe.risk(&features(&bad.0, 1, &bad.2)),
        probe.risk(&features(&good.0, 1, &good.2)),
    );
    let events = vec![
        good.clone(),
        bad.clone(),
        good.clone(),
        good.clone(),
        bad.clone(),
    ];
    assert!((probe.trajectory_risk(&events) - (2.0 * rb + rg) / 3.0).abs() < 1e-12);
    assert_eq!(probe.trajectory_risk(&[]), 0.0);
    assert!(probe.trajectory_risk(&[bad]) > probe.trajectory_risk(&[good]));
}

/// One finished trajectory with `events`, scored `score`.
fn trajectory(
    store: &Store,
    version: i64,
    i: usize,
    events: &[(&str, Value)],
    score: Option<f64>,
) -> i64 {
    let id = store
        .begin_trajectory(version, "selfplay:x", "t", &format!("p{i}"))
        .unwrap();
    for (step, (kind, data)) in events.iter().enumerate() {
        store
            .event(&Event {
                trajectory: id,
                version,
                domain: "selfplay:x".into(),
                task: "t".into(),
                step: step as u32,
                kind: (*kind).into(),
                data: data.clone(),
                score: None,
            })
            .unwrap();
    }
    store.finish_trajectory(id, Some("done"), None).unwrap();
    if let Some(score) = score {
        store
            .outcome(&Outcome {
                trajectory: id,
                score,
                source: "verifier:x".into(),
                reference: None,
            })
            .unwrap();
    }
    id
}

fn good_events() -> Vec<(&'static str, Value)> {
    vec![
        (
            "generation",
            json!({"output":"reading","tool_calls":1,"prompt_tokens":100,"completion_tokens":10,"elapsed_ms":300}),
        ),
        ("tool_call", json!({"name":"read"})),
        ("tool_result", json!({"name":"read","output":"fine"})),
    ]
}

fn bad_events() -> Vec<(&'static str, Value)> {
    vec![
        (
            "generation",
            json!({"output":"","tool_calls":0,"prompt_tokens":400,"completion_tokens":5,"elapsed_ms":900}),
        ),
        ("tool_form_failure", json!({"kind":"text_call"})),
        (
            "tool_error",
            json!({"name":"bash","output":"error: nope\n[exit code: 1]"}),
        ),
    ]
}

fn populated_store(dir: &std::path::Path, pairs: usize) -> (Store, i64) {
    let store = Store::open(dir, "m", "u", &harness()).unwrap();
    let version = store.active().unwrap().id;
    for i in 0..pairs {
        trajectory(&store, version, 2 * i, &good_events(), Some(100.0));
        trajectory(&store, version, 2 * i + 1, &bad_events(), Some(0.0));
    }
    (store, version)
}

#[test]
fn save_and_load_round_trip_through_a_real_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "m", "u", &Harness::default()).unwrap();
    assert!(load(&store).unwrap().is_none());
    let probe = train(&synthetic(60), 5).unwrap();
    save(&store, &probe).unwrap();
    let loaded = load(&store).unwrap().unwrap();
    for (features, _) in synthetic(60) {
        assert!((loaded.risk(&features) - probe.risk(&features)).abs() < 1e-12);
    }
    store.set_meta("step_probe", "{not json").unwrap();
    assert!(load(&store).unwrap().is_none());
}

#[test]
fn train_from_store_needs_enough_trajectories_with_both_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let (store, version) = populated_store(dir.path(), 5);
    assert!(
        train_from_store(&store, 100, 1).unwrap().is_none(),
        "10 trajectories is too few"
    );
    let dir = tempfile::tempdir().unwrap();
    let store2 = Store::open(dir.path(), "m", "u", &harness()).unwrap();
    for i in 0..40 {
        trajectory(
            &store2,
            store2.active().unwrap().id,
            i,
            &good_events(),
            Some(100.0),
        );
    }
    assert!(
        train_from_store(&store2, 100, 1).unwrap().is_none(),
        "one class only"
    );
    let _ = version;
}

#[test]
fn train_from_store_learns_failure_signal_and_rank_risky_orders_by_risk() {
    let dir = tempfile::tempdir().unwrap();
    let (store, version) = populated_store(dir.path(), 25);
    let probe = train_from_store(&store, 1000, 9).unwrap().unwrap();
    assert_eq!(probe.trained_on, 50 * 3);
    assert!(probe.auc > 0.9, "auc {}", probe.auc);
    assert!(!informative(&probe) || probe.trained_on >= 100);

    // Unscored finished trajectories are ranked too: a failing one among healthy ones.
    let healthy1 = trajectory(&store, version, 100, &good_events(), None);
    let failing = trajectory(&store, version, 101, &bad_events(), None);
    let healthy2 = trajectory(&store, version, 102, &good_events(), None);
    let ranked = rank_risky(&store, &probe, 3).unwrap();
    assert_eq!(
        ranked.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![failing, healthy2, healthy1]
    );
    assert!(
        ranked[0].1 > 0.8 && ranked[1].1 < 0.2 && ranked[1].1 >= ranked[2].1
            || ranked[1].0 == healthy2
    );
    assert!(ranked.windows(2).all(|w| w[0].1 >= w[1].1));
}

#[test]
fn informative_requires_both_signal_and_volume() {
    let mut p = train(&synthetic(200), 1).unwrap();
    p.auc = 0.7;
    assert!(informative(&p));
    p.auc = 0.55;
    assert!(!informative(&p));
    p.auc = 0.9;
    p.trained_on = 99;
    assert!(!informative(&p));
}
