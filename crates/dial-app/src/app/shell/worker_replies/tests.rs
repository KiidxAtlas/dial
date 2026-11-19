use super::*;
use serde_json::json;

#[test]
fn cancellation_states_what_was_requested_and_what_continues() {
    let text = text(&json!({"id":"task-3","cancellation_requested":true}));
    assert!(
        text.starts_with("Cancellation requested for task-3."),
        "{text}"
    );
    assert!(text.contains("/workers task-3"), "{text}");
    assert!(!text.contains('{'), "{text}");
}

#[test]
fn message_receipts_name_each_recipient_and_why_delivery_failed() {
    let text = text(&json!({"receipts":[
        {"to":"task-1","status":"queued"},
        {"to":"scout","status":"refused","reason":"inbox full"},
        {"to":"task-2","status":"finished","worker_status":"done","note":"not delivered"},
    ]}));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].starts_with("Message queued for task-1"), "{text}");
    assert_eq!(lines[1], "Message refused by scout: inbox full");
    assert_eq!(
        lines[2],
        "Message not delivered: task-2 has already finished (done)."
    );
}

#[test]
fn running_status_shows_identity_progress_and_acceptance_without_the_raw_record() {
    let text = text(&json!({
        "id":"task-1","name":"reviewer","status":"running","agent":"general",
        "model":"fixture-model","phase":"tool: bash","steps":4,"tools":9,
        "started_ms":1,"running_secs":95,"idle_secs":3,"health":"active",
        "acceptance":{"state":"pending","reason":""},
        "task":"review the\nparser","instruction":"long private prompt",
        "assignment":{"acceptance_checks":["cargo test"]},
    }));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "task-1 · reviewer · running · tool: bash");
    assert_eq!(
        lines[1],
        "agent general · model fixture-model · 4 steps, 9 tool calls · running 1m35s, quiet 3s (active)"
    );
    assert_eq!(lines[2], "Acceptance: pending");
    assert_eq!(lines[3], "Task: review the parser");
    assert!(lines[4].contains("/workers"), "{text}");
    assert!(
        !text.contains("long private prompt") && !text.contains('{'),
        "{text}"
    );
}

#[test]
fn finished_status_keeps_a_bounded_result_and_its_artifact() {
    let text = text(&json!({
        "id":"task-2","status":"done","phase":"done","elapsed_ms":61_000,
        "result":"é".repeat(EXCERPT_CHARS + 50),"artifact":"artifact://abc",
    }));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0], "task-2 · done",
        "a settled worker has no live phase"
    );
    assert_eq!(lines[1], "ran 1m01s");
    assert!(lines[2].ends_with("… (truncated)"), "{text}");
    assert_eq!(
        lines[2].chars().filter(|c| *c == 'é').count(),
        EXCERPT_CHARS
    );
    assert_eq!(lines[3], "Full report: artifact://abc");
}

#[test]
fn lists_pools_and_unknown_shapes_stay_complete() {
    assert_eq!(text(&json!([])), "No workers in this session.");
    assert_eq!(
        text(&json!([
            {"id":"task-1","status":"running","phase":"generating"},
            {"id":"task-2","name":"docs","status":"done"},
        ])),
        "task-1 · running · generating\ntask-2 · docs · done"
    );
    let pool = text(&json!({
        "id":"pool-1","name":"sweep","state":"open","concurrency":2,"closed":false,
        "counts":{"done":2,"running":1},
        "items":[{"id":"task-4","status":"running","phase":"starting"}],
    }));
    assert_eq!(
        pool,
        "pool-1 · sweep · open\nItems: 2 done, 1 running\n  task-4 · running · starting"
    );
    // Nothing is summarized away when the shape is not one this projection knows.
    let unknown = text(&json!({"surprise":{"kept":true}}));
    assert!(
        unknown.contains("\"surprise\"") && unknown.contains("\"kept\": true"),
        "{unknown}"
    );
}
