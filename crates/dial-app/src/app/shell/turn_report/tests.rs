use super::render;
use serde_json::{Value, json};

fn no_timing() -> Value {
    Value::Null
}

fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in {text}"))
}

#[test]
fn plain_answer_has_no_report() {
    for outcome in ["completed", "answered"] {
        let summary = json!({"outcome": outcome, "changed_files": [], "recorded_checks": [], "open_tasks": 0});
        assert_eq!(render(&summary, &no_timing()), None);
    }
}

#[test]
fn missing_summary_has_no_report() {
    assert_eq!(render(&serde_json::Value::Null, &no_timing()), None);
}

#[test]
fn completed_edit_is_short_and_omits_empty_sections() {
    let summary = json!({
        "outcome": "completed",
        "changed_files": ["src/a.rs"],
        "recorded_checks": [{"name": "cargo test", "status": "passed"}],
        "checks_current": true,
        "open_tasks": 0,
        "uncheckpointed_shell_commands": 0,
        "uncheckpointed_extension_calls": 0
    });
    let text = render(&summary, &no_timing()).expect("edits produce a report");
    assert_eq!(
        text,
        "Outcome: completed\n\
         Changed: src/a.rs\n\
         Checks: current · passed — cargo test passed\n\
         Evidence: no retained log for cargo test\n\
         Next: /review-file · /undo (checkpointed edits only)\n\
         Summary: 1 file changed · checks passed (current)"
    );
}

#[test]
fn interrupted_turn_names_effects_outside_undo() {
    let summary = json!({
        "outcome": "interrupted",
        "changed_files": [],
        "recorded_checks": [],
        "checks_current": false,
        "open_tasks": 2,
        "uncheckpointed_shell_commands": 1,
        "uncheckpointed_extension_calls": 0
    });
    let text = render(&summary, &no_timing()).expect("interruptions produce a report");
    assert_eq!(line(&text, "Outcome:"), "Outcome: interrupted");
    assert_eq!(line(&text, "Changed:"), "Changed: none");
    assert_eq!(line(&text, "Checks:"), "Checks: none recorded");
    assert_eq!(line(&text, "Open tasks:"), "Open tasks: 2");
    assert_eq!(
        line(&text, "Outside /undo:"),
        "Outside /undo: 1 shell command may have changed files"
    );
    assert_eq!(
        line(&text, "Summary:"),
        "Summary: interrupted · no files changed · no recorded checks · 2 open tasks · 1 effect outside /undo"
    );
}

#[test]
fn failed_turn_names_its_failure_class() {
    let summary = json!({
        "outcome": "failed",
        "error": "model server returned HTTP 429: too many requests",
        "error_class": "rate_limited",
        "changed_files": [],
        "open_tasks": 0
    });
    let text = render(&summary, &no_timing()).expect("failures produce a report");
    assert!(
        text.starts_with("Outcome: failed (rate limited)\n"),
        "{text}"
    );
    assert!(line(&text, "Summary:").starts_with("Summary: failed (rate limited) ·"));
    assert!(!text.contains("HTTP 429"), "{text}");
}

#[test]
fn stale_checks_and_long_file_lists_are_marked() {
    let files: Vec<String> = (0..10).map(|i| format!("f{i}.rs")).collect();
    let summary = json!({
        "outcome": "completed",
        "changed_files": files,
        "recorded_checks": [{"name": "lint", "status": "passed"}],
        "checks_current": false,
        "open_tasks": 0
    });
    let text = render(&summary, &no_timing()).expect("edits produce a report");
    assert!(line(&text, "Changed:").ends_with("f7.rs +2 more"), "{text}");
    assert_eq!(
        line(&text, "Checks:"),
        "Checks: stale · passed — lint passed"
    );
    assert_eq!(
        line(&text, "Summary:"),
        "Summary: 10 files changed · checks stale"
    );
}

#[test]
fn collapsed_check_state_prefers_failure_then_unverified() {
    let failed = json!({"outcome":"completed", "changed_files":["a.rs"], "checks_current":false,
        "recorded_checks":[{"name":"lint","status":"failed"},{"name":"test","status":"passed"}]});
    let unverified = json!({"outcome":"completed", "changed_files":["a.rs"], "checks_current":true,
        "recorded_checks":[{"name":"test","status":"skipped"}]});
    for (summary, expected) in [
        (failed, "Summary: 1 file changed · 1 check failed"),
        (unverified, "Summary: 1 file changed · checks unverified"),
    ] {
        let text = render(&summary, &no_timing()).unwrap();
        assert_eq!(line(&text, "Summary:"), expected);
    }
}

