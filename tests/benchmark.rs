use dial::benchmark::{BenchmarkProfile, BenchmarkSuite, RunMetrics, normalize, score_report};
use std::fs;

fn suite() -> BenchmarkSuite {
    BenchmarkSuite::parse_toml(r#"version=1
name="multi"
[[cases]]
id="goal"
prompt="private original goal"
categories=["goal_preservation"]
turns=[{prompt="continue",compact_before=true}]
checks=[{kind="file_equals",path="answer",content="correct"},{kind="metric",metric="compactions",min=1}]
"#).unwrap()
}

#[test]
fn all_capability_examples_have_executable_verified_checks() {
    let suite = BenchmarkSuite::parse_toml(include_str!("../examples/benchmarks/capabilities.toml")).unwrap();
    assert_eq!(suite.cases.len(), 6);
    assert!(suite.cases.iter().all(|case| !case.checks.is_empty()));
    let goal = suite.cases.iter().find(|case| case.id == "goal-compaction").unwrap();
    assert_eq!(goal.turns.len(), 2);
    assert!(goal.turns[1].compact_before);
    assert!(include_str!("../examples/benchmarks/project/context.txt").len() > 100_000);
}

#[test]
fn correct_file_cannot_hide_missing_turn_or_compaction() {
    let source = tempfile::tempdir().unwrap();
    let suite = suite();
    for metrics in [
        RunMetrics { agent_succeeded: true, turns_completed: 1, compactions: 1, ..Default::default() },
        RunMetrics { agent_succeeded: true, turns_completed: 2, ..Default::default() },
        RunMetrics { agent_succeeded: true, turns_completed: 2, compactions: 1, agent_error: true, ..Default::default() },
    ] {
        let report = suite.run(source.path(), |_, workspace| { fs::write(workspace.join("answer"), "correct")?; Ok(metrics) }, |_, _| Ok(true)).unwrap();
        assert!(!report.cases[0].passed);
    }
    let report = suite.run(source.path(), |case, workspace| {
        assert_eq!(case.prompt, "private original goal");
        assert_eq!(case.turns[0].prompt, "continue");
        fs::write(workspace.join("answer"), "correct")?;
        Ok(RunMetrics { agent_succeeded: true, turns_completed: 2, compactions: 1, ..Default::default() })
    }, |_, _| Ok(true)).unwrap();
    assert!(report.cases[0].passed);
    assert!(!serde_json::to_string(&report).unwrap().contains("private original goal"));
}

#[test]
fn profile_ranking_prefers_verified_success_to_speed_and_isolates_workspaces() {
    let source = tempfile::tempdir().unwrap();
    let profiles = vec![
        BenchmarkProfile { name: "fast-failure".into(), ablations: vec!["no_integrations".into()] },
        BenchmarkProfile { name: "slow-success".into(), ablations: vec![] },
    ];
    let mut roots = Vec::new();
    let comparison = suite().run_profiles(source.path(), &profiles, |profile, _, workspace| {
        assert!(!workspace.join("answer").exists());
        roots.push(workspace.to_path_buf());
        fs::write(workspace.join("answer"), if profile.name == "slow-success" { "correct" } else { "incorrect" })?;
        Ok(RunMetrics { agent_succeeded: true, turns_completed: 2, compactions: 1, elapsed_ms: if profile.name == "slow-success" { 1000 } else { 1 }, prompt_tokens: 20, output_tokens: 10, ..Default::default() })
    }, |_, _| Ok(true)).unwrap();
    assert_eq!(comparison.baseline_profile, "fast-failure");
    assert_eq!(comparison.profiles[0].profile.name, "slow-success");
    assert_eq!(comparison.profiles[0].normalized.verified_success_rate, 1.0);
    assert_eq!(comparison.profiles[0].normalized.tokens_relative_to_baseline, Some(1.0));
    assert_eq!(comparison.profiles[1].normalized.successful_latency_ms, None);
    assert_eq!(comparison.profiles[0].categories["goal_preservation"].verified_success_rate, 1.0);
    assert_ne!(roots[0], roots[1]);
    assert!(roots.iter().all(|root| !root.exists()));
}

#[test]
fn empty_normalization_has_no_fabricated_latency_or_ratio() {
    let empty = score_report("empty", vec![]);
    let metrics = normalize(&empty, Some(&empty));
    assert_eq!(metrics.verified_success_rate, 0.0);
    assert_eq!(metrics.successful_latency_ms, None);
    assert_eq!(metrics.latency_relative_to_baseline, None);
    assert_eq!(metrics.tokens_relative_to_baseline, None);
    serde_json::to_string(&metrics).unwrap();
}

#[test]
fn unknown_metrics_empty_turns_and_invalid_bounds_are_rejected() {
    let text = r#"version=1
name="invalid"
[[cases]]
id="case"
prompt="task"
checks=[{kind="metric",metric="compactions",min=2,max=1}]
"#;
    assert!(BenchmarkSuite::parse_toml(text).is_err());
    assert!(BenchmarkSuite::parse_toml(&text.replace("min=2,max=1", "min=0").replace("compactions", "imaginary")).is_err());
    assert!(BenchmarkSuite::parse_toml(&text.replace("min=2,max=1", "min=0").replace("prompt=\"task\"", "prompt=\"task\"\nturns=[{prompt=\" \"}]")).is_err());
    let source = tempfile::tempdir().unwrap();
    let duplicate = vec![BenchmarkProfile {name:"same".into(),ablations:vec![]};2];
    assert!(suite().run_profiles(source.path(), &duplicate, |_, _, _| unreachable!(), |_, _| unreachable!()).is_err());
}

#[test]
fn verifier_error_is_a_failed_check_not_a_success() {
    let suite = BenchmarkSuite::parse_toml(r#"version=1
name="command"
[[cases]]
id="case"
prompt="task"
checks=[{kind="command",command="real verification"}]
"#).unwrap();
    let source = tempfile::tempdir().unwrap();
    let report = suite.run(source.path(), |_, _| Ok(RunMetrics { agent_succeeded:true, ..Default::default() }), |_, _| anyhow::bail!("verifier unavailable")).unwrap();
    assert!(!report.cases[0].passed);
    assert_eq!(report.cases[0].checks_passed, 0);
}

#[test]
fn event_bounds_require_real_observed_recovery_and_limit_error_counts() {
    let suite = BenchmarkSuite::parse_toml(r#"version=1
name="events"
[[cases]]
id="recovery"
prompt="task"
checks=[{kind="metric",metric="tool_contract_errors",min=1,max=1},{kind="metric",metric="artifacts_retrieved",min=1},{kind="metric",metric="dynamic_tools_used",min=1},{kind="metric",metric="truncated_recoveries",min=1}]
"#).unwrap();
    let source = tempfile::tempdir().unwrap();
    for metrics in [
        RunMetrics { agent_succeeded: true, ..Default::default() },
        RunMetrics { agent_succeeded: true, tool_contract_errors: 2, artifacts_retrieved: 1, dynamic_tools_used: 1, truncated_recoveries: 1, ..Default::default() },
    ] {
        assert!(!suite.run(source.path(), |_, _| Ok(metrics), |_, _| unreachable!()).unwrap().cases[0].passed);
    }
    let observed = RunMetrics { agent_succeeded: true, tool_contract_errors: 1, artifacts_retrieved: 1, dynamic_tools_used: 1, truncated_recoveries: 1, ..Default::default() };
    let report = suite.run(source.path(), |_, _| Ok(observed), |_, _| unreachable!()).unwrap();
    assert!(report.cases[0].passed);
    let normalized = normalize(&report, None);
    assert_eq!(normalized.tool_contract_errors_per_case, 1.0);
    assert_eq!(normalized.artifacts_retrieved_per_case, 1.0);
    assert_eq!(normalized.dynamic_tools_used_per_case, 1.0);
    assert_eq!(normalized.truncated_recoveries_per_case, 1.0);
}

#[test]
fn aggregate_token_counters_saturate_instead_of_panicking_or_wrapping() {
    use dial::benchmark::CaseResult;
    let case = CaseResult { id: "case".into(), passed: false, checks_passed: 0, checks_total: 1, metrics: RunMetrics { prompt_tokens: u64::MAX, output_tokens: u64::MAX, tool_calls: u64::MAX, tool_errors: u64::MAX, ..Default::default() } };
    let report = score_report("overflow", vec![case.clone(), case]);
    assert_eq!(report.total_tokens, u64::MAX);
    assert_eq!(report.total_tool_calls, u64::MAX);
    assert_eq!(report.total_tool_errors, u64::MAX);
    serde_json::to_string(&normalize(&report, Some(&report))).unwrap();
}
