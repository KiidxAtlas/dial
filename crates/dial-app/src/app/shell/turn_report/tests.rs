use super::render;
use serde_json::{Value, json};

fn no_timing() -> Value {
    Value::Null
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
fn completed_edit_reports_files_checks_and_undo() {
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
    assert!(text.contains("Changed: 1 file(s): src/a.rs"), "{text}");
    assert!(text.contains("Checks: cargo test passed"), "{text}");
    assert!(text.contains("Undo: /undo"), "{text}");
    assert!(
        text.contains("Not reversible: no shell or extension effects recorded."),
        "{text}"
    );
    assert!(!text.contains("Resume:"), "{text}");
    assert!(!text.contains("Timing:"), "{text}");
}

#[test]
fn interrupted_turn_names_unreversible_effects_and_resume() {
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
    assert!(text.contains("Outcome: interrupted"), "{text}");
    assert!(text.contains("Changed: no files"), "{text}");
    assert!(text.contains("Checks: none run"), "{text}");
    assert!(text.contains("Open tasks: 2 not finished"), "{text}");
    assert!(
        text.contains("Not reversible: 1 shell command(s) and 0 extension call(s)"),
        "{text}"
    );
    assert!(text.contains("Resume:"), "{text}");
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
    assert!(!text.contains("HTTP 429"), "{text}");
}

#[test]
fn stale_checks_and_long_file_lists_are_marked() {
    let files: Vec<String> = (0..10).map(|i| format!("f{i}.rs")).collect();
    let summary = json!({
        "outcome": "completed",
        "changed_files": files,
        "recorded_checks": [{"name": "lint", "status": "failed"}],
        "checks_current": false,
        "open_tasks": 0
    });
    let text = render(&summary, &no_timing()).expect("edits produce a report");
    assert!(text.contains("Changed: 10 file(s): f0.rs, f1.rs"), "{text}");
    assert!(text.contains("and 2 more"), "{text}");
    assert!(
        text.contains("Checks: lint failed; not current with the final edits"),
        "{text}"
    );
}

#[test]
fn recorded_timing_separates_response_wait_from_generation_with_caveat() {
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
    let line = text
        .lines()
        .find(|line| line.starts_with("Timing:"))
        .unwrap_or_else(|| panic!("no timing line in {text}"));
    assert!(line.contains("response wait 1.5 s"), "{line}");
    assert!(line.contains("generation 2.5 s"), "{line}");
    assert!(line.contains("across 2 requests"), "{line}");
    assert!(!line.contains("loading"), "zero phases are omitted: {line}");
    assert!(!line.contains("retry"), "zero phases are omitted: {line}");
    assert!(!line.to_lowercase().contains("prefill"), "{line}");
    assert!(!line.to_lowercase().contains("first token"), "{line}");
    assert!(line.contains("not token latency"), "{line}");
    assert!(
        line.contains("transport and unreported provider work"),
        "{line}"
    );
    assert!(line.contains("reasoning and tool arguments"), "{line}");
}

#[test]
fn reported_loading_retry_and_prompt_phases_are_labelled_when_measured() {
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
    let line = text
        .lines()
        .find(|line| line.starts_with("Timing:"))
        .unwrap_or_else(|| panic!("no timing line in {text}"));
    assert!(line.contains("response wait 120 ms"), "{line}");
    assert!(line.contains("reported loading 800 ms"), "{line}");
    assert!(line.contains("reported prompt processing 300 ms"), "{line}");
    assert!(line.contains("retry backoff 4.0 s"), "{line}");
    let phases = line.split(". Observed").next().unwrap_or(line);
    assert!(!phases.contains("generation"), "{line}");
    assert!(
        !line.contains("across"),
        "single request names no count: {line}"
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
        assert!(text.contains("Changed: 1 file(s): a.rs"), "{text}");
    }
}