#[test]
fn recorded_timing_lists_measured_phases() {
    let summary = json!({"outcome": "completed", "changed_files": ["a.rs"], "open_tasks": 0});
    let latency = json!({
        "requests": 2,
        "response_wait_ms": 1500,
        "loading_ms": 0,
        "prompt_processing_ms": 0,
        "generation_ms": 2500,
        "retry_backoff_ms": 0,
        "total_ms": 4000
    });
    let text = render(&summary, &latency).expect("edits produce a report");
    assert_eq!(
        line(&text, "Timing:"),
        "Timing: wait 1.5 s, generation 2.5 s over 2 requests"
    );
}

#[test]
fn loading_retry_and_prompt_phases_appear_only_when_measured() {
    let summary = json!({"outcome": "failed", "error_class": "timeout", "changed_files": [], "open_tasks": 0});
    let latency = json!({
        "requests": 1,
        "response_wait_ms": 120,
        "loading_ms": 800,
        "prompt_processing_ms": 300,
        "generation_ms": 0,
        "retry_backoff_ms": 4000
    });
    let text = render(&summary, &latency).expect("failures produce a report");
    assert_eq!(
        line(&text, "Timing:"),
        "Timing: wait 120 ms, loading 800 ms, prompt processing 300 ms, retry backoff 4.0 s"
    );
}

#[test]
fn missing_null_or_zero_timing_adds_no_line() {
    let summary = json!({"outcome": "completed", "changed_files": ["a.rs"], "open_tasks": 0});
    let all_zero = json!({
        "requests": 0,
        "response_wait_ms": 0,
        "loading_ms": 0,
        "prompt_processing_ms": 0,
        "generation_ms": 0,
        "retry_backoff_ms": 0
    });
    let with_nulls = json!({"response_wait_ms": null, "generation_ms": null});
    let wrong_types = json!({"generation_ms": "fast", "response_wait_ms": -5});
    for latency in [no_timing(), json!({}), all_zero, with_nulls, wrong_types] {
        let text = render(&summary, &latency).expect("edits produce a report");
        assert!(!text.contains("Timing:"), "{latency}: {text}");
        assert!(text.contains("Changed: a.rs"), "{text}");
    }
}

#[test]
fn next_steps_follow_turn_outcome_without_suggesting_unavailable_undo() {
    for (summary, expected) in [
        (
            json!({"outcome":"completed", "changed_files":["a.rs"]}),
            "Next: /review-file · /undo (checkpointed edits only)",
        ),
        (
            json!({"outcome":"failed", "changed_files":["a.rs"]}),
            "Next: send a follow-up message after inspecting the retained failure and effects; continue only unfinished work, not the whole turn · /review-file · /undo (checkpointed edits only)",
        ),
        (
            json!({"outcome":"failed", "changed_files":[]}),
            "Next: send a follow-up message after inspecting the retained failure and effects; continue only unfinished work, not the whole turn",
        ),
    ] {
        let report = render(&summary, &no_timing()).unwrap();
        assert_eq!(line(&report, "Next:"), expected);
        assert!(report.contains("Summary:"));
    }
}

#[test]
fn check_evidence_and_reuse_are_disclosed() {
    let summary = json!({"outcome":"completed", "changed_files":["a.rs"],
    "checks_current":true, "recorded_checks":[
        {"name":"test", "status":"passed", "reused":true, "evidence_ref":"artifact://exact"},
        {"name":"lint", "status":"passed"}
    ]});
    let report = render(&summary, &no_timing()).unwrap();
    assert_eq!(
        line(&report, "Checks:"),
        "Checks: current · passed — test passed (reused), lint passed"
    );
    assert_eq!(
        line(&report, "Evidence:"),
        "Evidence: /evidence check test · no retained log for lint"
    );
}

