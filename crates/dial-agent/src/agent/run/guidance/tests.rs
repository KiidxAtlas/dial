use super::*;
use crate::agent::base::helpers::coalesce_harness_notes;

fn history() -> Vec<Value> {
    vec![
        json!({"role":"system","content":"rules"}),
        json!({"role":"user","content":"fix it"}),
        json!({"role":"tool","tool_call_id":"1","content":"error: missing file"}),
    ]
}

fn contents(messages: &[Value]) -> Vec<&str> {
    messages
        .iter()
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect()
}

#[test]
fn two_simultaneous_kinds_keep_only_the_higher_priority_note() {
    let mut messages = history();
    messages.push(note(Priority::Loop, "you repeated a call"));
    messages.push(note(Priority::Failure, "rounds failed"));
    let dropped = cap(&mut messages);
    assert_eq!(contents(&dropped), ["you repeated a call"]);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[3]["content"], "rounds failed");
}

#[test]
fn explicit_priority_order_holds_regardless_of_assembly_order() {
    assert!(Priority::ALL.windows(2).all(|pair| pair[0] < pair[1]));
    let order: Vec<_> = Priority::ALL.into_iter().rev().collect();
    for winner in 0..order.len() {
        let mut messages = history();
        // Only notes at or below the winner's priority are present, in reverse priority order.
        for priority in &order[..=winner] {
            messages.push(note(*priority, priority.as_str()));
        }
        cap(&mut messages);
        let tagged: Vec<_> = messages.iter().filter_map(Priority::of).collect();
        assert_eq!(tagged, [order[winner]]);
    }
}

#[test]
fn ties_keep_the_first_assembled_note_and_selection_is_deterministic() {
    let build = || {
        let mut messages = history();
        messages.push(note(Priority::Loop, "narration"));
        messages.push(note(Priority::Loop, "repeat"));
        cap(&mut messages);
        messages
    };
    let first = build();
    assert_eq!(first, build());
    assert_eq!(first.last().unwrap()["content"], "narration");
}

#[test]
fn protocol_notes_context_and_history_are_never_dropped() {
    let mut messages = history();
    // A tagged note already in stored history (before the tail) is not this request's guidance.
    messages.insert(2, note(Priority::Loop, "old warning"));
    messages.push(json!({"role":"user","content":"todo focus","dial_transient":true,"dial_task_context":true,"dial_guidance":"todo"}));
    messages.push(json!({"role":"user","content":"tool contract","dial_transient":true}));
    messages.push(note(Priority::Review, "checklist"));
    messages.push(json!({"role":"user","content":"job finished","dial_transient":true}));
    messages.push(note(Priority::Failure, "todo recovery"));
    messages.push(
        json!({"role":"user","content":"rules","dial_transient":true,"dial_portable_rules":true}),
    );
    let dropped = cap(&mut messages);
    assert_eq!(contents(&dropped), ["todo focus", "checklist"]);
    assert_eq!(
        contents(&messages),
        [
            "rules",
            "fix it",
            "old warning",
            "error: missing file",
            "tool contract",
            "job finished",
            "todo recovery",
            "rules",
        ]
    );
}

#[test]
fn a_lone_note_or_no_note_is_unchanged() {
    let mut messages = history();
    assert!(cap(&mut messages).is_empty());
    assert_eq!(messages, history());
    messages.push(note(Priority::Todo, "focus"));
    let before = messages.clone();
    assert!(cap(&mut messages).is_empty());
    assert_eq!(messages, before);
}

#[test]
fn capped_tail_coalesces_into_one_harness_message() {
    let mut messages = history();
    let start = messages.len();
    messages.push(json!({"role":"user","content":"tool contract","dial_transient":true}));
    messages.push(note(Priority::Failure, "stall warning"));
    messages.push(note(Priority::Loop, "pace advice"));
    cap(&mut messages);
    coalesce_harness_notes(&mut messages, start);
    assert_eq!(messages.len(), start + 1);
    assert_eq!(messages[start]["content"], "tool contract\n\nstall warning");
}

#[test]
fn parent_and_worker_sources_share_tags() {
    // The worker tags its narration warning with the same helper the parent uses, so the same
    // tail yields the same selection in either role.
    let parent = note(Priority::Loop, "narration");
    let worker = note(Priority::Loop, "narration");
    assert_eq!(parent, worker);
    assert_eq!(parent["dial_transient"], true);
    assert_eq!(parent[KEY], "loop");
}
