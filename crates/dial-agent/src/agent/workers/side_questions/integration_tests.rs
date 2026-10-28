//! Side-question integration with the agent: completion gating, exactly-once conversation
//! delivery, child exclusion, and the tool-free immutable snapshot sent to a loopback fake
//! provider. The fake is a deterministic test double; nothing here proves a live provider.
use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::config::types::{Config, EndpointConfig, Version};
use crate::llm::fake::{self, Script};
use crate::llm::provider::{Backend, ProviderKind};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

fn sse_text(text: &str) -> String {
    // A canonical stream ends with a finish_reason chunk and `[DONE]`. Without them the reply is
    // incomplete (`is_complete()` is false) and the side job never settles.
    let event = json!({"choices":[{"delta":{"content":text}}]});
    let finish = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
    format!("data: {event}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
}

/// Config whose endpoint is the loopback fake server, so `make_backend` resolves a plain endpoint.
fn side_config(url: &str, user_dir: &Path) -> Config {
    Config {
        provider: Some("endpoint".into()),
        model: "test".into(),
        user_dir: user_dir.to_path_buf(),
        endpoint: Some(EndpointConfig {
            headers: None,
            base_url: format!("{url}/v1"),
            api_key_env: None,
            allow_insecure: true,
            protocol: None,
            account_id: None,
        }),
        ..Config::default()
    }
}

fn hermetic_agent<'a>(config: &'a Config, workspace: &Path) -> Agent<'a> {
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    Agent::hermetic(config, backend, &version, workspace, None, None).unwrap()
}

fn wait_for(agent: &mut Agent<'_>, id: &str) {
    agent
        .side_questions
        .query(
            &json!({"action":"wait","id":id,"timeout":2}),
            &AtomicBool::new(false),
            &mut || false,
        )
        .unwrap();
}

fn delivered_count(agent: &Agent<'_>, id: &str) -> usize {
    agent
        .session
        .messages()
        .iter()
        .filter(|m| m["dial_side_result"].as_str() == Some(id))
        .count()
}

#[test]
fn running_side_question_blocks_completion_until_it_settles() {
    with_agent("", |agent| {
        let (send, receive) = mpsc::channel::<String>();
        let id = agent
            .side_questions
            .start(move |_| receive.recv().map_err(|e| e.to_string()))
            .unwrap();
        let blocker = agent.completion_blockers().unwrap_err();
        assert!(blocker.contains("side questions are running"), "{blocker}");

        send.send("done".into()).unwrap();
        wait_for(agent, &id);
        let blocker = agent.completion_blockers().unwrap_err();
        assert!(blocker.contains("have been delivered"), "{blocker}");
    });
}

#[test]
fn completed_side_result_blocks_completion_until_delivered() {
    with_agent("", |agent| {
        let id = agent.side_questions.start(|_| Ok("answer".into())).unwrap();
        wait_for(agent, &id);
        assert!(agent.side_questions.has_undelivered());
        let blocker = agent.completion_blockers().unwrap_err();
        assert!(blocker.contains("have been delivered"), "{blocker}");

        agent.deliver_worker_results().unwrap();
        assert!(!agent.side_questions.has_undelivered());
        if let Err(blocker) = agent.completion_blockers() {
            assert!(!blocker.contains("side"), "{blocker}");
        }
    });
}

#[test]
fn failed_side_result_also_blocks_completion_until_delivered() {
    with_agent("", |agent| {
        let id = agent
            .side_questions
            .start(|_| Err("provider failure".into()))
            .unwrap();
        wait_for(agent, &id);
        let blocker = agent.completion_blockers().unwrap_err();
        assert!(blocker.contains("have been delivered"), "{blocker}");
        agent.deliver_worker_results().unwrap();
        if let Err(blocker) = agent.completion_blockers() {
            assert!(!blocker.contains("side"), "{blocker}");
        }
    });
}

#[test]
fn each_settled_side_result_is_delivered_exactly_once_into_the_conversation() {
    with_agent("", |agent| {
        let ok = agent.side_questions.start(|_| Ok("answer".into())).unwrap();
        let failed = agent
            .side_questions
            .start(|_| Err("provider failure".into()))
            .unwrap();
        wait_for(agent, &ok);
        wait_for(agent, &failed);

        agent.deliver_worker_results().unwrap();
        assert_eq!(delivered_count(agent, &ok), 1);
        assert_eq!(delivered_count(agent, &failed), 1);

        agent.deliver_worker_results().unwrap();
        assert_eq!(
            delivered_count(agent, &ok),
            1,
            "repeat delivery must not re-append"
        );
        assert_eq!(
            delivered_count(agent, &failed),
            1,
            "repeat delivery must not re-append"
        );
        assert!(!agent.side_questions.has_undelivered());
    });
}

#[test]
fn side_request_is_offered_to_the_parent_but_never_to_children() {
    with_agent("", |agent| {
        assert!(
            agent.offered().iter().any(|n| n == "side_request"),
            "parent should be offered side_request: {:?}",
            agent.offered()
        );
        assert!(
            !agent
                .offered_for_children()
                .iter()
                .any(|n| n == "side_request"),
            "children must never inherit side_request"
        );
    });
}

