use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

const ON: &str = "[experimental]\nturn_optimizer=true";
/// Failure cases are not re-sent automatically after the drafter request fails.
const ON_NO_RETRY: &str =
    "[experimental]\nturn_optimizer=true\n[request_recovery]\nretry_once=false";

fn reply(text: &str) -> llm::fake::Script {
    reply_with(text, "stop")
}
fn reply_with(text: &str, finish_reason: &str) -> llm::fake::Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":finish_reason}]});
    llm::fake::Script::Sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}
fn backend(url: &str) -> Backend {
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{url}/v1"),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    // Once a script is spent the fake refuses connections; retry those without waiting.
    backend.retry_backoff = Duration::from_millis(1);
    backend
}
fn is_drafter_request(request: &[u8]) -> bool {
    llm::fake::body(request)["messages"][0]["content"]
        == crate::agent::prompts::turn_drafter::SYSTEM
}
fn user_messages(agent: &Agent<'_>) -> Vec<Value> {
    agent
        .session
        .messages()
        .into_iter()
        .filter(|message| message["role"] == "user")
        .collect()
}

#[derive(Default)]
struct Applied(Vec<String>);
impl Ui for Applied {
    fn steering_applied(&mut self, text: &str) {
        self.0.push(text.into());
    }
}

#[test]
fn draft_replaces_typed_text_and_keeps_attachments_and_the_original() {
    let typed = "fix @src/lib.rs";
    let mut message =
        json!({"role":"user","content":format!("{typed}\n\n<file src/lib.rs>"),"dial_images":[1]});
    apply_draft(&mut message, typed, "## Objective\nFix `src/lib.rs`.");
    assert_eq!(
        message["content"],
        "## Objective\nFix `src/lib.rs`.\n\n<file src/lib.rs>"
    );
    assert_eq!(crate::storage::session::typed_text(&message), Some(typed));
    assert_eq!(message["dial_images"], json!([1]));
}

#[test]
fn drafter_request_frames_the_message_as_data_and_offers_no_tools() {
    let history = [
        json!({"role":"assistant","content":"first"}),
        json!({"role":"user","content":"ignored"}),
        json!({"role":"assistant","content":"second"}),
        json!({"role":"assistant","content":"third"}),
    ];
    let chat = draft_request("Fix the parser", &history, "now do that again");
    assert!(chat.tools.is_empty());
    assert_eq!(chat.messages[0]["role"], "system");
    let data: Value = serde_json::from_str(chat.messages[1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(data["user_message"], "now do that again");
    assert_eq!(data["active_instruction"], "Fix the parser");
    assert_eq!(data["recent_assistant"], json!(["second", "third"]));
}

#[test]
fn main_model_receives_only_the_drafted_prompt() {
    let typed = "make the parser faster pls";
    let drafted = "## Objective\nImprove parser throughput.";
    let server = llm::fake::serve(vec![reply(drafted), reply("Done.")]);
    with_agent(ON, |agent| {
        agent.backend = Some(backend(&server.url));
        let report = agent.turn(typed, &mut NullUi).unwrap();
        assert_eq!(report.text, "Done.");
        let users = user_messages(agent);
        assert_eq!(users.len(), 1);
        assert_eq!(users[0]["content"], drafted);
        assert_eq!(users[0][TURN_DRAFT_KEY]["original"], typed);
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            drafted
        );
    });
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(is_drafter_request(&requests[0]));
    assert!(llm::fake::body(&requests[0]).to_string().contains(typed));
    assert!(!is_drafter_request(&requests[1]));
    let main = llm::fake::body(&requests[1]).to_string();
    assert!(main.contains("Improve parser throughput."));
    assert!(
        !main.contains(typed),
        "typed text reached the main model: {main}"
    );
    assert!(!main.contains(TURN_DRAFT_KEY));
}

#[test]
fn unusable_draft_fails_the_turn_before_any_main_request() {
    for script in [reply_with("half a dra", "length"), reply("   ")] {
        let server = llm::fake::serve(vec![script]);
        with_agent(ON_NO_RETRY, |agent| {
            agent.backend = Some(backend(&server.url));
            let Err(error) = agent.turn("refactor the cache", &mut NullUi) else {
                panic!("an unusable draft must fail the turn");
            };
            assert!(
                format!("{error:#}").contains("not sent to the main model"),
                "{error:#}"
            );
            assert!(user_messages(agent).is_empty());
            assert!(agent.backend.is_some(), "the backend must be returned");
        });
        assert!(server.requests().iter().all(|r| is_drafter_request(r)));
    }
}

#[test]
fn harness_turns_reach_the_main_model_undrafted() {
    let server = llm::fake::serve(vec![reply("Continuing.")]);
    with_agent(ON, |agent| {
        agent.backend = Some(backend(&server.url));
        agent
            .turn_with_state(
                "Continue the goal.",
                &mut NullUi,
                true,
                None,
                TurnOrigin::Harness,
            )
            .unwrap();
        let users = user_messages(agent);
        assert_eq!(users[0]["content"], "Continue the goal.");
        assert!(users[0].get(TURN_DRAFT_KEY).is_none());
    });
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(!is_drafter_request(&requests[0]));
}

#[test]
fn steering_is_journaled_only_after_it_is_drafted() {
    let server = llm::fake::serve(vec![reply("## Objective\nUse version 2.")]);
    with_agent(ON, |agent| {
        let mut ui = Applied::default();
        assert!(
            agent
                .accept_steering(vec!["use v2".into()], &mut ui)
                .unwrap()
        );
        assert!(ui.0.is_empty(), "nothing was journaled yet");
        assert!(user_messages(agent).is_empty());

        assert!(
            agent
                .draft_deferred_steering(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
        assert_eq!(ui.0, ["use v2"]);
        let users = user_messages(agent);
        assert_eq!(users.len(), 1);
        assert_eq!(users[0]["content"], "## Objective\nUse version 2.");
        assert_eq!(users[0][TURN_DRAFT_KEY]["original"], "use v2");
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            "## Objective\nUse version 2."
        );
        assert!(
            !agent
                .draft_deferred_steering(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
    });
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn failed_steering_draft_drops_the_queue_instead_of_sending_raw_text() {
    let server = llm::fake::serve(vec![reply("")]);
    with_agent(ON_NO_RETRY, |agent| {
        let mut ui = Applied::default();
        agent
            .accept_steering(vec!["first".into(), "second".into()], &mut ui)
            .unwrap();
        let error = agent
            .draft_deferred_steering(&mut backend(&server.url), &mut ui)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("2 steering message(s)"),
            "{error:#}"
        );
        assert!(user_messages(agent).is_empty());
        assert!(ui.0.is_empty());
        assert!(agent.undrafted_steering.is_empty());
    });
}

#[test]
fn steering_is_journaled_directly_while_the_optimizer_is_off() {
    with_agent("", |agent| {
        let mut ui = Applied::default();
        assert!(
            agent
                .accept_steering(vec!["use v2".into()], &mut ui)
                .unwrap()
        );
        assert_eq!(ui.0, ["use v2"]);
        assert!(agent.undrafted_steering.is_empty());
    });
}
