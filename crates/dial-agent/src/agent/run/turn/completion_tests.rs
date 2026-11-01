use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

pub(super) fn response(text: &str, call: Option<(&str, Value)>) -> llm::fake::Script {
    let mut delta = json!({"content":text});
    let reason = if let Some((name, args)) = call {
        delta["tool_calls"] = json!([{"index":0,"id":format!("call-{name}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]);
        "tool_calls"
    } else {
        "stop"
    };
    let event = json!({"choices":[{"delta":delta,"finish_reason":reason}]});
    llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

pub(super) fn connect(agent: &mut Agent<'_>, url: &str) {
    agent.backend = Some(Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{url}/v1"),
            model: "test".into(),
            api_key: None,
        },
        Default::default(),
        5,
    ));
}

pub(super) fn completion_requests(server: &llm::fake::Fake) -> Vec<Value> {
    server
        .requests()
        .iter()
        .filter_map(|request| {
            let body = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
            serde_json::from_slice::<Value>(&request[body + 4..]).ok()
        })
        .filter(|request| request["messages"].is_array())
        .collect()
}

const CHECK: &str = r#"
[compaction]
enabled=false
[verification]
strict_requirements=true
automatic_checks=true
[[verification.commands]]
name="ready"
command="test -f ready"
timeout_secs=5
[verification.requirements.delivery]
checks=["ready"]
"#;

#[test]
fn a_missing_requirement_stops_with_the_candidate_and_observed_failure() {
    let command = "test -f ready || { echo 'AssertionError: ready is missing'; exit 1; }";
    let config = CHECK.replace("test -f ready", command);
    for explicit in [false, true] {
        let candidate = "The delivery is ready.";
        let first = if explicit {
            response(
                "",
                Some(("finish", json!({"outcome":"completed","summary":candidate}))),
            )
        } else {
            response(candidate, None)
        };
        let server = llm::fake::serve(vec![first, llm::fake::json(200, &json!({"total_slots":1}))]);
        with_agent(&config, |agent| {
            // The trusted compound fixture requires approval; NullUi would deny its prompt.
            agent.approval = crate::config::types::Approval::Auto;
            connect(agent, &server.url);
            let report = agent.turn("Prepare the delivery", &mut NullUi).unwrap();
            assert_eq!(
                report.outcome,
                TurnOutcome::Incomplete,
                "explicit={explicit}: {}",
                report.text
            );
            assert!(report.text.contains(candidate), "{}", report.text);
            assert!(report.text.contains("ready"), "{}", report.text);
            assert!(!agent.workspace.root().join("ready").exists());
            assert_eq!(
                agent.step_checks.evidence.latest_status(command),
                Some(crate::agent::verify::verification::CheckStatus::CodeFailed)
            );
            assert!(agent.session.custom_records("training_turn").is_empty());
        });
        assert_eq!(completion_requests(&server).len(), 1, "explicit={explicit}");
    }
}

#[test]
fn a_blocked_todo_rejects_plain_and_explicit_success_without_retrying() {
    for explicit in [false, true] {
        let candidate = "The delivery is ready.";
        let reply = if explicit {
            response(
                "",
                Some(("finish", json!({"outcome":"completed","summary":candidate}))),
            )
        } else {
            response(candidate, None)
        };
        let server = llm::fake::serve(vec![reply, llm::fake::json(200, &json!({"total_slots":1}))]);
        with_agent("[compaction]\nenabled=false\n", |agent| {
            connect(agent, &server.url);
            assert!(
                !agent
                    .todo
                    .call(&json!({"op":"set","tasks":[{"content":"Prepare delivery"}]}))
                    .is_error
            );
            assert!(
                !agent
                    .todo
                    .call(&json!({"op":"block","reason":"Missing deployment credential"}))
                    .is_error
            );
            let report = agent.turn("Prepare the delivery", &mut NullUi).unwrap();
            assert_eq!(
                report.outcome,
                TurnOutcome::Blocked,
                "explicit={explicit}: {}",
                report.text
            );
            assert!(report.halts_automation());
            assert!(report.text.starts_with("Blocked:"), "{}", report.text);
            assert!(report.text.contains(candidate), "{}", report.text);
            assert!(
                report.text.contains("Missing deployment credential"),
                "{}",
                report.text
            );
            assert!(agent.session.custom_records("training_turn").is_empty());
        });
        assert_eq!(completion_requests(&server).len(), 1, "explicit={explicit}");
    }
}

#[test]
fn a_terminal_failed_batch_at_the_recovery_limit_retains_the_rejected_candidate() {
    let candidate = "The delivery is ready at the recovery boundary.";
    let mut scripts = vec![
        response("", Some(("read", json!({"path":"missing-0.txt"})))),
        llm::fake::json(200, &json!({"total_slots":1})),
    ];
    for index in 1..15 {
        scripts.push(response(
            "",
            Some(("read", json!({"path":format!("missing-{index}.txt")}))),
        ));
    }
    let event = json!({"choices":[{"delta":{"tool_calls":[
        {"index":0,"id":"final-read","type":"function","function":{"name":"read","arguments":json!({"path":"missing-final.txt"}).to_string()}},
        {"index":1,"id":"final-finish","type":"function","function":{"name":"finish","arguments":json!({"outcome":"completed","summary":candidate}).to_string()}}
    ]},"finish_reason":"tool_calls"}]});
    scripts.push(llm::fake::Script::Sse(format!(
        "data: {event}\n\ndata: [DONE]\n\n"
    )));
    let server = llm::fake::serve(scripts);
    with_agent(
        "[compaction]\nenabled=false\n[harness]\nmax_steps=0\n",
        |agent| {
            connect(agent, &server.url);
            let report = agent.turn("Prepare the delivery", &mut NullUi).unwrap();
            assert_eq!(report.outcome, TurnOutcome::Blocked, "{}", report.text);
            assert!(report.halts_automation());
            assert!(report.text.contains(candidate), "{}", report.text);
            assert!(
                report.text.contains("16 recovery attempts"),
                "{}",
                report.text
            );
            assert!(agent.session.custom_records("training_turn").is_empty());
        },
    );
    assert_eq!(completion_requests(&server).len(), 16);
}

fn missing_visual(max_steps: u32) -> String {
    format!(
        r#"
[compaction]
enabled=false
[harness]
max_steps={max_steps}
[verification]
strict_requirements=true
[verification.requirements.rendered]
visual=true
"#
    )
}

#[test]
fn unlimited_steps_do_not_retry_a_final_with_missing_evidence() {
    let server = llm::fake::serve(vec![
        response("Candidate report survives.", None),
        llm::fake::json(200, &json!({"total_slots":1})),
    ]);
    with_agent(&missing_visual(0), |agent| {
        connect(agent, &server.url);
        let report = agent
            .turn("Report the rendered result", &mut NullUi)
            .unwrap();
        assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
        assert!(
            report.text.contains("Candidate report survives."),
            "{}",
            report.text
        );
        assert!(report.text.contains("rendered"), "{}", report.text);
        assert!(agent.session.custom_records("training_turn").is_empty());
    });
    assert_eq!(completion_requests(&server).len(), 1);
}

#[test]
fn exhausted_step_budget_retains_rejected_candidate_and_missing_prerequisite() {
    let server = llm::fake::serve(vec![
        response("Candidate survives the budget boundary.", None),
        llm::fake::json(200, &json!({"total_slots":1})),
    ]);
    with_agent(&missing_visual(1), |agent| {
        connect(agent, &server.url);
        let report = agent
            .turn("Report the rendered result", &mut NullUi)
            .unwrap();
        assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
        assert!(agent.session.custom_records("training_turn").is_empty());
        assert!(
            report
                .text
                .contains("Candidate survives the budget boundary."),
            "{}",
            report.text
        );
        assert!(report.text.contains("rendered"), "{}", report.text);
    });
    assert_eq!(completion_requests(&server).len(), 1);
}

#[test]
fn hosted_turn_without_store_retains_exact_requests_and_original_identity() {
    let server = llm::fake::serve(vec![
        response("", Some(("read", json!({"path":"input.txt"})))),
        llm::fake::json(200, &json!({"total_slots":1})),
        response("The input says ready.", None),
    ]);
    with_agent("[compaction]\nenabled=false", |agent| {
        std::fs::write(agent.workspace.root().join("input.txt"), "ready\n").unwrap();
        connect(agent, &server.url);
        assert!(agent.store.is_none());
        let report = agent.turn("Read input.txt", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        assert!(!agent.logging, "retention must not enable event logging");
        let records = agent.session.custom_records("training_turn");
        assert_eq!(records.len(), 1);
        let record = &records[0].1;
        assert_eq!(record["version"], 1);
        assert_eq!(record["source_session"], agent.session.id());
        let user_entry = agent
            .session
            .branchable_user_messages()
            .into_iter()
            .find(|(_, text)| text == "Read input.txt")
            .unwrap()
            .0;
        assert_eq!(record["source_turn"], user_entry);
        let selection = agent.effective_config().selection();
        assert_eq!(
            record["source_model"],
            format!("{}/{}", selection.provider, selection.model)
        );
        assert!(record.get("source_state").is_none());
        assert!(record.get("source_trajectory").is_none());
        assert_eq!(record["prompt"], "Read input.txt");
        let requests = completion_requests(&server);
        let examples = record["payload"]["examples"].as_array().unwrap();
        assert_eq!(examples.len(), 2);
        assert_eq!(requests.len(), examples.len());
        for (example, request) in examples.iter().zip(&requests) {
            let messages = example["messages"].as_array().unwrap();
            // Training records the request exactly as this loopback route sent it.
            let context: Vec<_> = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(llm::messages::canonical)
                .collect();
            assert_eq!(&messages[..messages.len() - 1], context.as_slice());
            assert_eq!(example["tools"], request["tools"]);
        }
        assert_eq!(
            examples[0]["messages"].as_array().unwrap().last().unwrap()["tool_calls"][0]["function"]
                ["name"],
            "read"
        );
        assert_eq!(
            examples[1]["messages"].as_array().unwrap().last().unwrap()["content"],
            "The input says ready."
        );
        let reopened =
            crate::storage::session::Session::read(agent.session.path().unwrap()).unwrap();
        assert_eq!(reopened.custom_records("training_turn"), records);
        let fork = agent.session.fork(agent.layout.as_ref().unwrap()).unwrap();
        assert_ne!(fork.id(), agent.session.id());
        assert_eq!(fork.custom_records("training_turn"), records);
    });
}

/// The observed failures: a project runs another program's tests (or a non-terminal launch fails)
/// and the turn reports the requested program as working or unrunnable. Completion is refused on
/// controller facts, whatever the model writes.
const RUN_REQUEST: &str = "run the snake game in local testing/";

fn game_workspace(agent: &mut Agent<'_>, url: &str) {
    let root = agent.workspace.root().to_path_buf();
    std::fs::create_dir_all(root.join("local testing")).unwrap();
    // Exits zero only when stdin and stdout are terminals, like a curses program.
    std::fs::write(
        root.join("local testing/game.sh"),
        "test -t 0 && test -t 1 || { echo 'requires an interactive terminal' >&2; exit 1; }\necho terminal-ok\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("fixtures")).unwrap();
    std::fs::write(root.join("fixtures/check.sh"), "exit 0\n").unwrap();
    agent.approval = crate::config::types::Approval::Auto;
    connect(agent, url);
}

#[test]
fn another_programs_passing_check_does_not_complete_or_restart_a_run_request() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some(("bash", json!({"command":"sh fixtures/check.sh"}))),
        ),
        llm::fake::json(200, &json!({"total_slots":1})),
        response("The snake game works; verified with a mocked TTY.", None),
    ]);
    with_agent("[compaction]\nenabled=false", |agent| {
        game_workspace(agent, &server.url);
        let report = agent.turn(RUN_REQUEST, &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
        assert!(
            report.text.contains("no run naming this path"),
            "{}",
            report.text
        );
        assert!(
            report.text.contains("verified with a mocked TTY"),
            "{}",
            report.text
        );
        let evidence = agent.completion_evidence(report.outcome);
        let target = &evidence["requested_targets"]["targets"][0];
        assert_eq!(target["path"], "local testing");
        assert_eq!(target["status"], "not_run", "{evidence}");
        assert!(agent.session.custom_records("training_turn").is_empty());
    });
    assert_eq!(completion_requests(&server).len(), 2);
}

#[test]
fn a_nonterminal_failure_stops_with_the_observed_diagnostic_without_retrying() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some((
                "bash",
                json!({"command":"sh game.sh","cwd":"local testing"}),
            )),
        ),
        llm::fake::json(200, &json!({"total_slots":1})),
        response(
            "The environment has no TTY, so the game cannot run here.",
            None,
        ),
    ]);
    with_agent("[compaction]\nenabled=false", |agent| {
        game_workspace(agent, &server.url);
        let report = agent.turn(RUN_REQUEST, &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
        assert!(
            report
                .text
                .contains("exited with code 1; launched without a terminal"),
            "{}",
            report.text
        );
        let evidence = agent.completion_evidence(report.outcome);
        let target = &evidence["requested_targets"]["targets"][0];
        assert_eq!(target["status"], "failed", "{evidence}");
        assert_eq!(target["latest"]["exit_code"], 1);
        assert_eq!(target["terminal_launches"], 0);
    });
    assert_eq!(completion_requests(&server).len(), 2);
}

