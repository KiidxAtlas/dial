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
mod tests {
    use super::*;

    fn open(dir: &Path, ttl: u64, rows: u64) -> Cache {
        Cache::open(&dir.join("judgment.db"), ttl, rows).unwrap()
    }

    #[test]
    fn miss_then_hit_returns_stored_choice() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 100, 10);
        assert_eq!(cache.get("k", 1000).unwrap(), None);
        cache.put("k", "high", 1000).unwrap();
        assert_eq!(cache.get("k", 1001).unwrap().as_deref(), Some("high"));
    }

    #[test]
    fn entries_expire_at_the_ttl_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 100, 10);
        cache.put("k", "low", 1000).unwrap();
        // Written at 1000 with ttl 100: still valid at 1100, gone one second later.
        assert!(cache.get("k", 1100).unwrap().is_some());
        assert!(cache.get("k", 1101).unwrap().is_none());
    }

    #[test]
    fn reads_refresh_recency_so_the_least_recent_row_is_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 10_000, 2);
        cache.put("a", "1", 1).unwrap();
        cache.put("b", "2", 2).unwrap();
        // Touch "a" so "b" becomes the least recently used row.
        assert!(cache.get("a", 3).unwrap().is_some());
        cache.put("c", "3", 4).unwrap();
        assert_eq!(cache.len().unwrap(), 2);
        assert!(cache.get("a", 5).unwrap().is_some());
        assert!(cache.get("b", 5).unwrap().is_none());
        assert!(cache.get("c", 5).unwrap().is_some());
    }

    #[test]
    fn rewriting_a_key_replaces_its_choice() {
        let dir = tempfile::tempdir().unwrap();
        let cache = open(dir.path(), 100, 10);
        cache.put("k", "low", 1).unwrap();
        cache.put("k", "high", 2).unwrap();
        assert_eq!(cache.get("k", 3).unwrap().as_deref(), Some("high"));
        assert_eq!(cache.len().unwrap(), 1);
    }

    #[test]
    fn override_path_wins_and_empty_user_dir_disables_the_default() {
        let user = Path::new("/state/dial");
        assert_eq!(
            resolve_path(Some("/tmp/x.db".into()), user),
            Some(PathBuf::from("/tmp/x.db"))
        );
        assert_eq!(
            resolve_path(None, user),
            Some(user.join("cache/judgment.db"))
        );
        assert_eq!(
            resolve_path(Some(OsString::new()), user),
            Some(user.join("cache/judgment.db"))
        );
        assert_eq!(resolve_path(None, Path::new("")), None);
    }

    #[test]
    fn a_future_schema_version_is_refused_rather_than_misread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("judgment.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 99).unwrap();
        }
        let error = Cache::open(&path, 100, 10)
            .err()
            .expect("refused")
            .to_string();
        assert!(error.contains("schema 99"), "{error}");
    }
}
