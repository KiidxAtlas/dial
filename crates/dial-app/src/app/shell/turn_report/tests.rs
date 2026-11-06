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
    let summary = json!({"outcome": "completed", "changed_files": [], "recorded_checks": [], "open_tasks": 0});
    assert_eq!(render(&summary, &no_timing()), None);
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
         Checks: cargo test passed\n\
         Evidence: no retained log for cargo test\n\
         Next: /review · /undo\n\
         Summary: 1 file changed · checks passed"
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
        "Outside /undo: 1 shell command and 0 extension calls may have changed files"
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
        "Checks: lint passed — stale: predates the final edits"
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
            "Next: /review · /undo",
        ),
        (
            json!({"outcome":"failed", "changed_files":["a.rs"]}),
            "Next: continue · /undo",
        ),
        (
            json!({"outcome":"failed", "changed_files":[]}),
            "Next: continue",
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
        "Checks: test passed (reused), lint passed"
    );
    assert_eq!(
        line(&report, "Evidence:"),
        "Evidence: /evidence check test · no retained log for lint"
    );
}
