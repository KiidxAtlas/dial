//! End-of-turn report: what changed, what was checked, what cannot be undone, and what can resume.
//! It renders the `turn_summary` the agent saves at turn end, plus the observed request timing from
//! `control.efficiency.inference_latency`, so it keeps no state of its own.

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
    // The full cause is printed just above the report; the class says what kind of failure it was.
    let mut lines = vec![match summary["error_class"].as_str() {
        Some(class) => format!("Outcome: {outcome} ({})", class.replace('_', " ")),
        None => format!("Outcome: {outcome}"),
    }];
    lines.push(if files.is_empty() {
        "Changed: no files".to_owned()
    } else {
        format!("Changed: {}", changed_line(&files))
    });
    lines.push(checks_line(summary));
    if let Some(timing) = timing_line(latency) {
        lines.push(timing);
    }
    let open = summary["open_tasks"].as_u64().unwrap_or(0);
    if open > 0 {
        lines.push(format!("Open tasks: {open} not finished"));
    }
    if !files.is_empty() {
        lines.push(
            "Undo: /undo can revert checkpointed edits; /review-file shows their diffs.".to_owned(),
        );
    }
    let shell = summary["uncheckpointed_shell_commands"]
        .as_u64()
        .unwrap_or(0);
    let extension = summary["uncheckpointed_extension_calls"]
        .as_u64()
        .unwrap_or(0);
    lines.push(if shell + extension > 0 {
        format!(
            "Not reversible: {shell} shell command(s) and {extension} extension call(s) may have changed files outside checkpoints."
        )
    } else {
        "Not reversible: no shell or extension effects recorded.".to_owned()
    });
    if stopped {
        lines.push(
            "Resume: send your next message; the session and its open tasks are kept.".to_owned(),
        );
    }
    Some(lines.join("\n"))
}

fn changed_line(files: &[&str]) -> String {
    let shown = files
        .iter()
        .take(LISTED_FILES)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    match files.len().saturating_sub(LISTED_FILES) {
        0 => format!("{} file(s): {shown}", files.len()),
        rest => format!("{} file(s): {shown} and {rest} more", files.len()),
    }
}

fn checks_line(summary: &Value) -> String {
    let checks: Vec<String> = summary["recorded_checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|check| {
            format!(
                "{} {}",
                check["name"].as_str().unwrap_or("check"),
                check["status"].as_str().unwrap_or("unverified")
            )
        })
        .collect();
    if checks.is_empty() {
        return "Checks: none run".to_owned();
    }
    let current = if summary["checks_current"].as_bool() == Some(true) {
        ""
    } else {
        "; not current with the final edits"
    };
    format!("Checks: {}{current}", checks.join(", "))
}

/// Observed request timing: response wait and generation are the reported phases; loading, prompt
/// processing and retry backoff appear only when measured. Zero or missing values are omitted.
fn timing_line(latency: &Value) -> Option<String> {
    let positive = |key: &str| latency[key].as_u64().filter(|value| *value > 0);
    let phases: Vec<String> = [
        ("response wait", "response_wait_ms"),
        ("generation", "generation_ms"),
        ("reported loading", "loading_ms"),
        ("reported prompt processing", "prompt_processing_ms"),
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
        .map(|count| format!(" across {count} requests"))
        .unwrap_or_default();
    Some(format!(
        "Timing: {}{requests}. Observed wall time, not token latency; response wait includes transport and unreported provider work, generation includes reasoning and tool arguments.",
        phases.join(", ")
    ))
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