#[test]
fn transcript_handoff_reuses_recorded_results_without_promoting_freshness() {
    for (freshness, status, expected) in [
        (json!(true), "passed", "current · passed — test passed"),
        (json!(true), "failed", "current · failed — test failed"),
        (json!(false), "passed", "stale · passed — test passed"),
        (
            Value::Null,
            "passed",
            "freshness unknown · passed — test passed",
        ),
        (
            json!(true),
            "skipped",
            "current · unverified — test skipped",
        ),
    ] {
        let summary = json!({"outcome":"completed", "changed_files":["src/a.rs"],
            "checks_current":freshness, "recorded_checks":[{"name":"test", "status":status}]});
        let outcome = super::outcome(&summary, &Value::Null);
        let handoff = outcome.handoff.join("\n");
        assert!(handoff.contains(expected), "{handoff}");
        assert!(handoff.contains("src/a.rs"), "{handoff}");
        assert!(!handoff.contains("rerun"), "{handoff}");
    }
}

#[test]
fn blocked_handoff_names_specific_recorded_obligations_and_effect_limits() {
    let summary = json!({"outcome":"incomplete", "changed_files":["src/a.rs"], "open_tasks":1,
    "uncheckpointed_shell_commands":1, "evidence":{
        "deliverable_coverage":"open_obligations",
        "outstanding_tasks":[{"content":"Publish package", "status":"blocked", "note":"Signing identity unavailable"}],
        "outstanding_deliverables":[{"description":"Release artifact", "status":"open"}]
    }});
    let handoff = super::outcome(&summary, &Value::Null).handoff.join("\n");
    for expected in [
        "Blocked: Signing identity unavailable — Publish package",
        "Signing identity unavailable",
        "Release artifact (open)",
        "Checks: none recorded",
        "Outside /undo: 1 shell command",
        "checkpointed edits only",
        "follow-up message",
    ] {
        assert!(handoff.contains(expected), "missing {expected}: {handoff}");
    }
    assert!(!handoff.contains("running"), "{handoff}");
    assert!(
        !handoff.contains("(blocked)"),
        "the Blocked prefix states it once: {handoff}"
    );
}

#[test]
fn completion_without_edits_is_quiet_unless_obligations_remain() {
    let mut summary = json!({"outcome":"completed", "changed_files":[], "open_tasks":0});
    assert!(super::outcome(&summary, &Value::Null).handoff.is_empty());
    summary["evidence"] = json!({"deliverable_coverage":"open_obligations"});
    let handoff = super::outcome(&summary, &Value::Null).handoff.join("\n");
    assert!(handoff.contains("recorded open obligations"), "{handoff}");
    assert!(!handoff.contains("/undo"), "{handoff}");
}

#[test]
fn current_passing_checks_alone_need_no_report_but_gaps_still_do() {
    let clean = json!({"outcome":"answered", "changed_files":[], "open_tasks":0,
        "checks_current":true, "recorded_checks":[{"name":"test", "status":"passed"}]});
    assert_eq!(render(&clean, &no_timing()), None);
    for (current, status) in [
        (json!(false), "passed"),
        (json!(null), "passed"),
        (json!(true), "skipped"),
    ] {
        let gap = json!({"outcome":"answered", "changed_files":[], "open_tasks":0,
            "checks_current":current, "recorded_checks":[{"name":"test", "status":status}]});
        let report = render(&gap, &no_timing()).expect("verification gaps stay visible");
        assert!(
            line(&report, "Next:").contains("verification gaps"),
            "{report}"
        );
    }
}

#[test]
fn open_tasks_are_not_restated_as_outstanding_obligations_in_the_summary() {
    let summary = json!({"outcome":"completed", "changed_files":["src/a.rs"], "open_tasks":1,
        "evidence":{"outstanding_tasks":[{"content":"Ship change", "status":"pending"}]}});
    let report = render(&summary, &no_timing()).unwrap();
    let compact = line(&report, "Summary:");
    assert!(compact.contains("1 open task"), "{report}");
    assert!(!compact.contains("outstanding obligations"), "{report}");
    assert!(
        line(&report, "Outstanding:").contains("Ship change (pending)"),
        "{report}"
    );
}

