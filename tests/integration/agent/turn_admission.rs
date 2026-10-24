use super::*;

fn rejected_turn_has_fresh_evidence(block_with_hook: bool, keep_receipts: bool) {
    if skip_without_sandbox() {
        return;
    }
    let server = serve(vec![
        Reply::Call("write", json!({"path":"marker.txt","content":"checked\n"})),
        Reply::Text("Created marker."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.experimental.verification_receipts = keep_receipts;
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "marker-check".into(),
        command: "test -f marker.txt && echo 'test result: ok. 1 passed'".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    if block_with_hook {
        // First admission succeeds; the marker created by that turn blocks the next admission.
        e.config.hooks.push(dial::config::types::HookConfig {
            event: "user_prompt_submit".into(),
            matcher: None,
            command: vec![
                "sh".into(),
                "-c".into(),
                "if test -f marker.txt; then printf '%s' '{\"decision\":\"block\",\"reason\":\"fixture policy\"}'; fi".into(),
            ],
            timeout_secs: 5,
            fail_closed: Some(true),
        });
    }
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Create a marker", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let first = agent.session().control_state()["turn_summary"].clone();
    assert_eq!(first["changed_files"], json!(["marker.txt"]));
    assert_eq!(first["checks_current"], true);
    assert!(!first["recorded_checks"].as_array().unwrap().is_empty());
    let history = dial::tools::knowledge::history::ChangeHistory::open(&e.layout).unwrap();
    let prior_versions = history.search("", Some("marker.txt"), None, 10).unwrap();
    assert_eq!(prior_versions.len(), 1);

    // Historical receipts are a separate, opt-in projection, not current checks.
    let mut control = agent.session().control_state();
    let receipts = json!([{"historical_fixture":true}]);
    control["reliability"]["verification_receipts"] = receipts.clone();
    agent.session_mut().set_control_state(control).unwrap();
    let error = agent
        .turn(
            if block_with_hook {
                "Blocked follow-up"
            } else {
                ""
            },
            &mut Recorder::default(),
        )
        .err()
        .expect("admission must fail");
    assert!(error.to_string().contains(if block_with_hook {
        "fixture policy"
    } else {
        "message must be"
    }));
    let state = agent.session().control_state();
    let failed = &state["turn_summary"];
    assert_eq!(failed["outcome"], "failed");
    assert!(failed["checkpoint_turn"].is_null());
    assert_eq!(failed["changed_files"], json!([]));
    assert!(failed["recorded_checks"].is_null());
    assert_eq!(failed["checks_current"], false);
    assert!(failed["evidence"].is_null());
    assert_eq!(failed["uncheckpointed_shell_commands"], 0);
    assert_eq!(failed["uncheckpointed_extension_calls"], 0);
    assert_eq!(state["reliability"]["automatic_passing_checks"], 0);
    assert_eq!(
        state["reliability"]["verification_receipts"],
        if keep_receipts { receipts } else { Value::Null }
    );
    assert_eq!(
        history.search("", Some("marker.txt"), None, 10).unwrap(),
        prior_versions
    );
    assert_eq!(
        server.requests.lock().len(),
        2,
        "rejected prompts must not reach inference"
    );
}

#[test]
fn invalid_prompt_does_not_republish_the_previous_turns_evidence() {
    for receipts in [false, true] {
        rejected_turn_has_fresh_evidence(false, receipts);
    }
}

#[cfg(unix)]
#[test]
fn blocked_prompt_does_not_republish_the_previous_turns_evidence() {
    rejected_turn_has_fresh_evidence(true, true);
}
