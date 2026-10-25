//! Cross-session usage statistics.
//!
//! Two sources feed one private SQLite database at `<user_dir>/stats.sqlite3`:
//!
//! * runtime events from `crate::telemetry` — one row per model request (tokens, cache, cost,
//!   latency, finish reason) and per tool call (latency, error), written by the telemetry thread;
//! * every project's session transcripts (`<user_dir>/projects/*/sessions/*.jsonl`), synced
//!   incrementally by size/mtime for session counts, message counts, activity time and
//!   historical tool-call counts. Message bodies are discarded after counting.
//!
//! Raw request/tool rows are rolled up per hour, project and model (or tool) in the same
//! transaction that records them, so range queries read only the rollups and stay exact.
use crate::config::stats::StatsConfig;

pub mod local_models;
pub mod server;

use crate::platform;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
/// Transcript lines longer than this are skipped while counting (they are tool payloads).
const MAX_LINE: usize = 8 * 1024 * 1024;

pub fn db_path(user_dir: &Path) -> PathBuf {
    user_dir.join("stats.sqlite3")
}

#[derive(Debug, Clone, Default)]
pub struct RequestRecord {
    pub ts_ms: i64,
    pub session: String,
    pub project: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    /// The provider reported both input and output counts.
    pub reported: bool,
    pub cost_usd: Option<f64>,
    pub duration_ms: u64,
    pub finish_reason: String,
    pub error: bool,
    pub wire: Option<WireRecord>,
}