#[test]
fn side_answer_is_built_from_a_tool_free_snapshot_taken_at_start() {
    let server = fake::serve(vec![Script::Sse(sse_text("snapshot answer"))]);
    let dir = tempfile::tempdir().unwrap();
    let config = side_config(&server.url, dir.path());
    let mut agent = hermetic_agent(&config, dir.path());
    agent
        .push(json!({"role":"user","content":"earlier context"}))
        .unwrap();

    let started = agent.side_question(&json!({"action":"start","question":"why did it fail?"}));
    assert!(!started.is_error, "{}", started.text);
    let id = started.data.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Appended after the snapshot was taken: the side request must not see it.
    agent
        .push(json!({"role":"user","content":"later context"}))
        .unwrap();

    let done = agent.side_question(&json!({"action":"wait","id":id,"timeout":5}));
    assert!(!done.is_error, "{}", done.text);
    let data = done.data.unwrap();
    assert_eq!(data[0]["status"], "completed", "{data}");
    assert_eq!(data[0]["answer"], "snapshot answer");

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one side request, no extra model calls");
    let body = fake::body(&requests[0]);
    let messages = body["messages"].to_string();
    assert!(messages.contains("earlier context"), "{messages}");
    assert!(
        !messages.contains("later context"),
        "snapshot leaked later state"
    );
    assert!(messages.contains("why did it fail?"), "{messages}");
    let tools_empty = body
        .get("tools")
        .is_none_or(|tools| tools.as_array().is_none_or(|tools| tools.is_empty()));
    assert!(tools_empty, "side requests must be tool-free: {body}");
}

#[test]
fn side_question_usage_is_charged_to_the_shared_worker_ledger() {
    let answer = json!({"choices":[{"delta":{"content":"usage answer"}}],"usage":{"prompt_tokens":11,"completion_tokens":3}});
    let finish = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
    let body = format!("data: {answer}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
    let server = fake::serve(vec![Script::Sse(body)]);
    let dir = tempfile::tempdir().unwrap();
    let config = side_config(&server.url, dir.path());
    let mut agent = hermetic_agent(&config, dir.path());
    agent.usage_scope = agent.usage_scope.next_turn(4).unwrap();

    let started = agent.side_question(&json!({"action":"start","question":"why?"}));
    assert!(!started.is_error, "{}", started.text);
    let id = started.data.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let done = agent.side_question(&json!({"action":"wait","id":id,"timeout":5}));
    let data = done.data.unwrap();
    assert_eq!(data[0]["status"], "completed", "{data}");

    assert_eq!(server.requests().len(), 1);
    let (total, turn) = agent.usage_scope.snapshot();
    assert_eq!(
        turn.workers.requests, 1,
        "side request counts as one worker request"
    );
    assert_eq!(turn.main.requests, 0);
    assert_eq!(
        turn.by_purpose
            .get(&crate::llm::usage::Purpose::Auxiliary)
            .map(|counts| counts.requests),
        Some(1)
    );
    assert_eq!(total.workers.input, 11);
    assert_eq!(total.workers.output, 3);
}

#[test]
fn side_question_respects_the_shared_worker_cap_without_sending_a_request() {
    // No scripts: any connection would be refused, so `requests()` proves nothing was sent.
    let server = fake::serve(vec![]);
    let dir = tempfile::tempdir().unwrap();
    let config = side_config(&server.url, dir.path());
    let mut agent = hermetic_agent(&config, dir.path());
    agent.usage_scope = agent.usage_scope.next_turn(1).unwrap();
    // Hold the only worker slot of this turn in flight, as an external worker request would.
    let _held = agent
        .usage_scope
        .worker()
        .reserve_external("test".into())
        .unwrap();

    let started = agent.side_question(&json!({"action":"start","question":"q"}));
    assert!(!started.is_error, "{}", started.text);
    let id = started.data.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let done = agent.side_question(&json!({"action":"wait","id":id,"timeout":5}));
    let data = done.data.unwrap();
    assert_eq!(data[0]["status"], "failed", "{data}");
    let error = data[0]["error"].as_str().unwrap_or_default();
    assert!(error.contains("worker request budget exhausted"), "{error}");
    assert!(
        server.requests().is_empty(),
        "cap rejection must not send a request"
    );
    let (_, turn) = agent.usage_scope.snapshot();
    assert_eq!(turn.workers.requests, 0);
}

#[test]
fn unfinished_tool_batch_is_closed_only_in_the_side_copy() {
    let server = fake::serve(vec![Script::Sse(sse_text("closed"))]);
    let dir = tempfile::tempdir().unwrap();
    let config = side_config(&server.url, dir.path());
    let mut agent = hermetic_agent(&config, dir.path());
    agent
        .push(json!({"role":"user","content":"run the build"}))
        .unwrap();
    agent
        .push(json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_pending","type":"function","function":{"name":"bash","arguments":"{}"}}]}))
        .unwrap();

    let started = agent.side_question(&json!({"action":"start","question":"what is pending?"}));
    assert!(!started.is_error, "{}", started.text);
    let id = started.data.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let done = agent.side_question(&json!({"action":"wait","id":id,"timeout":5}));
    let data = done.data.unwrap();
    assert_eq!(data[0]["status"], "completed", "{data}");

    let requests = server.requests();
    let body = fake::body(&requests[0]);
    let messages = body["messages"].as_array().expect("messages array");
    let assistant = messages
        .iter()
        .position(|m| m["tool_calls"].is_array())
        .expect("assistant tool batch present");
    let closed = messages
        .iter()
        .position(|m| m["tool_call_id"] == "call_pending")
        .expect("pending call closed in the side copy");
    assert!(closed > assistant, "reply must follow its call: {body}");
    assert!(
        messages[closed]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("pending at snapshot time"),
        "{body}"
    );
    // The live session is unchanged: no synthetic reply was appended to it.
    assert!(
        !agent
            .session
            .messages()
            .iter()
            .any(|m| m["tool_call_id"] == "call_pending"),
        "live session must not gain a synthetic tool reply"
    );
}
