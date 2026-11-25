use super::*;

#[test]
fn lifecycle_counters_are_unknown_for_legacy_reports_and_the_live_slice_parses() {
    let mut metrics = RunMetrics::default();
    assert_eq!(metrics.observed(ObservedMetric::CompletionNudges), None);
    metrics.efficiency = Some(serde_json::json!({"completion_nudges":0,"full_builds":1}));
    assert_eq!(metrics.observed(ObservedMetric::CompletionNudges), Some(0));
    assert_eq!(metrics.observed(ObservedMetric::FullBuilds), Some(1));
    let suite = BenchmarkSuite::parse_toml(include_str!(
        "../../../../examples/benchmarks/lifecycle-efficiency.toml"
    ))
    .unwrap();
    assert_eq!(suite.cases.len(), 3);
}

#[test]
fn lifecycle_counters_sum_observed_turns_not_recovery_snapshots() {
    let mut metrics = RunMetrics {
        efficiency: Some(serde_json::json!({"turns":[
            {"send":1,"report":{"projection_bytes":{"completion_nudges":2}}},
            {"send":2,"report":{"projection_bytes":{"completion_nudges":3}}}
        ],"recovery_observation":{"projection_bytes":{"completion_nudges":99}}})),
        ..Default::default()
    };
    assert_eq!(metrics.observed(ObservedMetric::CompletionNudges), Some(5));
    assert_eq!(metrics.observed(ObservedMetric::FullBuilds), None);
    for turns in [
        serde_json::json!([]),
        serde_json::json!([{"report":{}}]),
        serde_json::json!([
            {"report":{"projection_bytes":{"completion_nudges":u64::MAX}}},
            {"report":{"projection_bytes":{"completion_nudges":1}}}
        ]),
    ] {
        metrics.efficiency = Some(serde_json::json!({"turns":turns}));
        assert_eq!(metrics.observed(ObservedMetric::CompletionNudges), None);
    }
}

fn case(passed: bool, outcome_checks: usize, tokens: u64) -> CaseResult {
    CaseResult {
        id: "case".into(),
        passed,
        checks_passed: usize::from(passed),
        checks_total: 1,
        metrics: RunMetrics {
            prompt_tokens: tokens,
            elapsed_ms: 100,
            agent_succeeded: passed,
            token_usage_observed: true,
            observed_cost_usd: Some(0.5),
            observed_cost_complete: true,
            ..RunMetrics::default()
        },
        evidence: Some(CaseEvidence {
            categories: vec![],
            outcome_checks,
            false_completion: None,
            failed_check_indices: vec![],
            unavailable_check_indices: vec![],
            contract_sha256: String::new(),
            agent_error_detail: None,
        }),
    }
}

#[test]
fn failed_and_unchecked_work_remain_in_efficiency_numerator() {
    let mut attempts = vec![case(true, 1, 10), case(false, 1, 20), case(true, 0, 30)];
    for attempt in &mut attempts {
        attempt.metrics.runtime_usage = Some(serde_json::json!({"all":{"requests":2}}));
    }
    let report = score_report("s", attempts);
    let work = report.useful_work.unwrap();
    assert_eq!(work.verified_completed_cases, 1);
    assert_eq!(work.outcome_checked_cases, 2);
    assert_eq!(work.elapsed_ms_per_verified_completion, Some(300.0));
    assert_eq!(work.requests_per_verified_completion, Some(6.0));
    assert_eq!(work.tokens_per_verified_completion, Some(60.0));
    assert_eq!(work.cost_usd_per_verified_completion, Some(1.5));
}

#[test]
fn unknown_usage_and_no_completions_do_not_become_zero_cost() {
    let mut partial = case(false, 1, 20);
    partial.metrics.token_usage_incomplete = true;
    partial.metrics.observed_cost_complete = false;
    let work = UsefulWork::measured(&[case(true, 1, 10), partial]);
    assert_eq!(work.tokens_per_verified_completion, None);
    assert_eq!(work.requests_per_verified_completion, None);
    assert_eq!(work.cost_usd_per_verified_completion, None);
    let work = UsefulWork::measured(&[case(false, 1, 10), case(true, 0, 20)]);
    assert_eq!(work.elapsed_ms_per_verified_completion, None);
    assert_eq!(work.requests_per_verified_completion, None);
    assert_eq!(work.tokens_per_verified_completion, None);
    assert_eq!(work.cost_usd_per_verified_completion, None);
}

