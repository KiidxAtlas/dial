use super::*;
use serde_json::json;

fn measured(name: &str, outcomes: &[bool], wall_time_ms: u64) -> MachineConfigurationReport {
    let configuration = MachineConfiguration {
        name: name.into(),
        provider: "mlx".into(),
        model: "/explicit/model".into(),
        context_window: 8192,
        quantization_bits: Some(4),
        adapter: None,
        assistance: Default::default(),
        draft_model: None,
        num_draft_tokens: None,
        kv_bits: None,
        kv_group_size: None,
        quantized_kv_start: None,
        efficiency: Default::default(),
    };
    let cases = outcomes
        .iter()
        .enumerate()
        .map(|(index, passed)| CaseResult {
            evidence: None,
            id: format!("0:{index}"),
            passed: *passed,
            checks_passed: usize::from(*passed),
            checks_total: 1,
            metrics: RunMetrics {
                elapsed_ms: 100,
                agent_succeeded: true,
                token_usage_observed: true,
                prompt_tokens: 10,
                output_tokens: 5,
                repair_attempts: Some(u64::from(!passed)),
                ..Default::default()
            },
        })
        .collect();
    MachineConfigurationReport::measured(
        configuration,
        serde_json::json!({}),
        score_report("identical", cases),
        wall_time_ms,
    )
}

#[test]
fn recommendations_rank_verified_throughput_without_rewarding_fast_failures() {
    let mut reports = vec![
        measured("baseline", &[true, true], 2000),
        measured("fast-failure", &[false, false], 1),
        measured("fast-correct", &[true, true], 1000),
    ];
    let winner =
        rank_machine_configurations(&mut reports, &MachineBenchmarkPolicy::default()).unwrap();
    assert_eq!(winner.as_deref(), Some("fast-correct"));
    assert_eq!(reports[0].correct_cases_per_second, Some(2.0));
    assert_eq!(reports[0].observed_prompt_tokens, Some(20));
    assert_eq!(reports[0].repair_attempts, Some(0));
    assert!(
        !reports
            .iter()
            .find(|report| report.configuration.name == "fast-failure")
            .unwrap()
            .eligible
    );
}

#[test]
fn correctness_floor_and_per_case_regressions_veto_faster_candidates() {
    let policy = MachineBenchmarkPolicy {
        minimum_correctness: 0.5,
        ..Default::default()
    };
    let mut reports = vec![
        measured("baseline", &[true, false], 2000),
        measured("different-success", &[false, true], 100),
    ];
    assert_eq!(
        rank_machine_configurations(&mut reports, &policy)
            .unwrap()
            .as_deref(),
        Some("baseline")
    );
    assert!(
        reports
            .iter()
            .find(|report| report.configuration.name == "different-success")
            .unwrap()
            .baseline_comparison
            .regressed
    );
    assert_eq!(
        rank_machine_configurations(&mut reports, &MachineBenchmarkPolicy::default()).unwrap(),
        None
    );
}

