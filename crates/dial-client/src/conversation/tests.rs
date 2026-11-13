use super::*;
use crate::{RequestId, RpcError};
use serde_json::json;

fn event(body: Value) -> Message {
    Message::Event(Event {
        kind: body["event"].as_str().unwrap().to_owned(),
        body,
    })
}

fn response(method: &str, result: Value) -> Message {
    Message::Response {
        id: RequestId(1),
        method: method.to_owned(),
        result: Ok(result),
    }
}

fn feed(view: &mut Conversation, frames: Vec<Value>) {
    for frame in frames {
        view.apply(&event(frame));
    }
}

#[test]
fn a_streamed_turn_becomes_user_assistant_and_tool_rows() {
    let mut view = Conversation::new();
    view.push_user("fix it", 1);
    assert!(view.turn_running());
    feed(
        &mut view,
        vec![
            json!({"event":"reasoning","seq":1,"delta":"think"}),
            json!({"event":"text","seq":2,"delta":"Look"}),
            json!({"event":"text","seq":3,"delta":"ing."}),
            json!({"event":"tool_arguments","seq":4,"id":"c1","name":"read","delta":"{\"path\""}),
            json!({"event":"tool_call","seq":5,"call_id":"c1","name":"read","summary":"read a.rs"}),
            json!({"event":"tool_card","seq":6,"call_id":"c1","label":"Read","target":"a.rs","tone":"tools"}),
            json!({"event":"tool_output","seq":7,"name":"read","delta":"line"}),
            json!({"event":"tool_result","seq":8,"call_id":"c1","name":"read","text":"ok","is_error":false,"status":"ok","changed_paths":["/w/a.rs"]}),
            json!({"event":"text","seq":9,"delta":"Done."}),
        ],
    );
    view.apply(&response("send", json!({"text":"Looking.Done."})));

    let entries = view.entries();
    assert_eq!(entries.len(), 4, "{entries:?}");
    assert_eq!(
        entries[0],
        Entry::User(UserEntry {
            text: "fix it".into(),
            images: 1
        })
    );
    assert_eq!(
        entries[1],
        Entry::Assistant(AssistantEntry {
            text: "Looking.".into(),
            reasoning: "think".into(),
            streaming: false
        })
    );
    let Entry::Tool(tool) = &entries[2] else {
        panic!("tool row expected: {entries:?}")
    };
    assert_eq!(tool.name, "read");
    assert_eq!(tool.summary, "read a.rs");
    assert_eq!(tool.arguments, "{\"path\"");
    assert_eq!(tool.output, "line");
    assert_eq!(tool.card.as_ref().unwrap().target, "a.rs");
    let result = tool.result.as_ref().unwrap();
    assert_eq!(result.status.as_deref(), Some("ok"));
    assert_eq!(result.changed_paths, ["/w/a.rs"]);
    assert!(matches!(&entries[3], Entry::Assistant(a) if a.text == "Done." && !a.streaming));
    assert!(!view.turn_running());
    assert!(!view.needs_reload());
}

#[test]
fn a_turn_without_streamed_text_keeps_the_response_text_and_marks_interruption() {
    let mut view = Conversation::new();
    view.push_user("hi", 0);
    view.apply(&response(
        "send",
        json!({"text":"hello","interrupted":true}),
    ));
    assert!(matches!(&view.entries()[1], Entry::Assistant(a) if a.text == "hello"));
    assert!(
        matches!(&view.entries()[2], Entry::Notice(n) if n.kind.as_deref() == Some("interrupted"))
    );
}

#[test]
fn a_failed_turn_shows_the_error_and_unrelated_responses_change_nothing() {
    let mut view = Conversation::new();
    view.push_user("hi", 0);
    assert!(!view.apply(&response("state", json!({}))));
    view.apply(&Message::Response {
        id: RequestId(2),
        method: "retry".into(),
        result: Err(RpcError {
            code: Some("busy".into()),
            message: "a turn is running".into(),
        }),
    });
    assert_eq!(
        view.entries()[1],
        Entry::Error("busy: a turn is running".into())
    );
    assert!(!view.turn_running());
}