#[test]
fn a_terminal_backed_run_with_observed_exit_completes() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some((
                "bg",
                json!({"action":"start","name":"game","command":"sh game.sh","cwd":"local testing","pty":true}),
            )),
        ),
        llm::fake::json(200, &json!({"total_slots":1})),
        response(
            "",
            Some(("bg", json!({"action":"wait","name":"game","timeout":20}))),
        ),
        response(
            "Ran local testing/game.sh in a terminal; it exited 0.",
            None,
        ),
    ]);
    with_agent("[compaction]\nenabled=false", |agent| {
        game_workspace(agent, &server.url);
        let report = agent.turn(RUN_REQUEST, &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        let evidence = agent.completion_evidence(report.outcome);
        let target = &evidence["requested_targets"]["targets"][0];
        assert_eq!(target["status"], "process_succeeded", "{evidence}");
        assert_eq!(target["terminal_launches"], 1);
        assert_eq!(target["latest"]["state"], "exited_zero");
        assert_eq!(evidence["requested_targets"]["behavior"], "not_assessed");
    });
    assert_eq!(completion_requests(&server).len(), 3);
}

#[test]
fn honest_non_success_reports_do_not_require_success_evidence() {
    for (text, expected) in [
        ("Failed: the game could not start.", TurnOutcome::Failed),
        (
            "Blocked: the required display is unavailable.",
            TurnOutcome::Blocked,
        ),
        (
            "Incomplete: no launch was attempted.",
            TurnOutcome::Incomplete,
        ),
    ] {
        let server = llm::fake::serve(vec![
            response(text, None),
            llm::fake::json(200, &json!({"total_slots":1})),
        ]);
        with_agent(&missing_visual(0), |agent| {
            game_workspace(agent, &server.url);
            let report = agent.turn(RUN_REQUEST, &mut NullUi).unwrap();
            assert_eq!(report.outcome, expected, "{}", report.text);
            assert_eq!(report.text, text);
            assert_eq!(
                agent.completion_evidence(report.outcome)["requested_targets"]["targets"][0]["status"],
                "not_run"
            );
        });
        assert_eq!(completion_requests(&server).len(), 1);
    }
}

