use super::*;
use crate::config::types::SuiteScore;
use serde_json::json;

#[test]
fn metrics_count_actual_calls_and_chatter_not_answers_or_results() {
    let mut metrics = Metrics::default();
    metrics.observe(
        "generation",
        &json!({"tool_calls":1,"output":"look","completion_tokens":7}),
    );
    metrics.observe("tool_call", &json!({"name":"read"}));
    metrics.observe("tool_result", &json!({"output":"long tool output"}));
    metrics.observe(
        "generation",
        &json!({"tool_calls":0,"output":"the requested final answer","completion_tokens":9}),
    );
    metrics.observe("tool_form_failure", &json!({"kind":"schema"}));
    assert_eq!(
        metrics,
        Metrics {
            generations: 2,
            tool_calls: 1,
            investigation_calls: 1,
            protocol_errors: 1,
            chatter_bytes: 4,
            completion_tokens: 16,
            ..Metrics::default()
        }
    );
}

#[test]
fn maintenance_and_repair_costs_do_not_count_as_task_success() {
    let mut metrics = Metrics::default();
    metrics.observe(
        "architecture_synthesis",
        &json!({"completion_tokens":120,"wall_ms":2000}),
    );
    metrics.observe("verification", &json!({"passed":false}));
    metrics.observe("repair_round", &json!({"attempt":1}));
    metrics.observe("change_review", &json!({"path":"main.rs"}));
    let mut combined = Metrics::default();
    combined.add(&metrics);
    assert_eq!(combined.architecture_tokens, 120);
    assert_eq!(combined.elapsed_ms, 2000);
    assert_eq!(
        (
            combined.verification_failures,
            combined.repair_rounds,
            combined.change_reviews
        ),
        (1, 1, 1)
    );
    assert_eq!(combined.generations, 0);
}

#[test]
fn timing_and_repetition_are_observed_not_inferred_from_token_counts() {
    let mut metrics = Metrics::default();
    metrics.observe("generation", &json!({"wall_ms":40,"tool_calls":1}));
    metrics.observe("tool_error", &json!({"name":"read","wall_ms":50}));
    assert_eq!(metrics.first_useful_ms, None);
    metrics.observe("tool_result", &json!({"name":"todo","wall_ms":55}));
    assert_eq!(metrics.first_useful_ms, None);
    metrics.observe("tool_result", &json!({"name":"read","wall_ms":70}));
    metrics.observe("investigation_gate", &json!({"wall_ms":90}));
    assert_eq!(metrics.first_useful_ms, Some(70));
    assert_eq!(metrics.elapsed_ms, 90);
    assert_eq!((metrics.tool_errors, metrics.repeated_calls), (1, 1));
}

fn evidence() -> Evidence {
    Evidence {
        source_version: 1,
        trajectories: 6,
        protocol_trajectories: 3,
        investigation_trajectories: 3,
        failure_kinds: BTreeMap::from([("schema".into(), 3)]),
        observed: Metrics {
            protocol_errors: 3,
            ..Metrics::default()
        },
    }
}

#[test]
fn recurrence_proposes_real_validated_harness_changes_not_verified_claims() {
    let harness = Harness {
        max_steps: 40,
        ..Default::default()
    };
    let failures = BTreeMap::new();
    let tried = HashSet::new();
    let valid = |_: &Harness| true;
    let ctx = mutate::Context {
        failure_kinds: &failures,
        selfplay_rates: &[],
        default_edit_mode: EditMode::Hashline,
        tried: &tried,
        valid: &valid,
    };
    let options = candidates(&harness, &ctx, &evidence(), 8);
    assert_eq!(options.len(), 3);
    assert_eq!(options[0].harness.temperature, 0.0);
    assert_eq!(options[1].harness.edit_mode, Some(EditMode::Replace));
    assert_eq!(options[2].harness.max_steps, 30);
    let unlimited = Harness {
        max_steps: 0,
        ..harness.clone()
    };
    assert!(
        candidates(&unlimited, &ctx, &evidence(), 8)
            .iter()
            .all(|c| c.harness.max_steps == 0)
    );
    assert!(
        options
            .iter()
            .all(|c| c.harness != harness && c.harness.tools == harness.tools)
    );
    let mut one_run = evidence();
    one_run.protocol_trajectories = 1;
    one_run.investigation_trajectories = 1;
    assert!(candidates(&harness, &ctx, &one_run, 8).is_empty());
    let tried = options
        .iter()
        .map(|c| mutate::harness_digest(&c.harness))
        .collect();
    let ctx = mutate::Context {
        tried: &tried,
        ..ctx
    };
    assert!(candidates(&harness, &ctx, &evidence(), 8).is_empty());
}

