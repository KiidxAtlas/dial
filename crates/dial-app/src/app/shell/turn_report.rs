//! End-of-turn report: what changed, what was checked, and what `/undo` cannot revert.
//! It renders the `turn_summary` the agent saves at turn end, plus the observed request timing from
//! `control.efficiency.inference_latency`, so it keeps no state of its own.

use crate::ui::transcript::{ReceiptCheck, TurnOutcome, TurnReceipt};
use serde_json::Value;
use std::time::Duration;

/// Changed files named before the report summarizes the rest.
const LISTED_FILES: usize = 8;

/// Adds the turn's observed API cost to a report that already has details, above `Next:`.
/// A plain answer keeps its empty footer, and unpriced or local usage adds no line.
pub(in crate::app) fn with_cost(
    mut outcome: crate::ui::transcript::TurnOutcome,
    turn: &crate::llm::usage::Counts,
) -> crate::ui::transcript::TurnOutcome {
    if outcome.handoff.is_empty() || turn.observed_cost_usd.is_none() {
        return outcome;
    }
    let at = outcome
        .handoff
        .iter()
        .position(|line| line.starts_with("Next:"))
        .unwrap_or(outcome.handoff.len());
    outcome
        .handoff
        .insert(at, format!("Cost: {}", turn.cost_label()));
    outcome
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

/// Project retained facts into the compact receipt and its expanded handoff. A plain answer
/// has neither; an absent completion record never borrows facts from an earlier turn.
pub(in crate::app) fn outcome(summary: &Value, latency: &Value) -> TurnOutcome {
    let status = summary["outcome"].as_str().unwrap_or("unknown");
    let mut files = summary["changed_files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let paths: Vec<_> = files.by_ref().take(LISTED_FILES).map(label).collect();
    let mut outcome = TurnOutcome {
        status: status.to_owned(),
        changed_files: paths.len() + files.count(),
        open_tasks: summary["open_tasks"].as_u64().unwrap_or(0) as usize,
        ..TurnOutcome::default()
    };
    if summary["outcome"].as_str().is_none() {
        return outcome;
    }
    let stopped = !matches!(status, "completed" | "answered");
    let evidence = &summary["evidence"];
    let effects_unknown = summary["effects_unknown"] == true;
    let obligations = obligation_lines(evidence);
    let obligations_missing =
        obligations.is_empty() && evidence["deliverable_coverage"] == "open_obligations";
    let outstanding = !obligations.is_empty() || obligations_missing;
    let checks: &[Value] = summary["recorded_checks"]
        .as_array()
        .map_or(&[], Vec::as_slice);
    // A previously current check cannot establish the state after an uncertain tool effect.
    let current = if effects_unknown {
        None
    } else {
        summary["checks_current"].as_bool()
    };
    let open = outcome.open_tasks;
    let count = |key: &str| summary[key].as_u64().unwrap_or(0);
    let (shell, extension) = (
        count("uncheckpointed_shell_commands"),
        count("uncheckpointed_extension_calls"),
    );
    if outcome.changed_files == 0
        && !stopped
        && !effects_unknown
        && !outstanding
        && checks.is_empty()
        && open == 0
        && shell == 0
        && extension == 0
    {
        return outcome;
    }
    let mut lines = Vec::new();
    if let Some(class) = summary["error_class"].as_str() {
        lines.push(format!("Failure: {}", label(&class.replace('_', " "))));
    }
    lines.push(if paths.is_empty() {
        if effects_unknown || shell > 0 || extension > 0 {
            "Changed: none recorded"
        } else {
            "Changed: none"
        }
        .to_owned()
    } else {
        let mut changed = format!("Changed: {}", paths.join(", "));
        let rest = outcome.changed_files - paths.len();
        if rest > 0 {
            use std::fmt::Write;
            let _ = write!(changed, " +{rest} more");
        }
        changed
    });
    lines.push(checks_line(checks, current));
    if let Some(evidence) = evidence_line(checks) {
        lines.push(evidence);
    }
    if open > 0 {
        lines.push(format!("Open tasks: {open}"));
    }
    lines.extend(obligations);
    if obligations_missing {
        lines.push("Outstanding: recorded open obligations; inspect /evidence".to_owned());
    }
    if effects_unknown {
        lines.push(
            "Effects: unknown — inspect retained tool status and effects; no replay is authorized"
                .to_owned(),
        );
    }
    if shell + extension > 0 {
        // Name only the kinds that occurred; "and 0 extension calls" is noise.
        let effects: Vec<String> = [(shell, "shell command"), (extension, "extension call")]
            .into_iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, noun)| plural(n, noun))
            .collect();
        lines.push(format!(
            "Outside /undo: {} may have changed files",
            effects.join(" and ")
        ));
    }
    if let Some(timing) = timing_line(latency) {
        lines.push(timing);
    }
    // Recovery and outstanding work take precedence over optional review/undo. This is
    // presentation of retained facts, never permission to replay a turn or its tools.
    let next = if effects_unknown {
        "inspect retained tool status and effects before continuing; do not replay uncertain or completed operations; send a follow-up message for unfinished work only"
    } else if stopped {
        "send a follow-up message after inspecting the retained failure and effects; continue only unfinished work, not the whole turn"
    } else if outstanding || open > 0 {
        "inspect /evidence for outstanding work, then send a follow-up message with the next scoped action"
    } else if checks.iter().any(|check| check["status"] == "failed") {
        "inspect /evidence for failed checks; resolve or disclose the cause before requesting scoped verification"
    } else if !checks.is_empty()
        && (current != Some(true) || checks.iter().any(|check| check["status"] != "passed"))
    {
        "inspect /evidence for verification gaps before requesting scoped verification"
    } else if shell + extension > 0 {
        "inspect shell/extension effects; /undo does not cover them"
    } else if !paths.is_empty() {
        "/review-file · /undo (checkpointed edits only)"
    } else {
        "send a follow-up message"
    };
    let review = if stopped && !effects_unknown && !paths.is_empty() {
        " · /review-file · /undo (checkpointed edits only)"
    } else {
        ""
    };
    lines.push(format!("Next: {next}{review}"));

    let (check, check_label, target) = checks_state(checks, current);
    outcome.receipt = Some(TurnReceipt {
        paths,
        check,
        check_label,
        check_target: target.map(label),
    });
    outcome.handoff = lines;
    outcome
}

