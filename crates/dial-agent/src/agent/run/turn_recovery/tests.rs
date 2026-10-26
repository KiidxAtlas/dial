use super::*;
#[test]
fn coherent_runtime_pending_work_shares_recovery_and_finished_reports_do_not_restart() {
    let mut recovery = Recovery::default();
    for _ in 0..3 {
        assert!(matches!(
            recovery.stopped("The requested work is pending.", false),
            StopAction::Continue { .. }
        ));
    }
    assert!(matches!(
        recovery.stopped("Still pending.", false),
        StopAction::Incomplete(_)
    ));
    for text in [
        "`The work is pending.` is a message in the log.",
        "Fixed the parser. Optional: run a deployment later.",
        "Blocked: the host is unavailable.",
    ] {
        assert!(matches!(recovery.stopped(text, false), StopAction::Accept));
    }
}

#[test]
fn malformed_empty_and_promised_replies_share_a_progress_bound() {
    let mut recovery = Recovery::default();
    for _ in 0..3 {
        assert!(recovery.form(true).is_ok());
        assert!(matches!(
            recovery.stopped("", false),
            StopAction::Continue { .. }
        ));
    }
    assert!(recovery.form(true).is_ok());
    assert!(matches!(
        recovery.stopped("I will run the tests next.", false),
        StopAction::Incomplete(_)
    ));
}

#[test]
fn productive_tools_rearm_recovery_without_limiting_work() {
    let mut recovery = Recovery::default();
    for _ in 0..100 {
        assert!(recovery.form(true).is_ok());
        assert!(matches!(recovery.stopped("", true), StopAction::Accept));
    }
}

#[test]
fn promises_cannot_become_completed_after_the_retry_cap() {
    let mut recovery = Recovery::default();
    for _ in 0..3 {
        assert!(matches!(
            recovery.stopped("Let me inspect the parser.", false),
            StopAction::Continue { .. }
        ));
    }
    assert!(matches!(
        recovery.stopped("Let me inspect the parser.", false),
        StopAction::Incomplete(_)
    ));
}

#[test]
fn an_incomplete_stop_keeps_the_last_model_reply() {
    let mut recovery = Recovery::default();
    let mut last = None;
    for _ in 0..4 {
        last = Some(recovery.stopped("Let me inspect the parser.", false));
    }
    match last {
        Some(StopAction::Incomplete(text)) => {
            assert!(text.contains("Stopped with incomplete work"), "{text}");
            assert!(text.contains("Let me inspect the parser."), "{text}");
        }
        _ => panic!("a repeated bare promise stops the turn"),
    }
}

#[test]
fn a_substantive_report_ending_in_a_promise_is_challenged_once_then_accepted() {
    let report = "Fixed the off-by-one in the tokenizer: spans now end at the last byte of the \
        token instead of one past it, and the lexer tests cover multi-byte identifiers and \
        trailing whitespace. Updated the changelog entry for the parser.\n\
        I'll run the full suite next.";
    let mut recovery = Recovery::default();
    assert!(
        matches!(recovery.stopped(report, false), StopAction::Continue { .. }),
        "the matcher's first hit asks the model to act or answer directly"
    );
    assert!(
        matches!(recovery.stopped(report, false), StopAction::Accept),
        "answering again without tools confirms the report"
    );
    // After productive work, a new report is challenged again.
    assert!(matches!(recovery.stopped("", true), StopAction::Accept));
    assert!(matches!(
        recovery.stopped(report, false),
        StopAction::Continue { .. }
    ));
}
