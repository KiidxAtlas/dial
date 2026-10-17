//! Persistent cache of commit-generation model replies.
//!
//! Entries are keyed by a SHA-256 over the request material. Failed attempts are recorded
//! (capped to the newest rows) and token usage is logged. Every operation swallows storage
//! errors: the cache is an optimization and must never fail a commit.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// Bumped when row layout or key material changes; rows from other versions are ignored.
const SCHEMA_VERSION: i64 = 3;
/// Expired-row pruning runs on timestamps divisible by this value.
const PRUNE_DIVISOR: i64 = 64;
/// Failure rows retained after each insert.
const MAX_FAILURES: i64 = 64;
/// Database location relative to the Dial user directory.
const DEFAULT_RELATIVE_PATH: &str = "cache/commit-inference.db";
/// Environment override for the database path.
const PATH_ENV: &str = "DIAL_COMMIT_CACHE_DB";
/// How long an open, read, or write waits for another process's lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS responses (
    key TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL,
    model TEXT NOT NULL,
    operation TEXT NOT NULL,
    request TEXT NOT NULL,
    response TEXT NOT NULL,
    stop_reason TEXT NOT NULL,
    cost_usd REAL,
    created_at INTEGER NOT NULL,
    accessed_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS responses_created_at ON responses(created_at);
CREATE TABLE IF NOT EXISTS failures (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    schema_version INTEGER NOT NULL,
    key TEXT NOT NULL,
    model TEXT NOT NULL,
    operation TEXT NOT NULL,
    request TEXT NOT NULL,
    response TEXT NOT NULL,
    error TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS failures_created_at ON failures(created_at);
CREATE TABLE IF NOT EXISTS usage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    model TEXT NOT NULL,
    operation TEXT NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    cost_usd REAL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_created_at ON usage(created_at);
";

/// Request material that determines a cache key.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CacheKeyMaterial<'a> {
    pub operation: &'a str,
    pub model: &'a str,
    pub api_mode: &'a str,
    pub tool_name: &'a str,
    pub system_prompt: &'a str,
    pub user_prompt: &'a str,
    /// Empty or absent values are omitted from the key, matching requests without reasoning.
    pub reasoning_effort: Option<&'a str>,
}

/// Stable SHA-256 key over the request material.
pub(crate) fn key_for(material: &CacheKeyMaterial<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"llm-cache/v1\n");
    let fields = [
        ("operation", material.operation),
        ("model", material.model),
        ("api_mode", material.api_mode),
        ("tool_name", material.tool_name),
        ("system", material.system_prompt),
        ("user", material.user_prompt),
    ];
    for (name, value) in fields {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    }
    if let Some(effort) = material
        .reasoning_effort
        .filter(|effort| !effort.is_empty())
    {
        hasher.update(b"reasoning_effort\0");
        hasher.update(effort.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"\n");
    format!("{:x}", hasher.finalize())
}

/// A stored model reply: the raw text and the provider's finish reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedResponse {
    pub text: String,
    pub finish_reason: String,
}

/// A successful reply to store under `key`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CacheEntry<'a> {
    pub key: &'a str,
    pub model: &'a str,
    pub operation: &'a str,
    pub request: &'a str,
    pub text: &'a str,
    pub finish_reason: &'a str,
    pub cost_usd: Option<f64>,
}

/// A failed attempt. `response` is the reply text when one arrived, else empty.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FailureEntry<'a> {
    pub key: &'a str,
    pub model: &'a str,
    pub operation: &'a str,
    pub request: &'a str,
    pub response: &'a str,
    pub error: &'a str,
}

/// Token counts from one successful call. `None` means the provider did not report the value.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UsageRecord<'a> {
    pub model: &'a str,
    pub operation: &'a str,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// SQLite-backed reply cache with TTL expiry.
pub(crate) struct ConventionalCache {
    conn: Mutex<Connection>,
    ttl_seconds: i64,
}

