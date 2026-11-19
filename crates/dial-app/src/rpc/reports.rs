//! Read-only local reports over plain RPC: `code_history` (`/code-history` search and show) and
//! `stats` (`/stats <range>`). They run as controller admin reads, off the agent actor, because
//! they read project and user storage rather than the live session. The engine owners are the
//! ones the terminal calls (`tools::knowledge::history`, `stats`). `/code-history clear` stays
//! terminal-only: it deletes every recorded diff and the terminal confirms it first.
use super::coded;
use crate::config::paths::Layout;
use crate::config::types::Config;
use crate::stats::{self, Db};
use crate::tools::knowledge::history::ChangeHistory;
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::{Value, json};

/// Default and largest number of versions one `code_history` search returns.
pub(super) const DEFAULT_HISTORY_LIMIT: u64 = 30;
pub(super) const MAX_HISTORY_LIMIT: u64 = 200;
/// Longest accepted query, path, branch or range text.
const MAX_TEXT_BYTES: usize = 1000;

/// `code_history`: `action` `search` (optional `query`, `path`, `branch`, `limit`) lists recorded
/// file versions newest first; `show` (`id`) returns one version and its stored diff.
pub(super) fn code_history(layout: &Layout, config: &Config, params: &Value) -> Result<Value> {
    if !(config.codebase.enabled && config.codebase.history) {
        return Err(coded(
            ErrorCode::Refused,
            "code history is off; enable codebase.history (and codebase.enabled) with set_setting",
        ));
    }
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("search");
    match action {
        "search" => {
            let query = optional_text(params, "query")?.unwrap_or_default();
            let path = optional_text(params, "path")?;
            let branch = optional_text(params, "branch")?;
            let limit = match params.get("limit") {
                None | Some(Value::Null) => DEFAULT_HISTORY_LIMIT,
                Some(value) => value
                    .as_u64()
                    .filter(|limit| (1..=MAX_HISTORY_LIMIT).contains(limit))
                    .ok_or_else(|| {
                        coded(
                            ErrorCode::InvalidParams,
                            format!(
                                "params.limit must be an integer from 1 to {MAX_HISTORY_LIMIT}"
                            ),
                        )
                    })?,
            };
            let store = open(layout)?;
            let versions = store
                .search(&query, path.as_deref(), branch.as_deref(), limit as usize)
                .map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))?;
            Ok(json!({"action":"search","versions":versions}))
        }
        "show" => {
            let id = params.get("id").and_then(Value::as_i64).ok_or_else(|| {
                coded(
                    ErrorCode::InvalidParams,
                    "code_history show requires integer params.id",
                )
            })?;
            let store = open(layout)?;
            match store
                .show(id)
                .map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))?
            {
                Some((version, diff)) => Ok(json!({"action":"show","version":version,"diff":diff})),
                None => Err(coded(
                    ErrorCode::NotFound,
                    format!("no recorded version #{id}"),
                )),
            }
        }
        "clear" => Err(coded(
            ErrorCode::Refused,
            "clearing code history deletes every recorded diff and is confirmed in the terminal (/code-history clear)",
        )),
        other => Err(coded(
            ErrorCode::InvalidParams,
            format!("code_history action {other} is not one of search, show"),
        )),
    }
}

/// `stats`: the persisted cross-session report `/stats` prints, for an optional `range` such as
/// `7d` (omit for all recorded time). Session transcripts are synced into the local statistics
/// index first, as the terminal does; no provider is contacted.
pub(super) fn stats(layout: &Layout, params: &Value) -> Result<Value> {
    let range = optional_text(params, "range")?.unwrap_or_default();
    let since = stats::parse_range(&range)
        .map_err(|error| coded(ErrorCode::InvalidParams, format!("{error:#}")))?;
    let failed = |error: anyhow::Error| coded(ErrorCode::Failed, format!("{error:#}"));
    let mut db = Db::open_default(&layout.user_dir).map_err(failed)?;
    db.sync_sessions(&layout.user_dir).map_err(failed)?;
    let report = db.report(since, stats::now_ms()).map_err(failed)?;
    Ok(json!({"range":range,"report":report}))
}

fn open(layout: &Layout) -> Result<ChangeHistory> {
    ChangeHistory::open(layout).map_err(|error| coded(ErrorCode::Failed, format!("{error:#}")))
}

/// An optional string of at most [`MAX_TEXT_BYTES`]; blank counts as absent.
fn optional_text(params: &Value, key: &str) -> Result<Option<String>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() <= MAX_TEXT_BYTES => {
            Ok(Some(text.trim().to_owned()).filter(|text| !text.is_empty()))
        }
        Some(_) => Err(coded(
            ErrorCode::InvalidParams,
            format!("params.{key} must be a string of at most {MAX_TEXT_BYTES} bytes"),
        )),
    }
}

#[cfg(test)]
mod tests;
