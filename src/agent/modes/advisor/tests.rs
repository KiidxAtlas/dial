use super::*;

#[test]
fn review_drives_only_one_repair_and_tracks_local_checks() {
    let mut state = CoachTurn::default();
    state.observe("bash", "test failed", true, true);
    let review = state.parse(r#"{"gaps":[{"requirement":"parser passes","evidence":1,"next_check":"fix parser then rerun test"}]}"#).unwrap();
    let message = state.repair(&review).unwrap();
    assert_eq!(message["role"], "user");
    assert_eq!(message["dial_transient"], true);
    assert!(state.repair(&review).is_none());
    state.observe("bash", "passed", false, true);
    state.observe("read", "contents", false, false);
    assert_eq!(state.checks_after_repair, 1);
    assert_eq!(state.failed_checks_after_repair, 0);
}

#[test]
fn rejects_ungrounded_or_malformed_reviews_and_accepts_no_gaps() {
    let state = CoachTurn::default();
    assert!(state.parse("looks good").is_err());
    assert!(
        state
            .parse(r#"{"gaps":[{"requirement":"x","evidence":1,"next_check":"test"}]}"#)
            .is_err()
    );
    assert!(
        state
            .parse(r#"{"gaps":[{"requirement":"x","evidence":0,"next_check":""}]}"#)
            .is_err()
    );
    let review = state.parse(r#"{"gaps":[]}"#).unwrap();
    assert!(CoachTurn::default().repair(&review).is_none());
}

#[test]
fn evidence_is_bounded_and_keeps_recent_results() {
    let mut state = CoachTurn::default();
    for n in 0..20 {
        state.observe("read", &format!("{n}:{}", "é".repeat(1000)), false, false);
    }
    let evidence = state.evidence();
    assert_eq!(evidence.len(), MAX_EVIDENCE);
    assert!(evidence[0].contains("12:"));
    assert!(evidence.iter().all(|item| item.len() < 1000));
}

#[test]
fn advisor_flag_is_launch_scoped_and_off_by_default() {
    assert!(!crate::agent::modes::cli::Launch::default().advisor);
}
