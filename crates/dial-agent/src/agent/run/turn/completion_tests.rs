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
