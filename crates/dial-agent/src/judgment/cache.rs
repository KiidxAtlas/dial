//! Persistent answers for judgments: one SQLite table with a time-to-live on read and a row bound
//! enforced on write (least recently used rows are evicted first).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA_VERSION: i64 = 1;
const DEFAULT_RELATIVE_PATH: &str = "cache/judgment.db";
const PATH_ENV: &str = "DIAL_JUDGE_CACHE_DB";

/// SQLite-backed judgment cache. Times are unix seconds, passed in so tests control the clock.
pub struct Cache {
    conn: Mutex<Connection>,
    ttl_secs: i64,
    max_rows: i64,
}

impl Cache {
    /// Open or create the cache at `path`. Rows older than `ttl_secs` are never returned.
    pub fn open(path: &Path, ttl_secs: u64, max_rows: u64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create judgment cache directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open judgment cache {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS judgments (
                key TEXT PRIMARY KEY,
                choice TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                accessed_at INTEGER NOT NULL
            );",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 0 {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version != SCHEMA_VERSION {
            anyhow::bail!(
                "judgment cache {} has schema {version}; this Dial reads {SCHEMA_VERSION}. Delete the file to rebuild it",
                path.display()
            );
        }
        Ok(Self {
            conn: Mutex::new(conn),
            ttl_secs: i64::try_from(ttl_secs).unwrap_or(i64::MAX),
            max_rows: i64::try_from(max_rows.max(1)).unwrap_or(i64::MAX),
        })
    }

    /// The cached choice for `key` if it was written within the time-to-live; refreshes its recency.
    pub fn get(&self, key: &str, now: i64) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let choice: Option<String> = conn
            .query_row(
                "SELECT choice FROM judgments WHERE key = ?1 AND created_at >= ?2",
                params![key, now.saturating_sub(self.ttl_secs)],
                |row| row.get(0),
            )
            .optional()?;
        if choice.is_some() {
            conn.execute(
                "UPDATE judgments SET accessed_at = ?2 WHERE key = ?1",
                params![key, now],
            )?;
        }
        Ok(choice)
    }

    /// Store `choice` for `key`, then evict the least recently used rows beyond the row bound.
    pub fn put(&self, key: &str, choice: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO judgments (key, choice, created_at, accessed_at) VALUES (?1, ?2, ?3, ?3)",
            params![key, choice, now],
        )?;
        conn.execute(
            "DELETE FROM judgments WHERE key IN (
                SELECT key FROM judgments ORDER BY accessed_at DESC LIMIT -1 OFFSET ?1
            )",
            params![self.max_rows],
        )?;
        Ok(())
    }

    /// Number of stored rows, expired ones included.
    pub fn len(&self) -> Result<u64> {
        let count: i64 =
            self.conn
                .lock()
                .query_row("SELECT COUNT(*) FROM judgments", [], |row| row.get(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Whether no rows are stored.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// Cache location: `DIAL_JUDGE_CACHE_DB` if set, else `<user_dir>/cache/judgment.db`.
pub fn default_path(user_dir: &Path) -> Option<PathBuf> {
    resolve_path(std::env::var_os(PATH_ENV), user_dir)
}

fn resolve_path(override_path: Option<OsString>, user_dir: &Path) -> Option<PathBuf> {
    match override_path {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ if user_dir.as_os_str().is_empty() => None,
        _ => Some(user_dir.join(DEFAULT_RELATIVE_PATH)),
    }
}

#[cfg(test)]
mod tests;