pub(super) fn plural(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// The latest representative of the most consequential status, not merely the last entry:
/// a later passing check for a different target cannot clear a failure.
fn checks_state(checks: &[Value], current: Option<bool>) -> (ReceiptCheck, String, Option<&str>) {
    let failed = checks.iter().filter(|c| c["status"] == "failed").count();
    let last_gap = checks.iter().rfind(|c| c["status"] != "passed");
    let (state, summary, representative) = if checks.is_empty() {
        (
            ReceiptCheck::Unverified,
            "unverified (no recorded checks)".into(),
            None,
        )
    } else if failed > 0 {
        let freshness = match current {
            Some(true) => "",
            Some(false) => " (stale)",
            None => " (freshness unknown)",
        };
        (
            ReceiptCheck::Failed,
            format!("{} failed{freshness}", plural(failed as u64, "check")),
            checks.iter().rfind(|c| c["status"] == "failed"),
        )
    } else if current == Some(false) {
        (ReceiptCheck::Stale, "stale".into(), checks.last())
    } else if current.is_none() {
        (
            ReceiptCheck::Unverified,
            "unverified (freshness unknown)".into(),
            checks.last(),
        )
    } else if let Some(gap) = last_gap {
        let status = label(gap["status"].as_str().unwrap_or("unknown status"));
        (
            ReceiptCheck::Unverified,
            format!("unverified ({status})"),
            Some(gap),
        )
    } else {
        (
            ReceiptCheck::Passed,
            format!("{} passed", plural(checks.len() as u64, "check")),
            checks.last(),
        )
    };
    let target = representative.and_then(|c| {
        c["command"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| c["name"].as_str())
    });
    (state, summary, target)
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
            let blocked = task["status"] == "blocked";
            // The `Blocked:` prefix already states that status; other statuses stay explicit.
            let status = task["status"]
                .as_str()
                .filter(|_| !blocked)
                .map(|s| format!(" ({})", label(s)))
                .unwrap_or_default();
            let prefix = if blocked { "Blocked" } else { "Outstanding" };
            match task["note"].as_str() {
                Some(note) => format!("{prefix}: {} — {}{status}", label(note), label(description)),
                None => format!("{prefix}: {}{status}", label(description)),
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

/// Observed request-phase durations, not provider compute attribution or turn wall time.
/// Concurrent requests can overlap; stream stalls are already included in generation.
/// Only measured, nonzero phases are shown; missing measurements are never filled with zero.
fn timing_line(latency: &Value) -> Option<String> {
    let positive = |key: &str| latency[key].as_u64().filter(|value| *value > 0);
    let phases: Vec<String> = [
        ("wait", "response_wait_ms"),
        ("generation", "generation_ms"),
        ("loading", "loading_ms"),
        ("prompt processing", "prompt_processing_ms"),
        ("retry backoff", "retry_backoff_ms"),
        ("cancellation", "cancellation_ms"),
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
    Some(format!(
        "Model time: {}{requests} (observed request phases, not turn wall time)",
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