#[test]
fn approvals_and_questions_open_and_close() {
    let mut view = Conversation::new();
    feed(
        &mut view,
        vec![
            json!({"event":"approval","approval_id":"a1","tool":"bash","summary":"rm","call_id":"c9"}),
            json!({"event":"question","request":{"question_id":"q1","text":"which?"}}),
        ],
    );
    assert_eq!(view.pending_approvals().len(), 1);
    assert_eq!(view.pending_approvals()[0].call_id.as_deref(), Some("c9"));
    assert_eq!(view.question().unwrap()["text"], "which?");
    assert!(!view.apply(&event(
        json!({"event":"question_closed","question_id":"other","outcome":"answered"})
    )));
    feed(
        &mut view,
        vec![
            json!({"event":"approval_resolved","approval_id":"a1"}),
            json!({"event":"question_closed","question_id":"q1","outcome":"answered"}),
        ],
    );
    assert!(view.pending_approvals().is_empty());
    assert!(view.question().is_none());
}

#[test]
fn history_loads_messages_with_images_reasoning_and_tool_results() {
    let mut view = Conversation::new();
    view.push_user("stale", 0);
    view.load_history(
        Some("s1"),
        &[
            json!({"role":"system","content":"rules"}),
            json!({"role":"user","content":[{"type":"text","text":"see"},{"type":"image_url","image_url":{"url":"data:"}}]}),
            json!({"role":"assistant","content":null,"reasoning_content":"plan","tool_calls":[{"id":"c1","type":"function","function":{"name":"grep","arguments":"{\"q\":1}"}}]}),
            json!({"role":"tool","tool_call_id":"c1","content":"3 hits"}),
            json!({"role":"assistant","content":"found"}),
        ],
    );
    let entries = view.entries();
    assert_eq!(view.session_id(), Some("s1"));
    assert_eq!(entries.len(), 4, "{entries:?}");
    assert_eq!(
        entries[0],
        Entry::User(UserEntry {
            text: "see".into(),
            images: 1
        })
    );
    assert!(
        matches!(&entries[1], Entry::Assistant(a) if a.reasoning == "plan" && a.text.is_empty())
    );
    let Entry::Tool(tool) = &entries[2] else {
        panic!("tool row expected: {entries:?}")
    };
    assert_eq!(
        (tool.name.as_str(), tool.arguments.as_str()),
        ("grep", "{\"q\":1}")
    );
    assert_eq!(tool.result.as_ref().unwrap().text, "3 hits");
    assert!(matches!(&entries[3], Entry::Assistant(a) if a.text == "found"));
}

#[test]
fn a_session_switch_replays_the_new_history() {
    let mut view = Conversation::new();
    feed(
        &mut view,
        vec![
            json!({"event":"ready","session_id":"s1"}),
            json!({"event":"text","delta":"old"}),
            json!({"event":"todo","tasks":[{"content":"x"}]}),
            json!({"event":"active_session","session_id":"s2"}),
            json!({"event":"replay_message","message":{"role":"user","content":"new"}}),
        ],
    );
    assert_eq!(view.session_id(), Some("s2"));
    assert!(view.todo().is_empty());
    assert_eq!(
        view.entries(),
        [Entry::User(UserEntry {
            text: "new".into(),
            images: 0
        })]
    );
}

#[test]
fn a_sequence_gap_asks_for_a_reload_and_history_clears_it() {
    let mut view = Conversation::new();
    feed(
        &mut view,
        vec![
            json!({"event":"text","seq":1,"delta":"a"}),
            json!({"event":"text","seq":3,"delta":"b"}),
        ],
    );
    assert!(view.needs_reload());
    view.load_history(None, &[]);
    assert!(!view.needs_reload());
    assert!(view.entries().is_empty());
}

#[test]
fn a_disconnect_ends_the_turn_and_drops_live_prompts() {
    let mut view = Conversation::new();
    view.push_user("hi", 0);
    feed(
        &mut view,
        vec![json!({"event":"approval","approval_id":"a1","tool":"bash","summary":"x"})],
    );
    view.apply(&Message::Disconnected {
        outstanding: Vec::new(),
    });
    assert!(!view.turn_running());
    assert!(view.pending_approvals().is_empty());
    assert!(matches!(view.entries().last(), Some(Entry::Error(_))));
}

#[test]
fn unknown_events_are_ignored() {
    let mut view = Conversation::new();
    assert!(!view.apply(&event(json!({"event":"from_the_future","x":1}))));
    assert!(view.entries().is_empty());
}