/// Cache diagnostics for one request: `invalidation` is `none`/`transport_changed` when the
/// prompt extended the previous one, so a low `cache_read` there is provider-side.
#[derive(Debug, Clone, Default)]
pub struct WireRecord {
    /// `http`, `websocket` or `websocket_chained` (delta over `previous_response_id`).
    pub transport: String,
    /// `initial`, `none`, `prompt_changed`, `model_changed` or `transport_changed`.
    pub invalidation: String,
    /// Serialized prompt bytes shared with the previous request of the same backend.
    pub common_prompt_bytes: u64,
    pub prompt_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ToolRecord {
    pub facts: crate::telemetry::ExecutionFacts,
    pub status: Option<crate::tools::ToolStatus>,
    pub ts_ms: i64,
    pub session: String,
    pub project: String,
    pub tool: String,
    pub duration_ms: u64,
    pub error: bool,
}

// ---------------------------------------------------------------- report types

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Totals {
    pub requests: u64,
    pub errors: u64,
    pub reported: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    /// Sum over priced requests only; see `priced_requests`.
    pub cost_usd: f64,
    pub priced_requests: u64,
    pub duration_ms: u64,
    /// `output / (duration / 1000)`.
    pub tokens_per_second: Option<f64>,
    /// `cache_read / input * 100` (Dial's input counts include cached prompt tokens).
    pub cache_rate: Option<f64>,
    /// `errors / requests * 100`.
    pub error_rate: Option<f64>,
    pub avg_latency_ms: Option<f64>,
}

impl Totals {
    fn finish(mut self) -> Self {
        let ratio = |num: f64, den: f64| (den > 0.0).then(|| num / den);
        self.tokens_per_second = ratio(self.output as f64, self.duration_ms as f64 / 1000.0);
        self.cache_rate = ratio(self.cache_read as f64 * 100.0, self.input as f64);
        self.error_rate = ratio(self.errors as f64 * 100.0, self.requests as f64);
        self.avg_latency_ms = ratio(self.duration_ms as f64, self.requests as f64);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Group {
    pub key: String,
    pub label: String,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolGroup {
    pub tool: String,
    pub calls: u64,
    pub errors: u64,
    pub duration_ms: u64,
    pub avg_ms: Option<f64>,
    pub error_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Day {
    pub day: String,
    pub start_ms: i64,
    #[serde(flatten)]
    pub totals: Totals,
    pub tool_calls: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SessionTotals {
    pub sessions: u64,
    pub projects: u64,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_calls: u64,
    /// Sum of first-to-last entry spans.
    pub active_ms: u64,
    pub avg_session_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NamedCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub generated_ms: i64,
    pub since_ms: Option<i64>,
    pub overall: Totals,
    pub by_model: Vec<Group>,
    pub by_project: Vec<Group>,
    pub by_tool: Vec<ToolGroup>,
    pub tool_outcomes: Vec<NamedCount>,
    pub daily: Vec<Day>,
    pub sessions: SessionTotals,
    /// Tool calls counted from transcripts (includes history from before runtime recording).
    pub transcript_tools: Vec<NamedCount>,
    pub transcript_models: Vec<NamedCount>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SyncReport {
    pub files: u64,
    pub updated: u64,
    pub removed: u64,
}

/// Decode speed of one model from recorded requests: output tokens per second of request time, the
/// same measure as `Totals::tokens_per_second`. `None` is unknown; it is never estimated.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DecodeSpeed {
    /// `None` when the stats database could not be read; `Some(0)` when no request matched.
    pub samples: Option<u64>,
    /// Median of the newest `SAMPLE_LIMIT` successful requests.
    pub median_tok_per_s: Option<f64>,
    /// Median of the newest `TREND_WINDOW` of those samples.
    pub recent_median_tok_per_s: Option<f64>,
    /// Median of the `TREND_WINDOW` samples before those; needs at least three of them.
    pub previous_median_tok_per_s: Option<f64>,
    /// `up` or `down` when recent differs from previous by more than 10%, `flat` otherwise.
    pub trend: Option<&'static str>,
    /// The samples behind the medians, oldest first.
    pub history: Vec<f64>,
}

/// Newest successful requests read per model for speed summaries.
pub const SAMPLE_LIMIT: usize = 40;
const TREND_WINDOW: usize = 20;

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    })
}

/// Summarizes tokens-per-second samples given oldest first.
pub fn speed_summary(rates: &[f64]) -> DecodeSpeed {
    let n = rates.len();
    let recent = &rates[n.saturating_sub(TREND_WINDOW)..];
    let previous =
        (n > TREND_WINDOW).then(|| &rates[n.saturating_sub(2 * TREND_WINDOW)..n - TREND_WINDOW]);
    let recent_median = median(recent);
    let previous_median = previous.filter(|p| p.len() >= 3).and_then(median);
    let trend = match (recent_median, previous_median) {
        (Some(recent), Some(previous)) if previous > 0.0 => Some(if recent > previous * 1.1 {
            "up"
        } else if recent < previous * 0.9 {
            "down"
        } else {
            "flat"
        }),
        _ => None,
    };
    DecodeSpeed {
        samples: Some(n as u64),
        median_tok_per_s: median(rates),
        recent_median_tok_per_s: recent_median,
        previous_median_tok_per_s: previous_median,
        trend,
        history: rates.to_vec(),
    }
}

/// Recorded use of one model over all time: request totals and decode speed. Both stay empty
/// (unknown) for a model without matching requests; nothing is estimated.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ModelUsage {
    pub totals: Totals,
    pub speed: DecodeSpeed,
}

impl ModelUsage {
    /// Mean recorded cost of a priced request, when any request was priced.
    pub fn avg_cost_usd(&self) -> Option<f64> {
        (self.totals.priced_requests > 0)
            .then(|| self.totals.cost_usd / self.totals.priced_requests as f64)
    }
}

/// Every model's recorded usage, keyed by the model name stats record (the backend's model label).
/// Empty when the stats database is missing or unreadable; the database is opened read-only.
pub fn read_model_usage(user_dir: &Path) -> BTreeMap<String, ModelUsage> {
    Db::open_read_only(&db_path(user_dir))
        .and_then(|db| db.model_usage())
        .unwrap_or_default()
}

const TOTALS_SQL: &str =
    "SUM(requests), SUM(errors), SUM(reported), SUM(input), SUM(output), SUM(cache_read),
    SUM(cache_write), SUM(reasoning), SUM(cost), SUM(priced), SUM(duration_ms)";

/// Reads the [`TOTALS_SQL`] columns starting at column `at`.
fn read_totals(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Totals> {
    let n = |i: usize| -> rusqlite::Result<u64> {
        Ok(r.get::<_, Option<i64>>(at + i)?.unwrap_or(0).max(0) as u64)
    };
    Ok(Totals {
        requests: n(0)?,
        errors: n(1)?,
        reported: n(2)?,
        input: n(3)?,
        output: n(4)?,
        cache_read: n(5)?,
        cache_write: n(6)?,
        reasoning: n(7)?,
        cost_usd: r.get::<_, Option<f64>>(at + 8)?.unwrap_or(0.0),
        priced_requests: n(9)?,
        duration_ms: n(10)?,
        ..Default::default()
    }
    .finish())
}

// ---------------------------------------------------------------- database
// ---------------------------------------------------------------- database

pub struct Db {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests(id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, session TEXT NOT NULL,
  project TEXT NOT NULL, model TEXT NOT NULL, input INTEGER NOT NULL, output INTEGER NOT NULL,
  cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL, reasoning INTEGER NOT NULL,
  reported INTEGER NOT NULL, cost REAL, duration_ms INTEGER NOT NULL, finish TEXT NOT NULL, error INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS requests_ts ON requests(ts);
CREATE TABLE IF NOT EXISTS tool_calls(id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, session TEXT NOT NULL,
  project TEXT NOT NULL, tool TEXT NOT NULL, duration_ms INTEGER NOT NULL, error INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS tool_outcomes(call_id INTEGER PRIMARY KEY, status TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tool_facts(call_id INTEGER PRIMARY KEY, facts TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS tool_calls_ts ON tool_calls(ts);
CREATE TABLE IF NOT EXISTS request_hourly(hour INTEGER NOT NULL, project TEXT NOT NULL, model TEXT NOT NULL,
  requests INTEGER NOT NULL, errors INTEGER NOT NULL, reported INTEGER NOT NULL, input INTEGER NOT NULL,
  output INTEGER NOT NULL, cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL, reasoning INTEGER NOT NULL,
  cost REAL NOT NULL, priced INTEGER NOT NULL, duration_ms INTEGER NOT NULL,
  PRIMARY KEY(hour, project, model)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS tool_hourly(hour INTEGER NOT NULL, project TEXT NOT NULL, tool TEXT NOT NULL,
  calls INTEGER NOT NULL, errors INTEGER NOT NULL, duration_ms INTEGER NOT NULL,
  PRIMARY KEY(hour, project, tool)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS sessions(path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
  session TEXT NOT NULL, project TEXT NOT NULL, cwd TEXT NOT NULL, model TEXT NOT NULL,
  started INTEGER NOT NULL, last INTEGER NOT NULL, user_msgs INTEGER NOT NULL,
  assistant_msgs INTEGER NOT NULL, tool_calls INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS sessions_last ON sessions(last);
CREATE TABLE IF NOT EXISTS session_tools(path TEXT NOT NULL, tool TEXT NOT NULL, calls INTEGER NOT NULL,
  PRIMARY KEY(path, tool)) WITHOUT ROWID;
";

fn private_dir(dir: &Path) -> Result<()> {
    platform::fs::create_dir_owner_only(fs::DirBuilder::new().recursive(true), dir)
        .with_context(|| format!("creating private directory {}", dir.display()))
}

/// Columns added after the first release; NULL on rows recorded before them.
const REQUEST_COLUMNS: [(&str, &str); 4] = [
    ("transport", "TEXT"),
    ("invalidation", "TEXT"),
    ("common_prompt_bytes", "INTEGER"),
    ("prompt_bytes", "INTEGER"),
];

/// Idempotent and safe against a concurrent opener: the write lock is held across check and alter.
fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let existing = conn
            .prepare("SELECT name FROM pragma_table_info('requests')")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        for (name, kind) in REQUEST_COLUMNS {
            if !existing.contains(name) {
                conn.execute_batch(&format!("ALTER TABLE requests ADD COLUMN {name} {kind}"))?;
            }
        }
        Ok(())
    })();
    conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
    result
}

fn sql_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            private_dir(parent)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        platform::fs::set_owner_only(path)
            .with_context(|| format!("restricting {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)
            .context("creating the stats schema")?;
        migrate(&conn).context("migrating the stats schema")?;
        Ok(Self { conn })
    }

    pub fn open_default(user_dir: &Path) -> Result<Self> {
        Self::open(&db_path(user_dir))
    }

    /// Opens an existing database read-only: it is never created, migrated or synced.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(1))?;
        Ok(Self { conn })
    }

    /// Every model name with recorded requests.
    pub fn model_names(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT model FROM request_hourly ORDER BY model")?;
        Ok(stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Output tokens per second of the newest successful requests to any of `models`, oldest first.
    /// Failed requests and requests without output or duration are not speed evidence.
    pub fn decode_speed(&self, models: &[&str]) -> Result<DecodeSpeed> {
        if models.is_empty() {
            return Ok(speed_summary(&[]));
        }
        let marks = vec!["?"; models.len()].join(",");
        let sql = format!(
            "SELECT output, duration_ms FROM (SELECT output, duration_ms, ts, id FROM requests
               WHERE model IN ({marks}) AND error=0 AND output>0 AND duration_ms>0
               ORDER BY ts DESC, id DESC LIMIT {SAMPLE_LIMIT}) ORDER BY ts, id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(models.iter().copied()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut rates = Vec::new();
        for row in rows {
            let (output, duration_ms) = row?;
            rates.push(output as f64 / (duration_ms as f64 / 1000.0));
        }
        Ok(speed_summary(&rates))
    }

    /// All-time totals and decode speed of every model with recorded requests.
    pub fn model_usage(&self) -> Result<BTreeMap<String, ModelUsage>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT model, {TOTALS_SQL} FROM request_hourly GROUP BY model"
        ))?;
        let totals: Vec<(String, Totals)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, read_totals(r, 1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        totals
            .into_iter()
            .map(|(model, totals)| {
                let speed = self.decode_speed(&[model.as_str()])?;
                Ok((model, ModelUsage { totals, speed }))
            })
            .collect()
    }

    pub fn record_request(&mut self, r: &RequestRecord) -> Result<()> {
        let tx = self.conn.transaction()?;
        let cost = r.cost_usd.filter(|c| c.is_finite() && *c >= 0.0);
        tx.execute(
            "INSERT INTO requests(ts, session, project, model, input, output, cache_read, cache_write, reasoning, reported, cost, duration_ms, finish, error,
               transport, invalidation, common_prompt_bytes, prompt_bytes)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![r.ts_ms, r.session, r.project, r.model, sql_u64(r.input), sql_u64(r.output),
                sql_u64(r.cache_read), sql_u64(r.cache_write), sql_u64(r.reasoning), r.reported,
                cost, sql_u64(r.duration_ms), r.finish_reason, r.error,
                r.wire.as_ref().map(|w| &w.transport), r.wire.as_ref().map(|w| &w.invalidation),
                r.wire.as_ref().map(|w| sql_u64(w.common_prompt_bytes)),
                r.wire.as_ref().map(|w| sql_u64(w.prompt_bytes))],
        )?;
        tx.execute(
            "INSERT INTO request_hourly VALUES(?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(hour, project, model) DO UPDATE SET requests = requests + 1,
               errors = errors + excluded.errors, reported = reported + excluded.reported,
               input = input + excluded.input, output = output + excluded.output,
               cache_read = cache_read + excluded.cache_read, cache_write = cache_write + excluded.cache_write,
               reasoning = reasoning + excluded.reasoning, cost = cost + excluded.cost,
               priced = priced + excluded.priced, duration_ms = duration_ms + excluded.duration_ms",
            params![r.ts_ms.div_euclid(HOUR_MS), r.project, r.model, r.error as i64, r.reported as i64,
                sql_u64(r.input), sql_u64(r.output), sql_u64(r.cache_read), sql_u64(r.cache_write),
                sql_u64(r.reasoning), cost.unwrap_or(0.0), cost.is_some() as i64, sql_u64(r.duration_ms)],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_tool(&mut self, r: &ToolRecord) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO tool_calls(ts, session, project, tool, duration_ms, error) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![r.ts_ms, r.session, r.project, r.tool, sql_u64(r.duration_ms), r.error],
        )?;
        let call_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO tool_facts(call_id, facts) VALUES(?1, ?2)",
            params![call_id, serde_json::to_string(&r.facts)?],
        )?;
        let status = r.status.unwrap_or(if r.error {
            crate::tools::ToolStatus::Error
        } else {
            crate::tools::ToolStatus::Ok
        });
        tx.execute(
            "INSERT INTO tool_outcomes(call_id, status) VALUES(?1, ?2)",
            params![call_id, status.as_str()],
        )?;
        tx.execute(
            "INSERT INTO tool_hourly VALUES(?1, ?2, ?3, 1, ?4, ?5)
             ON CONFLICT(hour, project, tool) DO UPDATE SET calls = calls + 1,
               errors = errors + excluded.errors, duration_ms = duration_ms + excluded.duration_ms",
            params![
                r.ts_ms.div_euclid(HOUR_MS),
                r.project,
                r.tool,
                r.error as i64,
                sql_u64(r.duration_ms)
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Bring transcript-derived rows up to date with every project's session files.
    pub fn sync_sessions(&mut self, user_dir: &Path) -> Result<SyncReport> {
        let mut report = SyncReport::default();
        let known: HashMap<String, (i64, i64)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path, size, mtime FROM sessions")?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut present = HashSet::new();
        let projects = user_dir.join("projects");
        let mut files = Vec::new();
        if let Ok(entries) = fs::read_dir(&projects) {
            for project in entries.flatten() {
                if !project.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let Ok(sessions) = fs::read_dir(project.path().join("sessions")) else {
                    continue;
                };
                for file in sessions.flatten() {
                    let name = file.file_name();
                    let name = name.to_string_lossy();
                    if file.file_type().is_ok_and(|t| t.is_file()) && name.ends_with(".jsonl") {
                        files.push((
                            project.file_name().to_string_lossy().into_owned(),
                            file.path(),
                        ));
                    }
                }
            }
        }
        for (project, path) in files {
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let key = path.to_string_lossy().into_owned();
            present.insert(key.clone());
            report.files += 1;
            let modified = meta.modified().unwrap_or(UNIX_EPOCH);
            let mtime = modified
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let stamp = (meta.len() as i64, i64::try_from(mtime).unwrap_or(i64::MAX));
            if known.get(&key) == Some(&stamp) {
                continue;
            }
            let Ok(summary) = summarize_transcript(&path) else {
                continue;
            };
            let tx = self.conn.transaction()?;
            tx.execute("DELETE FROM session_tools WHERE path = ?1", [&key])?;
            tx.execute(
                "INSERT OR REPLACE INTO sessions VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![key, stamp.0, stamp.1, summary.session, project, summary.cwd, summary.model,
                    summary.started_ms, summary.last_ms, summary.user_msgs, summary.assistant_msgs,
                    summary.tool_calls],
            )?;
            for (tool, calls) in &summary.tools {
                tx.execute(
                    "INSERT INTO session_tools VALUES(?1, ?2, ?3)",
                    params![key, tool, calls],
                )?;
            }
            tx.commit()?;
            report.updated += 1;
        }
        for gone in known.keys().filter(|k| !present.contains(*k)) {
            let tx = self.conn.transaction()?;
            tx.execute("DELETE FROM sessions WHERE path = ?1", [gone])?;
            tx.execute("DELETE FROM session_tools WHERE path = ?1", [gone])?;
            tx.commit()?;
            report.removed += 1;
        }
        Ok(report)
    }

    /// Aggregate everything since `since_ms` (rounded down to the hour; None = all time).
    pub fn report(&self, since_ms: Option<i64>, now_ms: i64) -> Result<Report> {
        let hour = since_ms.map_or(i64::MIN, |s| s.div_euclid(HOUR_MS));
        let since = since_ms.unwrap_or(i64::MIN);
        let labels = self.project_labels()?;
        let label = |key: &str| labels.get(key).cloned().unwrap_or_else(|| key.to_owned());
        let totals_sql = TOTALS_SQL;
        let overall = self.conn.query_row(
            &format!("SELECT {totals_sql} FROM request_hourly WHERE hour >= ?1"),
            [hour],
            |r| read_totals(r, 0),
        )?;
        let grouped = |column: &str| -> Result<Vec<(String, Totals)>> {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT {column}, {totals_sql} FROM request_hourly WHERE hour >= ?1 GROUP BY {column}
                 ORDER BY SUM(input) + SUM(output) DESC, SUM(requests) DESC"
            ))?;
            Ok(stmt
                .query_map([hour], |r| Ok((r.get::<_, String>(0)?, read_totals(r, 1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        };
        let by_model = grouped("model")?
            .into_iter()
            .map(|(key, totals)| Group {
                label: key.clone(),
                key,
                totals,
            })
            .collect();
        let by_project = grouped("project")?
            .into_iter()
            .map(|(key, totals)| Group {
                label: label(&key),
                key,
                totals,
            })
            .collect();
        let by_tool = {
            let mut stmt = self.conn.prepare(
                "SELECT tool, SUM(calls), SUM(errors), SUM(duration_ms) FROM tool_hourly WHERE hour >= ?1
                 GROUP BY tool ORDER BY SUM(calls) DESC, tool",
            )?;
            stmt.query_map([hour], |r| {
                let calls = r.get::<_, i64>(1)?.max(0) as u64;
                let errors = r.get::<_, i64>(2)?.max(0) as u64;
                let duration_ms = r.get::<_, i64>(3)?.max(0) as u64;
                Ok(ToolGroup {
                    tool: r.get(0)?,
                    calls,
                    errors,
                    duration_ms,
                    avg_ms: (calls > 0).then(|| duration_ms as f64 / calls as f64),
                    error_rate: (calls > 0).then(|| errors as f64 * 100.0 / calls as f64),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let tool_outcomes = {
            let mut stmt = self.conn.prepare("SELECT COALESCE(o.status, CASE WHEN c.error THEN 'error' ELSE 'ok' END) AS status, COUNT(*) FROM tool_calls c LEFT JOIN tool_outcomes o ON o.call_id=c.id WHERE c.ts >= ?1 GROUP BY status ORDER BY status")?;
            stmt.query_map([hour.saturating_mul(HOUR_MS)], |r| {
                Ok(NamedCount {
                    name: r.get(0)?,
                    count: r.get::<_, i64>(1)?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let daily = {
            let mut tools_by_day: HashMap<i64, u64> = HashMap::new();
            let mut stmt = self.conn.prepare(
                "SELECT hour / 24 AS day, SUM(calls) FROM tool_hourly WHERE hour >= ?1 GROUP BY day",
            )?;
            for row in stmt.query_map([hour], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
                let (day, calls) = row?;
                tools_by_day.insert(day, calls.max(0) as u64);
            }
            let mut stmt = self.conn.prepare(&format!(
                "SELECT hour / 24 AS day, {totals_sql} FROM request_hourly WHERE hour >= ?1 GROUP BY day ORDER BY day"
            ))?;
            let mut days: BTreeMap<i64, Day> = BTreeMap::new();
            for row in stmt.query_map([hour], |r| Ok((r.get::<_, i64>(0)?, read_totals(r, 1)?)))? {
                let (day, totals) = row?;
                days.insert(
                    day,
                    Day {
                        day: civil_day(day * DAY_MS),
                        start_ms: day * DAY_MS,
                        totals,
                        tool_calls: 0,
                    },
                );
            }
            for (day, calls) in tools_by_day {
                days.entry(day)
                    .or_insert_with(|| Day {
                        day: civil_day(day * DAY_MS),
                        start_ms: day * DAY_MS,
                        totals: Totals::default(),
                        tool_calls: 0,
                    })
                    .tool_calls = calls;
            }
            days.into_values().collect()
        };
        let sessions = self.conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT project), SUM(user_msgs), SUM(assistant_msgs), SUM(tool_calls),
                SUM(MAX(last - started, 0)) FROM sessions WHERE last >= ?1",
            [since],
            |r| {
                let n = |i: usize| -> rusqlite::Result<u64> { Ok(r.get::<_, Option<i64>>(i)?.unwrap_or(0).max(0) as u64) };
                let sessions = n(0)?;
                let active_ms = n(5)?;
                Ok(SessionTotals {
                    sessions,
                    projects: n(1)?,
                    user_messages: n(2)?,
                    assistant_messages: n(3)?,
                    tool_calls: n(4)?,
                    active_ms,
                    avg_session_ms: (sessions > 0).then(|| active_ms as f64 / sessions as f64),
                })
            },
        )?;
        let named = |sql: &str| -> Result<Vec<NamedCount>> {
            let mut stmt = self.conn.prepare(sql)?;
            Ok(stmt
                .query_map([since], |r| {
                    Ok(NamedCount {
                        name: r.get(0)?,
                        count: r.get::<_, i64>(1)?.max(0) as u64,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        };
        let transcript_tools = named(
            "SELECT t.tool, SUM(t.calls) FROM session_tools t JOIN sessions s ON s.path = t.path
             WHERE s.last >= ?1 GROUP BY t.tool ORDER BY SUM(t.calls) DESC, t.tool",
        )?;
        let transcript_models = named(
            "SELECT model, COUNT(*) FROM sessions WHERE last >= ?1 AND model != '' GROUP BY model ORDER BY COUNT(*) DESC, model",
        )?;
        Ok(Report {
            generated_ms: now_ms,
            since_ms,
            overall,
            by_model,
            by_project,
            by_tool,
            tool_outcomes,
            daily,
            sessions,
            transcript_tools,
            transcript_models,
        })
    }

    /// Project directory name -> working directory recorded in its session headers (`~` for home).
    fn project_labels(&self) -> Result<HashMap<String, String>> {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut stmt = self
            .conn
            .prepare("SELECT project, MAX(cwd) FROM sessions WHERE cwd != '' GROUP BY project")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (project, cwd) = row?;
            let cwd = match cwd.strip_prefix(&home) {
                Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
                    format!("~{rest}")
                }
                _ => cwd,
            };
            out.insert(project, cwd);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- session traces

/// One model request recorded for a session (`/api/trace`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceRequest {
    pub ts_ms: i64,
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
}

/// One runtime tool call recorded for a session (`/api/trace`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceTool {
    pub status: String,
    pub ts_ms: i64,
    pub tool: String,
    pub duration_ms: u64,
    pub error: bool,
}

/// Everything the stats database knows about one session transcript.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trace {
    pub session_file: String,
    pub session: String,
    pub project: String,
    pub cwd: String,
    pub model: String,
    pub started_ms: i64,
    pub last_ms: i64,
    pub user_messages: i64,
    pub assistant_messages: i64,
    pub transcript_tool_calls: i64,
    pub totals: Totals,
    pub requests: Vec<TraceRequest>,
    pub tools: Vec<TraceTool>,
    /// Tool calls counted from the transcript (includes history from before runtime recording).
    pub transcript_tools: Vec<NamedCount>,
}

impl Db {
    /// Trace of one session whose file is `file` (already validated by [`session_file_in_tree`]).
    /// Requests and tool calls are matched by the session id recorded in the transcript header.
    pub fn trace(&self, user_dir: &Path, project: &str, file: &Path) -> Result<Option<Trace>> {
        use rusqlite::OptionalExtension;
        let listed = user_dir
            .join("projects")
            .join(project)
            .join("sessions")
            .join(file.file_name().unwrap_or_default());
        let keys = [
            listed.to_string_lossy().into_owned(),
            file.to_string_lossy().into_owned(),
        ];
        let row = self
            .conn
            .query_row(
                "SELECT path, session, cwd, model, started, last, user_msgs, assistant_msgs, tool_calls
                 FROM sessions WHERE path IN (?1, ?2) LIMIT 1",
                params![keys[0], keys[1]],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, i64>(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            path,
            session,
            cwd,
            model,
            started_ms,
            last_ms,
            user_messages,
            assistant_messages,
            transcript_tool_calls,
        )) = row
        else {
            return Ok(None);
        };

        let mut totals = Totals::default();
        let mut requests = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT ts, model, input, output, cache_read, cache_write, reasoning, reported, cost, duration_ms, finish, error
             FROM requests WHERE session = ?1 ORDER BY ts, id",
        )?;
        let rows = stmt.query_map([&session], |r| {
            Ok(TraceRequest {
                ts_ms: r.get(0)?,
                model: r.get(1)?,
                input: r.get::<_, i64>(2)?.max(0) as u64,
                output: r.get::<_, i64>(3)?.max(0) as u64,
                cache_read: r.get::<_, i64>(4)?.max(0) as u64,
                cache_write: r.get::<_, i64>(5)?.max(0) as u64,
                reasoning: r.get::<_, i64>(6)?.max(0) as u64,
                reported: r.get(7)?,
                cost_usd: r
                    .get::<_, Option<f64>>(8)?
                    .filter(|c| c.is_finite() && *c >= 0.0),
                duration_ms: r.get::<_, i64>(9)?.max(0) as u64,
                finish_reason: r.get(10)?,
                error: r.get(11)?,
            })
        })?;
        for request in rows {
            let request = request?;
            totals.requests += 1;
            totals.errors += u64::from(request.error);
            totals.reported += u64::from(request.reported);
            totals.input += request.input;
            totals.output += request.output;
            totals.cache_read += request.cache_read;
            totals.cache_write += request.cache_write;
            totals.reasoning += request.reasoning;
            totals.duration_ms += request.duration_ms;
            if let Some(cost) = request.cost_usd {
                totals.cost_usd += cost;
                totals.priced_requests += 1;
            }
            requests.push(request);
        }
        drop(stmt);

        let mut tools = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT c.ts, c.tool, c.duration_ms, c.error, COALESCE(o.status, CASE WHEN c.error THEN 'error' ELSE 'ok' END) FROM tool_calls c LEFT JOIN tool_outcomes o ON o.call_id=c.id WHERE c.session = ?1 ORDER BY c.ts, c.id",
        )?;
        let rows = stmt.query_map([&session], |r| {
            Ok(TraceTool {
                status: r.get(4)?,
                ts_ms: r.get(0)?,
                tool: r.get(1)?,
                duration_ms: r.get::<_, i64>(2)?.max(0) as u64,
                error: r.get(3)?,
            })
        })?;
        for tool in rows {
            tools.push(tool?);
        }
        drop(stmt);

        let mut stmt = self.conn.prepare(
            "SELECT tool, calls FROM session_tools WHERE path = ?1 ORDER BY calls DESC, tool",
        )?;
        let transcript_tools = stmt
            .query_map([&path], |r| {
                Ok(NamedCount {
                    name: r.get(0)?,
                    count: r.get::<_, i64>(1)?.max(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Some(Trace {
            session_file: file.to_string_lossy().into_owned(),
            session,
            project: project.to_owned(),
            cwd,
            model,
            started_ms,
            last_ms,
            user_messages,
            assistant_messages,
            transcript_tool_calls,
            totals: totals.finish(),
            requests,
            tools,
            transcript_tools,
        }))
    }
}

/// Resolves `session_file` through symlinks and accepts it only when it is a `.jsonl` file directly
/// inside `<user_dir>/projects/<project>/sessions/`. Returns the project directory name and the
/// canonical file path; `None` for anything else (including missing files).
pub fn session_file_in_tree(user_dir: &Path, session_file: &Path) -> Option<(String, PathBuf)> {
    use std::path::Component;
    let projects = fs::canonicalize(user_dir.join("projects")).ok()?;
    let file = fs::canonicalize(session_file).ok()?;
    let rel = file.strip_prefix(&projects).ok()?;
    let mut parts = rel.components();
    let (
        Some(Component::Normal(project)),
        Some(Component::Normal(sessions)),
        Some(Component::Normal(name)),
        None,
    ) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let is_jsonl = Path::new(name).extension().and_then(|e| e.to_str()) == Some("jsonl");
    let accepted = sessions == "sessions" && is_jsonl && file.is_file();
    let project = project.to_string_lossy().into_owned();
    accepted.then_some((project, file))
}

/// Loopback trace server shared by the process: the user directory it serves and its bound port.
static TRACE_SERVER: parking_lot::Mutex<Option<(PathBuf, u16)>> = parking_lot::Mutex::new(None);

/// Starts (once per process and user directory) the loopback stats server on an ephemeral port and
/// returns the dashboard URL that opens `session_file`'s trace.
pub fn trace_url(user_dir: &Path, session_file: &Path) -> Result<String> {
    let port = trace_server_port(user_dir)?;
    let session = std::path::absolute(session_file)
        .with_context(|| format!("resolving {}", session_file.display()))?;
    Ok(format!(
        "http://127.0.0.1:{port}/#/traces?s={}",
        server::percent_encode(&session.to_string_lossy())
    ))
}

fn trace_server_port(user_dir: &Path) -> Result<u16> {
    let mut slot = TRACE_SERVER.lock();
    if let Some((dir, port)) = slot.as_ref()
        && dir == user_dir
    {
        return Ok(*port);
    }
    let listener = server::bind_loopback(0)?;
    let port = listener.local_addr()?.port();
    let dir = user_dir.to_path_buf();
    std::thread::Builder::new()
        .name("dial-stats-trace".into())
        .spawn(move || {
            if let Err(error) = server::serve_listener(listener, dir) {
                eprintln!("stats trace server stopped: {error:#}");
            }
        })
        .context("starting the stats trace server thread")?;
    *slot = Some((user_dir.to_path_buf(), port));
    Ok(port)
}

// ---------------------------------------------------------------- transcripts

#[derive(Debug, Default, PartialEq)]
struct TranscriptSummary {
    session: String,
    cwd: String,
    model: String,
    started_ms: i64,
    last_ms: i64,
    user_msgs: i64,
    assistant_msgs: i64,
    tool_calls: i64,
    tools: BTreeMap<String, i64>,
}

/// Harness-injected user-role messages that the person did not type.
fn injected(message: &Value) -> bool {
    [
        "dial_transient",
        "dial_portable_rules",
        "dial_context_notes",
        "dial_context_rollover",
    ]
    .iter()
    .any(|flag| message[flag] == true)
}

fn summarize_transcript(path: &Path) -> Result<TranscriptSummary> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut summary = TranscriptSummary::default();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = std::io::Read::by_ref(&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }
        if buf.len() > MAX_LINE && !buf.ends_with(b"\n") {
            // Oversized line: skip the rest of it.
            let mut sink = Vec::new();
            reader.read_until(b'\n', &mut sink)?;
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        if value["type"] == "session" {
            summary.session = value["id"].as_str().unwrap_or("").to_owned();
            summary.cwd = value["cwd"].as_str().unwrap_or("").to_owned();
            summary.model = value["model"].as_str().unwrap_or("").to_owned();
            summary.started_ms = value["started"]
                .as_str()
                .and_then(parse_iso_ms)
                .unwrap_or(0);
            summary.last_ms = summary.last_ms.max(summary.started_ms);
            continue;
        }
        if let Some(ts) = value["ts"].as_str().and_then(parse_iso_ms) {
            summary.last_ms = summary.last_ms.max(ts);
            if summary.started_ms == 0 {
                summary.started_ms = ts;
            }
        }
        if value["type"] != "message" {
            continue;
        }
        let message = &value["message"];
        match message["role"].as_str() {
            Some("user") if !injected(message) => summary.user_msgs += 1,
            Some("assistant") => {
                summary.assistant_msgs += 1;
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        summary.tool_calls += 1;
                        *summary.tools.entry(name.to_owned()).or_insert(0) += 1;
                    }
                }
            }
            _ => {}
        }
    }
    if summary.session.is_empty() {
        bail!("{} has no session header", path.display());
    }
    Ok(summary)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` (the format Dial writes) -> Unix milliseconds.
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (date.next()??, date.next()??, date.next()??);
    let (clock, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut clock = clock.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (h, min, s) = (clock.next()??, clock.next()??, clock.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || s > 60 {
        return None;
    }
    let millis = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        digits.parse::<i64>().ok()? * 10_i64.pow(3 - digits.len() as u32)
    };
    Some(days_from_civil(y, m, d) * DAY_MS + (h * 3600 + min * 60 + s) * 1000 + millis)
}

/// Unix milliseconds -> `YYYY-MM-DD` (UTC).
pub fn civil_day(ms: i64) -> String {
    let z = ms.div_euclid(DAY_MS) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `24h`, `7d`, `30d`, `2w`, `all` -> a lookback window in milliseconds (None = all time).
pub fn parse_range(text: &str) -> Result<Option<i64>> {
    let text = text.trim().to_ascii_lowercase();
    if text.is_empty() || text == "all" {
        return Ok(None);
    }
    let (number, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let count: i64 = number
        .parse()
        .with_context(|| format!("range {text:?}: expected e.g. 24h, 7d, 4w or all"))?;
    let unit_ms = match unit {
        "h" => HOUR_MS,
        "d" | "" => DAY_MS,
        "w" => 7 * DAY_MS,
        _ => bail!("range {text:?}: unit must be h, d or w"),
    };
    if count <= 0 {
        bail!("range {text:?} must be positive");
    }
    Ok(Some(count.saturating_mul(unit_ms)))
}

// ---------------------------------------------------------------- text output

fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.2}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.2}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn percent(value: Option<f64>) -> String {
    value.map_or("-".into(), |v| format!("{v:.1}%"))
}

fn cost_text(totals: &Totals) -> String {
    if totals.priced_requests == 0 {
        "unpriced".into()
    } else if totals.priced_requests < totals.requests - totals.errors.min(totals.requests) {
        format!(
            "${:.4} ({} of {} priced)",
            totals.cost_usd, totals.priced_requests, totals.requests
        )
    } else {
        format!("${:.4}", totals.cost_usd)
    }
}

fn duration_text(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{:.1}s", ms as f64 / 1000.0),
    }
}

pub fn render_text(report: &Report) -> String {
    let mut out = String::new();
    let o = &report.overall;
    let range = report.since_ms.map_or("all time".to_owned(), |since| {
        format!("since {}", civil_day(since))
    });
    out.push_str(&format!("Dial usage ({range})\n\n"));
    out.push_str(&format!(
        "Requests {} · errors {} ({}) · input {} · output {} · cache read {} ({} hit) · cache write {} · reasoning {}\n",
        o.requests, o.errors, percent(o.error_rate), tokens(o.input), tokens(o.output),
        tokens(o.cache_read), percent(o.cache_rate), tokens(o.cache_write), tokens(o.reasoning)
    ));
    out.push_str(&format!(
        "Cost {} · model time {} · avg latency {} · {} tok/s\n",
        cost_text(o),
        duration_text(o.duration_ms),
        o.avg_latency_ms
            .map_or("-".into(), |ms| format!("{:.0}ms", ms)),
        o.tokens_per_second
            .map_or("-".into(), |t| format!("{t:.1}")),
    ));
    let s = &report.sessions;
    out.push_str(&format!(
        "Sessions {} in {} projects · {} user / {} assistant messages · {} tool calls · active {} (avg {})\n",
        s.sessions, s.projects, s.user_messages, s.assistant_messages, s.tool_calls,
        duration_text(s.active_ms), s.avg_session_ms.map_or("-".into(), |ms| duration_text(ms as u64)),
    ));
    let groups = |title: &str, rows: &[Group], out: &mut String| {
        if rows.is_empty() {
            return;
        }
        out.push_str(&format!("\n{title}\n"));
        for row in rows {
            let t = &row.totals;
            out.push_str(&format!(
                "  {:<40} {:>6} req  {:>8} in  {:>8} out  cache {:>6}  {:>7} tok/s  {}\n",
                row.label,
                t.requests,
                tokens(t.input),
                tokens(t.output),
                percent(t.cache_rate),
                t.tokens_per_second
                    .map_or("-".into(), |v| format!("{v:.1}")),
                cost_text(t),
            ));
        }
    };
    groups("By model", &report.by_model, &mut out);
    groups("By project", &report.by_project, &mut out);
    if !report.by_tool.is_empty() {
        out.push_str("\nTools (timed at runtime)\n");
        for tool in &report.by_tool {
            out.push_str(&format!(
                "  {:<28} {:>7} calls  {:>6} errors ({})  avg {}\n",
                tool.tool,
                tool.calls,
                tool.errors,
                percent(tool.error_rate),
                tool.avg_ms.map_or("-".into(), |ms| format!("{ms:.0}ms")),
            ));
        }
    }
    if !report.tool_outcomes.is_empty() {
        out.push_str(&format!(
            "\nTool outcomes: {}\n",
            report
                .tool_outcomes
                .iter()
                .map(|row| format!("{} {}", row.count, row.name))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    if !report.transcript_tools.is_empty() {
        out.push_str("\nTool calls in transcripts\n");
        for tool in report.transcript_tools.iter().take(20) {
            out.push_str(&format!("  {:<28} {:>7}\n", tool.name, tool.count));
        }
    }
    if !report.daily.is_empty() {
        out.push_str("\nDaily\n");
        for day in report
            .daily
            .iter()
            .rev()
            .take(14)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            out.push_str(&format!(
                "  {}  {:>6} req  {:>8} tokens  {:>6} tools  {}\n",
                day.day,
                day.totals.requests,
                tokens(day.totals.input + day.totals.output),
                day.tool_calls,
                cost_text(&day.totals),
            ));
        }
    }
    if o.requests == 0 {
        out.push_str("\nNo recorded model requests yet; usage is recorded while Dial runs with [stats] enabled = true.\n");
    }
    out
}

/// `dial stats`: sync transcripts, then print, emit JSON, or serve the dashboard.
pub fn cli(
    user_dir: &Path,
    config: &StatsConfig,
    json: bool,
    serve: bool,
    port: Option<u16>,
    range: Option<&str>,
) -> Result<()> {
    if serve {
        return server::serve(user_dir.to_path_buf(), port.unwrap_or(config.port));
    }
    let mut db = Db::open_default(user_dir)?;
    db.sync_sessions(user_dir)?;
    let now = now_ms();
    let since = parse_range(range.unwrap_or("all"))?.map(|window| now - window);
    let report = db.report(since, now)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_text(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
