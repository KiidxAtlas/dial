use super::*;
#[test]
fn printed_success_cannot_replace_observed_process_termination() {
    let check = FocusedCheck {
        name: "fixture".into(),
        command: "cargo check".into(),
        timeout_secs: 1,
    };
    let prose = crate::tools::ToolOutput::ok("checks passed\n[exit code: 0]\n".into());
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
    use crate::tools::{ToolOutput, ToolStatus};
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
    let run = |sequence, output: &ToolOutput| super::super::repair_evidence::CheckRun {
        command: check.command.clone(),
        cwd: "/workspace".into(),
        objective: "fixture".into(),
        kind: super::super::repair_evidence::CheckKind::Behavioral,
        tool_call_id: format!("check-{sequence}"),
        evidence_ref: format!("artifact://check-{sequence}"),
        exit_code: classify(&check, output, false).repair_exit_code(output),
        sequence,
    };
    for unknown in unknowns {
        let mut tracker = super::super::repair_evidence::Tracker::default();
        assert!(tracker.observe(run(1, &failed), vec![]).is_none());
        let unknown_run = run(2, &unknown);
        assert_eq!(unknown_run.exit_code, None, "{}", unknown.text);
        assert!(tracker.observe(unknown_run, vec![]).is_none());
        let change = super::super::repair_evidence::FileChange {
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
