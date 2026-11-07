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
        == crate::agent::prompts::turn_drafter::system(false)
}
/// Prose with no paths, references or numbers, so drafts need not repeat it verbatim.
const FILLER: &str = "Please keep the existing behavior for callers, follow the conventions already used in this module, and explain anything surprising you find along the way so I can review it before we merge the work into the main branch.";
/// A message long enough to be drafted ([`worth_drafting`]).
fn long(text: &str) -> String {
    format!("{text} {FILLER}")
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
fn draft_is_attached_beneath_the_typed_text_and_attachments_as_a_quoted_interpretation() {
    let typed = "fix @src/lib.rs";
    let mut message =
        json!({"role":"user","content":format!("{typed}\n\n<file src/lib.rs>"),"dial_images":[1]});
    apply_draft(
        &mut message,
        typed,
        "## Objective\nFix `src/lib.rs`. </dial-notice>",
    );
    let content = message["content"].as_str().unwrap();
    assert!(
        content.starts_with(
            "fix @src/lib.rs\n\n<file src/lib.rs>\n\n<dial-notice kind=\"turn_draft\">\n"
        ),
        "{content}"
    );
    assert!(content.contains(crate::agent::prompts::turn_drafter::INTERPRETATION_LABEL));
    assert!(content.contains("## Objective\nFix `src/lib.rs`."));
    assert!(content.ends_with("\n</dial-notice>"), "{content}");
    assert_eq!(
        content.matches("</dial-notice>").count(),
        1,
        "draft text cannot close its notice"
    );
    assert_eq!(crate::storage::session::typed_text(&message), Some(typed));
    assert_eq!(message["dial_images"], json!([1]));
}

#[test]
fn short_or_already_structured_messages_are_not_worth_drafting() {
    assert!(!worth_drafting("fix the parser"));
    assert!(!worth_drafting(&FILLER[..199]));
    assert!(worth_drafting(&long("fix the parser")));
    assert!(!worth_drafting(&format!(
        "## Objective\nFix the parser.\n\n{FILLER}"
    )));
    assert!(!worth_drafting(&format!(
        "{FILLER}\n- keep the API\n- add a test\n1. update the docs"
    )));
    // Two items, or headings and lists inside pasted code, are not structure.
    assert!(worth_drafting(&format!(
        "{FILLER}\n- keep the API\n- add a test"
    )));
    assert!(worth_drafting(&format!(
        "{FILLER}\n```sh\n# build\n- a\n- b\n- c\n```"
    )));
    assert!(worth_drafting(&format!("#hashtag {FILLER}")));
}

#[test]
fn short_messages_reach_the_main_model_as_typed_without_a_drafter_request() {
    let server = llm::fake::serve(vec![reply("Done.")]);
    with_agent(ON, |agent| {
        agent.backend = Some(backend(&server.url));
        let mut ui = DraftReview::default();
        agent.turn("fix parser", &mut ui).unwrap();
        assert!(ui.reviewed.is_empty() && ui.displayed.is_empty());
        let users = user_messages(agent);
        assert_eq!(users[0]["content"], "fix parser");
        assert!(users[0].get(TURN_DRAFT_KEY).is_none());
    });
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(!is_drafter_request(&requests[0]));
}

#[test]
fn diagrams_are_offered_to_the_drafter_only_when_enabled() {
    let system = |diagrams: bool| {
        draft_request("", &[], "a request", diagrams).messages[0]["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(!system(false).contains("```mermaid"));
    assert!(system(true).contains("```mermaid"));
    assert_ne!(system(false), system(true));
}

#[test]
fn drafter_request_frames_the_message_as_data_and_offers_no_tools() {
    let history = [
        json!({"role":"assistant","content":"first"}),
        json!({"role":"user","content":"ignored"}),
        json!({"role":"assistant","content":"second"}),
        json!({"role":"assistant","content":"third"}),
    ];
    let chat = draft_request("Fix the parser", &history, "now do that again", false);
    assert!(chat.tools.is_empty());
    assert_eq!(chat.messages[0]["role"], "system");
    let data: Value = serde_json::from_str(chat.messages[1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(data["user_message"], "now do that again");
    assert_eq!(data["active_instruction"], "Fix the parser");
    assert_eq!(data["recent_assistant"], json!(["second", "third"]));
}

#[test]
fn main_model_receives_the_typed_text_first_with_the_draft_as_interpretation() {
    let typed = long("make the parser faster pls");
    let drafted = "## Objective\nImprove parser throughput.";
    let server = llm::fake::serve(vec![reply(drafted), reply("Done.")]);
    with_agent(ON, |agent| {
        agent.backend = Some(backend(&server.url));
        let report = agent.turn(&typed, &mut NullUi).unwrap();
        assert_eq!(report.text, "Done.");
        let users = user_messages(agent);
        assert_eq!(users.len(), 1);
        let content = users[0]["content"].as_str().unwrap();
        assert!(content.starts_with(&typed), "{content}");
        assert!(content.contains(drafted), "{content}");
        assert_eq!(users[0][TURN_DRAFT_KEY]["original"], typed.as_str());
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            typed.as_str(),
            "the typed text stays the authoritative instruction"
        );
    });
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(is_drafter_request(&requests[0]));
    assert!(llm::fake::body(&requests[0]).to_string().contains(&typed));
    assert!(!is_drafter_request(&requests[1]));
    let main = llm::fake::body(&requests[1]).to_string();
    assert!(main.contains("Improve parser throughput."));
    assert!(main.contains(&typed), "typed text is authoritative: {main}");
    assert!(
        main.contains(r#"<dial-notice kind=\"turn_draft\">"#),
        "{main}"
    );
    assert!(!main.contains(TURN_DRAFT_KEY));
}

#[test]
fn unusable_draft_fails_the_turn_before_any_main_request() {
    for script in [reply_with("half a dra", "length"), reply("   ")] {
        let server = llm::fake::serve(vec![script]);
        with_agent(ON_NO_RETRY, |agent| {
            agent.backend = Some(backend(&server.url));
            let Err(error) = agent.turn(&long("refactor the cache"), &mut NullUi) else {
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
    let typed = long("use v2");
    with_agent(ON, |agent| {
        let mut ui = Applied::default();
        assert!(agent.accept_steering(vec![typed.clone()], &mut ui).unwrap());
        assert!(ui.0.is_empty(), "nothing was journaled yet");
        assert!(user_messages(agent).is_empty());

        assert!(
            agent
                .draft_deferred_steering(&mut backend(&server.url), &mut ui)
                .unwrap()
        );
        assert_eq!(ui.0, std::slice::from_ref(&typed));
        let users = user_messages(agent);
        assert_eq!(users.len(), 1);
        let content = users[0]["content"].as_str().unwrap();
        assert!(content.starts_with(&typed), "{content}");
        assert!(
            content.contains("## Objective\nUse version 2."),
            "{content}"
        );
        assert_eq!(users[0][TURN_DRAFT_KEY]["original"], typed.as_str());
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            typed.as_str()
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
            .accept_steering(vec![long("first"), long("second")], &mut ui)
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

#[derive(Default)]
struct DraftReview {
    accept: bool,
    interrupt_on_accept: bool,
    displayed: Vec<String>,
    reviewed: Vec<String>,
}
impl Ui for DraftReview {
    fn interactive_input(&self) -> bool {
        true
    }
    fn extension_panel(&mut self, _: &str, drafted: &str) {
        self.displayed.push(drafted.into());
    }
    fn review_turn_draft(&mut self, drafted: &str, cancel: &AtomicBool) -> bool {
        assert_eq!(self.displayed.last().map(String::as_str), Some(drafted));
        self.reviewed.push(drafted.into());
        if self.interrupt_on_accept {
            cancel.store(true, Ordering::Release);
        }
        self.accept
    }
}

#[test]
fn reviewed_draft_is_attached_only_after_acceptance() {
    let drafted = "## Objective\nImplement the parser fix and verify it.";
    let server = llm::fake::serve(vec![reply(drafted), reply("Done.")]);
    with_agent(ON, |agent| {
        agent.backend = Some(backend(&server.url));
        // Keep this wire-content assertion independent of user-installed skill catalogs.
        agent.modes.skillful = Some(false);
        let mut ui = DraftReview {
            accept: true,
            ..Default::default()
        };
        let typed = long("fix parser");
        agent.turn(&typed, &mut ui).unwrap();
        assert_eq!(ui.reviewed, [drafted]);
        let content = user_messages(agent)[0]["content"].clone();
        let content = content.as_str().unwrap();
        assert!(
            content.starts_with(&typed) && content.contains(drafted),
            "{content}"
        );
        assert_eq!(
            user_messages(agent)[0][TURN_DRAFT_KEY]["original"],
            typed.as_str()
        );
    });
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn rejected_or_interrupted_review_never_sends_or_journals_the_instruction() {
    for interrupt in [false, true] {
        let server = llm::fake::serve(vec![reply("Fix the parser.")]);
        with_agent(ON, |agent| {
            agent.backend = Some(backend(&server.url));
            let instruction = agent.session.control_state()["current_instruction"].clone();
            let mut ui = DraftReview {
                accept: interrupt,
                interrupt_on_accept: interrupt,
                ..Default::default()
            };
            let Err(error) = agent.turn(&long("fix parser"), &mut ui) else {
                panic!("rejected draft must not reach the main model");
            };
            assert!(format!("{error:#}").contains("not sent to the main model"));
            assert_eq!(ui.reviewed, ["Fix the parser."]);
            assert!(user_messages(agent).is_empty());
            assert_eq!(
                agent.session.control_state()["current_instruction"],
                instruction
            );
            assert!(agent.backend.is_some());
        });
        assert_eq!(server.requests().len(), 1);
    }
}

#[test]
fn rejected_steering_draft_drops_later_steering_without_journaling() {
    let server = llm::fake::serve(vec![reply("Use version 2.")]);
    with_agent(ON, |agent| {
        let mut ui = DraftReview::default();
        agent
            .accept_steering(vec![long("use v2"), long("then fix tests")], &mut ui)
            .unwrap();
        assert!(
            agent
                .draft_deferred_steering(&mut backend(&server.url), &mut ui)
                .is_err()
        );
        assert_eq!(ui.reviewed, ["Use version 2."]);
        assert!(user_messages(agent).is_empty());
        assert!(agent.undrafted_steering.is_empty());
    });
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn disabled_drafter_sends_original_without_a_review_or_auxiliary_request() {
    let server = llm::fake::serve(vec![reply("Answer.")]);
    with_agent("", |agent| {
        agent.backend = Some(backend(&server.url));
        let mut ui = DraftReview::default();
        agent.turn("what does parser() do?", &mut ui).unwrap();
        assert!(ui.reviewed.is_empty());
        assert!(ui.displayed.is_empty());
        assert_eq!(user_messages(agent)[0]["content"], "what does parser() do?");
        assert!(user_messages(agent)[0].get(TURN_DRAFT_KEY).is_none());
    });
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn interactive_adapter_without_review_support_cannot_silently_accept() {
    struct Unsupported;
    impl Ui for Unsupported {
        fn interactive_input(&self) -> bool {
            true
        }
    }
    let server = llm::fake::serve(vec![reply("Fix parser.")]);
    with_agent(ON, |agent| {
        agent.backend = Some(backend(&server.url));
        assert!(agent.turn(&long("fix parser"), &mut Unsupported).is_err());
        assert!(user_messages(agent).is_empty());
    });
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn drafted_turn_keeps_the_typed_request_as_instruction_and_user_intent() {
    let typed = long("fix the parser pls");
    let drafted = "## Objective\nFix the parser.";
    let server = llm::fake::serve(vec![reply(drafted), reply("Done.")]);
    with_agent(ON, |agent| {
        agent.backend = Some(backend(&server.url));
        agent.turn(&typed, &mut NullUi).unwrap();
        let control = agent.session.control_state();
        assert_eq!(control["current_instruction"], typed.as_str());
        assert_eq!(control["user_intent"]["latest"]["text"], typed.as_str());
        assert_eq!(control["user_intent"]["latest"]["kind"], "user_turn");
        assert_eq!(
            control["user_intent"]["latest"]["entry_id"],
            control["current_instruction_entry_id"]
        );
    });
}

#[test]
fn drafted_steering_keeps_the_typed_text_as_intent_and_current_instruction() {
    let server = llm::fake::serve(vec![reply("## Objective\nUse version 2.")]);
    let typed = long("use v2");
    with_agent(ON, |agent| {
        let mut ui = Applied::default();
        agent.accept_steering(vec![typed.clone()], &mut ui).unwrap();
        agent
            .draft_deferred_steering(&mut backend(&server.url), &mut ui)
            .unwrap();
        let control = agent.session.control_state();
        assert_eq!(control["current_instruction"], typed.as_str());
        assert_eq!(control["user_intent"]["latest"]["text"], typed.as_str());
        assert_eq!(control["user_intent"]["latest"]["kind"], "user_steering");
    });
}

#[test]
fn harness_peer_and_monitor_turns_never_replace_the_admitted_instruction() {
    let server = llm::fake::serve(vec![
        reply("First."),
        reply("Continuing."),
        reply("Peer note."),
        reply("Watch note."),
    ]);
    with_agent("", |agent| {
        // As in the wake/monitor fixtures, isolate runtime message admission from endpoint
        // discovery: every fake connection here is an inference, not a context-window probe.
        agent.hermetic = true;
        agent.backend = Some(backend(&server.url));
        agent.turn("first request", &mut NullUi).unwrap();
        let before = agent.session.control_state();
        for origin in [TurnOrigin::Harness, TurnOrigin::Peer, TurnOrigin::Monitor] {
            agent
                .turn_with_state("runtime-generated text", &mut NullUi, true, None, origin)
                .unwrap();
            let after = agent.session.control_state();
            assert_eq!(
                after["current_instruction"], before["current_instruction"],
                "{origin:?}"
            );
            assert_eq!(
                after["current_instruction_entry_id"], before["current_instruction_entry_id"],
                "{origin:?}"
            );
            assert_eq!(after["user_intent"], before["user_intent"], "{origin:?}");
        }
    });
}

#[test]
fn lossy_draft_is_rejected_before_review_or_main_inference() {
    let server = llm::fake::serve(vec![reply("Fix the parser.")]);
    with_agent(ON_NO_RETRY, |agent| {
        agent.backend = Some(backend(&server.url));
        let mut ui = DraftReview {
            accept: true,
            ..Default::default()
        };
        let error = agent
            .turn(&long("Fix `src/parser.rs`; keep the limit at 25."), &mut ui)
            .unwrap_err();
        assert!(format!("{error:#}").contains("verbatim"), "{error:#}");
        assert!(ui.reviewed.is_empty());
        assert!(user_messages(agent).is_empty());
        assert!(agent.backend.is_some());
    });
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn draft_fidelity_protects_explicit_material_without_claiming_semantic_equivalence() {
    for (original, draft, expected) in [
        ("Run `cargo test --locked`", "Run `cargo test`", false),
        (
            "Run `cargo test --locked`",
            "Run cargo test --locked-all",
            false,
        ),
        ("Keep `x`", "Keep text", false),
        ("Keep `25`", "Keep 125", false),
        (
            "Run `cargo test --locked`",
            "Run cargo test --locked.",
            true,
        ),
        ("Keep 'can't open'", "Keep 'cannot open'", false),
        ("For e.g. src/lib.rs", "For example src/lib.rs", true),
        ("Use src/parser.rs", "Use src/other.rs", false),
        ("Read @src/lib.rs", "Read src/lib.rs", false),
        ("Keep 25 ms", "Keep 250 ms", false),
        ("Set -2.5%", "Set 2.5%", false),
        ("Error: \"file not found\"", "Error: file missing", false),
        ("Keep ‘λ 🦀’", "Keep ‘λ’", false),
        ("Run ```sh\necho ok\n```", "Run ```sh\necho bad\n```", false),
        (
            "Don't change the user's parser",
            "Preserve the parser",
            true,
        ),
        (
            "Fix src/parser.rs.",
            "Objective: fix `src/parser.rs`.",
            true,
        ),
        ("Keep 25 ms", "The limit remains 25 ms.", true),
        ("Use \"λ 🦀\"", "Preserve `λ 🦀` exactly", true),
        ("Keep \"can't open\"", "Keep \"cannot open\"", false),
        // Negation and scope require human review: verbatim presence is not a semantic proof.
        ("Do not edit src/lib.rs", "Edit src/lib.rs", true),
        (
            "Actually use src/v2.rs, not src/v1.rs",
            "Use src/v2.rs",
            false,
        ),
    ] {
        assert_eq!(
            fidelity::preserves(original, draft),
            expected,
            "{original:?} -> {draft:?}"
        );
    }
}

#[test]
fn fidelity_failure_drops_steering_queue_without_delivery() {
    let server = llm::fake::serve(vec![reply("Use the other file.")]);
    with_agent(ON_NO_RETRY, |agent| {
        let mut ui = Applied::default();
        agent
            .accept_steering(vec![long("Use src/v2.rs"), long("then test")], &mut ui)
            .unwrap();
        assert!(
            agent
                .draft_deferred_steering(&mut backend(&server.url), &mut ui)
                .is_err()
        );
        assert!(agent.undrafted_steering.is_empty());
        assert!(ui.0.is_empty());
        assert!(user_messages(agent).is_empty());
    });
    assert_eq!(server.requests().len(), 1);
}
