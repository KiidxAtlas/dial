use super::*;

#[test]
fn integration_allowlists_are_explicit_and_bounded() {
    assert_eq!(IntegrationOverrides::default().mode, IntegrationMode::Core);
    for input in [
        r#"{"mode":"mcp"}"#,
        r#"{"mode":"browser","mcp_servers":["ambient"]}"#,
        r#"{"mode":"managed_shell","mcp_servers":["ambient"]}"#,
        r#"{"mode":"mcp","mcp_servers":["*"]}"#,
        r#"{"mode":"mcp","mcp_servers":["same","same"]}"#,
    ] {
        assert!(
            serde_json::from_str::<IntegrationOverrides>(input)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let managed: IntegrationOverrides =
        serde_json::from_str(r#"{"mode":"managed_shell"}"#).unwrap();
    assert_eq!(managed.mode, IntegrationMode::ManagedShell);
    managed.validate().unwrap();
    for (text, tool) in [
        (
            include_str!("../../../../examples/benchmarks/integration-browser.toml"),
            "browser",
        ),
        (
            include_str!("../../../../examples/benchmarks/integration-delegation.toml"),
            "task",
        ),
        (
            include_str!("../../../../examples/benchmarks/integration-mcp.toml"),
            "mcp__fixture__multiply",
        ),
    ] {
        let suite = BenchmarkSuite::parse_toml(text).unwrap();
        assert!(
            suite.cases[0]
                .checks
                .iter()
                .any(|c| matches!(c, Assertion::ToolInvoked { tool: name } if name == tool))
        );
        assert!(
            suite.cases[0]
                .checks
                .iter()
                .any(|c| matches!(c, Assertion::Command { .. } | Assertion::FileEquals { .. }))
        );
    }
}

#[test]
fn unmet_integration_prerequisite_is_unavailable_not_a_measured_zero_score() {
    let suite = BenchmarkSuite::parse_toml("version=1\nname='integration'\n[[cases]]\nid='one'\nprompt='test'\nchecks=[{kind='file_exists',path='answer'},{kind='command',command='must not run'}]").unwrap();
    let root = tempfile::tempdir().unwrap();
    let report = suite
        .run(
            root.path(),
            |_, _| Err(AgentUnavailable("MCP unavailable: server missing".into()).into()),
            |_, _| panic!("unavailable prerequisites must not run checks"),
        )
        .unwrap();
    let case = &report.cases[0];
    assert!(!case.passed);
    assert_eq!(
        case.evidence.as_ref().unwrap().unavailable_check_indices,
        [0, 1]
    );
    assert!(report.invalid_reason().is_some());
    assert_eq!(case.metrics.tool_calls, 0);
}

#[test]
fn integration_path_coverage_is_not_an_independent_outcome() {
    let suite = BenchmarkSuite::parse_toml("version=1\nname='path'\n[[cases]]\nid='one'\nprompt='test'\nchecks=[{kind='tool_invoked',tool='browser'}]").unwrap();
    let root = tempfile::tempdir().unwrap();
    let report = suite
        .run(
            root.path(),
            |_, _| {
                Ok(RunMetrics {
                    agent_succeeded: true,
                    tool_trace: Some(ToolTrace {
                        complete: true,
                        successful_requests: 1,
                        invoked: [("browser".into(), 1)].into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            },
            |_, _| panic!("no command checks"),
        )
        .unwrap();
    assert!(report.cases[0].passed);
    assert_eq!(report.cases[0].evidence.as_ref().unwrap().outcome_checks, 0);
}

fn invoked_suite(tool: &str) -> BenchmarkSuite {
    BenchmarkSuite {
        version: 1,
        name: "invoked".into(),
        cases: vec![BenchmarkCase {
            id: "case".into(),
            prompt: "use the tool".into(),
            checks: vec![Assertion::ToolInvoked { tool: tool.into() }],
            ..Default::default()
        }],
    }
}

fn traced(invoked: &[(&str, u64)], complete: bool) -> RunMetrics {
    RunMetrics {
        agent_succeeded: true,
        tool_trace: Some(ToolTrace {
            complete,
            successful_requests: 1,
            invoked: invoked.iter().map(|(n, c)| ((*n).into(), *c)).collect(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn tool_invoked_distinguishes_observed_calls_missing_calls_and_incomplete_traces() {
    // A positive count proves a call even in an incomplete trace. Absence proves non-use only
    // when the trace is complete; otherwise the integration-path check is unavailable.
    let cases = [
        (traced(&[("ast_search", 2)], true), true, false),
        (traced(&[("ast_search", 0)], true), false, false),
        (traced(&[("tool_search", 1)], true), false, false),
        (traced(&[("ast_search", 1)], false), true, false),
        (traced(&[], false), false, true),
    ];
    for (metrics, want_pass, unavailable) in cases {
        let root = tempfile::tempdir().unwrap();
        let run_metrics = metrics.clone();
        let report = invoked_suite("ast_search")
            .run(
                root.path(),
                move |_, _| Ok(run_metrics.clone()),
                |_, _| panic!("no command checks"),
            )
            .unwrap();
        let case = &report.cases[0];
        assert_eq!(case.passed, want_pass, "trace {metrics:?}");
        assert_eq!(
            case.evidence.as_ref().unwrap().unavailable_check_indices,
            if unavailable { vec![0] } else { vec![] },
            "trace {metrics:?}"
        );
    }
}

#[test]
fn tool_invoked_without_any_trace_is_unavailable_and_fails_the_case() {
    let root = tempfile::tempdir().unwrap();
    let report = invoked_suite("ast_search")
        .run(
            root.path(),
            |_, _| {
                Ok(RunMetrics {
                    agent_succeeded: true,
                    ..Default::default()
                })
            },
            |_, _| panic!("no command checks"),
        )
        .unwrap();
    let case = &report.cases[0];
    assert!(!case.passed);
    assert_eq!(
        case.evidence.as_ref().unwrap().unavailable_check_indices,
        [0]
    );
    assert_eq!(case.evidence.as_ref().unwrap().failed_check_indices, [0]);
}

#[test]
fn tool_invoked_rejects_empty_control_and_overlong_names_at_parse_time() {
    // TOML basic strings: \u0007 is a valid escape, so these fail on the tool name, not on TOML syntax.
    let bad_names = [
        r#""""#.to_owned(),
        r#""bad\u0007name""#.to_owned(),
        format!("\"{}\"", "a".repeat(257)),
    ];
    for bad in bad_names {
        let text = format!(
            "version=1\nname='x'\n[[cases]]\nid='c'\nprompt='p'\nchecks=[{{kind='tool_invoked',tool={bad}}}]"
        );
        let error =
            BenchmarkSuite::parse_toml(&text).expect_err("invalid tool name must be refused");
        assert_eq!(
            error.to_string(),
            "tool_invoked needs an exact nonempty tool name",
            "error must name the tool check: {error:#}"
        );
    }
}

#[test]
fn local_task_suite_and_both_profile_templates_pass_the_runner_contract() {
    let suite = BenchmarkSuite::parse_toml(include_str!(
        "../../../../examples/benchmarks/local-tasks.toml"
    ))
    .unwrap();
    assert_eq!(suite.cases.len(), 5);
    for (text, allow_remote) in [
        (
            include_str!("../../../../examples/benchmarks/local-tasks-profiles.toml"),
            false,
        ),
        (
            include_str!("../../../../examples/benchmarks/local-tasks-hosted-profiles.toml"),
            true,
        ),
    ] {
        let profile: toml::Table = toml::from_str(text).unwrap();
        let policy: MachineBenchmarkPolicy = profile["machine"].clone().try_into().unwrap();
        let configurations: Vec<MachineConfiguration> =
            profile["configurations"].clone().try_into().unwrap();
        policy.validate(&suite, &configurations).unwrap();
        assert_eq!(policy.allow_remote, allow_remote);
        assert!(
            configurations
                .iter()
                .all(|config| config.integration.mode == IntegrationMode::Core)
        );
    }
}
