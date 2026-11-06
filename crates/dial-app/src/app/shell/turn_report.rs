//! End-of-turn report: what changed, what was checked, and what `/undo` cannot revert.
//! It renders the `turn_summary` the agent saves at turn end, plus the observed request timing from
//! `control.efficiency.inference_latency`, so it keeps no state of its own. The `Summary:` line is
//! the collapsed view; the other lines appear when tool details are expanded.

use serde_json::Value;
use std::time::Duration;

/// Changed files named before the report summarizes the rest.
const LISTED_FILES: usize = 8;

/// The report for a turn that edited files or did not complete; `None` for a plain answer or
/// when no summary was saved (for example, a turn that failed before it finished). `latency` is the
/// efficiency record's `inference_latency`; absent or unmeasured timing adds no line.
pub(super) fn render(summary: &Value, latency: &Value) -> Option<String> {
    let outcome = summary["outcome"].as_str()?;
    let files: Vec<&str> = summary["changed_files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let stopped = outcome != "completed";
    if files.is_empty() && !stopped {
        return None;
    }
    let checks: &[Value] = summary["recorded_checks"]
        .as_array()
        .map_or(&[], Vec::as_slice);
    let current = summary["checks_current"].as_bool() == Some(true);
    let open = summary["open_tasks"].as_u64().unwrap_or(0);
    let count = |key: &str| summary[key].as_u64().unwrap_or(0);
    let (shell, extension) = (
        count("uncheckpointed_shell_commands"),
        count("uncheckpointed_extension_calls"),
    );

    // The full cause is printed just above the report; the class says what kind of failure it was.
    let outcome_text = match summary["error_class"].as_str() {
        Some(class) => format!("{outcome} ({})", class.replace('_', " ")),
        None => outcome.to_owned(),
    };
    let mut lines = vec![format!("Outcome: {outcome_text}")];
    lines.push(if files.is_empty() {
        "Changed: none".to_owned()
    } else {
        format!("Changed: {}", changed_line(&files))
    });
    lines.push(checks_line(checks, current));
    if let Some(evidence) = evidence_line(checks) {
        lines.push(evidence);
    }
    if open > 0 {
        lines.push(format!("Open tasks: {open}"));
    }
    if shell + extension > 0 {
        lines.push(format!(
            "Outside /undo: {} and {} may have changed files",
            plural(shell, "shell command"),
            plural(extension, "extension call")
        ));
    }
    if let Some(timing) = timing_line(latency) {
        lines.push(timing);
    }
    lines.push(
        match (stopped, files.is_empty()) {
            (false, _) => "Next: /review · /undo",
            (true, true) => "Next: continue",
            (true, false) => "Next: continue · /undo",
        }
        .to_owned(),
    );

    let mut compact = Vec::new();
    if stopped {
        compact.push(outcome_text);
    }
    compact.push(match files.len() {
        0 => "no files changed".to_owned(),
        n => format!("{} changed", plural(n as u64, "file")),
    });
    compact.push(checks_state(checks, current));
    if open > 0 {
        compact.push(plural(open, "open task"));
    }
    if shell + extension > 0 {
        compact.push(format!(
            "{} outside /undo",
            plural(shell + extension, "effect")
        ));
    }
    lines.push(format!("Summary: {}", compact.join(" · ")));
    Some(crate::storage::redact::redact_text(&lines.join("\n")).into_owned())
}

fn plural(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn changed_line(files: &[&str]) -> String {
    let shown = files
        .iter()
        .take(LISTED_FILES)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    match files.len().saturating_sub(LISTED_FILES) {
        0 => shown,
        rest => format!("{shown} +{rest} more"),
    }
}

/// One word for the collapsed view: a failure wins, then staleness, then anything not passed.
fn checks_state(checks: &[Value], current: bool) -> String {
    let status = |check: &Value| check["status"].as_str().unwrap_or("unverified").to_owned();
    let failed = checks.iter().filter(|c| status(c) == "failed").count() as u64;
    if checks.is_empty() {
        "no recorded checks".to_owned()
    } else if failed > 0 {
        format!("{} failed", plural(failed, "check"))
    } else if !current {
        "checks stale".to_owned()
    } else if checks.iter().all(|c| status(c) == "passed") {
        "checks passed".to_owned()
    } else {
        "checks unverified".to_owned()
    }
}

fn checks_line(checks: &[Value], current: bool) -> String {
    if checks.is_empty() {
        return "Checks: none recorded".to_owned();
    }
    let listed: Vec<String> = checks
        .iter()
        .map(|check| {
            format!(
                "{} {}{}",
                check["name"].as_str().unwrap_or("check"),
                check["status"].as_str().unwrap_or("unverified"),
                if check["reused"] == true {
                    " (reused)"
                } else {
                    ""
                }
            )
        })
        .collect();
    let stale = if current {
        ""
    } else {
        " — stale: predates the final edits"
    };
    format!("Checks: {}{stale}", listed.join(", "))
}

/// Where each recorded check's retained output can be opened, naming checks without a log.
fn evidence_line(checks: &[Value]) -> Option<String> {
    let (with_log, without_log): (Vec<&Value>, Vec<&Value>) = checks.iter().partition(|check| {
        check["evidence_ref"]
            .as_str()
            .is_some_and(|r| !r.is_empty())
    });
    let name = |check: &&Value| check["name"].as_str().unwrap_or("unnamed check").to_owned();
    let mut parts: Vec<String> = with_log
        .iter()
        .map(|check| format!("/evidence check {}", name(check)))
        .collect();
    if !without_log.is_empty() {
        let names: Vec<String> = without_log.iter().map(name).collect();
        parts.push(format!("no retained log for {}", names.join(", ")));
    }
    (!parts.is_empty()).then(|| format!("Evidence: {}", parts.join(" · ")))
}

/// Observed wall-clock request timing; loading, prompt processing and retry backoff appear only
/// when measured. Zero or missing values are omitted.
fn timing_line(latency: &Value) -> Option<String> {
    let positive = |key: &str| latency[key].as_u64().filter(|value| *value > 0);
    let phases: Vec<String> = [
        ("wait", "response_wait_ms"),
        ("generation", "generation_ms"),
        ("loading", "loading_ms"),
        ("prompt processing", "prompt_processing_ms"),
        ("retry backoff", "retry_backoff_ms"),
    ]
    .into_iter()
    .filter_map(|(label, key)| positive(key).map(|ms| format!("{label} {}", duration(ms))))
    .collect();
    if phases.is_empty() {
        return None;
    }
    let requests = positive("requests")
        .filter(|count| *count > 1)
        .map(|count| format!(" over {count} requests"))
        .unwrap_or_default();
    Some(format!("Timing: {}{requests}", phases.join(", ")))
}

fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", Duration::from_millis(ms).as_secs_f64())
    }
}

#[cfg(test)]
mod tests;
