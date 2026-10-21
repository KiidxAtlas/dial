use super::*;
use crate::workspace_copy::{DEPENDENCY_DIRS, SECRET_DIRS};
use serde_json::json;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;

#[test]
fn efficiency_suite_and_fixture_are_valid() {
    let suite = BenchmarkSuite::parse_toml(include_str!(
        "../../../../examples/benchmarks/efficiency.toml"
    ))
    .unwrap();
    assert_eq!(suite.cases.len(), 5);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for case in &suite.cases {
        let fixture = root.join(case.project.as_ref().unwrap());
        assert!(fixture.join("calc.py").is_file());
        assert!(fixture.join("message.py").is_file());
        assert!(!case.checks.is_empty());
    }
    assert_eq!(suite.cases[4].turns.len(), 2);
    assert!(suite.cases[4].turns[1].compact_before);
}

#[test]
fn representative_suite_uses_external_outcomes_and_real_projects() {
    let suite = BenchmarkSuite::parse_toml(include_str!(
        "../../../../examples/benchmarks/representative.toml"
    ))
    .unwrap();
    assert_eq!(suite.cases.len(), 3);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for case in &suite.cases {
        assert!(root.join(case.project.as_ref().unwrap()).is_dir());
        assert!(
            case.checks
                .iter()
                .any(|c| matches!(c, Assertion::Command { .. }))
        );
        assert!(
            !case
                .checks
                .iter()
                .any(|c| matches!(c, Assertion::Metric { .. }))
        );
    }
    let journey = &suite.cases[2];
    assert_eq!(journey.turns.len(), 6);
    assert_eq!(journey.turns.iter().filter(|t| t.compact_before).count(), 3);
}

#[test]
fn parsing_limits_and_scoring_are_deterministic_and_serializable() {
    let suite = BenchmarkSuite::parse_toml(
        r#"version = 1
name = "basic"
[[cases]]
id = "one"
prompt = "private"
checks = [{ kind = "file_exists", path = "answer.txt" }]
"#,
    )
    .unwrap();
    assert_eq!(suite.cases[0].id, "one");
    let results = vec![
        CaseResult {
            evidence: None,
            id: "one".into(),
            passed: true,
            checks_passed: 1,
            checks_total: 1,
            metrics: RunMetrics {
                prompt_tokens: 4,
                output_tokens: 8,
                elapsed_ms: 10,
                tool_calls: 2,
                tool_errors: 1,
                agent_succeeded: true,
                ..Default::default()
            },
        },
        CaseResult {
            evidence: None,
            id: "two".into(),
            passed: false,
            checks_passed: 0,
            checks_total: 1,
            metrics: RunMetrics {
                prompt_tokens: 2,
                output_tokens: 1,
                elapsed_ms: 20,
                tool_calls: 1,
                tool_errors: 0,
                ..Default::default()
            },
        },
    ];
    let report = score_report("s", results.clone());
    assert_eq!(report, score_report("s", results));
    assert_eq!(report.pass_rate, 0.5);
    assert_eq!(report.average_latency_ms, 15.0);
    assert_eq!(
        (
            report.total_tokens,
            report.total_tool_calls,
            report.total_tool_errors
        ),
        (15, 3, 1)
    );
    let value = serde_json::to_value(report).unwrap();
    assert!(value.pointer("/cases/0/prompt").is_none());
    assert_eq!(
        serde_json::from_value::<BenchmarkReport>(value)
            .unwrap()
            .pass_rate,
        0.5
    );
    assert!(BenchmarkSuite::parse_toml("name='x'\n").is_err());
}

#[test]
fn rejects_unknown_versions_and_cases_without_deterministic_checks() {
    let unknown_version = r#"version = 2
name = "unsupported"
[[cases]]
id = "case"
prompt = "do work"
checks = [{ kind = "file_exists", path = "result.txt" }]
"#;
    assert!(BenchmarkSuite::parse_toml(unknown_version).is_err());
    let no_checks = r#"version = 1
name = "unchecked"
[[cases]]
id = "case"
prompt = "do work"
"#;
    assert!(BenchmarkSuite::parse_toml(no_checks).is_err());
}

