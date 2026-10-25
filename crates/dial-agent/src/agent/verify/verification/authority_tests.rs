use super::*;
#[test]
fn printed_success_cannot_replace_observed_process_termination() {
    let check = FocusedCheck {
        name: "fixture".into(),
        command: "cargo check".into(),
        timeout_secs: 1,
    };
    let prose =
        crate::runtime::tool_output::ToolOutput::ok("checks passed\n[exit code: 0]\n".into());
    assert_eq!(classify(&check, &prose, false), CheckStatus::Unverified);
    let failed = prose.with_process(crate::runtime::execution::ProcessOutcome {
        started: true,
        exit_code: Some(1),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    });
    assert_eq!(classify(&check, &failed, false), CheckStatus::CodeFailed);
}

#[test]
fn repair_exits_use_typed_completion_and_invalidate_unknown_runs() {
    use crate::runtime::execution::ProcessOutcome;
    use crate::runtime::tool_output::ToolOutput;
    use crate::tools::ToolStatus;
    let check = FocusedCheck {
        name: "python AST: fixture".into(),
        command: "python -m pytest".into(),
        timeout_secs: 1,
    };
    let process = ProcessOutcome {
        started: true,
        exit_code: Some(0),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    };
    let passed = ToolOutput::ok("[exit code: 9]".into()).with_process(process.clone());
    let failed = ToolOutput::err("[exit code: 0]").with_process(ProcessOutcome {
        exit_code: Some(7),
        ..process.clone()
    });
    for (output, expected) in [(&passed, Some(0)), (&failed, Some(7))] {
        let verdict = classify(&check, output, false);
        assert_eq!(verdict.repair_exit_code(output), expected);
    }
    let unknowns = [
        ToolOutput::ok("[exit code: 0]".into()),
        ToolOutput::err("[exit code: 7]"),
        ToolOutput::ok("partial\n[exit code: 0]".into()).with_process(ProcessOutcome {
            output_complete: false,
            ..process.clone()
        }),
        ToolOutput::ok("Ran 0 tests\n[exit code: 0]".into()).with_process(process.clone()),
        ToolOutput::err("No module named pytest\n[exit code: 1]").with_process(ProcessOutcome {
            exit_code: Some(1),
            ..process.clone()
        }),
        passed.clone().with_status(ToolStatus::Blocked),
        passed.clone().with_process(ProcessOutcome {
            cancelled: true,
            ..process.clone()
        }),
        passed.clone().with_process(ProcessOutcome {
            timed_out: true,
            ..process
        }),
    ];
    let run = |sequence, output: &ToolOutput| crate::storage::repair_evidence::CheckRun {
        command: check.command.clone(),
        cwd: "/workspace".into(),
        objective: "fixture".into(),
        kind: crate::storage::repair_evidence::CheckKind::Behavioral,
        tool_call_id: format!("check-{sequence}"),
        evidence_ref: format!("artifact://check-{sequence}"),
        exit_code: classify(&check, output, false).repair_exit_code(output),
        sequence,
    };
    for unknown in unknowns {
        let mut tracker = crate::agent::verify::repair_evidence::Tracker::default();
        assert!(tracker.observe(run(1, &failed), vec![]).is_none());
        let unknown_run = run(2, &unknown);
        assert_eq!(unknown_run.exit_code, None, "{}", unknown.text);
        assert!(tracker.observe(unknown_run, vec![]).is_none());
        let change = crate::storage::repair_evidence::FileChange {
            path: "fixture.py".into(),
            before_sha256: Some("a".repeat(64)),
            after_sha256: Some("b".repeat(64)),
            before_ref: Some("artifact://before".into()),
            after_ref: Some("artifact://after".into()),
        };
        assert!(tracker.observe(run(3, &passed), vec![change]).is_none());
    }
    assert_eq!(
        classify(&check, &passed, true).repair_exit_code(&passed),
        None
    );
}

