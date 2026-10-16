//! Read-only views over the user's local history: recent projects, their sessions with idle
//! recaps, and the global prompt history.
//!
//! Sources: session JSONL files under `<user_dir>/projects/*/sessions` (stat-ordered; only the
//! listed files get a bounded head/tail scan, never a full transcript parse) and
//! `<user_dir>/history.db` (deduplicated prompt history plus the idle-recap journal). Both live
//! in the user directory, outside every workspace. Each view returns plain records and a compact
//! text rendering for the transcript and the `archive` tool.
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, params, params_from_iter, types::Value as SqlValue};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::platform::fs as platform_fs;

/// Prompt history and recap journal, shared by every project.
pub const HISTORY_DB: &str = "history.db";
/// Header window of a session scan: the header line and the first prompt live here.
const HEAD_BYTES: u64 = 16 * 1024;
/// Tail window of a session scan: the newest title, last message and status live here.
const TAIL_BYTES: u64 = 32 * 1024;
/// Largest listing any view returns.
pub const MAX_LIMIT: usize = 500;
/// Listed id length. Dial session ids are 16 hex characters; 8 stay unique in practice and every
/// view accepts any unique prefix.
const SHORT_ID: usize = 8;
const PROMPT_PREVIEW: usize = 160;
const PROMPT_DETAIL: usize = 600;
const RECAP_PREVIEW: usize = 240;
const TITLE_CHARS: usize = 80;
/// Session files scanned per requested row when listing every project's sessions; empty sessions
/// are skipped, so a few extra are read, but never the whole archive.
const SCAN_FACTOR: usize = 4;

// ---------------------------------------------------------------------------------------------
// history.db
// ---------------------------------------------------------------------------------------------

/// A unique prompt with the provenance of its most recent submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    /// Normalized prompt text, unique across the database.
    pub prompt: String,
    /// Unix seconds of the most recent submission.
    pub created_at: i64,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    /// Submissions of this exact prompt, including the first.
    pub use_count: i64,
}

/// Narrows history reads; `None` matches every prompt.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryFilter<'a> {
    pub cwd: Option<&'a str>,
    pub session_id: Option<&'a str>,
}

/// One journaled idle recap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRecap {
    pub session_id: String,
    pub cwd: String,
    pub recap: String,
    /// Unix seconds.
    pub created_at: i64,
}

/// Narrows recap reads; omitted fields match everything. `Some(empty)` session ids match nothing.
#[derive(Debug, Clone, Default)]
pub struct RecapQuery {
    pub session_ids: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub limit: Option<usize>,
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    prompt TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER)),
    seq INTEGER NOT NULL DEFAULT 0,
    cwd TEXT,
    session_id TEXT,
    use_count INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_history_seq ON history(seq DESC);
CREATE VIRTUAL TABLE IF NOT EXISTS history_fts USING fts5(prompt, content='history', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS history_ai AFTER INSERT ON history BEGIN
    INSERT INTO history_fts(rowid, prompt) VALUES (new.id, new.prompt);
END;
CREATE TABLE IF NOT EXISTS session_recaps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    cwd TEXT NOT NULL,
    recap TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER))
);
CREATE INDEX IF NOT EXISTS idx_session_recaps_session ON session_recaps(session_id, created_at);
";

/// `?first` binds cwd and `?first+1` the session id; NULL disables that half.
fn filter_clause(first: usize) -> String {
    let session = first + 1;
    format!(
        "(?{first} IS NULL OR cwd = ?{first}) AND (?{session} IS NULL OR session_id = ?{session})"
    )
}

