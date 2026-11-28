//! Behavioral contracts for state-owned completion and request economy.
use super::completion_tests::{completion_requests, connect, response};
use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_default_agent as with_agent;

#[test]
fn open_execution_steps_cannot_finish_through_either_delivery_surface() {
    for outcome in [None, Some("completed"), Some("answered")] {
        let candidate = || {
            if let Some(outcome) = outcome {
                response(
                    "",
                    Some((
                        "finish",
                        json!({"outcome":outcome,"summary":"The answer is ready."}),
                    )),
                )
            } else {
                response("The answer is ready.", None)
            }
        };
        let server = serve(vec![candidate(), candidate()]);
        with_agent(
            "[compaction]\nenabled=false\n[retry]\nmax_attempts=1",
            |agent| {
                connect(agent, &server.url);
                assert!(!agent.todo.call(&json!({"op":"set","tasks":[{"content":"Produce the requested output"}]})).is_error);
                let report = agent.turn("Complete the delivery", &mut NullUi).unwrap();
                assert_eq!(
                    report.outcome,
                    TurnOutcome::Incomplete,
                    "outcome={outcome:?}: {}",
                    report.text
                );
                assert!(
                    agent.todo.has_open(),
                    "a reply cannot close an execution step"
                );
                assert!(
                    !agent.completion_evidence(report.outcome)["work_completion_accepted"]
                        .as_bool()
                        .unwrap()
                );
            },
        );
        assert!(completion_requests(&server).len() <= 2);
    }
}

#[test]
fn a_workspace_diagnosis_cannot_finish_on_a_promise_after_one_reminder() {
    let candidate = "Diagnosis is underway.\nThe source comes next, then a fix.";
    for second in [
        response(candidate, None),
        response(
            "",
            Some(("finish", json!({"outcome":"answered","summary":candidate}))),
        ),
    ] {
        let server = serve(vec![response(candidate, None), second]);
        with_agent(
            "[compaction]\nenabled=false\n[retry]\nmax_attempts=1",
            |agent| {
                std::fs::write(
                    agent.workspace.root().join("failure.txt"),
                    "the observed failure",
                )
                .unwrap();
                connect(agent, &server.url);
                let report = agent
                    .turn("Diagnose the failure in this project", &mut NullUi)
                    .unwrap();
                assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
                assert!(report.text.contains(candidate), "{}", report.text);
            },
        );
        assert_eq!(completion_requests(&server).len(), 2);
    }
}

#[test]
fn a_tool_free_blocker_or_self_contained_answer_is_not_a_workspace_diagnosis_claim() {
    for (prompt, answer, outcome) in [
        (
            "Diagnose the failure in this project",
            "Blocked: the incident log requires the operator's credentials.",
            TurnOutcome::Blocked,
        ),
        (
            "What does EADDRINUSE mean?",
            "Another process already holds the requested address.",
            TurnOutcome::Completed,
        ),
    ] {
        let server = serve(vec![response(answer, None)]);
        with_agent(
            "[compaction]\nenabled=false\n[retry]\nmax_attempts=1",
            |agent| {
                std::fs::write(agent.workspace.root().join("failure.txt"), "invalid port").unwrap();
                connect(agent, &server.url);
                let report = agent.turn(prompt, &mut NullUi).unwrap();
                assert_eq!(report.outcome, outcome, "{}", report.text);
                assert_eq!(report.text, answer);
            },
        );
        assert_eq!(completion_requests(&server).len(), 1);
    }
}

#[test]
fn a_question_before_workspace_investigation_continues_to_a_tool_call() {
    let server = serve(vec![
        response("Could you clarify what the failure is?", None),
        response("", Some(("read", json!({"path":"failure.txt"})))),
        response(
            "The recorded failure is an invalid port.\n\nThe log's status message was:\n> The source comes next, then a fix.",
            None,
        ),
    ]);
    with_agent(
        "[compaction]\nenabled=false\n[retry]\nmax_attempts=1",
        |agent| {
            std::fs::write(
                agent.workspace.root().join("failure.txt"),
                "invalid port\nThe source comes next, then a fix.",
            )
            .unwrap();
            connect(agent, &server.url);
            let report = agent
                .turn("Investigate this project's failure", &mut NullUi)
                .unwrap();
            assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
            assert!(report.text.contains("invalid port"));
        },
    );
    assert_eq!(completion_requests(&server).len(), 3);
}

#[test]
fn read_only_observation_and_answer_need_no_self_review_request() {
    let server = serve(vec![
        response("", Some(("read", json!({"path":"failure.txt"})))),
        response("The recorded failure is an invalid port.", None),
    ]);
    with_agent(
        "[compaction]\nenabled=false\n[retry]\nmax_attempts=1",
        |agent| {
            std::fs::write(agent.workspace.root().join("failure.txt"), "invalid port").unwrap();
            connect(agent, &server.url);
            let report = agent
                .turn("Read the recorded failure", &mut NullUi)
                .unwrap();
            assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        },
    );
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    let systems = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "system")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(systems(&requests[0]), systems(&requests[1]));
}

