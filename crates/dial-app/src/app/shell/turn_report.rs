//! End-of-turn report: what changed, what was checked, and what `/undo` cannot revert.
//! It renders the `turn_summary` the agent saves at turn end, plus the observed request timing from
//! `control.efficiency.inference_latency`, so it keeps no state of its own. The `Summary:` line is
//! the collapsed view; the other lines appear when tool details are expanded.

use serde_json::Value;
use std::time::Duration;

/// Changed files named before the report summarizes the rest.
const LISTED_FILES: usize = 8;

/// Project the same recorded report into the transcript's closing footer. No tools are replayed.
pub(in crate::app) fn outcome(
    summary: &Value,
    latency: &Value,
) -> crate::ui::transcript::TurnOutcome {
    let mut handoff = Vec::new();
    if let Some(report) = render(summary, latency) {
        handoff.extend(
            report
                .lines()
                .filter(|line| {
                    !line.starts_with("Outcome:")
                        && !line.starts_with("Summary:")
                        && !line.starts_with("Next:")
                })
                .take(10)
                .map(str::to_owned),
        );
        // Keep the actionable destination even when the detail budget is exhausted.
        if let Some(next) = report.lines().find(|line| line.starts_with("Next:")) {
            handoff.push(next.to_owned());
        }
    }
    crate::ui::transcript::TurnOutcome {
        status: summary["outcome"].as_str().unwrap_or("unknown").to_owned(),
        changed_files: summary["changed_files"].as_array().map_or(0, Vec::len),
        open_tasks: summary["open_tasks"].as_u64().unwrap_or(0) as usize,
        handoff,
    }
}

