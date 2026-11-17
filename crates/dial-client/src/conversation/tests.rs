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
            streaming: false,
            abandoned: false,
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
fn a_sequence_gap_asks_for_history_and_live_state_reload() {
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
    assert!(
        view.needs_reload(),
        "history cannot repair lost live prompts"
    );
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

#[test]
fn unrelated_turn_responses_do_not_settle_the_tracked_turn() {
    let mut view = Conversation::new();
    view.push_turn("fix it", 0, RequestId(5));
    let busy = Message::Response {
        id: RequestId(2),
        method: "retry".into(),
        result: Err(RpcError {
            code: Some("busy".into()),
            message: "a turn is running".into(),
        }),
    };
    assert!(!view.apply(&busy));
    assert!(view.turn_running());
    assert_eq!(view.entries().len(), 1);
    view.apply(&Message::Response {
        id: RequestId(5),
        method: "send".into(),
        result: Ok(json!({"text":"done"})),
    });
    assert!(!view.turn_running());
    assert!(matches!(view.entries().last(), Some(Entry::Assistant(a)) if a.text == "done"));
}

#[test]
fn a_retried_answer_is_kept_when_an_earlier_attempt_streamed_text() {
    let mut view = Conversation::new();
    view.push_user("hi", 0);
    feed(
        &mut view,
        vec![json!({"event":"text","seq":1,"delta":"partial"})],
    );
    view.apply(&response("send", json!({"text":"partial"})));
    view.expect_turn(RequestId(2));
    view.apply(&Message::Response {
        id: RequestId(2),
        method: "retry".into(),
        result: Ok(json!({"text":"full answer"})),
    });
    let entries = view.entries();
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert!(matches!(&entries[1], Entry::Assistant(a) if a.text == "partial"));
    assert!(matches!(&entries[2], Entry::Assistant(a) if a.text == "full answer"));
}

#[test]
fn tool_result_keeps_effects_and_structured_data() {
    let mut view = Conversation::new();
    feed(
        &mut view,
        vec![
            json!({"event":"tool_call","seq":1,"call_id":"c1","name":"bash","summary":"sleep"}),
            json!({"event":"tool_result","seq":2,"call_id":"c1","name":"bash","text":"timed out","is_error":true,"status":"timeout","effects":"unknown","data":{"exit_code":124},"changed_paths":[]}),
        ],
    );
    let Entry::Tool(tool) = &view.entries()[0] else {
        panic!("tool row expected: {:?}", view.entries())
    };
    let result = tool.result.as_ref().expect("settled");
    assert_eq!(result.effects.as_deref(), Some("unknown"));
    assert_eq!(result.data, Some(json!({"exit_code":124})));
}

#[test]
fn a_session_switch_forgets_tool_rows_so_call_ids_do_not_reattach() {
    let mut view = Conversation::new();
    feed(
        &mut view,
        vec![
            json!({"event":"tool_call","seq":1,"call_id":"c1","name":"read","summary":"old"}),
            json!({"event":"active_session","seq":2,"session_id":"s2"}),
            json!({"event":"tool_result","seq":3,"call_id":"c1","name":"read","text":"new","is_error":false}),
        ],
    );
    assert_eq!(view.entries().len(), 1, "{:?}", view.entries());
    let Entry::Tool(tool) = &view.entries()[0] else {
        panic!("tool row expected: {:?}", view.entries())
    };
    assert!(tool.summary.is_empty());
    assert_eq!(tool.result.as_ref().map(|r| r.text.as_str()), Some("new"));
}

#[test]
fn an_automatic_retry_abandons_the_streamed_attempt_and_streams_a_fresh_row() {
    let mut view = Conversation::new();
    view.push_turn("hi", 0, RequestId(5));
    feed(
        &mut view,
        vec![
            json!({"event":"reasoning","seq":1,"delta":"plan"}),
            json!({"event":"text","seq":2,"delta":"partial"}),
            json!({"event":"notice","seq":3,"kind":"retry","lane":"context","message":"Provider retry"}),
            json!({"event":"text","seq":4,"delta":"fresh"}),
        ],
    );
    let entries = view.entries();
    assert_eq!(entries.len(), 4, "{entries:?}");
    assert!(matches!(
        &entries[1],
        Entry::Assistant(a) if a.text == "partial" && a.reasoning == "plan" && a.abandoned && !a.streaming
    ));
    assert!(matches!(&entries[2], Entry::Notice(n) if n.kind.as_deref() == Some("retry")));
    assert!(matches!(
        &entries[3],
        Entry::Assistant(a) if a.text == "fresh" && !a.abandoned && a.streaming
    ));
    // The response text must not replace what the attempt streamed after the boundary.
    view.apply(&Message::Response {
        id: RequestId(5),
        method: "send".into(),
        result: Ok(json!({"text":"fresh"})),
    });
    assert_eq!(view.entries().len(), 4);
    assert!(!view.turn_running());
}

#[test]
fn an_abandoned_attempt_is_not_the_response_fallback_and_unrelated_replies_are_ignored() {
    let mut view = Conversation::new();
    view.push_turn("hi", 0, RequestId(5));
    feed(
        &mut view,
        vec![
            json!({"event":"text","seq":1,"delta":"partial"}),
            json!({"event":"notice","seq":2,"kind":"retry","lane":"context","message":"Provider retry"}),
        ],
    );
    let busy = Message::Response {
        id: RequestId(2),
        method: "retry".into(),
        result: Err(RpcError {
            code: Some("busy".into()),
            message: "a turn is running".into(),
        }),
    };
    assert!(!view.apply(&busy));
    assert!(view.turn_running());
    view.apply(&Message::Response {
        id: RequestId(5),
        method: "send".into(),
        result: Ok(json!({"text":"final answer"})),
    });
    let entries = view.entries();
    assert_eq!(entries.len(), 4, "{entries:?}");
    assert!(matches!(&entries[1], Entry::Assistant(a) if a.text == "partial" && a.abandoned));
    assert!(matches!(
        &entries[3],
        Entry::Assistant(a) if a.text == "final answer" && !a.abandoned && !a.streaming
    ));
}

#[test]
fn same_name_tool_output_and_progress_route_by_call_id() {
    let mut view = Conversation::new();
    feed(
        &mut view,
        vec![
            json!({"event":"tool_call","seq":1,"call_id":"c1","name":"bash","summary":"first"}),
            json!({"event":"tool_call","seq":2,"call_id":"c2","name":"bash","summary":"second"}),
            json!({"event":"tool_output","seq":3,"call_id":"c1","name":"bash","delta":"a"}),
            json!({"event":"tool_output","seq":4,"call_id":"c2","name":"bash","delta":"b"}),
            json!({"event":"tool_progress","seq":5,"call_id":"c1","name":"bash","text":"step 1"}),
            json!({"event":"tool_progress","seq":6,"call_id":"c1","name":"bash","text":"step 2"}),
            json!({"event":"tool_progress","seq":7,"call_id":"c2","name":"bash","text":"other"}),
            json!({"event":"tool_result","seq":8,"call_id":"c2","name":"bash","text":"done","is_error":false}),
            json!({"event":"tool_progress","seq":9,"call_id":"c2","name":"bash","text":"late"}),
        ],
    );
    let tools: Vec<&ToolEntry> = view
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::Tool(tool) => Some(tool.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 2, "{:?}", view.entries());
    assert_eq!(tools[0].call_id, "c1");
    assert_eq!(tools[0].output, "a");
    assert_eq!(tools[0].progress.as_deref(), Some("step 2"));
    assert!(tools[0].result.is_none());
    assert_eq!(tools[1].call_id, "c2");
    assert_eq!(tools[1].output, "b");
    assert!(
        tools[1].progress.is_none(),
        "a settled call has no live status"
    );
    assert!(tools[1].result.is_some());
}

fn ready_frame(session: &str) -> Ready {
    Ready::parse(json!({
        "event": "ready",
        "seq": 1,
        "session_id": session,
        "protocol_version": 2,
        "methods": [],
    }))
    .expect("a ready frame parses")
}

/// One reconnect scenario: the old connection's `(seq, delta)` text events, then a new connection
/// in `session` whose events follow `new_connection`.
struct Reconnect {
    name: &'static str,
    old: &'static [(u64, &'static str)],
    session: &'static str,
    new: &'static [(u64, &'static str)],
    reload: bool,
    /// The assistant rows shown afterwards. A new connection settles the streamed answer, so text
    /// from the new connection starts a new row.
    rows: &'static [&'static str],
}