/// Canonical stored form: CRLF/CR folded to LF, trailing whitespace stripped from every line,
/// outer whitespace trimmed. Terminal copies pad lines with spaces, so without this a resubmitted
/// copy of an existing prompt would land as a byte-distinct duplicate.
pub fn normalize_prompt(prompt: &str) -> String {
    let unified = prompt.replace("\r\n", "\n").replace('\r', "\n");
    unified
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Lowercase alphanumeric runs, mirroring FTS5's `unicode61` tokenizer.
fn tokenize(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .take(16)
        .map(|token| token.chars().take(64).collect())
        .collect()
}

fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Deduplicated prompt history plus the idle-recap journal (`<user_dir>/history.db`).
pub struct PromptHistory {
    conn: Connection,
}

impl PromptHistory {
    pub fn path(user_dir: &Path) -> PathBuf {
        user_dir.join(HISTORY_DB)
    }

    /// Open (creating) the user's history database.
    pub fn open(user_dir: &Path) -> Result<Self> {
        match platform_fs::create_dir_owner_only(&fs::DirBuilder::new().recursive(true), user_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let meta = fs::symlink_metadata(user_dir)?;
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "history directory must be a regular directory"
        );
        platform_fs::set_owner_only(user_dir)?;
        Self::open_path(&Self::path(user_dir))
    }

    /// Readers never create the database just to look.
    pub fn open_existing(user_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(user_dir);
        if !path.is_file() {
            return Ok(None);
        }
        Self::open_path(&path).map(Some)
    }

    pub fn open_path(path: &Path) -> Result<Self> {
        let _file = platform_fs::open_owner_only(
            &fs::OpenOptions::new().read(true).write(true).create(true),
            path,
        )
        .with_context(|| format!("opening {}", path.display()))?;
        platform_fs::set_owner_only(path)?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)
            .context("creating the prompt history schema (SQLite without FTS5?)")?;
        Ok(Self { conn })
    }

    /// Store a prompt (normalized), replace its provenance with this submission and bump its use
    /// count on resubmission. Returns false for blank prompts.
    pub fn add(&self, prompt: &str, cwd: Option<&str>, session_id: Option<&str>) -> Result<bool> {
        let prompt = normalize_prompt(prompt);
        if prompt.is_empty() {
            return Ok(false);
        }
        self.conn
            .execute(
                "INSERT INTO history (prompt, created_at, seq, cwd, session_id)
                 VALUES (?1, CAST(strftime('%s','now') AS INTEGER),
                         (SELECT COALESCE(MAX(seq), 0) + 1 FROM history), ?2, ?3)
                 ON CONFLICT(prompt) DO UPDATE SET
                     created_at = excluded.created_at,
                     seq = excluded.seq,
                     cwd = excluded.cwd,
                     session_id = excluded.session_id,
                     use_count = history.use_count + 1",
                params![prompt, cwd, session_id.filter(|id| !id.is_empty())],
            )
            .context("recording prompt history")?;
        Ok(true)
    }

    fn rows(&self, sql: &str, values: Vec<SqlValue>) -> Result<Vec<HistoryEntry>> {
        let mut statement = self.conn.prepare(sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| {
            Ok(HistoryEntry {
                id: row.get(0)?,
                prompt: row.get(1)?,
                created_at: row.get(2)?,
                cwd: row.get(3)?,
                session_id: row.get(4)?,
                use_count: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("reading prompt history")
    }

    fn filter_values(filter: HistoryFilter<'_>) -> [SqlValue; 2] {
        let text =
            |value: Option<&str>| value.map_or(SqlValue::Null, |v| SqlValue::Text(v.to_owned()));
        [text(filter.cwd), text(filter.session_id)]
    }

    /// Unique prompts, newest submission first.
    pub fn recent(&self, limit: usize, filter: HistoryFilter<'_>) -> Result<Vec<HistoryEntry>> {
        let limit = limit.min(1000);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut values: Vec<SqlValue> = Self::filter_values(filter).into();
        values.push(SqlValue::Integer(limit as i64));
        self.rows(
            &format!(
                "SELECT id, prompt, created_at, cwd, session_id, use_count FROM history
                 WHERE {} ORDER BY seq DESC, id DESC LIMIT ?3",
                filter_clause(1)
            ),
            values,
        )
    }

    /// Unique prompts containing every query token (FTS prefix match, then substring fallback
    /// for infix matches such as `mit` in `commit`), newest submission first.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        filter: HistoryFilter<'_>,
    ) -> Result<Vec<HistoryEntry>> {
        let limit = limit.min(1000);
        let tokens = tokenize(query);
        if limit == 0 || tokens.is_empty() {
            return Ok(Vec::new());
        }
        let fts_query = tokens
            .iter()
            .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        let mut values = vec![SqlValue::Text(fts_query)];
        values.extend(Self::filter_values(filter));
        values.push(SqlValue::Integer(limit as i64));
        // A malformed FTS expression falls through to the substring path.
        let fts = self
            .rows(
                &format!(
                    "SELECT h.id, h.prompt, h.created_at, h.cwd, h.session_id, h.use_count
                     FROM history_fts f JOIN history h ON h.id = f.rowid
                     WHERE history_fts MATCH ?1 AND (?2 IS NULL OR h.cwd = ?2)
                       AND (?3 IS NULL OR h.session_id = ?3)
                     ORDER BY h.seq DESC, h.id DESC LIMIT ?4"
                ),
                values,
            )
            .unwrap_or_default();
        let clauses = (1..=tokens.len())
            .map(|i| format!("prompt LIKE ?{i} ESCAPE '\\'"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let mut values: Vec<SqlValue> = tokens
            .iter()
            .map(|token| SqlValue::Text(format!("%{}%", escape_like(token))))
            .collect();
        values.extend(Self::filter_values(filter));
        values.push(SqlValue::Integer(limit as i64));
        let substring = self.rows(
            &format!(
                "SELECT id, prompt, created_at, cwd, session_id, use_count FROM history
                 WHERE {clauses} AND {} ORDER BY seq DESC, id DESC LIMIT ?{}",
                filter_clause(tokens.len() + 1),
                tokens.len() + 3
            ),
            values,
        )?;
        let order: HashMap<i64, i64> = self.seq_of(fts.iter().chain(&substring).map(|e| e.id))?;
        let mut seen = HashSet::new();
        let mut merged: Vec<HistoryEntry> = fts
            .into_iter()
            .chain(substring)
            .filter(|entry| seen.insert(entry.id))
            .collect();
        merged.sort_by_key(|entry| {
            std::cmp::Reverse((order.get(&entry.id).copied().unwrap_or(0), entry.id))
        });
        merged.truncate(limit);
        Ok(merged)
    }

    fn seq_of(&self, ids: impl Iterator<Item = i64>) -> Result<HashMap<i64, i64>> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT seq FROM history WHERE id = ?1")?;
        let mut out = HashMap::new();
        for id in ids {
            if out.contains_key(&id) {
                continue;
            }
            let seq: i64 = statement.query_row([id], |row| row.get(0))?;
            out.insert(id, seq);
        }
        Ok(out)
    }

    /// Append an idle recap to the session's recap journal.
    pub fn record_recap(&self, session_id: &str, cwd: &str, recap: &str) -> Result<()> {
        ensure!(!session_id.is_empty(), "a recap needs a session id");
        let recap = recap.trim();
        ensure!(!recap.is_empty(), "an empty recap is not journaled");
        self.conn
            .execute(
                "INSERT INTO session_recaps (session_id, cwd, recap) VALUES (?1, ?2, ?3)",
                params![session_id, cwd, recap],
            )
            .context("journaling session recap")?;
        Ok(())
    }

    /// Journaled recaps matching `query`, newest first.
    pub fn recaps(&self, query: &RecapQuery) -> Result<Vec<SessionRecap>> {
        if query.session_ids.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let mut clauses = Vec::new();
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(ids) = &query.session_ids {
            let start = values.len() + 1;
            let marks = (start..start + ids.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("session_id IN ({marks})"));
            values.extend(ids.iter().map(|id| SqlValue::Text(id.clone())));
        }
        if let Some(cwd) = &query.cwd {
            values.push(SqlValue::Text(cwd.clone()));
            clauses.push(format!("cwd = ?{}", values.len()));
        }
        let mut sql = "SELECT session_id, cwd, recap, created_at FROM session_recaps".to_owned();
        if !clauses.is_empty() {
            sql.push_str(&format!(" WHERE {}", clauses.join(" AND ")));
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC");
        if let Some(limit) = query.limit {
            values.push(SqlValue::Integer(limit.min(1000) as i64));
            sql.push_str(&format!(" LIMIT ?{}", values.len()));
        }
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(values), |row| {
            Ok(SessionRecap {
                session_id: row.get(0)?,
                cwd: row.get(1)?,
                recap: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .context("reading the recap journal")
    }
}

/// Record one submitted prompt. Failures never block a submission; callers report or ignore them.
pub fn record_prompt(
    user_dir: &Path,
    cwd: &Path,
    session_id: Option<&str>,
    prompt: &str,
) -> Result<()> {
    if normalize_prompt(prompt).is_empty() {
        return Ok(());
    }
    let cwd = cwd.display().to_string();
    PromptHistory::open(user_dir)?.add(prompt, Some(&cwd), session_id)?;
    Ok(())
}

/// Provider for the composer's Ctrl+R search over every project's prompts: an empty query lists
/// the newest prompts. The connection stays open; each call is one indexed query.
pub fn history_provider(
    user_dir: &Path,
) -> Option<Arc<dyn Fn(&str, usize) -> Vec<String> + Send + Sync>> {
    let history = Mutex::new(PromptHistory::open(user_dir).ok()?);
    Some(Arc::new(move |query: &str, limit: usize| {
        let history = history.lock().unwrap_or_else(|e| e.into_inner());
        let entries = if query.trim().is_empty() {
            history.recent(limit, HistoryFilter::default())
        } else {
            history.search(query, limit, HistoryFilter::default())
        };
        entries
            .unwrap_or_default()
            .into_iter()
            .map(|entry| entry.prompt)
            .collect()
    }))
}

// ---------------------------------------------------------------------------------------------
// Bounded session scans
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// The last message is an assistant reply without pending tool calls.
    Complete,
    /// The last message is a tool call or result that never got a reply.
    Interrupted,
    /// The last message is a user prompt still awaiting a reply.
    Pending,
    Unknown,
}

impl SessionStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Interrupted => "interrupted",
            Self::Pending => "pending",
            Self::Unknown => "unknown",
        }
    }
}

/// A listed session. Times are ISO-8601 (UTC).
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveSession {
    pub id: String,
    /// Session JSONL file.
    pub file: PathBuf,
    /// Working directory the session ran in.
    pub project: String,
    /// Title, else first prompt, else a timestamp label.
    pub title: String,
    pub created: String,
    pub modified: String,
    /// Messages seen by the bounded scan.
    pub messages: usize,
    /// False when the file is larger than the scan windows; `messages` is then a lower bound.
    pub messages_exact: bool,
    pub status: SessionStatus,
    /// Newest journaled idle recap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recap: Option<String>,
    #[serde(skip)]
    modified_secs: i64,
    #[serde(skip)]
    parent: Option<String>,
}

/// A project with its newest session.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveProject {
    /// Working directory; pass it as `project` to scope other views.
    pub path: String,
    /// Session files in the project, empty ones included.
    pub sessions: usize,
    pub last_active: String,
    pub latest: ArchiveSession,
}

/// A unique prompt with its latest submission's provenance.
#[derive(Debug, Clone, Serialize)]
pub struct ArchivePrompt {
    pub text: String,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub uses: i64,
    #[serde(skip)]
    at_secs: i64,
}

/// A journaled idle recap.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveRecap {
    pub text: String,
    pub at: String,
    pub session: String,
    pub project: String,
    #[serde(skip)]
    at_secs: i64,
}

/// One session with its whole recap journal and its newest prompts.
#[derive(Debug, Clone, Serialize)]
pub struct ArchiveSessionDetail {
    #[serde(flatten)]
    pub session: ArchiveSession,
    /// Session id this one was forked from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Oldest first.
    pub recaps: Vec<ArchiveRecap>,
    /// Newest `limit` prompts submitted in this session, oldest first.
    pub prompts: Vec<ArchivePrompt>,
}

/// Records plus their text rendering.
#[derive(Debug, Clone, Serialize)]
pub struct View<T> {
    pub text: String,
    pub records: T,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `<ts>_<id>.jsonl` -> `<id>`.
fn file_id(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".jsonl")?;
    let (_, id) = stem.split_once('_')?;
    valid_id(id).then_some(id)
}

struct SessionFile {
    path: PathBuf,
    modified: i64,
}

/// Session files of one sessions directory with their modification times (stat only).
fn session_files(dir: &Path) -> Vec<SessionFile> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter(|entry| entry.file_name().to_str().and_then(file_id).is_some())
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some(SessionFile {
                path: entry.path(),
                modified: unix_secs(modified),
            })
        })
        .collect()
}

