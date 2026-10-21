use super::*;

#[test]
fn integration_allowlists_are_explicit_and_bounded() {
    assert_eq!(IntegrationOverrides::default().mode, IntegrationMode::Core);
    for input in [
        r#"{"mode":"mcp"}"#,
        r#"{"mode":"browser","mcp_servers":["ambient"]}"#,
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