/// Reconnect continuity versus reload gaps. The view starts in session `s1`, as a loaded history
/// does, so a reconnect to `s1` keeps its rows and a reconnect to `s2` replaces them.
#[test]
fn a_reconnect_continues_the_sequence_unless_events_were_lost() {
    let cases = [
        Reconnect {
            name: "same session, contiguous seq from ready",
            old: &[(2, "a"), (3, "b")],
            session: "s1",
            new: &[(2, "c"), (3, "d")],
            reload: false,
            rows: &["ab", "cd"],
        },
        Reconnect {
            name: "a gap on the new connection asks for a reload",
            old: &[(2, "a"), (3, "b")],
            session: "s1",
            new: &[(2, "c"), (4, "d")],
            reload: true,
            rows: &["ab", "cd"],
        },
        Reconnect {
            name: "a gap on the old connection survives the reconnect",
            old: &[(2, "a"), (4, "b")],
            session: "s1",
            new: &[(2, "c")],
            reload: true,
            rows: &["ab", "c"],
        },
        Reconnect {
            name: "a different session replaces the rows and needs no reload",
            old: &[(2, "a"), (3, "b")],
            session: "s2",
            new: &[(2, "c")],
            reload: false,
            rows: &["c"],
        },
    ];
    let frames = |pairs: &[(u64, &str)]| {
        pairs
            .iter()
            .map(|(seq, delta)| json!({"event":"text","seq":seq,"delta":delta}))
            .collect::<Vec<_>>()
    };
    for case in cases {
        let mut view = Conversation::new();
        view.load_history(Some("s1"), &[]);
        feed(&mut view, frames(case.old));
        view.new_connection(&ready_frame(case.session));
        assert_eq!(view.session_id(), Some(case.session), "{}", case.name);
        feed(&mut view, frames(case.new));
        assert_eq!(view.needs_reload(), case.reload, "{}", case.name);
        let rows: Vec<&str> = view
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                Entry::Assistant(answer) => Some(answer.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(rows, case.rows, "{}", case.name);
    }
}

/// A reconnect voids the old connection's ids: its approvals, question and turn. The rows and the
/// tool results already shown stay, and the streaming answer and running tool are settled.
#[test]
fn a_new_connection_voids_its_prompts_and_settles_its_turn_without_losing_history() {
    let mut view = Conversation::new();
    view.load_history(Some("s1"), &[json!({"role":"user","content":"earlier"})]);
    view.push_turn("now", 0, RequestId(7));
    feed(
        &mut view,
        vec![
            json!({"event":"text","seq":2,"delta":"half"}),
            json!({"event":"tool_call","seq":3,"call_id":"c1","name":"bash","summary":"sleep"}),
            json!({"event":"tool_progress","seq":4,"call_id":"c1","name":"bash","text":"step 1"}),
            json!({"event":"approval","seq":5,"approval_id":"a1","tool":"bash","summary":"rm"}),
            json!({"event":"question","seq":6,"request":{"question_id":"q1"}}),
        ],
    );
    let shown = view.entries().len();

    view.new_connection(&ready_frame("s1"));

    assert!(view.pending_approvals().is_empty(), "approval ids are void");
    assert!(view.question().is_none(), "question ids are void");
    assert!(
        !view.turn_running(),
        "the old turn cannot be settled by the new connection"
    );
    assert!(!view.needs_reload());
    let entries = view.entries();
    assert_eq!(
        entries.len(),
        shown + 1,
        "history stays and one error row is added"
    );
    assert!(matches!(entries.last(), Some(Entry::Error(_))));
    assert!(entries.iter().any(|entry| matches!(
        entry,
        Entry::Assistant(answer) if answer.text == "half" && !answer.streaming
    )));
    let Some(Entry::Tool(tool)) = entries.iter().find(|entry| matches!(entry, Entry::Tool(_)))
    else {
        panic!("the tool row stays: {entries:?}");
    };
    assert!(tool.progress.is_none(), "stale live status is cleared");
    let outcome = tool
        .result
        .as_ref()
        .expect("lost calls have explicit unknown outcomes");
    assert_eq!(outcome.succeeded(), None);
    assert_eq!(outcome.effects.as_deref(), Some("unknown"));

    // A late answer from the old connection cannot resolve the voided approval.
    view.apply(&Message::Event(Event {
        kind: "approval_resolved".into(),
        body: json!({"event":"approval_resolved","approval_id":"a1"}),
    }));
    assert!(view.pending_approvals().is_empty());
}

#[test]
fn historical_outcomes_preserve_metadata_and_do_not_invent_success() {
    assert!(ToolOutcome::default().is_error);
    assert_eq!(ToolOutcome::default().succeeded(), None);
    for (metadata, expected_error, status, succeeded) in [
        (json!({}), true, None, None),
        (json!({"is_error":false}), false, None, Some(true)),
        (json!({"is_error":true}), true, None, Some(false)),
        (
            json!({"is_error":false,"status":"ok"}),
            false,
            Some("ok"),
            Some(true),
        ),
        (json!({"status":"ok"}), false, Some("ok"), Some(true)),
        (json!({"status":"unknown"}), true, Some("unknown"), None),
        (
            json!({"status":"blocked","is_error":false}),
            true,
            Some("blocked"),
            Some(false),
        ),
        (
            json!({"status":"ok","is_error":true}),
            true,
            Some("ok"),
            Some(false),
        ),
        (
            json!({"is_error":true,"status":"timeout","effects":"unknown"}),
            true,
            Some("timeout"),
            Some(false),
        ),
    ] {
        let mut message = json!({"role":"tool","tool_call_id":"c","content":"result"});
        message
            .as_object_mut()
            .unwrap()
            .extend(metadata.as_object().unwrap().clone());
        let mut view = Conversation::new();
        view.load_history(None, &[message]);
        let Entry::Tool(tool) = &view.entries()[0] else {
            panic!("expected tool")
        };
        let result = tool.result.as_ref().unwrap();
        assert_eq!(result.is_error, expected_error);
        assert_eq!(result.status.as_deref(), status);
        assert_eq!(result.succeeded(), succeeded);
        assert_eq!(result.outcome_known, succeeded.is_some());
    }
}

#[test]
fn gap_recovery_requires_a_complete_current_snapshot_and_replaces_live_state() {
    let mut view = Conversation::new();
    feed(
        &mut view,
        vec![
            json!({"event":"ready","session_id":"s1","seq":1}),
            json!({"event":"approval","seq":3,"approval_id":"stale","tool":"bash","summary":"old"}),
            json!({"event":"question","seq":4,"request":{"question_id":"old"}}),
            json!({"event":"todo","seq":5,"tasks":[{"content":"old"}]}),
        ],
    );
    view.push_user("stale turn", 0);
    let snapshot = json!({"session_id":"s1","seq":5,"messages":[{"role":"user","content":"saved"}],
        "busy":false,"pending_approvals":[],"question":null,"todo":[]});
    for key in [
        "session_id",
        "seq",
        "messages",
        "busy",
        "pending_approvals",
        "question",
        "todo",
    ] {
        let mut incomplete = snapshot.clone();
        incomplete.as_object_mut().unwrap().remove(key);
        assert!(!view.load_snapshot(&incomplete), "missing {key}");
        assert!(view.needs_reload());
        assert_eq!(view.pending_approvals()[0].request.approval_id, "stale");
    }
    for (key, invalid) in [
        ("seq", json!(4)),
        ("session_id", json!("other")),
        ("question", json!({})),
        ("pending_approvals", json!([{}])),
    ] {
        let mut malformed = snapshot.clone();
        malformed[key] = invalid;
        assert!(!view.load_snapshot(&malformed));
    }
    assert!(view.load_snapshot(&snapshot));
    assert!(!view.needs_reload());
    assert!(!view.turn_running());
    assert!(view.pending_approvals().is_empty());
    assert!(view.question().is_none());
    assert!(view.todo().is_empty());
    assert!(matches!(&view.entries()[0], Entry::User(user) if user.text == "saved"));
    let mut busy = snapshot;
    busy["busy"] = json!(true);
    busy["pending_approvals"] =
        json!([{"approval_id":"new","tool":"bash","summary":"new","call_id":"c"}]);
    busy["question"] = json!({"question_id":"new"});
    busy["todo"] = json!([{"content":"new"}]);
    assert!(view.load_snapshot(&busy));
    assert!(view.turn_running());
    assert_eq!(view.pending_approvals()[0].call_id.as_deref(), Some("c"));
    assert_eq!(view.question().unwrap()["question_id"], "new");
    assert_eq!(view.todo()[0]["content"], "new");
    feed(
        &mut view,
        vec![json!({"event":"text","seq":6,"delta":"next"})],
    );
    assert!(!view.needs_reload());
}

#[test]
fn tracked_goal_and_remote_completion_settle_only_their_own_turns() {
    let mut view = Conversation::new();
    view.expect_turn(RequestId(1));
    assert!(view.apply(&event(
        json!({"event":"collaboration_turn","outcome":"completed","result":{"text":"remote"}})
    )));
    assert!(view.turn_running());
    assert!(view.apply(&response("goal", json!({"text":"goal done"}))));
    assert!(!view.turn_running());
    assert!(view.apply(&event(
        json!({"event":"collaboration_turn","outcome":"completed","result":{"text":"remote"}})
    )));
    assert!(matches!(view.entries().last(), Some(Entry::Assistant(a)) if a.text == "remote"));
    view.apply(&event(json!({"event":"collaboration_turn","outcome":"failed","error":{"message":"remote failed"}})));
    assert!(matches!(view.entries().last(), Some(Entry::Error(e)) if e == "remote failed"));
}

#[test]
fn retry_discards_all_assistant_fragments_and_only_unexecuted_tool_drafts() {
    let mut view = Conversation::new();
    view.push_user("go", 0);
    feed(
        &mut view,
        vec![
            json!({"event":"text","delta":"first"}),
            json!({"event":"notice","message":"context"}),
            json!({"event":"text","delta":"second"}),
            json!({"event":"tool_arguments","id":"draft","name":"write","delta":"{"}),
            json!({"event":"tool_call","call_id":"executed","name":"bash","summary":"run"}),
            json!({"event":"tool_result","call_id":"executed","text":"done","status":"ok","effects":"known"}),
            json!({"event":"notice","kind":"retry","message":"retry"}),
        ],
    );
    assert!(
        view.entries()
            .iter()
            .filter_map(|e| match e {
                Entry::Assistant(a) => Some(a),
                _ => None,
            })
            .all(|a| a.abandoned && !a.streaming)
    );
    let draft = view
        .entries()
        .iter()
        .find_map(|e| match e {
            Entry::Tool(t) if t.call_id == "draft" => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        draft.result.as_ref().unwrap().effects.as_deref(),
        Some("none")
    );
    let executed = view
        .entries()
        .iter()
        .find_map(|e| match e {
            Entry::Tool(t) if t.call_id == "executed" => Some(t),
            _ => None,
        })
        .unwrap();
    assert_eq!(executed.result.as_ref().unwrap().succeeded(), Some(true));
}

#[test]
fn disconnect_invalidates_live_tools_with_unknown_outcomes() {
    let mut view = Conversation::new();
    feed(
        &mut view,
        vec![
            json!({"event":"tool_call","call_id":"c","name":"bash"}),
            json!({"event":"tool_progress","call_id":"c","text":"still running"}),
        ],
    );
    view.apply(&Message::Disconnected {
        outstanding: Vec::new(),
    });
    let Entry::Tool(tool) = &view.entries()[0] else {
        panic!()
    };
    assert!(tool.progress.is_none());
    let result = tool.result.as_ref().unwrap();
    assert_eq!(result.succeeded(), None);
    assert_eq!(result.effects.as_deref(), Some("unknown"));
}

#[test]
fn completed_inference_keeps_earlier_rounds_out_of_later_retry_discard() {
    let mut view = Conversation::new();
    feed(
        &mut view,
        vec![
            json!({"event":"text","delta":"earlier"}),
            json!({"event":"state_node","node":{"kind":"inference","parent":null,"result":{"phase":"completed"}}}),
            json!({"event":"text","delta":"partial"}),
            json!({"event":"notice","kind":"retry","message":"retry"}),
        ],
    );
    let answers: Vec<_> = view
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::Assistant(a) => Some((a.text.as_str(), a.abandoned)),
            _ => None,
        })
        .collect();
    assert_eq!(answers, [("earlier", false), ("partial", true)]);
}
