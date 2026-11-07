use super::*;
#[test]
fn nonempty_reply_recovery_defers_semantic_completion_to_controller_state() {
    for text in [
        "Still pending.",
        "Next I'd inspect the source.",
        "All done.",
        "Blocked: no access.",
    ] {
        assert!(matches!(
            Recovery::default().stopped(text, false),
            StopAction::Accept
        ));
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
        recovery.stopped("", false),
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
fn empty_replies_exhaust_bounded_recovery() {
    let mut recovery = Recovery::default();
    for _ in 0..3 {
        assert!(matches!(
            recovery.stopped("", false),
            StopAction::Continue { .. }
        ));
    }
    assert!(matches!(
        recovery.stopped("", false),
        StopAction::Incomplete(_)
    ));
}