impl ConventionalCache {
    /// Open or create the database at `path`. Returns `None` when the file cannot be opened,
    /// so generation proceeds without a cache.
    pub(crate) fn open(path: &Path, ttl_days: f64) -> Option<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok()?;
        }
        let conn = Connection::open(path).ok()?;
        conn.busy_timeout(BUSY_TIMEOUT).ok()?;
        conn.execute_batch(SCHEMA).ok()?;
        let ttl_seconds = (ttl_days * 86_400.0).trunc().max(0.0) as i64;
        Some(Self {
            conn: Mutex::new(conn),
            ttl_seconds,
        })
    }

    /// Return the cached reply for `key`, or `None` on a miss, an expired row, or any error.
    pub(crate) fn get(&self, key: &str) -> Option<CachedResponse> {
        let conn = self.conn.lock();
        let now = now_seconds();
        let row: Option<(i64, String, String)> = conn
            .query_row(
                "SELECT created_at, response, stop_reason FROM responses WHERE key = ?1 AND schema_version = ?2",
                params![key, SCHEMA_VERSION],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .ok()?;
        let (created_at, text, finish_reason) = row?;
        if self.is_expired(created_at, now) {
            let _ = conn.execute("DELETE FROM responses WHERE key = ?1", params![key]);
            return None;
        }
        let _ = conn.execute(
            "UPDATE responses SET accessed_at = ?1 WHERE key = ?2",
            params![now, key],
        );
        Some(CachedResponse {
            text,
            finish_reason,
        })
    }

    /// Store a successful reply, replacing any row with the same key.
    pub(crate) fn put(&self, entry: &CacheEntry<'_>) {
        let conn = self.conn.lock();
        let now = now_seconds();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO responses \
             (key, schema_version, model, operation, request, response, stop_reason, cost_usd, created_at, accessed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                entry.key,
                SCHEMA_VERSION,
                entry.model,
                entry.operation,
                entry.request,
                entry.text,
                entry.finish_reason,
                entry.cost_usd,
                now,
            ],
        );
        if self.ttl_seconds > 0 && now % PRUNE_DIVISOR == 0 {
            let _ = conn.execute(
                "DELETE FROM responses WHERE created_at < ?1",
                params![now - self.ttl_seconds],
            );
        }
    }

    /// Record a failed attempt, expire old failures, and keep only the newest `MAX_FAILURES`.
    pub(crate) fn put_failure(&self, entry: &FailureEntry<'_>) {
        let conn = self.conn.lock();
        let now = now_seconds();
        let _ = conn.execute(
            "INSERT INTO failures (schema_version, key, model, operation, request, response, error, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                SCHEMA_VERSION,
                entry.key,
                entry.model,
                entry.operation,
                entry.request,
                entry.response,
                entry.error,
                now,
            ],
        );
        if self.ttl_seconds > 0 {
            let _ = conn.execute(
                "DELETE FROM failures WHERE created_at < ?1",
                params![now - self.ttl_seconds],
            );
        }
        let _ = conn.execute(
            "DELETE FROM failures WHERE id NOT IN (SELECT id FROM failures ORDER BY id DESC LIMIT ?1)",
            params![MAX_FAILURES],
        );
    }

    /// Log token usage for a successful call.
    pub(crate) fn record_usage(&self, usage: &UsageRecord<'_>) {
        let conn = self.conn.lock();
        let _ = conn.execute(
            "INSERT INTO usage (model, operation, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
             cost_usd, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                usage.model,
                usage.operation,
                to_sql_count(usage.input_tokens),
                to_sql_count(usage.output_tokens),
                to_sql_count(usage.cache_read_tokens),
                to_sql_count(usage.cache_write_tokens),
                usage.cost_usd,
                now_seconds(),
            ],
        );
    }

    fn is_expired(&self, created_at: i64, now: i64) -> bool {
        self.ttl_seconds > 0 && created_at < now - self.ttl_seconds
    }
}

/// Resolve the cache database path: `PATH_ENV` when set and non-empty, else
/// `<user_dir>/cache/commit-inference.db`. `None` when there is no absolute location to use.
pub(crate) fn default_path(user_dir: &Path) -> Option<PathBuf> {
    resolve_path(std::env::var_os(PATH_ENV), user_dir)
}

fn resolve_path(override_path: Option<OsString>, user_dir: &Path) -> Option<PathBuf> {
    match override_path {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ if user_dir.as_os_str().is_empty() => None,
        _ => Some(user_dir.join(DEFAULT_RELATIVE_PATH)),
    }
}

fn to_sql_count(value: Option<u64>) -> Option<i64> {
    value.and_then(|count| i64::try_from(count).ok())
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
