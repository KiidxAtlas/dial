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
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn cand(id: &str, cwd: &str, age_days: f64) -> Candidate {
        Candidate {
            path: PathBuf::from(format!("/s/{id}.jsonl")),
            session_id: id.to_string(),
            cwd: cwd.to_string(),
            modified_ms: NOW - (age_days * DAY_MS as f64) as i64 - WRITE_GRACE_MS * 2,
        }
    }

    fn policy(global: usize, per_cwd: usize, cold: u64) -> RetentionPolicy {
        RetentionPolicy {
            cold_archive_after_days: cold,
            retain_newest_global: global,
            retain_newest_per_cwd: per_cwd,
        }
    }

    fn verdicts(out: &[(Candidate, Verdict)]) -> Vec<(&str, Verdict)> {
        out.iter()
            .map(|(c, v)| (c.session_id.as_str(), *v))
            .collect()
    }

    #[test]
    fn global_protection_keeps_newest_n_regardless_of_cwd() {
        let input = vec![
            cand("a", "/x", 100.0),
            cand("b", "/y", 90.0),
            cand("c", "/z", 80.0),
        ];
        let out = classify(input, &policy(2, 0, 30), NOW);
        assert_eq!(
            verdicts(&out),
            vec![
                ("c", Verdict::KeptNewestGlobal),
                ("b", Verdict::KeptNewestGlobal),
                ("a", Verdict::Archive),
            ]
        );
    }

    #[test]
    fn per_cwd_protection_counts_within_each_cwd() {
        // Newest-first: a(x), b(y), c(x), d(x). Global protection is off, per-cwd keeps the
        // newest one in each cwd, so only c and d are cold.
        let input = vec![
            cand("a", "/x", 10.0),
            cand("b", "/y", 20.0),
            cand("c", "/x", 30.0),
            cand("d", "/x", 40.0),
        ];
        let out = classify(input, &policy(0, 1, 30), NOW);
        assert_eq!(
            verdicts(&out),
            vec![
                ("a", Verdict::KeptNewestPerCwd),
                ("b", Verdict::KeptNewestPerCwd),
                ("c", Verdict::Archive),
                ("d", Verdict::Archive),
            ]
        );
    }

    #[test]
    fn recent_sessions_do_not_consume_protection_slots() {
        let mut fresh = cand("fresh", "/x", 0.0);
        fresh.modified_ms = NOW - 1_000;
        let input = vec![fresh, cand("old", "/x", 60.0)];
        let out = classify(input, &policy(1, 0, 30), NOW);
        assert_eq!(
            verdicts(&out),
            vec![
                ("fresh", Verdict::Recent),
                ("old", Verdict::KeptNewestGlobal)
            ]
        );
    }

    #[test]
    fn cold_threshold_separates_young_from_archived() {
        let input = vec![cand("young", "/x", 5.0), cand("cold", "/x", 45.0)];
        let out = classify(input, &policy(0, 0, 30), NOW);
        assert_eq!(
            verdicts(&out),
            vec![("young", Verdict::Young), ("cold", Verdict::Archive)]
        );
    }

    #[test]
    fn zero_cold_days_archives_every_unprotected_session() {
        let input = vec![cand("a", "/x", 0.5), cand("b", "/x", 0.1)];
        let out = classify(input, &policy(1, 0, 0), NOW);
        assert_eq!(
            verdicts(&out),
            vec![("b", Verdict::KeptNewestGlobal), ("a", Verdict::Archive)]
        );
    }

    #[test]
    fn store_round_trip_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let settings = serde_json::json!({"coldArchiveAfterDays": 30});
        let run = store.begin_run(&settings).unwrap();
        let record = ArchiveRecord {
            session_id: "abc".into(),
            original_path: "/a/sessions/1_abc.jsonl".into(),
            archived_path: "/a/sessions/archive/1_abc.jsonl".into(),
            archived_at_ms: 42,
            cwd: "/proj".into(),
            reason: "cold".into(),
        };
        store.record_archive(run, &record).unwrap();
        assert_eq!(store.archived("abc").unwrap(), Some(record.clone()));
        assert_eq!(store.archived("missing").unwrap(), None);
        assert_eq!(store.last_run_settings().unwrap(), Some(settings));
        // Re-archiving the same session replaces its record.
        let mut moved = record.clone();
        moved.archived_at_ms = 99;
        store.record_archive(run, &moved).unwrap();
        assert_eq!(store.archived("abc").unwrap().unwrap().archived_at_ms, 99);
    }
}
