//! Retention for `dial gc`: the cold-session selection rules and a persistent record of every
//! session gc archived (original path, archived path, cwd, reason) together with the retention
//! settings each applied run used. The record lives in `<user_dir>/gc.sqlite3`.
//!
//! Selection: non-recent sessions are ranked newest first; the newest
//! `retain_newest_global` are protected globally, and the newest `retain_newest_per_cwd` per working
//! directory are protected; of the rest, sessions untouched for `cold_archive_after_days` are cold.
use crate::platform::fs as platform_fs;
use crate::storage::settings_store;
use anyhow::{Context, Result};
#[cfg(test)]
use rusqlite::OptionalExtension;
use rusqlite::{Connection, params};
use serde::Serialize;
#[cfg(test)]
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const DB_FILE: &str = "gc.sqlite3";
pub const DAY_MS: i64 = 86_400_000;
/// Files modified inside this window may still be in use by a live writer; gc never touches them.
pub const WRITE_GRACE_MS: i64 = 5 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionPolicy {
    pub cold_archive_after_days: u64,
    pub retain_newest_global: usize,
    pub retain_newest_per_cwd: usize,
}

/// One top-level session file considered for archiving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub session_id: String,
    pub cwd: String,
    pub modified_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Modified inside [`WRITE_GRACE_MS`]; never considered.
    Recent,
    KeptNewestGlobal,
    KeptNewestPerCwd,
    /// Unprotected but not yet past `cold_archive_after_days`.
    Young,
    Archive,
}

/// Rank candidates newest first and decide each one's fate. Recent sessions do not count toward
/// the newest-N protections; every other session does, globally and within its cwd.
pub fn classify(
    mut candidates: Vec<Candidate>,
    policy: &RetentionPolicy,
    now_ms: i64,
) -> Vec<(Candidate, Verdict)> {
    candidates.sort_by(|a, b| {
        b.modified_ms
            .cmp(&a.modified_ms)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    let recent_cutoff = now_ms.saturating_sub(WRITE_GRACE_MS);
    // u64 days can exceed i64 milliseconds; saturate so huge values keep sessions instead of wrapping.
    let cold_span = i64::try_from(policy.cold_archive_after_days.saturating_mul(DAY_MS as u64))
        .unwrap_or(i64::MAX);
    let cold_cutoff = now_ms.saturating_sub(cold_span);
    let mut seen_global = 0usize;
    let mut seen_per_cwd: HashMap<String, usize> = HashMap::new();
    candidates
        .into_iter()
        .map(|candidate| {
            if candidate.modified_ms > recent_cutoff {
                return (candidate, Verdict::Recent);
            }
            let global_rank = seen_global;
            seen_global += 1;
            let cwd_rank = seen_per_cwd.entry(candidate.cwd.clone()).or_insert(0);
            let per_cwd_rank = *cwd_rank;
            *cwd_rank += 1;
            let verdict = if global_rank < policy.retain_newest_global {
                Verdict::KeptNewestGlobal
            } else if per_cwd_rank < policy.retain_newest_per_cwd {
                Verdict::KeptNewestPerCwd
            } else if policy.cold_archive_after_days > 0 && candidate.modified_ms > cold_cutoff {
                Verdict::Young
            } else {
                Verdict::Archive
            };
            (candidate, verdict)
        })
        .collect()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Modification time of a file in milliseconds since the Unix epoch (0 when unavailable).
pub fn modified_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One session archived by gc, as recorded in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveRecord {
    pub session_id: String,
    pub original_path: String,
    pub archived_path: String,
    pub archived_at_ms: i64,
    pub cwd: String,
    pub reason: String,
}

/// Persistent gc record: one row per applied run plus one row per archived session.
pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(user_dir: &Path) -> Result<Self> {
        settings_store::private_dir(user_dir)?;
        let path = user_dir.join(DB_FILE);
        let mut options = fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        platform_fs::open_owner_only(&options, &path).context("open gc database file")?;
        platform_fs::set_owner_only(&path)?;
        let conn =
            Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS retention_runs(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 applied_at_ms INTEGER NOT NULL,
                 settings TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS archived_sessions(
                 session_id TEXT PRIMARY KEY,
                 original_path TEXT NOT NULL,
                 archived_path TEXT NOT NULL UNIQUE,
                 archived_at_ms INTEGER NOT NULL,
                 cwd TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 run_id INTEGER REFERENCES retention_runs(id));",
        )?;
        Ok(Self { conn })
    }

    /// Record the settings an apply run used; returns the run id archives attach to.
    pub fn begin_run(&self, settings: &impl Serialize) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO retention_runs(applied_at_ms, settings) VALUES (?1, ?2)",
            params![now_ms(), serde_json::to_string(settings)?],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Insert or replace the archive record for a session (the latest archive wins).
    pub fn record_archive(&self, run_id: i64, record: &ArchiveRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO archived_sessions(session_id, original_path, archived_path, archived_at_ms,
                 cwd, reason, run_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id) DO UPDATE SET
                 original_path=excluded.original_path,
                 archived_path=excluded.archived_path,
                 archived_at_ms=excluded.archived_at_ms,
                 cwd=excluded.cwd,
                 reason=excluded.reason,
                 run_id=excluded.run_id",
            params![
                record.session_id,
                record.original_path,
                record.archived_path,
                record.archived_at_ms,
                record.cwd,
                record.reason,
                run_id,
            ],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn archived(&self, session_id: &str) -> Result<Option<ArchiveRecord>> {
        self.conn
            .query_row(
                "SELECT session_id, original_path, archived_path, archived_at_ms, cwd, reason
                 FROM archived_sessions WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok(ArchiveRecord {
                        session_id: row.get(0)?,
                        original_path: row.get(1)?,
                        archived_path: row.get(2)?,
                        archived_at_ms: row.get(3)?,
                        cwd: row.get(4)?,
                        reason: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Settings of the most recent applied run, if any.
    #[cfg(test)]
    pub fn last_run_settings(&self) -> Result<Option<Value>> {
        let text: Option<String> = self
            .conn
            .query_row(
                "SELECT settings FROM retention_runs ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        text.map(|t| serde_json::from_str(&t).map_err(Into::into))
            .transpose()
    }
}

#[cfg(test)]
mod tests;