fn unix_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Parse the complete JSON lines of `bytes`. `drop_head`/`drop_tail` discard a leading/trailing
/// fragment cut by a scan window.
fn lines(bytes: &[u8], drop_head: bool, drop_tail: bool) -> Vec<Value> {
    let mut slice = bytes;
    if drop_head {
        match slice.iter().position(|&b| b == b'\n') {
            Some(at) => slice = &slice[at + 1..],
            None => return Vec::new(),
        }
    }
    if drop_tail {
        match slice.iter().rposition(|&b| b == b'\n') {
            Some(at) => slice = &slice[..at],
            None => return Vec::new(),
        }
    }
    slice
        .split(|&b| b == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

/// Whether the final record of `window` is complete JSON. A window that begins inside a record
/// (`line_start` false) cannot prove its last record unless an earlier newline shows where it starts.
fn terminal_record_parses(window: &[u8], line_start: bool) -> bool {
    let body = window.trim_ascii_end();
    let record = match body.iter().rposition(|&b| b == b'\n') {
        Some(at) => &body[at + 1..],
        None if line_start => body,
        None => return false,
    };
    serde_json::from_slice::<Value>(record).is_ok()
}

#[derive(Default)]
struct ScanState {
    id: Option<String>,
    cwd: String,
    started: String,
    parent: Option<String>,
    title: Option<String>,
    first_user: Option<String>,
    messages: usize,
    last_role: Option<String>,
    last_has_calls: bool,
}

impl ScanState {
    fn absorb(&mut self, entries: &[Value], header_window: bool) {
        for value in entries {
            match value.get("type").and_then(Value::as_str) {
                Some("session") if header_window && self.id.is_none() => {
                    self.id = value
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| valid_id(id))
                        .map(str::to_owned);
                    self.cwd = value
                        .get("cwd")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    self.started = value
                        .get("started")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    self.parent = value
                        .get("forked_from")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                Some("message") => {
                    let Some(message) = value.get("message") else {
                        continue;
                    };
                    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
                    if role.is_empty() || role == "dial_state" {
                        continue;
                    }
                    self.messages += 1;
                    if role == "user"
                        && self.first_user.is_none()
                        && message["dial_transient"] != true
                    {
                        let text = one_line(&content_text(message));
                        if !text.is_empty() {
                            self.first_user = Some(text);
                        }
                    }
                    self.last_role = Some(role.to_owned());
                    self.last_has_calls = message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| !calls.is_empty());
                }
                Some("meta") if value.get("kind").and_then(Value::as_str) == Some("title") => {
                    if let Some(title) = value.get("title").and_then(Value::as_str) {
                        self.title = Some(title.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
}

/// Scan one session file reading at most [`HEAD_BYTES`] + [`TAIL_BYTES`].
fn scan_session(path: &Path) -> Option<ArchiveSession> {
    let mut file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let size = meta.len();
    let modified = meta.modified().map(unix_secs).unwrap_or(0);
    let mut state = ScanState::default();
    let exact = size <= HEAD_BYTES + TAIL_BYTES;
    let terminal_observed = if exact {
        let mut bytes = Vec::with_capacity(size as usize);
        file.read_to_end(&mut bytes).ok()?;
        state.absorb(&lines(&bytes, false, false), true);
        terminal_record_parses(&bytes, true)
    } else {
        let mut head = vec![0; HEAD_BYTES as usize];
        file.read_exact(&mut head).ok()?;
        state.absorb(&lines(&head, false, true), true);
        // One byte before the window shows whether the window starts on a line boundary.
        let start = size - TAIL_BYTES;
        file.seek(SeekFrom::Start(start - 1)).ok()?;
        let mut window = Vec::with_capacity(TAIL_BYTES as usize + 1);
        file.read_to_end(&mut window).ok()?;
        let line_start = window.first() == Some(&b'\n');
        let tail = &window[1..];
        let messages_before = state.messages;
        // The tail window starts mid-file; a partial first line belongs to an unseen record.
        state.absorb(&lines(tail, !line_start, false), false);
        // The last message is known only when this window holds a message and its final record
        // is complete. Otherwise that message may sit in the unseen middle or be cut off.
        state.messages > messages_before && terminal_record_parses(tail, line_start)
    };
    let name_id = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(file_id)
        .map(str::to_owned);
    let id = state.id.clone().or(name_id)?;
    let status = match (
        terminal_observed,
        state.last_role.as_deref(),
        state.last_has_calls,
    ) {
        (false, _, _) | (_, None, _) => SessionStatus::Unknown,
        (_, Some("user"), _) => SessionStatus::Pending,
        (_, Some("assistant"), false) => SessionStatus::Complete,
        _ => SessionStatus::Interrupted,
    };
    let title = state
        .title
        .clone()
        .or_else(|| {
            state
                .first_user
                .as_deref()
                .map(|text| clip(text, TITLE_CHARS))
        })
        .unwrap_or_else(|| {
            if state.started.is_empty() {
                "(no messages)".to_owned()
            } else {
                format!("(no messages) {}", state.started)
            }
        });
    let created = if state.started.is_empty() {
        iso(modified)
    } else {
        state.started.clone()
    };
    Some(ArchiveSession {
        id,
        file: path.to_path_buf(),
        project: state.cwd,
        title,
        created,
        modified: iso(modified),
        messages: state.messages,
        messages_exact: exact,
        status,
        recap: None,
        modified_secs: modified,
        parent: state.parent,
    })
}

// ---------------------------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------------------------

/// Read-only access to every project's sessions and the global history database.
pub struct Archive {
    user_dir: PathBuf,
    now: i64,
}

impl Archive {
    pub fn new(user_dir: &Path) -> Self {
        Self {
            user_dir: user_dir.to_path_buf(),
            now: unix_secs(SystemTime::now()),
        }
    }

    fn projects_root(&self) -> PathBuf {
        self.user_dir.join("projects")
    }

    fn history(&self) -> Result<Option<PromptHistory>> {
        PromptHistory::open_existing(&self.user_dir)
    }

    /// Every per-project state directory's sessions directory.
    fn session_dirs(&self) -> Vec<PathBuf> {
        let Ok(read) = fs::read_dir(self.projects_root()) else {
            return Vec::new();
        };
        read.filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
            .map(|entry| entry.path().join("sessions"))
            .filter(|dir| dir.is_dir())
            .collect()
    }

    /// The sessions directory of `project`: the state layout when the directory still exists,
    /// else the directory whose newest session ran there.
    fn project_sessions_dir(&self, project: &Path) -> Option<PathBuf> {
        if let Ok(layout) = super::paths::Layout::with_user_dir(project, self.user_dir.clone()) {
            return Some(layout.sessions_dir());
        }
        let wanted = project.display().to_string();
        self.session_dirs().into_iter().find(|dir| {
            let mut files = session_files(dir);
            files.sort_by_key(|f| std::cmp::Reverse(f.modified));
            files
                .first()
                .and_then(|f| scan_session(&f.path))
                .is_some_and(|s| s.project == wanted)
        })
    }

    fn attach_recaps(&self, sessions: &mut [ArchiveSession]) -> Result<()> {
        let Some(history) = self.history()? else {
            return Ok(());
        };
        let ids = sessions.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        let mut latest: HashMap<String, String> = HashMap::new();
        for row in history.recaps(&RecapQuery {
            session_ids: Some(ids),
            ..Default::default()
        })? {
            latest.entry(row.session_id).or_insert(row.recap);
        }
        for session in sessions {
            session.recap = latest.get(&session.id).cloned();
        }
        Ok(())
    }

    /// Recent projects, newest activity first.
    pub fn projects(&self, limit: usize) -> Result<View<Vec<ArchiveProject>>> {
        let limit = limit.clamp(1, MAX_LIMIT);
        let mut candidates: Vec<(usize, SessionFile)> = self
            .session_dirs()
            .iter()
            .filter_map(|dir| {
                let files = session_files(dir);
                let count = files.len();
                files
                    .into_iter()
                    .max_by_key(|f| f.modified)
                    .map(|newest| (count, newest))
            })
            .collect();
        candidates.sort_by_key(|(_, newest)| std::cmp::Reverse(newest.modified));
        let mut records = Vec::new();
        for (count, newest) in candidates {
            if records.len() >= limit {
                break;
            }
            let Some(latest) = scan_session(&newest.path) else {
                continue;
            };
            records.push(ArchiveProject {
                path: latest.project.clone(),
                sessions: count,
                last_active: latest.modified.clone(),
                latest,
            });
        }
        let mut latest: Vec<ArchiveSession> = records.iter().map(|p| p.latest.clone()).collect();
        self.attach_recaps(&mut latest)?;
        for (project, session) in records.iter_mut().zip(latest) {
            project.latest = session;
        }
        if records.is_empty() {
            return Ok(View {
                text: "No projects with sessions yet.".into(),
                records,
            });
        }
        let mut lines = vec!["Recent projects, newest first:".to_owned()];
        for project in &records {
            lines.push(format!(
                "- {} · {} · active {}",
                shorten_path(&project.path),
                count(project.sessions, "session"),
                self.ago(project.latest.modified_secs)
            ));
            lines.push(format!(
                "  latest {} · {}",
                short_id(&project.latest.id),
                project.latest.title
            ));
            if let Some(recap) = &project.latest.recap {
                lines.push(format!(
                    "  recap: {}",
                    clip(&one_line(recap), RECAP_PREVIEW)
                ));
            }
        }
        Ok(View {
            text: lines.join("\n"),
            records,
        })
    }

    /// Newest non-empty sessions of one project, or of every project when `project` is `None`.
    pub fn sessions(
        &self,
        project: Option<&Path>,
        limit: usize,
    ) -> Result<View<Vec<ArchiveSession>>> {
        let limit = limit.clamp(1, MAX_LIMIT);
        let mut files: Vec<SessionFile> = match project {
            Some(project) => self
                .project_sessions_dir(project)
                .map(|dir| session_files(&dir))
                .unwrap_or_default(),
            None => self
                .session_dirs()
                .iter()
                .flat_map(|dir| session_files(dir))
                .collect(),
        };
        files.sort_by_key(|f| std::cmp::Reverse(f.modified));
        let mut records: Vec<ArchiveSession> = files
            .iter()
            .take(limit.saturating_mul(SCAN_FACTOR))
            .filter_map(|f| scan_session(&f.path))
            .filter(|s| s.messages > 0)
            .take(limit)
            .collect();
        self.attach_recaps(&mut records)?;
        let scope = scope_label(project);
        if records.is_empty() {
            return Ok(View {
                text: format!("No sessions {scope}."),
                records,
            });
        }
        let mut lines = vec![format!("Sessions {scope}, newest first:")];
        for session in &records {
            let mut facts = vec![
                short_id(&session.id).to_owned(),
                self.ago(session.modified_secs),
            ];
            if project.is_none() && !session.project.is_empty() {
                facts.push(shorten_path(&session.project));
            }
            facts.push(message_count(session));
            if session.status != SessionStatus::Complete {
                facts.push(session.status.label().to_owned());
            }
            facts.push(session.title.clone());
            lines.push(format!("- {}", facts.join(" · ")));
            if let Some(recap) = &session.recap {
                lines.push(format!(
                    "  recap: {}",
                    clip(&one_line(recap), RECAP_PREVIEW)
                ));
            }
        }
        Ok(View {
            text: lines.join("\n"),
            records,
        })
    }

    /// Resolve a session id prefix, or a `.jsonl` path inside the archive, to its file.
    pub fn resolve_session(&self, id_or_file: &str) -> Result<PathBuf> {
        let input = id_or_file.trim();
        let as_path = Path::new(input);
        if as_path.is_absolute() {
            let canonical =
                fs::canonicalize(as_path).with_context(|| format!("no session file at {input}"))?;
            let root =
                fs::canonicalize(self.projects_root()).context("no archived sessions yet")?;
            ensure!(
                canonical.starts_with(&root)
                    && canonical
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(file_id)
                        .is_some(),
                "{input} is not a session file inside the Dial archive"
            );
            return Ok(canonical);
        }
        ensure!(
            valid_id(input),
            "session ids contain only letters, digits, '-' and '_'; got {input:?}"
        );
        let mut matches: Vec<PathBuf> = self
            .session_dirs()
            .iter()
            .flat_map(|dir| session_files(dir))
            .map(|f| f.path)
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(file_id)
                    .is_some_and(|id| id.starts_with(input))
            })
            .collect();
        matches.sort();
        match matches.len() {
            0 => bail!(
                "No session id starts with {input:?}. List sessions with action \"sessions\" (project \"*\" spans every project)."
            ),
            1 => Ok(matches.remove(0)),
            n => {
                let ids = matches
                    .iter()
                    .take(5)
                    .filter_map(|p| p.file_name().and_then(|n| n.to_str()).and_then(file_id))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("{input:?} matches {n} sessions ({ids}); use a longer prefix.")
            }
        }
    }

    /// One session with every recap and its newest `limit` prompts.
    pub fn session(&self, id_or_file: &str, limit: usize) -> Result<View<ArchiveSessionDetail>> {
        let limit = limit.clamp(1, MAX_LIMIT);
        let file = self.resolve_session(id_or_file)?;
        let session = scan_session(&file).with_context(|| {
            format!(
                "not a readable session file: {}",
                shorten_path(&file.display().to_string())
            )
        })?;
        let (mut recaps, mut prompts) = (Vec::new(), Vec::new());
        if let Some(history) = self.history()? {
            recaps = history
                .recaps(&RecapQuery {
                    session_ids: Some(vec![session.id.clone()]),
                    ..Default::default()
                })?
                .into_iter()
                .map(to_recap)
                .collect();
            recaps.reverse();
            prompts = history
                .recent(
                    limit,
                    HistoryFilter {
                        session_id: Some(&session.id),
                        cwd: None,
                    },
                )?
                .into_iter()
                .map(to_prompt)
                .collect();
            prompts.reverse();
        }
        let mut session = session;
        session.recap = recaps.last().map(|r| r.text.clone());
        let mut facts = vec![
            format!("project {}", shorten_path(&session.project)),
            format!("created {}", session.created),
            format!("active {}", self.ago(session.modified_secs)),
            message_count(&session),
        ];
        facts.push(session.status.label().to_owned());
        let mut lines = vec![
            format!("Session {} · {}", session.id, session.title),
            facts.join(" · "),
            format!("file {}", shorten_path(&session.file.display().to_string())),
        ];
        if let Some(parent) = &session.parent {
            lines.push(format!("forked from {parent}"));
        }
        lines.push(String::new());
        if recaps.is_empty() {
            lines.push("No recaps journaled.".into());
        } else {
            lines.push("Recaps, oldest first:".into());
            for recap in &recaps {
                lines.push(format!(
                    "- {}: {}",
                    self.ago(recap.at_secs),
                    clip(&one_line(&recap.text), RECAP_PREVIEW)
                ));
            }
        }
        if prompts.is_empty() {
            lines.push("No prompts in prompt history.".into());
        } else {
            let more = if prompts.len() == limit {
                format!("latest {limit}, ")
            } else {
                String::new()
            };
            lines.push(format!("Prompts, {more}oldest first:"));
            for prompt in &prompts {
                lines.push(format!(
                    "- {}: {}",
                    self.ago(prompt.at_secs),
                    clip(&one_line(&prompt.text), PROMPT_DETAIL)
                ));
            }
        }
        let parent = session.parent.clone();
        Ok(View {
            text: lines.join("\n"),
            records: ArchiveSessionDetail {
                session,
                parent,
                recaps,
                prompts,
            },
        })
    }

    /// Prompt history of one project or every project, newest first; a query switches to search.
    pub fn prompts(
        &self,
        query: Option<&str>,
        project: Option<&Path>,
        limit: usize,
    ) -> Result<View<Vec<ArchivePrompt>>> {
        let limit = limit.clamp(1, MAX_LIMIT);
        let cwd = project.map(|p| p.display().to_string());
        let filter = HistoryFilter {
            cwd: cwd.as_deref(),
            session_id: None,
        };
        let query = query.map(str::trim).filter(|q| !q.is_empty());
        let records: Vec<ArchivePrompt> = match self.history()? {
            None => Vec::new(),
            Some(history) => match query {
                Some(query) => history.search(query, limit, filter)?,
                None => history.recent(limit, filter)?,
            }
            .into_iter()
            .map(to_prompt)
            .collect(),
        };
        let scope = scope_label(project);
        let subject = match query {
            Some(query) => format!("{scope} matching {query:?}"),
            None => scope,
        };
        if records.is_empty() {
            return Ok(View {
                text: format!("No prompts {subject}."),
                records,
            });
        }
        let mut lines = vec![format!("Prompts {subject}, newest first:")];
        for prompt in &records {
            let mut facts = vec![self.ago(prompt.at_secs)];
            if project.is_none()
                && let Some(project) = &prompt.project
            {
                facts.push(shorten_path(project));
            }
            if let Some(session) = &prompt.session {
                facts.push(short_id(session).to_owned());
            }
            if prompt.uses > 1 {
                facts.push(format!("used {}×", prompt.uses));
            }
            lines.push(format!("- {}", facts.join(" · ")));
            lines.push(format!(
                "  {}",
                clip(&one_line(&prompt.text), PROMPT_PREVIEW)
            ));
        }
        Ok(View {
            text: lines.join("\n"),
            records,
        })
    }

    /// Journaled idle recaps of one project or every project, newest first.
    pub fn recaps(&self, project: Option<&Path>, limit: usize) -> Result<View<Vec<ArchiveRecap>>> {
        let limit = limit.clamp(1, MAX_LIMIT);
        let records: Vec<ArchiveRecap> = match self.history()? {
            None => Vec::new(),
            Some(history) => history
                .recaps(&RecapQuery {
                    session_ids: None,
                    cwd: project.map(|p| p.display().to_string()),
                    limit: Some(limit),
                })?
                .into_iter()
                .map(to_recap)
                .collect(),
        };
        let scope = scope_label(project);
        if records.is_empty() {
            return Ok(View {
                text: format!("No recaps {scope}."),
                records,
            });
        }
        let mut lines = vec![format!("Recaps {scope}, newest first:")];
        for recap in &records {
            let mut facts = vec![self.ago(recap.at_secs)];
            if project.is_none() {
                facts.push(shorten_path(&recap.project));
            }
            facts.push(short_id(&recap.session).to_owned());
            lines.push(format!("- {}", facts.join(" · ")));
            lines.push(format!("  {}", clip(&one_line(&recap.text), RECAP_PREVIEW)));
        }
        Ok(View {
            text: lines.join("\n"),
            records,
        })
    }

    /// Relative age (`5m ago`); sub-minute ages read `just now`.
    fn ago(&self, then: i64) -> String {
        let secs = (self.now - then).max(0);
        match secs {
            0..=59 => "just now".into(),
            60..=3599 => format!("{}m ago", secs / 60),
            3600..=86_399 => format!("{}h ago", secs / 3600),
            _ => format!("{}d ago", secs / 86_400),
        }
    }
}

fn to_prompt(entry: HistoryEntry) -> ArchivePrompt {
    ArchivePrompt {
        at: iso(entry.created_at),
        at_secs: entry.created_at,
        text: entry.prompt,
        project: entry.cwd,
        session: entry.session_id,
        uses: entry.use_count,
    }
}

fn to_recap(row: SessionRecap) -> ArchiveRecap {
    ArchiveRecap {
        at: iso(row.created_at),
        at_secs: row.created_at,
        text: row.recap,
        session: row.session_id,
        project: row.cwd,
    }
}

fn message_count(session: &ArchiveSession) -> String {
    let prefix = if session.messages_exact { "" } else { "≥" };
    format!("{prefix}{}", count(session.messages, "msg"))
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn short_id(id: &str) -> &str {
    match id.char_indices().nth(SHORT_ID) {
        Some((at, _)) => &id[..at],
        None => id,
    }
}

fn scope_label(project: Option<&Path>) -> String {
    match project {
        Some(project) => format!("in {}", shorten_path(&project.display().to_string())),
        None => "across all projects".into(),
    }
}

fn shorten_path(path: &str) -> String {
    match std::env::var("HOME") {
        // Only whole path components: `/Users/al` must not shorten `/Users/alice/x`.
        Ok(home)
            if home.len() > 1
                && path.starts_with(&home)
                && matches!(path.as_bytes().get(home.len()), None | Some(b'/')) =>
        {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
fn iso(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `~/x` and relative paths resolve like a shell would; the archive matches canonical roots.
pub fn expand_home(path: &str) -> PathBuf {
    let expanded = match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ if path == "~" => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
        _ => PathBuf::from(path),
    };
    fs::canonicalize(&expanded).unwrap_or(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn history() -> (tempfile::TempDir, PromptHistory) {
        let dir = tempfile::tempdir().unwrap();
        let history = PromptHistory::open(dir.path()).unwrap();
        (dir, history)
    }

    #[test]
    fn normalization_folds_line_endings_and_padding() {
        assert_eq!(
            normalize_prompt("  fix it   \r\nplease  \r\n"),
            "fix it\nplease"
        );
        assert_eq!(normalize_prompt("a\rb"), "a\nb");
        assert_eq!(normalize_prompt(" \n\t "), "");
    }

    #[test]
    fn history_dedupes_resubmissions_and_keeps_latest_provenance() {
        let (_dir, history) = history();
        assert!(
            history
                .add("fix the parser", Some("/p/one"), Some("s1"))
                .unwrap()
        );
        assert!(
            history
                .add("run tests", Some("/p/one"), Some("s1"))
                .unwrap()
        );
        // A terminal copy with trailing padding is the same prompt.
        assert!(
            history
                .add("fix the parser   \r\n", Some("/p/two"), Some("s2"))
                .unwrap()
        );
        assert!(!history.add("   ", None, None).unwrap());
        let recent = history.recent(10, HistoryFilter::default()).unwrap();
        assert_eq!(recent.len(), 2, "duplicates collapse into one row");
        assert_eq!(
            recent[0].prompt, "fix the parser",
            "resubmission moves to the front"
        );
        assert_eq!(recent[0].use_count, 2);
        assert_eq!(recent[0].cwd.as_deref(), Some("/p/two"));
        assert_eq!(recent[0].session_id.as_deref(), Some("s2"));
        assert_eq!(recent[1].prompt, "run tests");
        let scoped = history
            .recent(
                10,
                HistoryFilter {
                    cwd: Some("/p/one"),
                    session_id: None,
                },
            )
            .unwrap();
        assert_eq!(
            scoped.iter().map(|e| e.prompt.as_str()).collect::<Vec<_>>(),
            ["run tests"]
        );
    }

    #[test]
    fn search_matches_prefixes_and_infixes_without_fts_injection() {
        let (_dir, history) = history();
        history.add("git commit the fix", None, None).unwrap();
        history.add("deploy to staging", None, None).unwrap();
        history.add("50% off_sale", None, None).unwrap();
        let prompts = |q: &str| {
            history
                .search(q, 10, HistoryFilter::default())
                .unwrap()
                .into_iter()
                .map(|e| e.prompt)
                .collect::<Vec<_>>()
        };
        assert_eq!(prompts("com"), ["git commit the fix"], "prefix match");
        assert_eq!(
            prompts("mit"),
            ["git commit the fix"],
            "infix match via substring fallback"
        );
        assert_eq!(
            prompts("git fix"),
            ["git commit the fix"],
            "every token must match"
        );
        assert!(
            prompts("NEAR( \"x").is_empty(),
            "FTS operators in queries are plain tokens"
        );
        assert_eq!(prompts("off_sale"), ["50% off_sale"]);
        assert!(
            prompts("%").is_empty(),
            "punctuation-only queries match nothing"
        );
    }

    #[test]
    fn recaps_are_journaled_newest_first_and_filtered() {
        let (_dir, history) = history();
        history.record_recap("s1", "/p/a", "first").unwrap();
        history.record_recap("s1", "/p/a", "second").unwrap();
        history.record_recap("s2", "/p/b", "other").unwrap();
        let all = history.recaps(&RecapQuery::default()).unwrap();
        assert_eq!(all.len(), 3);
        let s1 = history
            .recaps(&RecapQuery {
                session_ids: Some(vec!["s1".into()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            s1.iter().map(|r| r.recap.as_str()).collect::<Vec<_>>(),
            ["second", "first"]
        );
        let none = history
            .recaps(&RecapQuery {
                session_ids: Some(vec![]),
                ..Default::default()
            })
            .unwrap();
        assert!(none.is_empty());
        let b = history
            .recaps(&RecapQuery {
                cwd: Some("/p/b".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(b.len(), 1);
        assert!(history.record_recap("s1", "/p/a", "  ").is_err());
    }

    fn write_session(
        dir: &Path,
        stamp: &str,
        id: &str,
        cwd: &str,
        lines: &[serde_json::Value],
    ) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{stamp}_{id}.jsonl"));
        let mut file = File::create(&path).unwrap();
        let header = serde_json::json!({"type":"session","id":id,"started":"2026-01-02T03:04:05.000Z","cwd":cwd,"model":"m"});
        writeln!(file, "{header}").unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    fn message(id: &str, role: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"type":"message","id":id,"parent":null,"ts":"t","message":{"role":role,"content":text}})
    }

    #[test]
    fn archive_lists_projects_sessions_and_details_with_recaps() {
        let home = tempfile::tempdir().unwrap();
        let user = home.path().join("dial");
        let a = user.join("projects/alpha-11111111/sessions");
        let b = user.join("projects/beta-22222222/sessions");
        write_session(
            &a,
            "20260101T000000000Z",
            "aaaa000011112222",
            "/work/alpha",
            &[
                message("1", "user", "build the parser"),
                message("2", "assistant", "done"),
            ],
        );
        write_session(
            &a,
            "20260102T000000000Z",
            "aaaa333344445555",
            "/work/alpha",
            &[],
        );
        let titled = write_session(
            &b,
            "20260103T000000000Z",
            "bbbb000011112222",
            "/work/beta",
            &[
                message("1", "user", "  explain\n the   cache "),
                serde_json::json!({"type":"meta","id":"2","parent":"1","ts":"t","kind":"title","title":"Cache talk"}),
            ],
        );
        let history = PromptHistory::open(&user).unwrap();
        history
            .record_recap(
                "bbbb000011112222",
                "/work/beta",
                "Explaining the cache; next: diagram",
            )
            .unwrap();
        history
            .add(
                "explain the cache",
                Some("/work/beta"),
                Some("bbbb000011112222"),
            )
            .unwrap();
        drop(history);
        let archive = Archive::new(&user);

        let projects = archive.projects(10).unwrap();
        assert_eq!(projects.records.len(), 2);
        let alpha = projects
            .records
            .iter()
            .find(|p| p.path == "/work/alpha")
            .unwrap();
        assert_eq!(
            alpha.sessions, 2,
            "empty sessions still count toward the project"
        );

        let all = archive.sessions(None, 10).unwrap();
        assert_eq!(all.records.len(), 2, "empty sessions are not listed");
        let beta = all
            .records
            .iter()
            .find(|s| s.id == "bbbb000011112222")
            .unwrap();
        assert_eq!(beta.title, "Cache talk");
        assert_eq!(beta.status, SessionStatus::Pending);
        assert!(beta.messages_exact);
        assert_eq!(
            beta.recap.as_deref(),
            Some("Explaining the cache; next: diagram")
        );
        let alpha_session = all
            .records
            .iter()
            .find(|s| s.id == "aaaa000011112222")
            .unwrap();
        assert_eq!(alpha_session.title, "build the parser");
        assert_eq!(alpha_session.status, SessionStatus::Complete);
        assert_eq!(alpha_session.messages, 2);

        let scoped = archive.sessions(Some(Path::new("/work/beta")), 10).unwrap();
        assert_eq!(
            scoped.records.len(),
            1,
            "a vanished project resolves through its session headers"
        );

        let detail = archive.session("bbbb", 10).unwrap();
        assert_eq!(detail.records.session.file.file_name(), titled.file_name());
        assert_eq!(detail.records.session.id, "bbbb000011112222");
        assert_eq!(detail.records.recaps.len(), 1);
        assert_eq!(detail.records.prompts.len(), 1);
        assert!(
            archive.session("aaaa", 10).is_err(),
            "ambiguous prefixes are refused"
        );
        assert!(archive.session("../etc", 10).is_err());
        assert!(
            archive.session("/etc/passwd", 10).is_err(),
            "paths outside the archive are refused"
        );

        let prompts = archive.prompts(Some("cache"), None, 10).unwrap();
        assert_eq!(prompts.records.len(), 1);
        let recaps = archive.recaps(Some(Path::new("/work/beta")), 10).unwrap();
        assert_eq!(recaps.records.len(), 1);
    }

    #[test]
    fn large_sessions_are_scanned_in_bounded_windows() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("projects/big-33333333/sessions");
        let filler = "x".repeat(1024);
        let mut lines = vec![message("1", "user", "first prompt")];
        for i in 0..200 {
            lines.push(message(&format!("a{i}"), "assistant", &filler));
        }
        lines.push(serde_json::json!({"type":"meta","id":"t","parent":null,"ts":"t","kind":"title","title":"Late title"}));
        lines.push(message("z", "tool", "result"));
        let path = write_session(
            &dir,
            "20260101T000000000Z",
            "cccc000011112222",
            "/work/big",
            &lines,
        );
        let session = scan_session(&path).unwrap();
        assert!(!session.messages_exact);
        assert!(
            session.messages < 202,
            "the middle of a large file is never read"
        );
        assert_eq!(
            session.title, "Late title",
            "titles come from the tail window"
        );
        assert_eq!(session.status, SessionStatus::Interrupted);
        assert_eq!(session.project, "/work/big");
    }

    #[test]
    fn unobserved_terminal_records_never_inherit_complete() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("projects/term-44444444/sessions");
        let filler = "x".repeat(1024);
        let mut base = vec![message("1", "user", "first prompt")];
        for i in 0..60 {
            base.push(message(&format!("a{i}"), "assistant", &filler));
        }
        // A short completed exchange, then a final record larger than the tail window.
        let mut oversized = base.clone();
        oversized.push(message("done", "assistant", "all done"));
        oversized.push(message("big", "user", &"y".repeat(40 * 1024)));
        let path = write_session(
            &dir,
            "20260101T000000000Z",
            "dddd000011112222",
            "/work/big",
            &oversized,
        );
        let session = scan_session(&path).unwrap();
        assert!(!session.messages_exact);
        assert_eq!(session.status, SessionStatus::Unknown);

        // A completed exchange whose final record was cut off mid-JSON.
        let mut truncated = base.clone();
        truncated.push(message("done", "assistant", "all done"));
        let path = write_session(
            &dir,
            "20260101T000000001Z",
            "eeee000011112222",
            "/work/big",
            &truncated,
        );
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"type\":\"message\",\"message\":{\"role\":\"assistant\"")
            .unwrap();
        assert_eq!(scan_session(&path).unwrap().status, SessionStatus::Unknown);
    }

    #[test]
    fn a_final_record_ending_exactly_at_the_window_boundary_is_observed() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("projects/edge-55555555/sessions");
        let filler = "x".repeat(1024);
        let mut lines = vec![message("1", "user", "first prompt")];
        for i in 0..60 {
            lines.push(message(&format!("a{i}"), "assistant", &filler));
        }
        // The final record is exactly one tail window long, so the window starts on its line.
        let line_len = serde_json::to_string(&message("z", "assistant", ""))
            .unwrap()
            .len()
            + 1;
        let text = "y".repeat(TAIL_BYTES as usize - line_len);
        lines.push(message("z", "assistant", &text));
        let path = write_session(
            &dir,
            "20260101T000000002Z",
            "ffff000011112222",
            "/work/edge",
            &lines,
        );
        let session = scan_session(&path).unwrap();
        assert!(!session.messages_exact);
        assert_eq!(session.status, SessionStatus::Complete);
    }

    #[test]
    fn iso_formats_known_instants() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_401), "2000-02-29T00:00:01Z");
    }
}