#[test]
fn regression_metric_budget_is_applied_before_throughput_ranking() {
    let mut reports = vec![
        measured("baseline", &[true], 2000),
        measured("faster", &[true], 100),
    ];
    reports[1].report.cases[0].metrics.prompt_tokens = 100;
    let policy = MachineBenchmarkPolicy {
        regression_thresholds: RegressionThresholds {
            max_prompt_token_increase: Some(0),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        rank_machine_configurations(&mut reports, &policy)
            .unwrap()
            .as_deref(),
        Some("baseline")
    );
}

#[test]
fn unavailable_or_partial_metrics_are_not_reported_as_zero() {
    let mut report = measured("unknown", &[true], 0);
    report.report.cases[0].metrics.token_usage_incomplete = true;
    report.report.cases[0].metrics.repair_attempts = None;
    let report = MachineConfigurationReport::measured(
        report.configuration,
        report.settings,
        report.report,
        0,
    );
    assert_eq!(report.observed_prompt_tokens, None);
    assert_eq!(report.observed_output_tokens, None);
    assert_eq!(report.repair_attempts, None);
    assert_eq!(report.correct_cases_per_second, None);
    assert_eq!(
        rank_machine_configurations(&mut [report], &MachineBenchmarkPolicy::default()).unwrap(),
        None
    );
}

#[test]
fn invalid_configurations_are_never_recommended() {
    let mut reports = vec![
        measured("baseline", &[true], 2000),
        measured("flagged", &[true], 100),
    ];
    reports[1].report.invalid_run = Some("invalid run: test".into());
    assert_eq!(
        rank_machine_configurations(&mut reports, &MachineBenchmarkPolicy::default())
            .unwrap()
            .as_deref(),
        Some("baseline")
    );
    // A baseline that never reached its model makes every comparison against it invalid.
    let mut reports = vec![
        measured("baseline", &[false], 20),
        measured("candidate", &[true], 100),
    ];
    reports[0].report.cases[0].metrics = RunMetrics {
        agent_error: true,
        elapsed_ms: 16,
        ..Default::default()
    };
    assert_eq!(
        rank_machine_configurations(&mut reports, &MachineBenchmarkPolicy::default()).unwrap(),
        None
    );
    assert!(reports.iter().all(|report| {
        report.baseline_comparison.invalid && report.baseline_comparison.regressed
    }));
    let names = named_invalid_runs(
        reports
            .iter()
            .map(|report| (report.configuration.name.as_str(), &report.report)),
    );
    assert_eq!(names.len(), 1);
    assert!(names[0].starts_with("baseline: invalid run: all 1 cases"));
}

#[test]
fn legacy_configurations_and_reports_remain_readable() {
    // Pre-LeanLocal boolean configs and pre-efficiency configurations keep their meaning.
    let legacy: AssistanceOverrides = serde_json::from_value(json!({"lean_local": true})).unwrap();
    assert_eq!(legacy.lean_local, Some(LeanLocal::On));
    // Reports written before the local profile carry no `local_profile`: it stays unset.
    assert_eq!(legacy.local_profile, None);
    let profiled: AssistanceOverrides =
        serde_json::from_value(json!({"local_profile": false})).unwrap();
    assert_eq!(profiled.local_profile, Some(false));
    let auto: AssistanceOverrides = serde_json::from_value(json!({"lean_local": "auto"})).unwrap();
    assert_eq!(auto.lean_local, Some(LeanLocal::Auto));
    let configuration: MachineConfiguration = serde_json::from_value(json!({
        "name": "old", "provider": "mlx", "model": "m", "context_window": 8192
    }))
    .unwrap();
    assert_eq!(configuration.efficiency, EfficiencyOverrides::default());
    // Reports written before the ledger fields have no cost and no request count.
    let metrics: RunMetrics = serde_json::from_value(json!({
        "prompt_tokens": 3, "output_tokens": 4, "elapsed_ms": 5,
        "tool_calls": 0, "tool_errors": 0
    }))
    .unwrap();
    assert_eq!((metrics.observed_cost_usd, metrics.requests), (None, 0));
    // Absent cost is not serialized, so legacy consumers see the same shape.
    let value = serde_json::to_value(metrics).unwrap();
    assert!(value.get("observed_cost_usd").is_none());
}

#[test]
fn cumulative_cost_needs_complete_usage_in_every_case() {
    let mut complete = measured("priced", &[true, true], 1000);
    for case in &mut complete.report.cases {
        case.metrics.observed_cost_usd = Some(0.5);
        case.metrics.observed_cost_complete = true;
    }
    let report = MachineConfigurationReport::measured(
        complete.configuration.clone(),
        complete.settings.clone(),
        complete.report.clone(),
        1000,
    );
    assert_eq!(report.observed_cost_usd, Some(1.0));
    // One case with incomplete usage makes the whole configuration's cost unknown.
    let mut partial = complete.report.clone();
    partial.cases[1].metrics.token_usage_incomplete = true;
    let report = MachineConfigurationReport::measured(
        complete.configuration.clone(),
        complete.settings.clone(),
        partial,
        1000,
    );
    assert_eq!(report.observed_cost_usd, None);
    assert_eq!(report.observed_prompt_tokens, None);
    // Complete token counts do not imply complete prices.
    let mut unpriced = complete.report.clone();
    unpriced.cases[1].metrics.observed_cost_complete = false;
    let report = MachineConfigurationReport::measured(
        complete.configuration.clone(),
        complete.settings.clone(),
        unpriced,
        1000,
    );
    assert_eq!(report.observed_cost_usd, None);
    assert!(report.observed_prompt_tokens.is_some());
    // Local routes with complete usage carry no priced cost, which stays absent, not zero.
    let local = measured("local", &[true], 1000);
    let report = MachineConfigurationReport::measured(
        local.configuration,
        local.settings,
        local.report,
        1000,
    );
    assert_eq!(report.observed_cost_usd, None);
}

#[test]
fn mismatched_tasks_and_unbounded_work_are_rejected() {
    let mut reports = vec![
        measured("baseline", &[true], 100),
        measured("mismatch", &[true], 100),
    ];
    reports[1].report.cases[0].id = "other-task".into();
    assert!(rank_machine_configurations(&mut reports, &MachineBenchmarkPolicy::default()).is_err());
    let suite = BenchmarkSuite {
        version: 1,
        name: "bounded".into(),
        cases: vec![BenchmarkCase {
            id: "case".into(),
            prompt: "solve".into(),
            checks: vec![Assertion::FileExists {
                path: "answer".into(),
            }],
            ..Default::default()
        }],
    };
    let configuration = reports[0].configuration.clone();
    let mut policy = MachineBenchmarkPolicy::default();
    assert!(
        policy
            .validate(&suite, std::slice::from_ref(&configuration))
            .is_ok()
    );
    policy.repetitions = 11;
    assert!(
        policy
            .validate(&suite, std::slice::from_ref(&configuration))
            .is_err()
    );
    policy.repetitions = 1;
    policy.case_timeout_secs = 601;
    assert!(
        policy
            .validate(&suite, std::slice::from_ref(&configuration))
            .is_err()
    );
    policy.case_timeout_secs = 1;
    policy.minimum_correctness = f64::NAN;
    assert!(policy.validate(&suite, &[configuration]).is_err());
}