#[test]
fn failed_checks_never_pass_and_command_checks_are_delegated() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("present"), "ok").unwrap();
    let suite = BenchmarkSuite {
        version: 1,
        name: "x".into(),
        cases: vec![BenchmarkCase {
            id: "c".into(),
            project: None,
            prompt: "do not report".into(),
            checks: vec![
                Assertion::FileExists {
                    path: "missing".into(),
                },
                Assertion::Command {
                    command: "opaque command".into(),
                    expect_stdout: None,
                    expect_exit: 0,
                },
            ],
            ..Default::default()
        }],
    };
    let report = suite
        .run(
            source.path(),
            |_, _| Ok(RunMetrics::default()),
            |check, _| {
                assert_eq!(check.command, "opaque command");
                Ok(false)
            },
        )
        .unwrap();
    assert!(!report.cases[0].passed);
    assert_eq!(report.cases[0].checks_passed, 0);
}
#[test]
fn agent_errors_fail_even_when_deterministic_checks_match() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("result.txt"), "correct").unwrap();
    let suite = BenchmarkSuite {
        version: 1,
        name: "agent-error".into(),
        cases: vec![BenchmarkCase {
            id: "case".into(),
            project: None,
            prompt: "produce result".into(),
            checks: vec![Assertion::FileEquals {
                path: "result.txt".into(),
                content: "correct".into(),
            }],
            ..Default::default()
        }],
    };
    let report = suite
        .run(
            source.path(),
            |_, _| anyhow::bail!("agent failed"),
            |_, _| Ok(true),
        )
        .unwrap();
    assert!(!report.cases[0].passed);
    assert_eq!(report.cases[0].checks_passed, 1);
    assert!(report.cases[0].metrics.agent_error);
    assert!(!report.cases[0].metrics.agent_succeeded);
    let old_metrics = serde_json::json!({"prompt_tokens": 3, "output_tokens": 4, "elapsed_ms": 5, "tool_calls": 0, "tool_errors": 0});
    let parsed: RunMetrics = serde_json::from_value(old_metrics).unwrap();
    assert!(!parsed.agent_succeeded && !parsed.agent_error);
}

fn two_case_suite() -> BenchmarkSuite {
    let case = |id: &str| BenchmarkCase {
        id: id.into(),
        prompt: "write answer".into(),
        checks: vec![Assertion::FileExists {
            path: "answer".into(),
        }],
        ..Default::default()
    };
    BenchmarkSuite {
        version: 1,
        name: "invalid".into(),
        cases: vec![case("a"), case("b")],
    }
}

fn credentials_failure() -> anyhow::Error {
    AgentFailure {
        metrics: RunMetrics {
            elapsed_ms: 17,
            ..Default::default()
        },
        detail: failure_detail(
            "benchmark initialization failed: \"provider credentials are missing\"",
            &format!(
                "{}\nlast words Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345",
                "noise ".repeat(2_000)
            ),
        ),
    }
    .into()
}

#[test]
fn agent_failure_details_are_recorded_redacted_and_bounded() {
    let source = tempfile::tempdir().unwrap();
    let report = two_case_suite()
        .run(
            source.path(),
            |case, _| match case.id.as_str() {
                "a" => Err(credentials_failure()),
                _ => anyhow::bail!("plain runner error"),
            },
            |_, _| Ok(true),
        )
        .unwrap();
    let detail = |index: usize| {
        report.cases[index]
            .evidence
            .as_ref()
            .unwrap()
            .agent_error_detail
            .clone()
            .unwrap()
    };
    let first = detail(0);
    assert!(first.len() <= MAX_FAILURE_DETAIL_BYTES, "{}", first.len());
    assert!(first.starts_with("benchmark initialization failed"));
    assert!(first.contains("credentials are missing"));
    assert!(first.ends_with("Bearer [REDACTED:BearerToken]"), "{first}");
    assert!(!first.contains("abcdefghijklmnopqrstuvwxyz012345"));
    assert_eq!(report.cases[0].metrics.elapsed_ms, 17);
    assert!(report.cases[0].metrics.agent_error && !report.cases[0].metrics.agent_succeeded);
    assert_eq!(detail(1), "plain runner error");
    // Multibyte text is cut on character boundaries, never past the byte budget.
    let long = failure_detail(&"é".repeat(5_000), &"ü".repeat(5_000));
    assert!(long.len() <= MAX_FAILURE_DETAIL_BYTES);
    // Successful runs carry no detail and keep the legacy JSON shape.
    let ok = two_case_suite()
        .run(
            source.path(),
            |_, _| {
                Ok(RunMetrics {
                    agent_succeeded: true,
                    turns_completed: 1,
                    ..Default::default()
                })
            },
            |_, _| Ok(true),
        )
        .unwrap();
    let value = serde_json::to_value(&ok).unwrap();
    assert!(value.get("invalid_run").is_none());
    assert!(
        value
            .pointer("/cases/0/evidence/agent_error_detail")
            .is_none()
    );
}

