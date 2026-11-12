//! Extension changes over plain RPC (`extensions` group). `list_extensions` returns the rows
//! `/extensions` shows; `extension_action` runs one `/extensions` intent (install, import, enable,
//! disable, remove, test, Smithery steps, catalog browsing, skill install) through the same owner
//! as the overlay (`app::commands::extensions_browser`), including its health check and its rule
//! for which config layer an existing server is changed in.
//!
//! Saved changes apply to the running agent on `apply_settings`, as other saved settings do.
//! Intents that change files, run a downloaded command, or remove or disable something need
//! `confirm: true`, the overlay's confirm step. A form the owner asks for (a Smithery key, a
//! server's settings or its required values) comes back as `form`; the client asks the user and
//! sends the named `submit` intent with the values. Field values are never echoed.
use super::{Sender, coded, settings};
use crate::config::Config;
use crate::config::paths::Layout;
use crate::ui::overlay::extensions::model::{
    CatalogUpdate, Form, FormKind, Intent, Row, SharedProgress,
};
use anyhow::{Result, anyhow};
use dial_protocol::ErrorCode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(100);

/// The one extension change running at a time.
#[derive(Default)]
pub(super) struct ExtensionJobs {
    running: AtomicBool,
}

/// `list_extensions`: the rows `/extensions` shows, from disk and `config`.
pub(super) fn list(layout: &Layout, config: &Config) -> Value {
    let snapshot = crate::app::extensions_snapshot(layout, config);
    json!({
        "mcp": snapshot.mcp.iter().map(row).collect::<Vec<_>>(),
        "skills": snapshot.skills.iter().map(row).collect::<Vec<_>>(),
    })
}

fn row(row: &Row) -> Value {
    json!({
        "id": row.id,
        "title": row.title,
        "summary": row.summary,
        "tags": row.tags,
        "origin": row.origin,
        "state": row.state,
        "details": row.details,
    })
}

/// The intent in `params.intent`, refused before anything runs when it is malformed or needs a
/// confirmation the client did not give.
fn intent(params: &Value) -> Result<Intent> {
    let intent: Intent = serde_json::from_value(params["intent"].clone())
        .map_err(|error| coded(ErrorCode::InvalidParams, format!("params.intent: {error}")))?;
    if intent.needs_confirm() && params["confirm"] != true {
        return Err(coded(
            ErrorCode::InvalidParams,
            format!(
                "{}: this changes files, runs a downloaded command, or removes or disables something; ask the user, then send it again with confirm: true",
                intent.describe()
            ),
        ));
    }
    Ok(intent)
}

/// Starts `extension_action` on a worker thread that answers `id` when the intent finishes.
/// Progress lines arrive as `extension_progress` events meanwhile.
pub(super) fn start(
    jobs: &Arc<ExtensionJobs>,
    out: &Sender,
    layout: &Layout,
    config: Config,
    id: &Value,
    params: &Value,
) -> Result<()> {
    let intent = intent(params)?;
    let scope = settings::scope(params)?;
    if jobs.running.swap(true, Ordering::SeqCst) {
        return Err(coded(
            ErrorCode::Busy,
            "another extension change is still running",
        ));
    }
    let (jobs, out, layout, id) = (jobs.clone(), out.clone(), layout.clone(), id.clone());
    thread::spawn(move || {
        let progress = SharedProgress::default();
        let label = intent.describe();
        let worker = {
            let (progress, intent) = (progress.clone(), intent.clone());
            thread::spawn(move || {
                crate::app::extensions_run_and_save(&layout, &config, &intent, scope, &progress)
            })
        };
        let mut sent = 0;
        loop {
            let finished = worker.is_finished();
            sent = forward_lines(&out, &progress, &label, sent);
            if finished {
                break;
            }
            thread::sleep(POLL);
        }
        let result = worker
            .join()
            .map_err(|_| anyhow!("the extension change failed unexpectedly"))
            .and_then(|result| result)
            .map(|(outcome, saved)| {
                let shown = progress.lock().unwrap_or_else(|p| p.into_inner());
                json!({
                    "ok": outcome.ok,
                    "message": outcome.message,
                    "action": label,
                    "saved_scope": saved.map(settings::scope_name),
                    "apply_settings": saved.is_some(),
                    "form": shown.form.as_ref().map(form),
                    "catalog": shown.catalog.as_ref().map(catalog),
                })
            });
        jobs.running.store(false, Ordering::SeqCst);
        out.result(&id, result);
    });
    Ok(())
}

/// Emits the progress lines after the first `sent`; returns the new count.
fn forward_lines(out: &Sender, progress: &SharedProgress, label: &str, sent: usize) -> usize {
    let lines = {
        let shown = progress.lock().unwrap_or_else(|p| p.into_inner());
        shown.lines.get(sent..).unwrap_or_default().to_vec()
    };
    for line in &lines {
        out.emit(json!({"event":"extension_progress","action":label,"message":line}));
    }
    sent + lines.len()
}

/// A form the owner asks for, without any typed values. `submit` names the intent `op` to send
/// with the answers: `smithery_key` (`name`, `key`), `smithery_deploy` (`name`, `values`) or
/// `install_mcp` (`id`, `values`), where `values` maps field keys to answers.
fn form(form: &Form) -> Value {
    let (submit, target) = match &form.kind {
        FormKind::Key { name } => ("smithery_key", name),
        FormKind::Config { name } => ("smithery_deploy", name),
        FormKind::McpInputs { id } => ("install_mcp", id),
    };
    json!({
        "submit": submit,
        "target": target,
        "title": form.title,
        "note": form.note,
        "error": form.error,
        "fields": form.fields.iter().map(|field| json!({
            "key": field.key,
            "label": field.label,
            "hint": field.hint,
            "required": field.required,
            "secret": field.secret,
        })).collect::<Vec<_>>(),
    })
}

fn catalog(update: &CatalogUpdate) -> Value {
    json!({
        "tab": update.tab,
        "query": update.query,
        "page": update.page,
        "rows": update.rows.iter().map(row).collect::<Vec<_>>(),
        "more": update.more,
        "message": update.message,
        "error": update.error,
    })
}

#[cfg(test)]
mod tests;
