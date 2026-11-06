use super::*;
use crate::agent::Agent;
use crate::agent::run::intent::{Kind, record};
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::agent::workers::task_packet::PacketCache;
use crate::agent::workflow::evidence::Supersession;

const CORRECTION: &str = "No, fix the application in private\\ work, not the example.";
const EFFICIENCY_OFF: &str = "[efficiency]\ntask_packet=false";
const LEAN_LOCAL: &str = "[assistance]\nlean_local=true";

/// The request tail is an older plan, then a user correction, then the assistant's tool round.
fn history_after_correction() -> Vec<Value> {
    vec![
        json!({"role":"user","content":"Build the sample editor app."}),
        json!({"role":"assistant","content":"","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"old","content":"app/src/main.rs contents"}),
        json!({"role":"user","content":CORRECTION}),
        json!({"role":"assistant","content":"","tool_calls":[{"id":"next","function":{"name":"read","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"next","content":"private work/src/app.rs contents"}),
    ]
}

fn project(agent: &mut Agent<'_>, messages: &mut Vec<Value>) {
    let mut packet = PacketCache::default();
    let mut superseded = Supersession::default();
    agent
        .project_history(messages, &[], 0, &mut packet, &mut superseded, None)
        .unwrap();
}

fn task_context(messages: &[Value]) -> &Value {
    let last = messages.last().unwrap();
    assert_eq!(
        last["role"], "user",
        "task context must be the request tail"
    );
    assert_eq!(last["dial_task_context"], true);
    last
}

/// The intent payload is the JSON object at the end of the note; the prose before it is not parsed.
fn intent_payload(content: &str) -> Option<Value> {
    content.match_indices('{').find_map(|(index, _)| {
        serde_json::from_str::<Value>(&content[index..])
            .ok()
            .filter(|value| value.get("entry_id").is_some())
    })
}

fn set_control(agent: &mut Agent<'_>, control: Value) {
    agent.session.set_control_state(control).unwrap();
}

#[test]
fn a_visible_single_request_is_not_reissued_after_each_tool_round() {
    with_agent("", |agent| {
        let request = "Write summary.csv from the logs and verify its counts.";
        let mut control = json!({});
        record(&mut control, request, json!("entry-request"), Kind::Turn);
        set_control(agent, control);
        let history = vec![
            json!({"role":"user","content":request,"dial_context_request":true}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"verify","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"verify","content":"All requested counts match."}),
        ];
        for _ in 0..2 {
            let mut messages = history.clone();
            project(agent, &mut messages);
            assert!(
                messages.iter().all(|message| {
                    message["dial_task_context"] != true
                        || intent_payload(message["content"].as_str().unwrap_or_default()).is_none()
                }),
                "the already visible request must not look like a new instruction after verification: {messages:#?}"
            );
            assert_eq!(messages[0]["content"], request);
        }

        // Losing the original during compaction must restore the admitted request context.
        let mut compacted = vec![json!({"role":"user","content":"Earlier work was summarized."})];
        project(agent, &mut compacted);
        let payload =
            intent_payload(task_context(&compacted)["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["text"], request);
    });
}

#[test]
fn matching_tool_text_does_not_suppress_the_admitted_request() {
    with_agent("", |agent| {
        let request = "Fix the requested application.";
        let mut control = json!({});
        record(&mut control, request, json!("entry-request"), Kind::Turn);
        set_control(agent, control);
        let mut messages =
            vec![json!({"role":"tool","content":request,"dial_context_request":true})];
        project(agent, &mut messages);
        let payload = intent_payload(task_context(&messages)["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["text"], request);
    });
}

#[test]
fn a_visible_latest_turn_is_not_reissued_even_after_earlier_turns() {
    with_agent("", |agent| {
        let mut control = json!({});
        record(
            &mut control,
            "Build the sample editor app.",
            json!("entry-old"),
            Kind::Turn,
        );
        record(
            &mut control,
            CORRECTION,
            json!("entry-correction"),
            Kind::Turn,
        );
        set_control(agent, control);
        let mut messages = history_after_correction();
        messages[3]["dial_context_request"] = json!(true);
        project(agent, &mut messages);
        assert!(
            messages.iter().all(|message| {
                message["dial_task_context"] != true
                    || intent_payload(message["content"].as_str().unwrap_or_default()).is_none()
            }),
            "{messages:#?}"
        );
        // Once compaction removes it, the reminder returns.
        let mut compacted = vec![json!({"role":"user","content":"Earlier work was summarized."})];
        project(agent, &mut compacted);
        let payload =
            intent_payload(task_context(&compacted)["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["text"], CORRECTION);
    });
}

#[test]
fn ordinary_correction_after_tools_is_the_explicit_request_tail() {
    with_agent("", |agent| {
        let mut control = json!({
            "original_goal":"Build the sample editor app.",
            "current_instruction":"Build the sample editor app.",
            "steering_active":false,
        });
        record(
            &mut control,
            "Build the sample editor app.",
            json!("entry-old"),
            Kind::Turn,
        );
        record(
            &mut control,
            CORRECTION,
            json!("entry-correction"),
            Kind::Turn,
        );
        set_control(agent, control);

        let mut messages = history_after_correction();
        project(agent, &mut messages);
        let tail = task_context(&messages);
        let payload = intent_payload(tail["content"].as_str().unwrap())
            .expect("the admitted correction must be projected as a JSON payload");
        assert_eq!(payload["entry_id"], "entry-correction");
        assert_eq!(payload["kind"], "user_turn");
        assert_eq!(payload["text"], CORRECTION);
        assert_eq!(payload["text_truncated"], false);
        assert!(
            !tail["content"].as_str().unwrap().contains("entry-old"),
            "an older admitted request must not be promoted over the correction"
        );
    });
}

#[test]
fn task_context_note_is_request_only_and_not_persisted_to_the_session() {
    with_agent("", |agent| {
        let mut control = json!({});
        record(
            &mut control,
            CORRECTION,
            json!("entry-correction"),
            Kind::Turn,
        );
        set_control(agent, control);
        let persisted_before = agent.session.messages().len();

        let mut messages = history_after_correction();
        project(agent, &mut messages);
        task_context(&messages);
        assert_eq!(
            agent.session.messages().len(),
            persisted_before,
            "the projected note must not be appended to the durable session"
        );
    });
}

#[test]
fn projected_note_survives_task_packet_and_profile_variants() {
    for overlay in ["", EFFICIENCY_OFF, LEAN_LOCAL] {
        with_agent(overlay, |agent| {
            let mut control = json!({});
            record(
                &mut control,
                CORRECTION,
                json!("entry-correction"),
                Kind::Turn,
            );
            set_control(agent, control);

            let mut messages = history_after_correction();
            project(agent, &mut messages);
            let payload = intent_payload(task_context(&messages)["content"].as_str().unwrap())
                .unwrap_or_else(|| panic!("no intent payload for overlay {overlay:?}"));
            assert_eq!(
                payload["entry_id"], "entry-correction",
                "overlay {overlay:?}"
            );
            assert_eq!(payload["text"], CORRECTION, "overlay {overlay:?}");
        });
    }
}

#[test]
fn no_admitted_user_provenance_yields_no_fabricated_latest_context() {
    with_agent("", |agent| {
        // Legacy effective instruction without an admitted user record must not become "latest".
        set_control(
            agent,
            json!({"current_instruction":"legacy drafted prompt","current_instruction_entry_id":"legacy"}),
        );
        let mut messages = history_after_correction();
        project(agent, &mut messages);
        let mentions_payload = messages.iter().any(|message| {
            message["dial_task_context"] == true
                && intent_payload(message["content"].as_str().unwrap_or_default()).is_some()
        });
        assert!(
            !mentions_payload,
            "no admitted provenance, no intent payload"
        );
    });
}

#[test]
fn steering_supersedes_an_older_admitted_user_message() {
    with_agent("", |agent| {
        let mut control = json!({});
        record(
            &mut control,
            "Build the example.",
            json!("entry-old"),
            Kind::Turn,
        );
        record(
            &mut control,
            CORRECTION,
            json!("entry-steer"),
            Kind::Steering,
        );
        control["steering_active"] = json!(true);
        set_control(agent, control);

        let mut messages = history_after_correction();
        project(agent, &mut messages);
        let content = task_context(&messages)["content"]
            .as_str()
            .unwrap()
            .to_owned();
        let payload = intent_payload(&content).expect("steering payload");
        assert_eq!(payload["entry_id"], "entry-steer");
        assert_eq!(payload["kind"], "user_steering");
        assert_eq!(payload["text"], CORRECTION);
        assert!(!content.contains("entry-old"));
    });
}

#[test]
fn long_unicode_request_is_utf8_safe_with_explicit_truncation() {
    with_agent("", |agent| {
        let head = "前置✓🚀";
        let long = format!("{}{}END-SUFFIX", head, "ü字".repeat(4000));
        let mut control = json!({});
        record(&mut control, &long, json!("entry-long"), Kind::Turn);
        set_control(agent, control);

        let mut messages = history_after_correction();
        project(agent, &mut messages);
        let content = task_context(&messages)["content"]
            .as_str()
            .unwrap()
            .to_owned();
        let payload = intent_payload(&content).expect("long request payload");
        let text = payload["text"].as_str().unwrap();
        assert_eq!(payload["text_truncated"], true);
        assert!(text.len() <= 4096, "excerpt is {} bytes", text.len());
        assert!(text.starts_with(head), "head preserved: {text:?}");
        assert!(
            text.ends_with("END-SUFFIX"),
            "raw suffix preserved: {text:?}"
        );
    });
}

#[test]
fn short_unicode_request_is_exact_and_not_marked_truncated() {
    with_agent("", |agent| {
        let short = "fix \"parser\"\n\tuse \\ path 🚀 ünïcode";
        let mut control = json!({});
        record(&mut control, short, json!("entry-short"), Kind::Turn);
        set_control(agent, control);

        let mut messages = history_after_correction();
        project(agent, &mut messages);
        let payload = intent_payload(task_context(&messages)["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["text"], short);
        assert_eq!(payload["text_truncated"], false);
    });
}

#[test]
fn correction_survives_failure_cap_while_bare_todo_focus_is_dropped() {
    with_agent(EFFICIENCY_OFF, |agent| {
        let mut control = json!({});
        record(
            &mut control,
            CORRECTION,
            json!("entry-correction"),
            Kind::Turn,
        );
        set_control(agent, control);
        let set = agent.todo.call(&json!({"op":"set","tasks":[
            {"content":"Inspect the parser","status":"in_progress"},
            {"content":"Patch the parser","status":"pending"}
        ]}));
        assert!(!set.is_error, "todo setup failed: {}", set.text);

        let mut messages = history_after_correction();
        project(agent, &mut messages);
        let tool_end = messages.iter().rposition(|m| m["role"] == "tool").unwrap();
        // Bare todo focus is its own Todo-tagged message; the admitted correction stays untagged.
        assert!(
            messages.iter().any(|m| m["dial_guidance"] == "todo"),
            "bare todo focus must be projected as a Todo-tagged note"
        );
        let mut failure =
            json!({"role":"user","content":"Recover from the failed edit.","dial_transient":true});
        crate::agent::run::guidance::tag(
            &mut failure,
            crate::agent::run::guidance::Priority::Failure,
        );
        messages.push(failure);
        let dropped = crate::agent::run::guidance::cap(&mut messages);
        assert!(
            dropped.iter().any(|m| m["dial_guidance"] == "todo"),
            "the lower-priority todo focus must be the note dropped by the cap"
        );
        assert!(
            dropped
                .iter()
                .all(|m| { intent_payload(m["content"].as_str().unwrap_or_default()).is_none() }),
            "the user-correction note must never be dropped by the advice cap"
        );
        let survivor_index = messages
            .iter()
            .rposition(|m| {
                m["dial_task_context"] == true
                    && intent_payload(m["content"].as_str().unwrap_or_default()).is_some()
            })
            .expect("correction context must survive the cap");
        assert!(
            survivor_index > tool_end,
            "correction must remain after the tool round"
        );
        let payload =
            intent_payload(messages[survivor_index]["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["entry_id"], "entry-correction");
        assert_eq!(payload["text"], CORRECTION);
        assert!(
            messages
                .iter()
                .any(|m| m["content"] == "Recover from the failed edit."),
            "the higher-priority failure note must survive"
        );
    });
}

#[test]
fn hermetic_requests_inject_no_user_request_context() {
    with_agent("", |agent| {
        let mut control = json!({});
        record(
            &mut control,
            CORRECTION,
            json!("entry-correction"),
            Kind::Turn,
        );
        set_control(agent, control);
        agent.hermetic = true;

        let mut messages = history_after_correction();
        project(agent, &mut messages);
        assert!(
            messages
                .iter()
                .all(|message| message["dial_task_context"] != true
                    || intent_payload(message["content"].as_str().unwrap_or_default()).is_none()),
            "a hermetic request must not carry admitted user provenance"
        );
    });
}

#[test]
fn empty_or_non_user_latest_records_yield_no_request_context() {
    let cases = [
        json!({"latest":{"text":"   ","entry_id":"e","kind":"user_turn"}}),
        json!({"latest":{"text":"peer note","entry_id":"e","kind":"peer_message"}}),
        json!({"latest":{"text":"draft","entry_id":"e","kind":"effective_instruction"}}),
        json!({"latest":{"text":"no kind","entry_id":"e"}}),
    ];
    for intent in cases {
        with_agent("", |agent| {
            set_control(agent, json!({"user_intent": intent}));
            let mut messages = history_after_correction();
            project(agent, &mut messages);
            assert!(
                messages
                    .iter()
                    .all(|message| message["dial_task_context"] != true
                        || intent_payload(message["content"].as_str().unwrap_or_default())
                            .is_none()),
                "non-admitted latest record must not be projected: {intent}"
            );
        });
    }
}

#[test]
fn turn_optimizer_keeps_raw_provenance_out_of_the_request_tail() {
    with_agent("[experimental]\nturn_optimizer=true", |agent| {
        let mut control = json!({"current_instruction":"Reviewed parser task"});
        record(&mut control, CORRECTION, json!("entry-drafted"), Kind::Turn);
        set_control(agent, control);
        let mut messages = vec![json!({"role":"user","content":"Reviewed parser task"})];

        project(agent, &mut messages);

        assert!(messages.iter().all(|message| {
            message["dial_task_context"] != true
                || intent_payload(message["content"].as_str().unwrap_or_default()).is_none()
        }));
        assert_eq!(
            agent.session.control_state()["user_intent"]["latest"]["text"],
            CORRECTION
        );
        assert!(
            messages
                .iter()
                .any(|message| message["content"] == "Reviewed parser task")
        );
    });
}
