//! `--restore`: rebind the interrupted workers of a resumed session before the user takes control.
//!
//! Workers rebind parent-first through the live `task` resume path, which checks the saved
//! contract, workspace, history, and lease. Restoring never submits the main editor's prompt.

use crate::agent::Agent;
use crate::agent::workers::child_transcript::{self, TranscriptSummary};
use crate::app::bridge::Bridge;
use crate::app::core::App;
use crate::app::shell::{PAINT_EVERY, Shell};
use crate::config::paths::Layout;
use anyhow::{Result, bail};
use serde_json::json;
use std::collections::HashSet;
use std::time::Instant;

/// Continuation sent to each restored worker. It resumes the saved assignment; it is not the
/// user's prompt.
const CONTINUE: &str = "The session was restored after an interruption. Continue your assigned task from the point it stopped. Do not repeat tool calls that already finished.";

/// Outcome of restoring one parent session's interrupted workers.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Report {
    pub(crate) restored: Vec<String>,
    /// Worker id and the reason it stayed interrupted.
    pub(crate) rejected: Vec<(String, String)>,
}

/// A worker is a restore candidate when it was interrupted: its transcript has no closing status
/// (the process died mid-run), or it persisted `interrupted`. Completed, failed and cancelled
/// workers never rerun.
fn interrupted(entry: &TranscriptSummary) -> bool {
    match &entry.status {
        None => true,
        Some(status) => status["status"] == "interrupted",
    }
}

/// Interrupted workers of `parent_session`, from each id's most recent transcript.
pub(crate) fn interrupted_workers(layout: &Layout, parent_session: &str) -> Vec<String> {
    let mut decided = HashSet::new();
    child_transcript::scan(layout, parent_session)
        .into_iter()
        .filter_map(|entry| {
            let id = entry.header["id"].as_str()?.to_owned();
            // `scan` is newest first: only the most recent transcript of an id decides.
            (decided.insert(id.clone()) && interrupted(&entry)).then_some(id)
        })
        .collect()
}

/// Rebinds each interrupted worker of the agent's session through `start(action: "resume")`.
/// Rejections keep the worker interrupted and carry the reason the resume path gave.
pub(super) fn restore_interrupted(
    app: &App,
    shell: &mut Shell,
    agent: &mut Agent<'_>,
) -> Result<Report> {
    let parent = agent.session().id().to_owned();
    let mut report = Report::default();
    for id in interrupted_workers(&app.layout, &parent) {
        let args = json!({"action": "resume", "id": id, "message": CONTINUE});
        let (result, failed) = {
            let mut bridge = Bridge {
                app,
                shell: &mut *shell,
                interrupt: app.interrupt.clone(),
                started: Instant::now(),
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            let result = agent.start_subagents(&args, &mut bridge);
            (result, bridge.failed)
        };
        if failed {
            bail!("terminal draw failed");
        }
        match result {
            Ok(value) => match check_started(&value, &id) {
                Ok(()) => report.restored.push(id),
                Err(error) => report.rejected.push((id, error.to_string())),
            },
            Err(error) => report.rejected.push((id, format!("{error:#}"))),
        }
    }
    announce(shell, &report);
    Ok(report)
}

/// An accepted API call may contain a per-worker setup failure; only an actual spawn is restored.
pub(super) fn check_started(value: &serde_json::Value, id: &str) -> Result<()> {
    let item = value
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == id))
        .ok_or_else(|| anyhow::anyhow!("worker {id} did not acknowledge a start"))?;
    if item["status"] != "running" {
        bail!(
            "{}",
            item["result"]
                .as_str()
                .unwrap_or("worker setup failed before starting")
        );
    }
    Ok(())
}

/// Shows the restored and rejected counts, with each rejection's reason.
fn announce(shell: &mut Shell, report: &Report) {
    let mut lines = Vec::new();
    if report.restored.is_empty() && report.rejected.is_empty() {
        lines.push("no interrupted workers in this session".to_owned());
    } else if !report.restored.is_empty() {
        lines.push(format!(
            "restored {}: {}",
            report.restored.len(),
            report.restored.join(", ")
        ));
    }
    if !report.rejected.is_empty() {
        lines.push(format!(
            "rejected {} (still interrupted):",
            report.rejected.len()
        ));
        for (id, reason) in &report.rejected {
            lines.push(format!("  {id}: {reason}"));
        }
    }
    shell.transcript.notice("Restore", &lines.join("\n"));
}

#[cfg(test)]
mod tests;