fn serve(mut scripts: Vec<llm::fake::Script>) -> llm::fake::Fake {
    scripts.insert(1, llm::fake::json(200, &json!({"total_slots":1})));
    llm::fake::serve(scripts)
}

#[test]
fn a_settled_edit_reuses_its_delivery_check_instead_of_running_it_again_at_completion() {
    let server = serve(vec![
        response(
            "",
            Some((
                "write",
                json!({"path":"payload.txt","content":"ready","verify":["full_delivery"]}),
            )),
        ),
        response("Saved payload.txt and the delivery check passed.", None),
    ]);
    let overlay = r#"
approval='auto'
[compaction]
enabled=false
[retry]
max_attempts=1
[verification]
automatic_checks=false
[[verification.commands]]
name='full_delivery'
command='test "$(cat payload.txt)" = ready && printf x >> runs'
timeout_secs=5
[verification.inputs.'full_delivery']
paths=['payload.txt']
environment=[]
"#;
    with_agent(overlay, |agent| {
        connect(agent, &server.url);
        let report = agent
            .turn(
                "Create payload.txt with ready and verify the delivery",
                &mut NullUi,
            )
            .unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
        assert_eq!(
            std::fs::read_to_string(agent.workspace.root().join("runs")).unwrap(),
            "x",
            "the post-edit pass and completion share the same observed check"
        );
    });
    assert_eq!(completion_requests(&server).len(), 2);
}

#[test]
fn a_current_focused_check_does_not_relabel_an_older_shell_check_as_current() {
    let server = serve(vec![
        response(
            "",
            Some((
                "bash",
                json!({"command":"test ! -f payload.txt","check":true}),
            )),
        ),
        response(
            "",
            Some((
                "write",
                json!({"path":"payload.txt","content":"ready","verify":["delivery"]}),
            )),
        ),
        response(
            "Saved payload.txt; delivery passed. The earlier absence check is historical.",
            None,
        ),
    ]);
    with_agent(
        r#"
approval='auto'
[sandbox]
mode='off'
[compaction]
enabled=false
[verification]
automatic_checks=false
[[verification.commands]]
name='delivery'
command='test "$(cat payload.txt)" = ready'
timeout_secs=5
[verification.inputs.'delivery']
paths=['payload.txt']
environment=[]
[assistance]
lean_local=false
"#,
        |agent| {
            connect(agent, &server.url);
            let report = agent
                .turn("Check that payload.txt is absent, then create it with ready and verify delivery", &mut NullUi)
                .unwrap();
            let state = agent.session.control_state();
            let summary = &state["turn_summary"];
            let attempts = &summary["main_request_receipt"]["attempts"];
            assert!(attempts["local_or_lan"].as_u64().unwrap() >= 2);
            assert_eq!(attempts["remote"], 0);
            assert!(attempts["tool_results"].as_u64().unwrap() > 0);
            assert_eq!(summary["checks_current"], false, "{summary}");
            let checks = summary["recorded_checks"].as_array().unwrap();
            assert!(
                checks
                    .iter()
                    .any(|check| check["name"] == "delivery" && check["status"] == "passed"),
                "{checks:?}; report: {report:?}"
            );
            assert!(
                checks
                    .iter()
                    .any(|check| check["kind"] == "shell" && check["status"] == "passed"),
                "{checks:?}"
            );
        },
    );
}

#[test]
fn unoffered_calls_do_not_implicitly_load_specialists_or_fail_the_session() {
    let server = serve(vec![
        response("", Some(("git", json!({"action":"status"})))),
        response("", Some(("tool_search", json!({"names":["git"]})))),
        response("No repository was inspected.", None),
    ]);
    with_agent("[compaction]\nenabled=false", |agent| {
        connect(agent, &server.url);
        let report = agent.turn("Explain git status", &mut NullUi).unwrap();
        assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    });
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["tools"], requests[1]["tools"]);
    assert!(
        requests[2]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "git")
    );
}

#[test]
fn schema_and_system_bytes_are_identical_on_follow_up_turns_without_explicit_loading() {
    let server = serve(vec![
        response("", Some(("read", json!({"path":"source.txt"})))),
        response("The source is ready.", None),
        response("", Some(("read", json!({"path":"source.txt"})))),
        response("The source remains ready.", None),
    ]);
    with_agent("[compaction]\nenabled=false", |agent| {
        std::fs::write(agent.workspace.root().join("source.txt"), "ready").unwrap();
        connect(agent, &server.url);
        for prompt in ["Read source.txt", "Inspect source.txt again"] {
            assert_eq!(
                agent.turn(prompt, &mut NullUi).unwrap().outcome,
                TurnOutcome::Completed
            );
        }
    });
    let requests = completion_requests(&server);
    assert_eq!(requests.len(), 4);
    let systems = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "system")
            .cloned()
            .collect::<Vec<_>>()
    };
    for request in &requests[1..] {
        assert_eq!(request["tools"], requests[0]["tools"]);
        assert_eq!(systems(request), systems(&requests[0]));
    }
}