#[test]
fn an_effectful_command_after_a_successful_run_makes_it_stale() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some((
                "bg",
                json!({"action":"start","name":"game","command":"sh game.sh","cwd":"local testing","pty":true}),
            )),
        ),
        llm::fake::json(200, &json!({"total_slots":1})),
        response(
            "",
            Some(("bg", json!({"action":"wait","name":"game","timeout":20}))),
        ),
        // An unchecked shell write to the program after its successful run.
        response(
            "",
            Some((
                "bash",
                json!({"command":"printf 'echo changed\\n' >> 'local testing/game.sh'"}),
            )),
        ),
        response("Ran the game; it works.", None),
    ]);
    with_agent("[compaction]\nenabled=false", |agent| {
        game_workspace(agent, &server.url);
        let report = agent.turn(RUN_REQUEST, &mut NullUi).unwrap();
        assert_eq!(
            report.outcome,
            TurnOutcome::Incomplete,
            "{}\n{:#}\n{:#}",
            report.text,
            agent.completion_evidence(TurnOutcome::Incomplete)["requested_targets"],
            agent.step_checks.status()
        );
        let evidence = agent.completion_evidence(TurnOutcome::Incomplete);
        let target = &evidence["requested_targets"]["targets"][0];
        assert_eq!(target["status"], "stale", "{evidence}");
        assert_eq!(target["latest"]["state"], "exited_zero");
        assert_eq!(target["latest"]["current"], false);
    });
    assert_eq!(completion_requests(&server).len(), 4);
}