#[test]
fn answered_handoff_retains_open_work_and_effect_warnings() {
    for extra in [
        json!({"open_tasks":2}),
        json!({"evidence":{"deliverable_coverage":"open_obligations"}}),
        json!({"uncheckpointed_shell_commands":1}),
        json!({"changed_files":["src/a.rs"]}),
    ] {
        let mut summary = json!({"outcome":"answered", "changed_files":[], "open_tasks":0});
        summary
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let rendered = render(&summary, &Value::Null).expect("open work stays visible");
        assert_eq!(line(&rendered, "Outcome:"), "Outcome: answered");
        assert!(!line(&rendered, "Summary:").contains("answered"));
        let outcome = super::outcome(&summary, &Value::Null);
        assert_eq!(outcome.status, "answered");
        assert_eq!(
            outcome.open_tasks,
            summary["open_tasks"].as_u64().unwrap() as usize
        );
        let handoff = outcome.handoff.join("\n");
        if summary["open_tasks"] == 2 {
            assert!(handoff.contains("Open tasks: 2"));
        } else if summary["evidence"].is_object() {
            assert!(handoff.contains("recorded open obligations"));
        } else if summary["uncheckpointed_shell_commands"] == 1 {
            assert!(handoff.contains("Outside /undo: 1 shell command"));
        } else {
            assert!(handoff.contains("src/a.rs"));
        }
    }
}

#[test]
fn handoff_sanitizes_redacts_and_bounds_recorded_labels() {
    let summary = json!({"outcome":"interrupted", "changed_files":["src/\u{1b}[31ma.rs\nINJECTED"],
        "evidence":{"outstanding_tasks":[{"content":"x".repeat(1000), "status":"blocked", "note":"Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345"}]}});
    let outcome = super::outcome(&summary, &Value::Null);
    let handoff = outcome.handoff.join("\n");
    assert!(!handoff.contains('\u{1b}'), "{handoff}");
    assert!(
        !handoff.contains("abcdefghijklmnopqrstuvwxyz012345"),
        "{handoff}"
    );
    assert!(!handoff.contains(&"x".repeat(241)), "{handoff}");
    assert!(outcome.handoff.iter().all(|line| !line.contains('\n')));
    assert!(handoff.contains("follow-up message"), "{handoff}");
}

#[test]
fn completed_shell_effect_is_not_hidden_as_a_plain_answer() {
    let summary = json!({"outcome":"completed", "changed_files":[], "open_tasks":0,
        "uncheckpointed_shell_commands":1});
    let handoff = super::outcome(&summary, &Value::Null).handoff.join("\n");
    assert!(
        handoff.contains("Outside /undo: 1 shell command"),
        "{handoff}"
    );
    assert!(
        handoff.contains("inspect shell/extension effects"),
        "{handoff}"
    );
    assert!(!handoff.contains("checkpointed edits only"), "{handoff}");
}

#[test]
fn many_obligations_keep_next_action_and_a_pointer_to_omitted_evidence() {
    let tasks: Vec<_> = (0..20)
        .map(|i| json!({"content":format!("Task {i}"), "status":"pending"}))
        .collect();
    let summary = json!({"outcome":"incomplete", "changed_files":["src/a.rs"], "open_tasks":20,
        "uncheckpointed_shell_commands":1, "checks_current":false,
        "recorded_checks":[{"name":"test", "status":"failed", "evidence_ref":"artifact://test"}],
        "evidence":{"outstanding_tasks":tasks}});
    let outcome = super::outcome(&summary, &Value::Null);
    assert_eq!(outcome.handoff.len(), 10);
    assert!(outcome.handoff.last().unwrap().starts_with("Next:"));
    let handoff = outcome.handoff.join("\n");
    assert!(handoff.contains("+17 more; /evidence"), "{handoff}");
    assert!(handoff.contains("Checks: stale · failed"), "{handoff}");
}

#[test]
fn measured_latency_refresh_keeps_a_single_handoff_with_next_action_last() {
    let summary = json!({"outcome":"completed", "changed_files":["src/a.rs"]});
    let initial = super::outcome(&summary, &Value::Null);
    assert!(
        !initial
            .handoff
            .iter()
            .any(|line| line.starts_with("Timing:"))
    );
    let refreshed = super::outcome(&summary, &json!({"requests":2,"generation_ms":1500}));
    assert_eq!(
        refreshed
            .handoff
            .iter()
            .filter(|line| line.starts_with("Timing:"))
            .count(),
        1
    );
    assert!(
        refreshed
            .handoff
            .iter()
            .any(|line| line.contains("generation 1.5 s over 2 requests"))
    );
    assert!(refreshed.handoff.last().unwrap().starts_with("Next:"));
}

