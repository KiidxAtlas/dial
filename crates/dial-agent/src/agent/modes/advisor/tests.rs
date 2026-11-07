use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

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
fn selective_review_matches_thresholds_and_path_components() {
    use crate::config::types::IndependentReviewConfig;
    let mut config = IndependentReviewConfig::default();
    let covered = json!({"requirements":{"coverage":"declared_requirements_observed"}});
    assert!(
        config.enabled.is_none(),
        "unset means auto: on with a usable coach"
    );
    assert!(!selected_review(
        &config,
        &["src/auth/key.rs".into()],
        false,
        &covered
    ));
    config.sensitive_paths = vec!["src/auth".into()];
    assert!(selected_review(
        &config,
        &["src/auth/key.rs".into()],
        false,
        &covered
    ));
    assert!(selected_review(
        &config,
        &["src/auth".into()],
        false,
        &covered
    ));
    assert!(!selected_review(
        &config,
        &["src/author.rs".into()],
        false,
        &covered
    ));
    assert!(!selected_review(&config, &[], false, &json!({})));
    assert!(selected_review(&config, &[], true, &covered));
    // No declared requirements: one changed file stays below `min_changed_files`.
    for evidence in [
        json!({}),
        json!({"requirements":{"coverage":"not_independently_assessed"}}),
    ] {
        assert!(!selected_review(
            &config,
            &["result.csv".into()],
            false,
            &evidence
        ));
    }
    assert!(selected_review(
        &config,
        &["result.csv".into()],
        false,
        &json!({"requirements":{"coverage":"requirements_uncovered"}})
    ));
    assert!(selected_review(
        &config,
        &vec!["changed.rs".into(); 4],
        false,
        &covered
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

#[test]
fn round_review_parses_severities_and_rejects_malformed_output() {
    let round =
        parse_round(r#"{"severity":"concern","note":"  the test was never run  "}"#).unwrap();
    assert_eq!(round.severity, Severity::Concern);
    assert_eq!(round.note, "the test was never run");
    assert_eq!(
        parse_round(r#"{"severity":"none"}"#).unwrap().severity,
        Severity::None
    );
    assert!(parse_round("looks fine").is_err());
    assert!(parse_round(r#"{"severity":"fatal","note":"x"}"#).is_err());
    assert!(parse_round(r#"{"severity":"blocker","note":"  "}"#).is_err());
    assert!(parse_round(r#"{"severity":"aside","note":"x","extra":1}"#).is_err());
    let long = format!(r#"{{"severity":"aside","note":"{}"}}"#, "é".repeat(5000));
    assert!(parse_round(&long).unwrap().note.len() < 2 * MAX_NOTE_CHARS + 64);
}

#[test]
fn severity_maps_to_guidance_priority_with_blocker_above_loop_and_todo() {
    assert_eq!(Severity::None.priority(), None);
    assert_eq!(Severity::Aside.priority(), Some(Priority::Todo));
    assert_eq!(Severity::Concern.priority(), Some(Priority::Review));
    assert_eq!(Severity::Blocker.priority(), Some(Priority::Blocker));
    assert!(Priority::Safety < Priority::Blocker);
    assert!(Priority::Blocker < Priority::Loop && Priority::Blocker < Priority::Todo);
    let mut state = CoachTurn::default();
    let note = state
        .apply_round(&parse_round(r#"{"severity":"blocker","note":"wrong file"}"#).unwrap())
        .unwrap();
    assert_eq!(note["dial_guidance"], "blocker");
    assert_eq!(note["dial_transient"], true);
    assert!(note["content"].as_str().unwrap().contains("wrong file"));
    assert!(
        state
            .apply_round(&parse_round(r#"{"severity":"none"}"#).unwrap())
            .is_none()
    );
}

#[test]
fn blocker_is_reraised_once_then_reported_and_cleared_when_resolved() {
    let mut state = CoachTurn::default();
    assert!(state.reraise_blocker().is_none());
    state.apply_round(&parse_round(r#"{"severity":"blocker","note":"wrong file"}"#).unwrap());
    assert!(state.reraise_blocker().is_some());
    assert!(state.reraise_blocker().is_none());
    assert!(
        state
            .completion_note()
            .contains("Unresolved advisor blocker")
    );
    assert!(state.completion_note().contains("wrong file"));
    state.apply_round(&parse_round(r#"{"severity":"none"}"#).unwrap());
    assert!(state.completion_note().is_empty());
}

#[test]
fn round_evidence_is_bounded_and_consumed_per_review() {
    let mut state = CoachTurn::default();
    assert!(state.round_question("goal").is_none());
    for n in 0..20 {
        state.observe("read", &format!("{n}"), false, false);
    }
    let question = state.round_question("goal").unwrap();
    assert_eq!(
        question["evidence"].as_array().unwrap().len(),
        MAX_ROUND_EVIDENCE
    );
    assert!(state.round_question("goal").is_none());
}

#[test]
fn completion_only_launch_is_off_by_default() {
    assert!(!crate::agent::modes::cli::Launch::default().advisor_completion_only);
}

#[test]
fn content_free_coach_notes_neither_block_nor_reach_the_model() {
    let mut state = CoachTurn::default();
    for filler in ["Stop.", "  LOOKS good! ", "No issues; continue.", "..."] {
        let round = RoundNote {
            severity: Severity::Blocker,
            note: filler.into(),
        };
        assert!(state.apply_round(&round).is_none(), "{filler:?}");
        assert!(state.completion_note().is_empty(), "{filler:?}");
    }
    let real = parse_round(r#"{"severity":"blocker","note":"Stop: the migration drops a column"}"#)
        .unwrap();
    assert!(state.apply_round(&real).is_some());
}

#[test]
fn advisor_goal_review_falls_back_to_legacy_instruction_and_shows_original_plus_amendment() {
    use crate::agent::run::intent::{Kind, record};
    with_agent("", |agent| {
        // Legacy session: only the effective instruction exists; the review sees it unchanged.
        agent
            .session
            .set_control_state(json!({"current_instruction":"legacy request"}))
            .unwrap();
        assert_eq!(agent.current_goal("goal"), "legacy request");

        // Admitted original, then a steering amendment: both reach the review, latest is labelled.
        let mut control = agent.session.control_state();
        record(&mut control, "ship the parser", json!("turn-1"), Kind::Turn);
        record(
            &mut control,
            "use the v2 parser",
            json!("steer-1"),
            Kind::Steering,
        );
        control["current_instruction"] = json!("use the v2 parser");
        agent.session.set_control_state(control).unwrap();
        let review = agent.current_goal("goal");
        assert!(review.starts_with("use the v2 parser\n\n"));
        assert!(review.contains("ship the parser"));
        assert!(review.contains("\"latest\""));
        assert!(review.contains("use the v2 parser"));
    });
}

#[test]
fn unset_completion_review_requires_a_permitted_coach_route() {
    const LOCAL_COACH: &str = "[model_roles.coach]\nprovider='endpoint'\nmodel='coach'\nbase_url='http://127.0.0.1:9/v1'\n";
    const REMOTE_COACH: &str = "[model_roles.coach]\nprovider='endpoint'\nmodel='coach'\nbase_url='https://coach.example.com/v1'\n";
    with_agent("", |agent| {
        assert!(!agent.completion_review_enabled());
        assert!(!solve::coach_route_allowed(&agent.auth_config));
    });
    with_agent(LOCAL_COACH, |agent| {
        assert!(agent.completion_review_enabled());
        assert!(solve::coach_route_allowed(&agent.auth_config));
    });
    with_agent(REMOTE_COACH, |agent| {
        assert!(!agent.completion_review_enabled());
        assert!(!solve::coach_route_allowed(&agent.auth_config));
    });
    with_agent(
        &format!("[escalation]\nallow_remote=true\n{REMOTE_COACH}"),
        |agent| assert!(solve::coach_route_allowed(&agent.auth_config)),
    );
    with_agent(
        &format!("[verification.review]\nenabled=false\n{LOCAL_COACH}"),
        |agent| assert!(!agent.completion_review_enabled()),
    );
    with_agent("[verification.review]\nenabled=true", |agent| {
        assert!(agent.completion_review_enabled())
    });
}

#[test]
fn completion_review_receives_reconciliation_without_promoting_claims() {
    let state = CoachTurn::default();
    let reconciliation = json!({"deliverables":[
        {"id":"remaining","claimed_status":"open"},
        {"id":"claim","evidence_status":"claim_only"}
    ],"undeclared_requirements":"unassessed"});
    let question = state.question("Implement both requirements", "Done", &reconciliation);
    assert_eq!(question["goal"], "Implement both requirements");
    let prompt = question["question"].as_str().unwrap();
    assert!(prompt.contains(&reconciliation.to_string()));
    assert!(question["evidence"][0].as_str().unwrap().contains("Done"));
    assert!(state.parse(r#"{"gaps":[{"requirement":"remaining outcome","evidence":0,"next_check":"inspect second outcome"}]}"#).is_ok());
}

#[test]
fn default_completion_review_never_starts_an_independent_self_review() {
    with_agent("", |agent| {
        agent.step_checks.uncheckpointed_shell_commands = 1;
        agent.step_checks.evidence.observe(
            "validate-artifact",
            "observed-check",
            crate::agent::verify::verification::CheckStatus::CodeFailed,
            agent.step_checks.revision(),
            false,
        );
        let mut backend = crate::llm::provider::Backend::new(
            crate::llm::provider::ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            std::path::PathBuf::new(),
            5,
        );
        let before = agent.completion_evidence(crate::agent::TurnOutcome::Completed);
        let mut state = CoachTurn::default();
        assert!(
            agent
                .coach_completion(
                    &mut state,
                    &mut backend,
                    "write result.csv",
                    "Done",
                    0,
                    &mut crate::agent::NullUi
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(state.reviews, 0);
        assert_eq!(
            agent.completion_evidence(crate::agent::TurnOutcome::Completed),
            before
        );
        assert!(agent.session.control_state()["completion_review"].is_null());
    });
}