fn admission(passed: bool, cases: usize) -> Admission {
    Admission {
        baseline: 1,
        candidate: 2,
        passed,
        reason: "sealed gate verdict".into(),
        scores: vec![SuiteScore {
            name: "private".into(),
            primary: true,
            baseline: 50.0,
            candidate: 100.0,
            cases,
            candidate_only: 1,
            baseline_only: 0,
            tier: None,
        }],
    }
}

fn pair(baseline_pass: bool, candidate_pass: bool) -> (CaseMeasurement, CaseMeasurement) {
    (
        CaseMeasurement {
            passed: baseline_pass,
            metrics: Metrics {
                tool_calls: 5,
                investigation_calls: 4,
                protocol_errors: 2,
                ..Metrics::default()
            },
        },
        CaseMeasurement {
            passed: candidate_pass,
            metrics: Metrics {
                tool_calls: 3,
                investigation_calls: 2,
                protocol_errors: 0,
                ..Metrics::default()
            },
        },
    )
}

#[test]
fn efficiency_can_never_override_gate_rejection_or_paired_regression() {
    let mut rejected = admission(false, 1);
    assert!(!compare("workflow:protocol", &mut rejected, &[pair(false, true)]).admitted);
    assert_eq!(rejected.reason, "sealed gate verdict");
    let mut regression = admission(true, 2);
    assert!(
        !compare(
            "workflow:bounded_investigation",
            &mut regression,
            &[pair(true, false), pair(false, true)]
        )
        .admitted
    );
    assert!(regression.reason.contains("previously solved"));
}

#[test]
fn requires_complete_measurements_and_targeted_reduction_without_hidden_tradeoffs() {
    let mut passed = admission(true, 1);
    assert!(compare("workflow:protocol", &mut passed, &[pair(false, true)]).admitted);
    let mut incomplete = admission(true, 2);
    assert!(!compare("workflow:protocol", &mut incomplete, &[pair(false, true)]).admitted);
    let mut unchanged = pair(false, true);
    unchanged.1.metrics = unchanged.0.metrics.clone();
    let mut no_gain = admission(true, 1);
    assert!(!compare("workflow:protocol", &mut no_gain, &[unchanged]).admitted);
    let mut costly = pair(false, true);
    costly.1.metrics.chatter_bytes = 1;
    let mut tradeoff = admission(true, 1);
    assert!(!compare("workflow:protocol", &mut tradeoff, &[costly]).admitted);
}

#[test]
fn profile_provenance_and_measurements_persist_in_model_bound_store() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::default();
    let store = Store::open(dir.path(), "local-model", "coding", &harness).unwrap();
    let base = store.active().unwrap();
    let candidate = Candidate {
        name: "protocol",
        rationale: "recurring failures".into(),
        harness: base.harness.clone(),
        evidence: Evidence {
            source_version: base.id,
            ..evidence()
        },
    };
    let version = store
        .candidate(base.id, "workflow:protocol", &candidate.harness, None)
        .unwrap();
    register(&store, &version, &candidate).unwrap();
    let mut verdict = admission(true, 1);
    verdict.baseline = base.id;
    verdict.candidate = version.id;
    verdict.scores[0].baseline = 0.0;
    verdict.scores.push(SuiteScore {
        name: "guard".into(),
        primary: false,
        baseline: 100.0,
        candidate: 100.0,
        cases: 1,
        candidate_only: 0,
        baseline_only: 0,
        tier: None,
    });
    record_comparison(
        &store,
        &base,
        &version,
        &mut verdict,
        &[pair(false, true), pair(true, true)],
    )
    .unwrap();
    store.admit(&verdict).unwrap();
    drop(store);
    let store = Store::open(dir.path(), "local-model", "coding", &harness).unwrap();
    assert_eq!(store.active().unwrap().id, version.id);
    let proposal: Value = serde_json::from_str(
        &store
            .get_meta(&format!("workflow_proposal:{}", version.id))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(proposal["model"], "local-model");
    let comparison: Comparison = serde_json::from_str(
        &store
            .get_meta(&format!("workflow_comparison:{}", version.id))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(comparison.admitted && comparison.measured_reduction);
    store.rollback().unwrap();
    assert_eq!(store.active().unwrap().id, base.id);
    assert!(Store::open(dir.path(), "different-model", "coding", &harness).is_err());
}
