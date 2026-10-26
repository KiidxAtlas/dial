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

#[test]
fn steering_drops_stale_advice_without_losing_protocol_findings_or_safety() {
    let result = json!({"role":"user","content":"worker findings", "dial_transient":true});
    let context = json!({"role":"user","content":"task evidence", "dial_task_context":true, "dial_transient":true});
    let safety = note(Priority::Safety, "uncertain external effect");
    let mut pending = vec![
        note(Priority::Failure, "old task failed"),
        result.clone(),
        context.clone(),
        note(Priority::Review, "finish the old task"),
        safety.clone(),
    ];
    reset_for_steering(&mut pending);
    assert_eq!(pending, [result, context, safety]);
}

#[test]
fn delivered_notes_retire_across_requests_even_when_buried_behind_tool_history() {
    let mut messages = history();
    let protected = vec![
        json!({"role":"developer","content":"operator","dial_transient":true}),
        json!({"role":"user","content":"coordination","dial_child_coordination":true}),
        json!({"role":"user","content":"memory","dial_memory_context":true,"dial_transient":true}),
        json!({"role":"user","content":"rules","dial_portable_rules":true,"dial_transient":true}),
        json!({"role":"user","content":"evidence","dial_evidence":{"artifact":"artifact://proof"},"dial_transient":true}),
        json!({"role":"user","content":"persistent"}),
        note(Priority::Safety, "enforced boundary"),
    ];
    messages.extend(protected.clone());
    messages.push(
        json!({"role":"user","content":"continue after premature stop","dial_transient":true}),
    );
    messages.push(note(Priority::Loop, "old narration warning"));
    // A later tool result buries both notes outside cap's trailing transient segment.
    messages.push(json!({"role":"assistant","content":"checked","tool_calls":[{"id":"2"}]}));
    messages.push(json!({"role":"tool","tool_call_id":"2","content":"current evidence"}));
    let undelivered = messages.clone();
    retire_delivered(&mut messages, false);
    assert_eq!(
        messages, undelivered,
        "failed inference must preserve advice for retry"
    );
    retire_delivered(&mut messages, true);
    assert!(!contents(&messages).contains(&"continue after premature stop"));
    assert!(!contents(&messages).contains(&"old narration warning"));
    for context in &protected {
        assert!(messages.contains(context));
    }
    assert!(contents(&messages).contains(&"current evidence"));
    // Request two selects only its fresh advice; request three cannot inherit it.
    messages.push(note(Priority::Review, "fresh review"));
    cap(&mut messages);
    assert_eq!(messages.last().unwrap()["content"], "fresh review");
    retire_delivered(&mut messages, true);
    assert!(!contents(&messages).contains(&"fresh review"));
}

fn advice(messages: &[Value]) -> Vec<&str> {
    messages
        .iter()
        .filter(|message| Priority::of(message).is_some())
        .map(|message| message["content"].as_str().unwrap_or_default())
        .collect()
}

#[test]
fn normalization_folds_case_whitespace_and_punctuation() {
    assert_eq!(normalize("  Looks   GOOD!! "), "looks good");
    assert_eq!(normalize("No issue; continue."), "no issue continue");
    assert_eq!(normalize("?!..."), "");
    assert!(is_content_free(""));
    assert!(is_content_free(" - . "));
    assert!(is_content_free("LGTM."));
    assert!(is_content_free("Looks good"));
    assert!(!is_content_free(
        "Looks good, but the migration drops a column"
    ));
    assert!(!is_content_free(
        "Stop: await is missing on writeStream.end()"
    ));
}

#[test]
fn content_free_advice_is_dropped_before_it_can_crowd_out_real_advice() {
    let mut emissions = Emissions::default();
    let mut messages = history();
    messages.push(note(Priority::Review, "Looks good."));
    messages.push(note(Priority::Todo, "run the failing test first"));
    let dropped = curate(&mut messages, &mut emissions);
    assert_eq!(contents(&dropped), ["Looks good."]);
    assert_eq!(advice(&messages), ["run the failing test first"]);
}

#[test]
fn identical_advice_is_suppressed_for_the_window_then_allowed_again() {
    let mut emissions = Emissions::default();
    let mut delivered = Vec::new();
    for _ in 0..6 {
        let mut messages = history();
        // Case and spacing differ, but the normalized text is the same advice.
        messages.push(note(Priority::Review, "Verify the  build passes."));
        curate(&mut messages, &mut emissions);
        delivered.push(!advice(&messages).is_empty());
    }
    assert_eq!(delivered, [true, false, false, true, false, false]);
}

#[test]
fn suppressed_repeat_lets_a_lower_priority_note_through_and_new_advice_is_unaffected() {
    let mut emissions = Emissions::default();
    let mut first = history();
    first.push(note(Priority::Review, "check the diff"));
    curate(&mut first, &mut emissions);
    assert_eq!(advice(&first), ["check the diff"]);
    let mut second = history();
    second.push(note(Priority::Review, "check the diff"));
    second.push(note(Priority::Todo, "finish step 2"));
    curate(&mut second, &mut emissions);
    assert_eq!(advice(&second), ["finish step 2"]);
    let mut third = history();
    third.push(note(Priority::Review, "check the diff again, line 40"));
    curate(&mut third, &mut emissions);
    assert_eq!(advice(&third), ["check the diff again, line 40"]);
}

#[test]
fn safety_and_failure_notes_are_never_suppressed_or_filtered() {
    let mut emissions = Emissions::default();
    for priority in [Priority::Safety, Priority::Failure] {
        for _ in 0..5 {
            let mut messages = history();
            messages.push(note(priority, "Stop."));
            let dropped = curate(&mut messages, &mut emissions);
            assert!(dropped.is_empty(), "{priority:?}");
            assert_eq!(advice(&messages), ["Stop."]);
        }
    }
}

#[test]
fn duplicates_within_one_request_collapse_and_protocol_notes_stay() {
    let mut emissions = Emissions::default();
    let mut messages = history();
    messages.push(json!({"role":"user","content":"looks good","dial_transient":true}));
    messages.push(note(Priority::Loop, "narration"));
    messages.push(note(Priority::Loop, "Narration"));
    curate(&mut messages, &mut emissions);
    assert_eq!(advice(&messages), ["narration"]);
    assert!(contents(&messages).contains(&"looks good"));
}

#[test]
fn a_retracted_request_does_not_suppress_its_note_on_retry_but_a_confirmed_one_does() {
    let mut emissions = Emissions::default();
    let mut first = history();
    first.push(note(Priority::Review, "check the diff"));
    curate(&mut first, &mut emissions);
    // The request was discarded before any reply: the retry must carry the same advice.
    emissions.retract();
    let mut retry = history();
    retry.push(note(Priority::Review, "check the diff"));
    curate(&mut retry, &mut emissions);
    assert_eq!(advice(&retry), ["check the diff"]);
    // Delivered: the next request suppresses the repeat as before.
    emissions.confirm();
    emissions.retract();
    let mut next = history();
    next.push(note(Priority::Review, "check the diff"));
    curate(&mut next, &mut emissions);
    assert!(advice(&next).is_empty());
}
