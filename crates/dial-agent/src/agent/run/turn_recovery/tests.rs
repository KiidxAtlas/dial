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