#[test]
fn declared_runner_diagnosis_requires_launch_facts_and_preserves_app_errors() {
    use crate::runtime::{execution::ProcessOutcome, tool_output::ToolOutput};
    let check = FocusedCheck {
        name: "custom".into(),
        command: "python3 -m pytest tests".into(),
        timeout_secs: 1,
    };
    let runner = Some(("python3", Some("pytest")));
    let failed = |text: &str| {
        ToolOutput::err(text).with_process(ProcessOutcome {
            started: true,
            exit_code: Some(1),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        })
    };
    for text in [
        "python3: No module named pytest",
        "python3: command not found",
    ] {
        assert_eq!(
            classify_with_runner(Some(&check), runner, &failed(text), false),
            CheckStatus::EnvironmentUnavailable
        );
        assert_eq!(
            classify_with_runner(Some(&check), runner, &ToolOutput::err(text), false),
            CheckStatus::Unverified
        );
    }
    for text in [
        "ModuleNotFoundError: No module named 'app'",
        "ModuleNotFoundError: No module named 'pytest'",
        "AssertionError: broken\npython3: command not found",
    ] {
        assert_eq!(
            classify_with_runner(Some(&check), runner, &failed(text), false),
            CheckStatus::CodeFailed
        );
    }
    let nested = FocusedCheck {
        command: "echo python3 && python3 -m pytest".into(),
        ..check.clone()
    };
    assert_eq!(
        classify_with_runner(
            Some(&nested),
            runner,
            &failed("python3: No module named pytest"),
            false
        ),
        CheckStatus::CodeFailed
    );
    let app = FocusedCheck {
        command: "python3 app.py".into(),
        ..check
    };
    assert_eq!(
        classify_with_runner(
            Some(&app),
            runner,
            &failed("python3: No module named pytest"),
            false
        ),
        CheckStatus::CodeFailed
    );
}

#[test]
fn rust_single_test_targets_are_not_hidden_by_empty_targets() {
    use crate::runtime::{execution::ProcessOutcome, tool_output::ToolOutput};
    let check = FocusedCheck {
        name: "workspace tests".into(),
        command: "cargo test --workspace".into(),
        timeout_secs: 1,
    };
    let completed = |text: &str| {
        ToolOutput::ok(text.into()).with_process(ProcessOutcome {
            started: true,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        })
    };
    for text in [
        "running 1 test\ntest only_test ... ok\nrunning 0 tests\n",
        "running 0 tests\nrunning 1 test\ntest only_test ... ok\n",
        "running 1 test\ntest only_test ... ok\n",
    ] {
        assert!(!zero_tests(text), "{text}");
        assert_eq!(
            classify(&check, &completed(text), false),
            CheckStatus::Passed
        );
    }
    let empty = "running 0 tests\nrunning 0 tests\n";
    assert!(zero_tests(empty));
    assert_eq!(
        classify(&check, &completed(empty), false),
        CheckStatus::Unverified
    );
}

#[test]
fn clipped_checks_use_complete_artifacts_and_never_infer_from_missing_evidence() {
    use crate::runtime::{
        execution::{Completeness, ProcessOutcome},
        tool_output::ToolOutput,
    };
    let root = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(root.path()).unwrap();
    for (raw, expected) in [
        (
            format!(
                "{}\nRan 0 tests\n{}",
                "prefix\n".repeat(4000),
                "suffix\n".repeat(4000)
            ),
            CheckStatus::Unverified,
        ),
        (
            "running 1 test\nrunning 0 tests\n".into(),
            CheckStatus::Passed,
        ),
        (
            "? pkg [no test files]\nok other 0.01s\n".into(),
            CheckStatus::Passed,
        ),
        ("build complete\n".into(), CheckStatus::Passed),
        ("x".repeat(1024 * 1024 + 1), CheckStatus::Unverified),
    ] {
        let (uri, mut integrity) = store.retain_with_integrity(&raw).unwrap();
        integrity.capture = Completeness::Complete;
        integrity.presentation = Completeness::Partial;
        integrity.artifact = Some(uri);
        let display = if expected == CheckStatus::Unverified {
            "prefix\n[... clipped ...]\nsuffix\n[exit code: 0]"
        } else {
            "running 0 tests\n[exit code: 0]"
        };
        let mut output = ToolOutput::ok(display.into())
            .with_process(ProcessOutcome {
                started: true,
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                cancelled: false,
                output_complete: true,
            })
            .with_output_integrity(integrity);
        assert_eq!(
            classify_with_artifacts(None, None, &output, false, Some(&store)),
            expected
        );
        assert_eq!(
            classify_output(None, &output, false),
            CheckStatus::Unverified
        );
        output.execution.output.as_mut().unwrap().capture = Completeness::Partial;
        assert_eq!(
            classify_with_artifacts(None, None, &output, false, Some(&store)),
            CheckStatus::Unverified
        );
        output.execution.output.as_mut().unwrap().capture = Completeness::Complete;
        output.execution.output.as_mut().unwrap().retention = Completeness::Partial;
        assert_eq!(
            classify_with_artifacts(None, None, &output, false, Some(&store)),
            CheckStatus::Unverified
        );
        output.execution.output.as_mut().unwrap().retention = Completeness::Complete;
        output.execution.output.as_mut().unwrap().artifact =
            Some(format!("artifact://{}", "0".repeat(64)));
        assert_eq!(
            classify_with_artifacts(None, None, &output, false, Some(&store)),
            CheckStatus::Unverified
        );
    }
}