#[test]
fn requests_that_do_not_ask_to_run_a_named_path_are_unaffected() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some(("bash", json!({"command":"sh fixtures/check.sh"}))),
        ),
        llm::fake::json(200, &json!({"total_slots":1})),
        response("game.sh needs a terminal.", None),
    ]);
    with_agent("[compaction]\nenabled=false", |agent| {
        game_workspace(agent, &server.url);
        let report = agent
            .turn("explain what local testing/game.sh does", &mut NullUi)
            .unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        assert!(agent.completion_evidence(TurnOutcome::Completed)["requested_targets"].is_null());
    });
}

#[test]
fn disabling_target_evidence_restores_the_unverified_completion() {
    let server = llm::fake::serve(vec![
        response(
            "",
            Some(("bash", json!({"command":"sh fixtures/check.sh"}))),
        ),
        llm::fake::json(200, &json!({"total_slots":1})),
        response("The snake game works; verified with a mocked TTY.", None),
    ]);
    let overlay = "[compaction]\nenabled=false\n[verification]\ntarget_evidence=false";
    with_agent(overlay, |agent| {
        game_workspace(agent, &server.url);
        let report = agent.turn(RUN_REQUEST, &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        assert!(agent.completion_evidence(TurnOutcome::Completed)["requested_targets"].is_null());
    });
}
