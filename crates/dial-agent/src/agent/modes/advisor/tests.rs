use super::*;

#[test]
fn review_drives_only_one_repair_and_tracks_local_checks() {
    let mut state = CoachTurn::default();
    state.observe("bash", "test failed", true, true);
    let review = state.parse(r#"{"gaps":[{"requirement":"parser passes","evidence":1,"next_check":"fix parser then rerun test"}]}"#).unwrap();
    let message = state.repair(&review, 1).unwrap();
    assert_eq!(message["role"], "user");
    assert_eq!(message["dial_transient"], true);
    assert!(state.repair(&review, 1).is_none());
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
    assert!(CoachTurn::default().repair(&review, 1).is_none());
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

#[test]
fn selective_review_is_opt_in_and_matches_path_components() {
    use crate::config::types::IndependentReviewConfig;
    let mut config = IndependentReviewConfig::default();
    assert!(!selected_review(&config, &["src/auth/key.rs".into()], true));
    config.enabled = true;
    config.sensitive_paths = vec!["src/auth".into()];
    assert!(selected_review(&config, &["src/auth/key.rs".into()], false));
    assert!(selected_review(&config, &["src/auth".into()], false));
    assert!(!selected_review(&config, &["src/author.rs".into()], false));
    assert!(!selected_review(&config, &[], false));
    assert!(selected_review(&config, &[], true));
    assert!(selected_review(
        &config,
        &vec!["changed.rs".into(); 4],
        false
    ));
}

#[test]
fn configured_repair_allowance_is_bounded_and_no_gaps_never_repairs() {
    let mut state = CoachTurn::default();
    let review = state.parse(r#"{"gaps":[{"requirement":"boundary","evidence":0,"next_check":"run the boundary test"}]}"#).unwrap();
    assert!(state.repair(&review, 2).is_some());
    assert!(state.repair(&review, 2).is_some());
    assert!(state.repair(&review, 2).is_none());
    let empty = state.parse(r#"{"gaps":[]}"#).unwrap();
    assert!(state.repair(&empty, 3).is_none());
}

#[test]
fn review_allowance_survives_continuations_but_resets_on_new_instruction() {
    let control = json!({"completion_review":{"instruction":"request-1","reviews":2,"repairs":1,"unresolved":"boundary gap"}});
    let mut state = CoachTurn::default();
    state.restore_allowance(&control, &json!("request-1"));
    assert_eq!(state.reviews, 2);
    assert_eq!(state.repairs, 1);
    assert!(state.completion_note().contains("boundary gap"));
    state.restore_allowance(&control, &json!("request-2"));
    assert_eq!(state.reviews, 0);
    assert_eq!(state.repairs, 0);
    assert!(state.completion_note().is_empty());
}
