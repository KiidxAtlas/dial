//! Request-level views of the ledger: the newest requests, one request's full row, and the live
//! session (the most recently active transcript).

use super::{Db, Totals, Trace, TraceRequest, TraceTool, session_file_in_tree};
use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;
use std::path::Path;

/// A session with no transcript activity for this long is no longer live.
pub const LIVE_WINDOW_MS: i64 = 5 * 60_000;

/// Tool calls shown in the live view.
const RECENT_TOOLS: usize = 10;

/// One recorded model request, with its wire-level detail (`/api/requests`, `/api/request/<id>`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestRow {
    pub id: i64,
    pub ts_ms: i64,
    pub session: String,
    pub project: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub reported: bool,
    pub cost_usd: Option<f64>,
    pub duration_ms: u64,
    pub finish_reason: String,
    pub error: bool,
    pub transport: Option<String>,
    pub invalidation: Option<String>,
    pub common_prompt_bytes: Option<u64>,
    pub prompt_bytes: Option<u64>,
}

/// The session the ledger saw most recently, with its totals and newest activity.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveSession {
    pub session_file: String,
    pub session: String,
    pub project: String,
    pub cwd: String,
    pub model: String,
    pub started_ms: i64,
    pub last_ms: i64,
    pub idle_ms: i64,
    /// True while the last activity is within [`LIVE_WINDOW_MS`] of `now_ms`.
    pub active: bool,
    pub user_messages: i64,
    pub assistant_messages: i64,
    pub transcript_tool_calls: i64,
    pub totals: Totals,
    pub last_request: Option<TraceRequest>,
    pub recent_tools: Vec<TraceTool>,
}

const ROW_SQL: &str = "SELECT id, ts, session, project, model, input, output, cache_read, \
     cache_write, reasoning, reported, cost, duration_ms, finish, error, transport, \
     invalidation, common_prompt_bytes, prompt_bytes FROM requests";

fn count(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    Ok(row.get::<_, i64>(index)?.max(0) as u64)
}

fn nullable_count(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    Ok(row.get::<_, Option<i64>>(index)?.map(|v| v.max(0) as u64))
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: row.get(0)?,
        ts_ms: row.get(1)?,
        session: row.get(2)?,
        project: row.get(3)?,
        model: row.get(4)?,
        input: count(row, 5)?,
        output: count(row, 6)?,
        cache_read: count(row, 7)?,
        cache_write: count(row, 8)?,
        reasoning: count(row, 9)?,
        reported: row.get(10)?,
        cost_usd: row.get(11)?,
        duration_ms: count(row, 12)?,
        finish_reason: row.get(13)?,
        error: row.get(14)?,
        transport: row.get(15)?,
        invalidation: row.get(16)?,
        common_prompt_bytes: nullable_count(row, 17)?,
        prompt_bytes: nullable_count(row, 18)?,
    })
}

impl Db {
    /// Newest requests first, at most `limit`; `since_ms` (None = all time) bounds by timestamp.
    pub fn recent_requests(&self, since_ms: Option<i64>, limit: usize) -> Result<Vec<RequestRow>> {
        let since = since_ms.unwrap_or(i64::MIN);
        let mut stmt = self.conn.prepare(&format!(
            "{ROW_SQL} WHERE ts >= ?1 ORDER BY id DESC LIMIT ?2"
        ))?;
        Ok(stmt
            .query_map(params![since, limit as i64], read_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// One recorded request by its row id.
    pub fn request(&self, id: i64) -> Result<Option<RequestRow>> {
        Ok(self
            .conn
            .query_row(&format!("{ROW_SQL} WHERE id = ?1"), [id], read_row)
            .optional()?)
    }

    /// The most recently active session in the ledger. `None` when there is no session, or when
    /// its transcript is no longer inside `<user_dir>/projects/*/sessions/`.
    pub fn live(&self, user_dir: &Path, now_ms: i64) -> Result<Option<LiveSession>> {
        let newest: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM sessions ORDER BY last DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let Some(stored) = newest else {
            return Ok(None);
        };
        let Some((project, file)) = session_file_in_tree(user_dir, Path::new(&stored)) else {
            return Ok(None);
        };
        let Some(trace) = self.trace(user_dir, &project, &file)? else {
            return Ok(None);
        };
        let last_request = trace.requests.last().cloned();
        let skip = trace.tools.len().saturating_sub(RECENT_TOOLS);
        let recent_tools = trace.tools[skip..].to_vec();
        let idle_ms = (now_ms - trace.last_ms).max(0);
        Ok(Some(LiveSession {
            session_file: trace.session_file,
            session: trace.session,
            project: trace.project,
            cwd: trace.cwd,
            model: trace.model,
            started_ms: trace.started_ms,
            last_ms: trace.last_ms,
            idle_ms,
            active: idle_ms <= LIVE_WINDOW_MS,
            user_messages: trace.user_messages,
            assistant_messages: trace.assistant_messages,
            transcript_tool_calls: trace.transcript_tool_calls,
            totals: trace.totals,
            last_request,
            recent_tools,
        }))
    }
}

#[cfg(test)]
mod tests;
