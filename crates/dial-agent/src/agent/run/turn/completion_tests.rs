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
fn rejected_plain_final_can_repair_and_complete_like_explicit_finish() {
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
        let server = llm::fake::serve(vec![
            first,
            llm::fake::json(200, &json!({"total_slots":1})),
            response(
                "",
                Some(("write", json!({"path":"ready","content":"ready\n"}))),
            ),
            response("", Some(("check", json!({"names":["ready"]})))),
            response("The delivery is ready and the required check passed.", None),
        ]);
        with_agent(CHECK, |agent| {
            connect(agent, &server.url);
            let report = agent.turn("Prepare the delivery", &mut NullUi).unwrap();
            assert_eq!(
                report.outcome,
                TurnOutcome::Completed,
                "explicit={explicit}: {}",
                report.text
            );
            assert!(agent.workspace.root().join("ready").exists());
            assert_eq!(
                agent.step_checks.evidence.latest_status("test -f ready"),
                Some(crate::agent::verify::verification::CheckStatus::Passed)
            );
        });
        let requests = completion_requests(&server);
        assert_eq!(requests.len(), 4, "explicit={explicit}");
        assert!(
            requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["content"].as_str().is_some_and(|text| {
                        (text.contains("delivery") && text.contains("missing"))
                            || text.contains("focused checks failed")
                    })
                }),
            "the repair request must receive the rejection reason"
        );
    }
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
fn unlimited_steps_still_bound_rejected_plain_finals_and_preserve_last_candidate() {
    let mut scripts: Vec<_> = (0..4)
        .map(|index| response(&format!("Candidate report {index}."), None))
        .collect();
    scripts.insert(1, llm::fake::json(200, &json!({"total_slots":1})));
    let server = llm::fake::serve(scripts);
    with_agent(&missing_visual(0), |agent| {
        connect(agent, &server.url);
        let report = agent
            .turn("Report the rendered result", &mut NullUi)
            .unwrap();
        assert_eq!(report.outcome, TurnOutcome::Incomplete, "{}", report.text);
        assert!(
            report.text.contains("Candidate report 3."),
            "{}",
            report.text
        );
        assert!(report.text.contains("rendered"), "{}", report.text);
    });
    let requests = completion_requests(&server);
    assert_eq!(
        requests.len(),
        4,
        "initial attempt plus exactly three recovery requests"
    );
    for request in &requests[1..] {
        assert!(
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["role"] == "user"
                        && message["content"].as_str().is_some_and(|text| {
                            text.contains("Completion was not accepted")
                                && text.contains("rendered")
                        })
                })
        );
    }
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
