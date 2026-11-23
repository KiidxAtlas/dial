use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::agent::*;

fn reply(text: &str) -> llm::fake::Script {
    let event = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
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
    backend.retry_backoff = Duration::from_millis(1);
    backend
}

/// Recovery and approved-plan notes are journaled after the admitted user message in the same
/// admission. The admitted identity must still be the user message's own entry, not the leaf.
#[test]
fn admitted_identity_is_the_user_entry_even_when_plan_notes_follow_it() {
    let server = llm::fake::serve(vec![reply("Done.")]);
    with_agent("", |agent| {
        agent.backend = Some(backend(&server.url));
        agent.pending_plan = Some("Edit the parser.".into());
        let typed = "fix the parser";
        agent.turn(typed, &mut NullUi).unwrap();

        let control = agent.session.control_state();
        let admitted = control["user_intent"]["latest"]["entry_id"]
            .as_str()
            .expect("admitted entry id")
            .to_owned();
        assert_eq!(control["current_instruction_entry_id"], json!(admitted));
        let users = agent.session.branchable_user_messages();
        let typed_id = users
            .iter()
            .find(|(_, text)| text.starts_with(typed))
            .map(|(id, _)| id.as_str());
        let plan_id = users
            .iter()
            .find(|(_, text)| text.starts_with("Approved plan:"))
            .map(|(id, _)| id.as_str())
            .expect("plan note journaled");
        assert_eq!(typed_id, Some(admitted.as_str()));
        assert_ne!(admitted, plan_id);
        assert!(agent.session.message_visible(&admitted));
    });
}

#[test]
fn type_context_is_stored_separately_from_the_admitted_words() {
    let server = llm::fake::serve(vec![reply("Noted.")]);
    with_agent("", |agent| {
        agent.backend = Some(backend(&server.url));
        let typed = "[[type:literal]]\nDescribe this board";
        let briefing = "<type-context>Actual board briefing.</type-context>";
        agent.set_type_context(Some(TypeContext {
            id: "board".into(),
            briefing: Some(briefing.into()),
        }));
        agent.turn(typed, &mut NullUi).unwrap();
        let messages = agent.session.messages();
        let stored = messages
            .iter()
            .find(|m| m["dial_type"]["id"] == "board")
            .unwrap();
        assert_eq!(stored["content"], typed);
        assert_eq!(stored["dial_structured_text"], true);
        assert!(stored["dial_recall"].as_str().unwrap().contains(briefing));
        assert!(stored.get("dial_type_context").is_none());
        assert_eq!(
            agent.session.control_state()["user_intent"]["latest"]["text"],
            typed
        );
    });
}