/// Single-line, bounded untrusted labels. Redaction happens before truncation so secrets are not
/// made unrecognizable to the redactor; terminal protocols are removed before display.
fn label(raw: &str) -> String {
    let redacted = crate::storage::redact::redact_text(raw);
    let clean = crate::ui::text::one_line(&redacted);
    let mut chars = clean.chars();
    let mut shown: String = chars.by_ref().take(240).collect();
    if chars.next().is_some() {
        shown.push('…');
    }
    shown
}

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
    let stopped = !matches!(outcome, "completed" | "answered");
    let evidence = &summary["evidence"];
    let obligations = obligation_lines(evidence);
    if files.is_empty()
        && !stopped
        && obligations.is_empty()
        && evidence["deliverable_coverage"] != "open_obligations"
        && summary["open_tasks"].as_u64().unwrap_or(0) == 0
        && summary["uncheckpointed_shell_commands"]
            .as_u64()
            .unwrap_or(0)
            == 0
        && summary["uncheckpointed_extension_calls"]
            .as_u64()
            .unwrap_or(0)
            == 0
    {
        return None;
    }
    let checks: &[Value] = summary["recorded_checks"]
        .as_array()
        .map_or(&[], Vec::as_slice);
    let current = summary["checks_current"].as_bool();
    let open = summary["open_tasks"].as_u64().unwrap_or(0);
    let count = |key: &str| summary[key].as_u64().unwrap_or(0);
    let (shell, extension) = (
        count("uncheckpointed_shell_commands"),
        count("uncheckpointed_extension_calls"),
    );

    // The full cause is printed just above the report; the class says what kind of failure it was.
    let outcome_text = match summary["error_class"].as_str() {
        Some(class) => format!("{} ({})", label(outcome), label(&class.replace('_', " "))),
        None => label(outcome),
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
    for obligation in obligations {
        lines.push(obligation);
    }
    if obligations_missing(evidence) {
        lines.push("Outstanding: recorded open obligations; inspect /evidence".to_owned());
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
            (false, true) if shell + extension > 0 => {
                "Next: inspect shell/extension effects; /undo does not cover them"
            }
            (false, true) => "Next: send a follow-up message to resolve open obligations",
            (false, false) => "Next: /review-file · /undo (checkpointed edits only)",
            (true, true) => "Next: send a follow-up message to continue",
            (true, false) => {
                "Next: send a follow-up message · /review-file · /undo (checkpointed edits only)"
            }
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
        .map(|file| label(file))
        .collect::<Vec<_>>()
        .join(", ");
    match files.len().saturating_sub(LISTED_FILES) {
        0 => shown,
        rest => format!("{shown} +{rest} more"),
    }
}

/// One word for the collapsed view: a failure wins, then staleness, then anything not passed.
fn checks_state(checks: &[Value], current: Option<bool>) -> String {
    let status = |check: &Value| check["status"].as_str().unwrap_or("unverified").to_owned();
    let failed = checks.iter().filter(|c| status(c) == "failed").count() as u64;
    if checks.is_empty() {
        "no recorded checks".to_owned()
    } else if failed > 0 {
        format!("{} failed", plural(failed, "check"))
    } else if current == Some(false) {
        "checks stale".to_owned()
    } else if current.is_none() {
        "check freshness unknown".to_owned()
    } else if checks.iter().all(|c| status(c) == "passed") {
        "checks passed (current)".to_owned()
    } else {
        "checks unverified".to_owned()
    }
}

fn checks_line(checks: &[Value], current: Option<bool>) -> String {
    if checks.is_empty() {
        return "Checks: none recorded".to_owned();
    }
    let listed: Vec<String> = checks
        .iter()
        .take(LISTED_FILES)
        .map(|check| {
            format!(
                "{} {}{}",
                label(check["name"].as_str().unwrap_or("check")),
                label(check["status"].as_str().unwrap_or("unverified")),
                if check["reused"] == true {
                    " (reused)"
                } else {
                    ""
                }
            )
        })
        .collect();
    let freshness = match current {
        Some(true) => "current",
        Some(false) => "stale",
        None => "freshness unknown",
    };
    let more = checks.len().saturating_sub(LISTED_FILES);
    let more = if more > 0 {
        format!(" +{more} more")
    } else {
        String::new()
    };
    let result = if checks.iter().any(|check| check["status"] == "failed") {
        "failed"
    } else if checks.iter().all(|check| check["status"] == "passed") {
        "passed"
    } else {
        "unverified"
    };
    format!(
        "Checks: {freshness} · {result} — {}{more}",
        listed.join(", ")
    )
}

/// Where each recorded check's retained output can be opened, naming checks without a log.
fn evidence_line(checks: &[Value]) -> Option<String> {
    let (with_log, without_log): (Vec<&Value>, Vec<&Value>) = checks.iter().partition(|check| {
        check["evidence_ref"]
            .as_str()
            .is_some_and(|r| !r.is_empty())
    });
    let name = |check: &&Value| label(check["name"].as_str().unwrap_or("unnamed check"));
    let mut parts: Vec<String> = with_log
        .iter()
        .take(LISTED_FILES)
        .map(|check| format!("/evidence check {}", name(check)))
        .collect();
    if !without_log.is_empty() {
        let names: Vec<String> = without_log.iter().take(LISTED_FILES).map(name).collect();
        parts.push(format!("no retained log for {}", names.join(", ")));
    }
    (!parts.is_empty()).then(|| format!("Evidence: {}", parts.join(" · ")))
}

/// Specific obligations already retained by the completion owner, not a second task reducer.
fn obligation_lines(evidence: &Value) -> Vec<String> {
    let tasks = evidence["outstanding_tasks"]
        .as_array()
        .into_iter()
        .flatten();
    let deliverables = evidence["outstanding_deliverables"]
        .as_array()
        .into_iter()
        .flatten();
    let obligations: Vec<_> = tasks.chain(deliverables).collect();
    let mut lines: Vec<_> = obligations
        .iter()
        .take(3)
        .map(|task| {
            let description = task["content"]
                .as_str()
                .or_else(|| task["description"].as_str())
                .or_else(|| task["id"].as_str())
                .or_else(|| task.as_str())
                .unwrap_or("unnamed obligation");
            let status = task["status"]
                .as_str()
                .map(|s| format!(" ({})", label(s)))
                .unwrap_or_default();
            match task["note"].as_str() {
                Some(note) => format!("Blocked: {} — {}{status}", label(note), label(description)),
                None => format!("Outstanding: {}{status}", label(description)),
            }
        })
        .collect();
    if obligations.len() > 3 {
        lines.push(format!(
            "Outstanding: +{} more; /evidence",
            obligations.len() - 3
        ));
    }
    lines
}

fn obligations_missing(evidence: &Value) -> bool {
    evidence["deliverable_coverage"] == "open_obligations" && obligation_lines(evidence).is_empty()
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