#[test]
fn runs_without_model_activity_are_invalid_and_refused_as_baselines() {
    let source = tempfile::tempdir().unwrap();
    let suite = two_case_suite();
    let invalid = suite
        .run(
            source.path(),
            |_, _| Err(credentials_failure()),
            |_, _| Ok(true),
        )
        .unwrap();
    let reason = invalid.invalid_run.clone().unwrap();
    assert!(reason.contains("all 2 cases") && reason.contains("credentials are missing"));
    // Legacy reports lack the field; the reason is re-derived from their cases.
    let mut legacy = serde_json::to_value(&invalid).unwrap();
    legacy.as_object_mut().unwrap().remove("invalid_run");
    let legacy: BenchmarkReport = serde_json::from_value(legacy).unwrap();
    assert_eq!(legacy.invalid_run, None);
    assert!(legacy.invalid_reason().is_some());

    // Any observed activity, or a runner that did not report an error, is a real result.
    for metrics in [
        RunMetrics {
            tool_calls: 1,
            ..Default::default()
        },
        RunMetrics {
            token_usage_incomplete: true,
            ..Default::default()
        },
        RunMetrics {
            turns_completed: 1,
            ..Default::default()
        },
    ] {
        let report = suite
            .run(
                source.path(),
                |case, _| match case.id.as_str() {
                    "a" => Err(AgentFailure {
                        metrics: metrics.clone(),
                        detail: "stopped".into(),
                    }
                    .into()),
                    _ => Err(credentials_failure()),
                },
                |_, _| Ok(true),
            )
            .unwrap();
        assert_eq!(report.invalid_reason(), None, "{metrics:?}");
    }
    let valid = suite
        .run(
            source.path(),
            |_, _| Ok(RunMetrics::default()),
            |_, _| Ok(true),
        )
        .unwrap();
    assert_eq!(valid.invalid_reason(), None);

    let comparison = compare_baseline(&valid, &invalid);
    assert!(comparison.invalid && comparison.regressed);
    let comparison = compare_baseline(&invalid, &valid);
    assert!(comparison.invalid && comparison.regressed);
    assert!(!compare_baseline(&valid, &valid).invalid);
    let legacy_comparison: BaselineComparison =
        serde_json::from_value(json!({"score_delta":0.0,"pass_rate_delta":0.0,"regressed":false}))
            .unwrap();
    assert!(!legacy_comparison.invalid);

    let profiles = [
        BenchmarkProfile {
            name: "working".into(),
            ablations: vec![],
        },
        BenchmarkProfile {
            name: "unconfigured".into(),
            ablations: vec![],
        },
    ];
    let comparison = suite
        .run_profiles(
            source.path(),
            &profiles,
            |profile, _, _| match profile.name.as_str() {
                "working" => Ok(RunMetrics::default()),
                _ => Err(credentials_failure()),
            },
            |_, _| Ok(true),
        )
        .unwrap();
    assert_eq!(comparison.invalid_runs.len(), 1);
    assert!(comparison.invalid_runs[0].starts_with("unconfigured: invalid run"));
}

#[test]
fn clones_are_isolated_and_skip_generated_directories() {
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("file"), "original").unwrap();
    for excluded in SECRET_DIRS.iter().chain(DEPENDENCY_DIRS) {
        fs::create_dir(source.path().join(excluded)).unwrap();
        fs::write(source.path().join(excluded).join("x"), "x").unwrap();
    }
    let first = clone_project(source.path()).unwrap();
    let second = clone_project(source.path()).unwrap();
    assert_ne!(first.path(), second.path());
    fs::write(first.path().join("file"), "changed").unwrap();
    assert_eq!(
        fs::read_to_string(second.path().join("file")).unwrap(),
        "original"
    );
    assert!(
        SECRET_DIRS
            .iter()
            .chain(DEPENDENCY_DIRS)
            .all(|d| !first.path().join(d).exists())
    );
    assert_eq!(
        json!({"pass": score_report("", vec![]).pass_rate}),
        json!({"pass": 0.0})
    );
}

#[test]
fn rejects_escape_paths_and_symlinks_and_detects_baseline_regressions() {
    let outer = tempfile::tempdir().unwrap();
    let source = outer.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(outer.path().join("secret"), "private").unwrap();
    symlink(outer.path().join("secret"), source.join("escape")).unwrap();
    assert!(clone_project(&source).is_err());
    assert!(validate_relative(Path::new("../secret")).is_err());
    let metrics = RunMetrics {
        prompt_tokens: 5,
        output_tokens: 8,
        elapsed_ms: 10,
        tool_calls: 2,
        tool_errors: 0,
        agent_succeeded: true,
        ..Default::default()
    };
    let good = score_report(
        "s",
        vec![CaseResult {
            evidence: None,
            id: "a".into(),
            passed: true,
            checks_passed: 1,
            checks_total: 1,
            metrics: metrics.clone(),
        }],
    );
    let bad = score_report(
        "s",
        vec![CaseResult {
            evidence: None,
            id: "a".into(),
            passed: false,
            checks_passed: 0,
            checks_total: 1,
            metrics: metrics.clone(),
        }],
    );
    assert!(compare_baseline(&bad, &good).regressed);
    assert!(!compare_baseline(&good, &good).regressed);
    let slower = score_report(
        "s",
        vec![CaseResult {
            evidence: None,
            id: "a".into(),
            passed: true,
            checks_passed: 1,
            checks_total: 1,
            metrics: RunMetrics {
                elapsed_ms: 20,
                ..metrics.clone()
            },
        }],
    );
    assert!(
        compare_baseline_with_thresholds(
            &slower,
            &good,
            RegressionThresholds {
                max_latency_increase_ms: Some(5),
                ..Default::default()
            }
        )
        .regressed
    );
}