#[test]
fn next_action_prioritizes_open_work_and_verification_over_optional_review() {
    for (extra, expected) in [
        (json!({"open_tasks":1}), "outstanding work"),
        (
            json!({"evidence":{"deliverable_coverage":"open_obligations"}}),
            "outstanding work",
        ),
        (
            json!({"checks_current":true,"recorded_checks":[{"name":"test","status":"failed"}]}),
            "failed checks",
        ),
        (
            json!({"checks_current":false,"recorded_checks":[{"name":"test","status":"passed"}]}),
            "verification gaps",
        ),
        (
            json!({"checks_current":true,"recorded_checks":[{"name":"test","status":"skipped"}]}),
            "verification gaps",
        ),
        (
            json!({"recorded_checks":[{"name":"test","status":"passed"}]}),
            "verification gaps",
        ),
    ] {
        let mut summary = json!({"outcome":"completed","changed_files":["src/a.rs"]});
        summary
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let handoff = super::outcome(&summary, &Value::Null).handoff.join("\n");
        assert!(line(&handoff, "Next:").contains(expected), "{handoff}");
        assert!(!line(&handoff, "Next:").contains("/undo"), "{handoff}");
    }
    // A no-edit answer must not hide recorded verification failures either.
    let summary = json!({"outcome":"answered","changed_files":[],"checks_current":true,
        "recorded_checks":[{"name":"test","status":"failed"}]});
    let report = render(&summary, &Value::Null).unwrap();
    assert!(line(&report, "Next:").contains("failed checks"), "{report}");
}

#[test]
fn task_notes_are_not_blockers_without_a_blocked_status() {
    for (status, prefix) in [
        ("pending", "Outstanding:"),
        ("in_progress", "Outstanding:"),
        ("blocked", "Blocked:"),
    ] {
        let summary = json!({"outcome":"completed","changed_files":["src/a.rs"],"evidence":{
            "outstanding_tasks":[{"content":"Ship change","status":status,"note":"Await review"}]
        }});
        let report = render(&summary, &Value::Null).unwrap();
        assert!(
            line(&report, prefix).contains("Await review — Ship change"),
            "{report}"
        );
        assert!(
            line(&report, "Summary:").contains("outstanding obligations"),
            "{report}"
        );
        assert!(
            line(&report, "Next:").contains("outstanding work"),
            "{report}"
        );
    }
}

#[test]
fn observed_cost_joins_a_detailed_report_above_next_and_never_a_plain_answer() {
    use crate::llm::usage::{CostCoverage, Counts};
    let priced = Counts {
        observed_cost_usd: Some(0.0125),
        cost_coverage: CostCoverage::ObservedPriced,
        ..Counts::default()
    };
    let edited = json!({"outcome": "completed", "changed_files": ["src/a.rs"], "recorded_checks": [], "open_tasks": 0});
    let outcome = super::with_cost(super::outcome(&edited, &no_timing()), &priced);
    let cost = outcome
        .handoff
        .iter()
        .position(|line| line == "Cost: $0.012500 USD observed API cost")
        .unwrap_or_else(|| panic!("{:?}", outcome.handoff));
    assert!(outcome.handoff[cost + 1].starts_with("Next:"));
    // Unpriced or local usage adds nothing, and a plain answer keeps its empty footer.
    let unpriced = super::with_cost(super::outcome(&edited, &no_timing()), &Counts::default());
    assert!(
        !unpriced
            .handoff
            .iter()
            .any(|line| line.starts_with("Cost:"))
    );
    let plain =
        json!({"outcome": "answered", "changed_files": [], "recorded_checks": [], "open_tasks": 0});
    assert!(
        super::with_cost(super::outcome(&plain, &no_timing()), &priced)
            .handoff
            .is_empty()
    );
}

#[test]
fn shell_checks_are_reported_as_checks() {
    let summary = json!({
        "outcome": "completed",
        "changed_files": ["src/main.rs"],
        "recorded_checks": [{"name": "rustc main.rs && ./main", "status": "passed", "kind": "shell"}],
        "checks_current": true,
        "uncheckpointed_shell_commands": 0,
        "uncheckpointed_extension_calls": 0,
    });
    let text = render(&summary, &Value::Null).unwrap();
    assert!(!text.contains("none recorded"), "{text}");
    assert!(text.contains("rustc main.rs && ./main passed"), "{text}");
    assert!(text.contains("checks passed (current)"), "{text}");
}
