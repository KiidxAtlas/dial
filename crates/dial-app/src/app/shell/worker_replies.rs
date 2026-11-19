//! Transcript replies for the worker commands (`/workers <id>`, `/message`, `/cancel`).
//!
//! The engine answers these with the structured values protocol clients consume. The terminal
//! shows the same facts as text, through one projection for the idle and the busy route.

use crate::ui::text::{duration_label, one_line};
use serde_json::Value;

/// Characters of a worker's task or result kept in a status reply; the worker panel has the rest.
const EXCERPT_CHARS: usize = 400;

/// Readable form of a worker command's reply. A shape this projection does not know keeps every
/// field as indented JSON instead of a guessed summary.
pub(super) fn text(value: &Value) -> String {
    if let Some(rows) = value["receipts"].as_array() {
        return receipts(rows);
    }
    if value["cancellation_requested"] == true {
        let id = value["id"].as_str().unwrap_or("the worker");
        return format!(
            "Cancellation requested for {id}. The main answer and other workers continue; completed edits and external effects remain. /workers {id} shows when it has stopped."
        );
    }
    if let Some(jobs) = value.as_array() {
        if jobs.is_empty() {
            return "No workers in this session.".to_owned();
        }
        return jobs.iter().map(headline).collect::<Vec<_>>().join("\n");
    }
    if value["status"].is_string() {
        return status(value);
    }
    if value["items"].is_array() && value["counts"].is_object() {
        return pool(value);
    }
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// One delivery line per recipient of a `/message`.
fn receipts(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "No recipient received the message.".to_owned();
    }
    rows.iter()
        .map(|row| {
            let to = one_line(row["to"].as_str().unwrap_or("?"));
            match row["status"].as_str().unwrap_or("unknown") {
                "queued" => format!("Message queued for {to}."),
                "refused" => match row["reason"].as_str().filter(|reason| !reason.is_empty()) {
                    Some(reason) => format!("Message refused by {to}: {}", one_line(reason)),
                    None => format!("Message refused by {to}."),
                },
                "finished" => format!(
                    "Message not delivered: {to} has already finished ({}).",
                    row["worker_status"].as_str().unwrap_or("status unknown")
                ),
                other => format!("{to}: {other}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `task-1 · reviewer · running · tool: bash`: the facts that identify a worker and its state.
fn headline(job: &Value) -> String {
    let field = |key: &str| {
        job[key]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(one_line)
    };
    let mut parts = vec![field("id").unwrap_or_else(|| "worker".to_owned())];
    parts.extend(field("name"));
    parts.push(
        field("status")
            .or_else(|| field("state"))
            .unwrap_or_else(|| "status unknown".to_owned()),
    );
    if job["status"] == "running" {
        parts.extend(field("phase"));
    }
    parts.join(" · ")
}

/// One worker: identity, progress, the controller's acceptance receipt, then task and result.
fn status(job: &Value) -> String {
    let field = |key: &str| job[key].as_str().filter(|value| !value.is_empty());
    let mut lines = vec![headline(job)];

    let mut facts = Vec::new();
    if let Some(agent) = field("agent") {
        facts.push(format!("agent {}", one_line(agent)));
    }
    if let Some(model) = field("model") {
        facts.push(format!("model {}", one_line(model)));
    }
    if let (Some(steps), Some(tools)) = (job["steps"].as_u64(), job["tools"].as_u64()) {
        facts.push(format!("{steps} steps, {tools} tool calls"));
    }
    if let Some(running) = job["running_secs"].as_u64() {
        let mut clock = format!("running {}", duration_label(running as f64));
        if let Some(idle) = job["idle_secs"].as_u64() {
            clock.push_str(&format!(", quiet {}", duration_label(idle as f64)));
        }
        if let Some(health) = field("health") {
            clock.push_str(&format!(" ({health})"));
        }
        facts.push(clock);
    } else if let Some(elapsed) = job["elapsed_ms"].as_u64() {
        facts.push(format!("ran {}", duration_label(elapsed as f64 / 1000.0)));
    }
    if !facts.is_empty() {
        lines.push(facts.join(" · "));
    }

    if let Some(state) = job["acceptance"]["state"].as_str() {
        let mut line = format!("Acceptance: {state}");
        if let Some(reason) = job["acceptance"]["reason"]
            .as_str()
            .filter(|reason| !reason.is_empty())
        {
            line.push_str(&format!(" ({})", one_line(reason)));
        }
        lines.push(line);
    }
    for key in ["artifact_error", "transcript_error"] {
        if let Some(error) = field(key) {
            lines.push(format!("Error: {}", one_line(error)));
        }
    }
    if let Some(task) = field("task") {
        lines.push(format!("Task: {}", excerpt(task)));
    }
    if let Some(result) = field("result") {
        lines.push(format!("Result: {}", excerpt(result)));
    }
    if let Some(artifact) = field("artifact") {
        lines.push(format!("Full report: {}", one_line(artifact)));
    }
    lines.push("Open /workers for the full transcript and controls.".to_owned());
    lines.join("\n")
}

/// A worker pool: its state, the item counts by status, then one headline per reported item.
fn pool(report: &Value) -> String {
    let mut lines = vec![headline(report)];
    let counts: Vec<String> = report["counts"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(status, count)| Some(format!("{} {status}", count.as_u64()?)))
        .collect();
    if !counts.is_empty() {
        lines.push(format!("Items: {}", counts.join(", ")));
    }
    lines.extend(
        report["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| format!("  {}", headline(item))),
    );
    lines.join("\n")
}

/// One line, cut at a character boundary with an explicit marker when text was dropped.
fn excerpt(text: &str) -> String {
    let line = one_line(text);
    if line.chars().count() <= EXCERPT_CHARS {
        return line;
    }
    let kept: String = line.chars().take(EXCERPT_CHARS).collect();
    format!("{}… (truncated)", kept.trim_end())
}

#[cfg(test)]
mod tests;
