use super::*;
use serde_json::json;

fn call(op: &str) -> ToolCall {
    ToolCall {
        id: "test".into(),
        name: "todo".into(),
        arguments: json!({"op":op}),
        malformed_arguments: false,
    }
}

#[test]
fn a_done_after_a_refused_edit_is_flagged_including_shorthand() {
    let shorthand = ToolCall {
        arguments: json!({"done": true, "task": 2}),
        ..call("start")
    };
    for (todo, succeeded, refused, expected) in [
        (call("done"), true, true, true),
        (shorthand, true, true, true),
        (call("done"), true, false, false),
        (call("done"), false, true, false),
        (call("start"), true, true, false),
    ] {
        assert_eq!(
            refused_edit_done_note(&todo, succeeded, refused).is_some(),
            expected,
            "{} succeeded={succeeded} refused={refused}",
            todo.arguments
        );
    }
}

#[test]
fn todo_only_responses_are_noted_once_and_not_when_forced() {
    let read = ToolCall {
        id: "r".into(),
        name: "read".into(),
        arguments: json!({"path":"a.rs"}),
        malformed_arguments: false,
    };
    for (planning_required, single_call, already_noted, expected) in [
        (false, false, false, true),
        (true, false, false, false),
        (false, true, false, false),
        (true, true, false, false),
        (false, false, true, false),
    ] {
        assert_eq!(
            solo_update_note(
                &[call("set")],
                planning_required,
                single_call,
                already_noted
            )
            .is_some(),
            expected,
            "planning_required={planning_required}, single_call={single_call}, already_noted={already_noted}"
        );
    }
    assert!(solo_update_note(&[call("done"), call("start")], false, false, false).is_some());
    assert!(solo_update_note(&[call("done"), read], false, false, false).is_none());
    assert!(solo_update_note(&[], false, false, false).is_none());
}

#[test]
fn empty_plan_recovery_does_not_recommend_done() {
    let mut recovery = TodoRecovery::default();
    let advice = recovery
        .observe(
            &call("set"),
            true,
            false,
            "investigations must start open",
            &TodoList::new(),
        )
        .unwrap();
    assert!(advice.contains("\"op\":\"set\""));
    assert!(advice.contains("otherwise omit todo"));
    assert!(!advice.contains("\"op\":\"done\""));
}

#[test]
fn optional_notes_recovery_keeps_the_simple_completion_contract() {
    let mut todo = TodoList::new();
    todo.call(
        &serde_json::json!({"op":"set","tasks":[{"content":"Inspect","investigation":true}]}),
    );
    let mut recovery = TodoRecovery::default();
    let advice = recovery
        .observe(
            &call("done"),
            true,
            false,
            "finding must be a string",
            &todo,
        )
        .unwrap();
    assert!(advice.contains("{\"op\":\"done\"}"));
    assert!(advice.contains("no finding or evidence is required"));
    assert!(!advice.contains("fill placeholders"));
}

#[test]
fn only_a_successful_plan_change_clears_recovery() {
    let mut recovery = TodoRecovery::default();
    let todo = TodoList::new();
    recovery.observe(&call("done"), true, false, "no active task", &todo);
    recovery.observe(&call("view"), false, false, "", &todo);
    assert!(recovery.reminder.is_some());
    recovery.observe(&call("set"), false, false, "", &todo);
    assert_eq!(recovery.failures, 1);
    recovery.observe(&call("set"), false, true, "", &todo);
    assert!(recovery.reminder.is_none());
    assert_eq!(recovery.failures, 0);
}

#[test]
fn identical_rejected_updates_escalate_even_across_plan_changes() {
    let mut recovery = TodoRecovery::default();
    let todo = TodoList::new();
    // A successful plan change resets the whole rejection history, so each run counts from zero.
    for _ in 0..FAILURE_LIMIT {
        recovery.observe(&call("done"), true, false, "no active task", &todo);
        recovery.observe(&call("set"), false, true, "", &todo);
    }
    assert_eq!(recovery.failures, 0);
    let mut last = None;
    for _ in 0..FAILURE_LIMIT {
        last = recovery.observe(&call("done"), true, false, "no active task", &todo);
    }
    assert!(last.is_some_and(|text| text.contains("Escalation")));
}