#[test]
fn response_check_requires_the_correct_observed_answer_not_just_no_tools() {
    let suite = BenchmarkSuite::parse_toml(
        r#"version = 1
name = "answer"
[[cases]]
id = "simple"
prompt = "What is 2 + 2?"
checks = [{kind="response_equals",content="4"},{kind="metric",metric="tool_calls",max=0}]
"#,
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    for (answer, completed, tools, passed, false_completion) in [
        (Some(" 4\n"), Some(true), 0, true, Some(false)),
        (Some("5"), Some(true), 0, false, Some(true)),
        (None, Some(true), 0, false, None),
        (Some("4"), Some(true), 1, false, Some(false)), // Only the metric failed.
        (Some("5"), Some(false), 0, false, Some(false)), // Answered, not completed.
        (Some("5"), None, 0, false, None),              // No explicit settlement.
    ] {
        let report = suite
            .run(
                root.path(),
                |_, _| {
                    Ok(RunMetrics {
                        agent_succeeded: true,
                        last_turn_completed: completed,
                        tool_calls: tools,
                        response_sha256: answer.map(response_sha256),
                        ..RunMetrics::default()
                    })
                },
                |_, _| unreachable!(),
            )
            .unwrap();
        assert_eq!(report.cases[0].passed, passed);
        let evidence = report.cases[0].evidence.as_ref().unwrap();
        assert_eq!(evidence.outcome_checks, 1);
        assert_eq!(evidence.false_completion, false_completion);
        assert_eq!(
            evidence.unavailable_check_indices.contains(&0),
            answer.is_none()
        );
        assert!(
            !serde_json::to_string(&report.cases[0].metrics)
                .unwrap()
                .contains(" 4")
        );
    }
}

#[test]
fn lifecycle_read_only_task_requires_the_correct_diagnosis() {
    let mut suite = BenchmarkSuite::parse_toml(include_str!(
        "../../../../examples/benchmarks/lifecycle-efficiency.toml"
    ))
    .unwrap();
    suite.cases.retain(|c| c.id == "investigate-before-asking");
    assert_eq!(suite.cases.len(), 1);
    let case = &mut suite.cases[0];
    // Exercise the shipped independent outcome in isolation from the live efficiency metrics.
    case.checks
        .retain(|c| matches!(c, Assertion::ResponseEquals { .. }));
    assert_eq!(case.checks.len(), 1);
    case.project = None;
    let root = tempfile::tempdir().unwrap();
    for (answer, passed) in [
        ("calc.add: - instead of +", true),
        ("calc.add: + instead of -", false),
    ] {
        let report = suite
            .run(
                root.path(),
                |_, _| {
                    Ok(RunMetrics {
                        agent_succeeded: true,
                        response_sha256: Some(response_sha256(answer)),
                        ..Default::default()
                    })
                },
                |_, _| unreachable!(),
            )
            .unwrap();
        assert_eq!(report.cases[0].passed, passed, "{answer}");
    }
}

#[test]
fn additive_metrics_keep_legacy_reports_readable() {
    let mut report = serde_json::to_value(score_report("s", vec![case(true, 1, 10)])).unwrap();
    report.as_object_mut().unwrap().remove("useful_work");
    let parsed: BenchmarkReport = serde_json::from_value(report).unwrap();
    assert_eq!(parsed.useful_work, None);
    assert_eq!(parsed.cases[0].metrics.runtime_usage, None);
    assert_eq!(parsed.cases[0].metrics.efficiency, None);
}

#[test]
fn manual_review_is_not_verified_useful_work() {
    let mut manual = case(true, 1, 20);
    manual.evidence.as_mut().unwrap().categories = vec!["answer_requires_manual_review".into()];
    let work = UsefulWork::measured(&[case(true, 1, 10), manual, case(true, 0, 30)]);
    assert_eq!(work.verified_completed_cases, 1);
    assert_eq!(work.outcome_checked_cases, 1);
    assert_eq!(work.tokens_per_verified_completion, Some(60.0));
}
