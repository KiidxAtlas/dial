//! Sessions: an append-only JSONL tree per conversation (omp-style) plus full-text search across
//! all sessions of a project (Hermes-style).
//!
//! File layout (`<sessions_dir>/<ts>_<id>.jsonl`): the first line is a header
//! `{type:"session", id, started, cwd, model, version}`; every further line is an entry
//! `{type:"message"|"compaction"|"meta", id, parent, ts, ...}`. The active branch is the parent
//! chain of the *last* entry in the file, so branching (rewind) is just another appended line and
//! no history is ever deleted.
//!
//! Search keeps an FTS5 index in `<state_dir>/sessions.sqlite3`. The index is derived data: each
//! search first catches up on every session file from the byte offset it last indexed, so it also
//! picks up lines written by other processes and survives a crash between append and indexing.
use crate::platform::fs::OpenOptions;
#[cfg(test)]
use crate::platform::fs::PermissionsExt;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::paths::Layout;
use crate::tools::ToolOutput;

/// Longest line we write or read. Longer lines on disk are skipped; longer appends are refused.
const MAX_LINE: usize = 8 * 1024 * 1024;
/// Tool outputs larger than this are not searchable (their names and the model's replies are).
const MAX_INDEXED_TOOL_BODY: usize = 2048;
const MAX_INDEXED_TEXT: usize = 16 * 1024;
const TITLE_CHARS: usize = 80;

pub struct SessionMeta {
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    pub started: String,
    pub title: String,
    pub messages: usize,
    pub model: String,
    pub parent_id: Option<String>,
    pub first_message: String,
    pub project: String,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub session_id: String,
    pub role: String,
    pub snippet: String,
    pub when: String,
}

#[derive(Debug, Clone)]
enum Kind {
    Message(Value),
    Runtime(crate::runtime::Node),
    Compaction {
        summary: String,
        first_kept: Option<String>,
    },
    Meta {
        kind: String,
        removed: usize,
    },
    Title(String),
}

#[derive(Debug, Clone)]
struct Entry {
    id: String,
    parent: Option<String>,
    ts: String,
    kind: Kind,
}

pub struct Session {
    runtime: crate::runtime::Tree,
    id: String,
    model: String,
    started: String,
    parent_id: Option<String>,
    path: Option<PathBuf>,
    file: Option<File>,
    /// The file's last byte was not `\n` (truncated write): the next append starts a fresh line.
    pending_newline: bool,
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    leaf: Option<String>,
}

// ---------------------------------------------------------------- time / ids

static LAST_MS: AtomicU64 = AtomicU64::new(0);

/// Wall-clock milliseconds, strictly increasing within the process so session files sort by creation.
fn now_ms() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut prev = LAST_MS.load(Ordering::Relaxed);
    loop {
        let next = now.max(prev + 1);
        match LAST_MS.compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(p) => prev = p,
        }
    }
}

fn iso(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86400, secs % 86400);
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

fn iso_now() -> String {
    iso(now_ms())
}

fn random_hex(bytes: usize) -> String {
    (0..bytes)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect()
}

/// Ids are only ever used as labels and map keys, never joined into a path; still, anything read from
/// disk that is not plain `[A-Za-z0-9_-]` is replaced.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `<ts>_<id>.jsonl` -> `<id>` (None for any other file name).
fn file_id(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(".jsonl")?;
    let (_, id) = stem.split_once('_')?;
    valid_id(id).then_some(id)
}

// ---------------------------------------------------------------- file helpers

fn private_dir(dir: &Path) -> Result<()> {
    crate::storage::settings_store::private_dir(dir)
}

struct RawLine {
    consumed: usize,
    terminated: bool,
    oversized: bool,
}

/// Reads one line into `buf` (without the newline). Lines over `MAX_LINE` are consumed and flagged
/// rather than buffered. `None` at EOF.
fn read_bounded_line<R: BufRead>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<Option<RawLine>> {
    buf.clear();
    let (mut consumed, mut oversized, mut terminated) = (0usize, false, false);
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let (take, found) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !oversized {
            let body = if found { take - 1 } else { take };
            if buf.len() + body > MAX_LINE {
                oversized = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..body]);
            }
        }
        r.consume(take);
        consumed += take;
        if found {
            terminated = true;
            break;
        }
    }
    Ok((consumed > 0).then_some(RawLine {
        consumed,
        terminated,
        oversized,
    }))
}

/// Session files in a directory, newest first. Symlinks and odd names are ignored.
fn session_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut names: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| file_id(n).is_some())
        .collect();
    names.sort_unstable_by(|a, b| b.cmp(a));
    Ok(names.into_iter().map(|n| dir.join(n)).collect())
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

fn role_of(message: &Value) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or("")
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

// ---------------------------------------------------------------- entry (de)serialisation

fn entry_json(e: &Entry, copied: bool) -> Value {
    let mut v = match &e.kind {
        Kind::Runtime(node) => {
            json!({"type":"runtime","id":e.id,"parent":e.parent,"ts":e.ts,"node":node})
        }
        Kind::Message(m) => {
            json!({"type": "message", "id": e.id, "parent": e.parent, "ts": e.ts, "message": m})
        }
        Kind::Title(title) => {
            json!({"type": "meta", "id": e.id, "parent": e.parent, "ts": e.ts, "kind": "title", "title": title})
        }
        Kind::Compaction {
            summary,
            first_kept,
        } => {
            json!({"type": "compaction", "id": e.id, "parent": e.parent, "ts": e.ts, "summary": summary, "first_kept": first_kept})
        }
        Kind::Meta { kind, removed } => {
            json!({"type": "meta", "id": e.id, "parent": e.parent, "ts": e.ts, "kind": kind, "removed": removed})
        }
    };
    if copied {
        v["copied"] = Value::Bool(true);
    }
    v
}

fn parse_entry(v: &Value) -> Option<Entry> {
    let id = v.get("id")?.as_str()?;
    if id.is_empty() || id.len() > 64 {
        return None;
    }
    let parent = v.get("parent").and_then(Value::as_str).map(str::to_string);
    let ts = v
        .get("ts")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let kind = match v.get("type")?.as_str()? {
        "runtime" => Kind::Runtime(serde_json::from_value(v.get("node")?.clone()).ok()?),
        "message" => {
            let m = v.get("message")?;
            m.is_object().then_some(())?;
            m.get("role")?.as_str()?;
            Kind::Message(m.clone())
        }
        "compaction" => Kind::Compaction {
            summary: v.get("summary")?.as_str()?.to_string(),
            first_kept: v
                .get("first_kept")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "meta" if v["kind"] == "title" => Kind::Title(v["title"].as_str().unwrap_or("").to_owned()),
        "meta" => Kind::Meta {
            kind: v
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            removed: v.get("removed").and_then(Value::as_u64).unwrap_or(0) as usize,
        },
        _ => return None,
    };
    Some(Entry {
        id: id.to_string(),
        parent,
        ts,
        kind,
    })
}

// ---------------------------------------------------------------- Session

impl Session {
    /// A new session file with its header written.
    pub fn create(layout: &Layout, meta: SessionMeta) -> Result<Session> {
        Self::create_file(layout, &meta.model, None)
    }

    fn create_file(layout: &Layout, model: &str, forked_from: Option<&str>) -> Result<Session> {
        let dir = layout.sessions_dir();
        private_dir(&dir)?;
        let ms = now_ms();
        let started = iso(ms);
        let id = random_hex(8);
        let stamp = started.replace(['-', ':', '.'], "");
        let path = dir.join(format!("{stamp}_{id}.jsonl"));
        let mut header = json!({
            "type": "session", "id": id, "started": started, "cwd": layout.project_root,
            "model": model, "version": env!("CARGO_PKG_VERSION"),
        });
        if let Some(parent) = forked_from {
            header["forked_from"] = json!(parent);
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("creating session {}", path.display()))?;
        file.write_all(format!("{header}\n").as_bytes())
            .context("writing session header")?;
        Ok(Session {
            runtime: crate::runtime::Tree::default(),
            id,
            model: model.to_string(),
            started,
            parent_id: forked_from.map(str::to_owned),
            path: Some(path),
            file: Some(file),
            pending_newline: false,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf: None,
        })
    }

    /// In-memory only: nothing is written or indexed.
    pub fn ephemeral() -> Session {
        Session {
            runtime: crate::runtime::Tree::default(),
            id: random_hex(8),
            model: String::new(),
            started: iso_now(),
            parent_id: None,
            path: None,
            file: None,
            pending_newline: false,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf: None,
        }
    }

    /// Opens an existing session for reading and appending. Unparseable lines (a truncated tail from a
    /// crash, garbage, oversized lines) are skipped.
    pub fn open(path: &Path) -> Result<Session> {
        let mut s = Self::load(path)?;
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("opening session {}", path.display()))?;
        s.file = Some(file);
        s.repair_incomplete_tools()?;
        Ok(s)
    }

    /// Read-only parse of a session file.
    fn load(path: &Path) -> Result<Session> {
        let meta = fs::metadata(path).with_context(|| format!("session {}", path.display()))?;
        ensure!(meta.is_file(), "{} is not a session file", path.display());
        let mut reader = BufReader::new(
            File::open(path).with_context(|| format!("opening session {}", path.display()))?,
        );
        let name_id = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(file_id)
            .map(str::to_string);
        let mut s = Session {
            runtime: crate::runtime::Tree::default(),
            id: String::new(),
            model: String::new(),
            started: String::new(),
            parent_id: None,
            path: Some(path.to_path_buf()),
            file: None,
            pending_newline: false,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf: None,
        };
        let mut header_id: Option<String> = None;
        let mut saw_header = false;
        let mut buf = Vec::new();
        let mut ends_with_newline = true;
        while let Some(line) = read_bounded_line(&mut reader, &mut buf)? {
            ends_with_newline = line.terminated;
            if line.oversized {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(&buf) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) == Some("session") {
                if !saw_header && s.entries.is_empty() {
                    saw_header = true;
                    header_id = v
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|i| valid_id(i))
                        .map(str::to_string);
                    s.model = v
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    s.started = v
                        .get("started")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    s.parent_id = v
                        .get("forked_from")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                continue;
            }
            let Some(entry) = parse_entry(&v) else {
                continue;
            };
            if s.by_id.contains_key(&entry.id) {
                continue;
            }
            s.leaf = Some(entry.id.clone());
            s.by_id.insert(entry.id.clone(), s.entries.len());
            s.entries.push(entry);
        }
        s.pending_newline = !ends_with_newline;
        s.id = header_id
            .or(name_id)
            .unwrap_or_else(|| "unknown".to_string());
        s.restore_runtime();
        Ok(s)
    }

    /// Newest session file for this project, if any.
    pub fn latest(layout: &Layout) -> Result<Option<PathBuf>> {
        Ok(session_files(&layout.sessions_dir())?.into_iter().next())
    }

    /// Newest first. Files that cannot be parsed are skipped.
    pub fn list(layout: &Layout, limit: usize) -> Result<Vec<SessionInfo>> {
        Self::list_paths(session_files(&layout.sessions_dir())?, limit)
    }

    /// Sessions across every project, newest first.
    pub fn list_all(layout: &Layout, limit: usize) -> Result<Vec<SessionInfo>> {
        let mut paths = Vec::new();
        let projects = layout.user_dir.join("projects");
        if projects.is_dir() {
            for entry in fs::read_dir(projects)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    paths.extend(session_files(&entry.path().join("sessions"))?);
                }
            }
        }
        paths.sort_unstable_by(|a, b| b.file_name().cmp(&a.file_name()));
        Self::list_paths(paths, limit)
    }

    fn list_paths(paths: Vec<PathBuf>, limit: usize) -> Result<Vec<SessionInfo>> {
        let mut out = Vec::new();
        for path in paths {
            if out.len() >= limit {
                break;
            }
            let Ok(s) = Self::load(&path) else { continue };
            let first_message = s.first_message();
            let project = path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(SessionInfo {
                title: s.title(),
                messages: s.messages().len(),
                first_message,
                project,
                id: s.id,
                path,
                started: s.started,
                model: s.model,
                parent_id: s.parent_id,
            });
        }
        Ok(out)
    }

    fn first_message(&self) -> String {
        self.entries
            .iter()
            .find_map(|e| match &e.kind {
                Kind::Message(m) if role_of(m) == "user" => Some(content_text(m)),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub fn title(&self) -> String {
        self.entries
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                Kind::Title(title) => Some(title.clone()),
                _ => None,
            })
            .unwrap_or_else(|| {
                truncate_chars(
                    &self
                        .first_message()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                    TITLE_CHARS,
                )
            })
    }

    /// Append metadata; never rewrite conversation history.
    pub fn rename(&mut self, title: &str) -> Result<()> {
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        ensure!(!title.is_empty(), "a session title is required");
        self.push(Kind::Title(title)).map(|_| ())
    }

    /// Export the active conversation with tool payloads collapsed and HTML safely escaped.
    pub fn export(&self, path: &Path, html: bool) -> Result<()> {
        let escape = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let mut body = String::new();
        let title = self.title();
        if html {
            body.push_str(&format!("<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>body{{font:16px system-ui;max-width:900px;margin:2rem auto;padding:0 1rem;line-height:1.5}}pre{{white-space:pre-wrap;overflow-wrap:anywhere}}details{{border:1px solid #888;padding:.6rem;margin:1rem 0}}h2{{border-bottom:1px solid #888}}</style></head><body><h1>{}</h1>", escape(&title), escape(&title)));
        } else {
            body.push_str(&format!("# {}\n\n", escape(&title)));
        }
        for i in self.branch() {
            let Kind::Message(message) = &self.entries[i].kind else {
                continue;
            };
            let role = role_of(message);
            let content = content_text(message);
            let calls = message
                .get("tool_calls")
                .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()));
            if role == "tool" || calls.is_some() {
                if !content.is_empty() && role != "tool" {
                    body.push_str(&format!(
                        "<h2>{}</h2><pre>{}</pre>\n\n",
                        escape(role),
                        escape(&content)
                    ));
                }
                let payload = calls
                    .map(|c| serde_json::to_string_pretty(c).unwrap_or_default())
                    .unwrap_or(content);
                body.push_str(&format!(
                    "<details><summary>{}</summary><pre>{}</pre></details>\n\n",
                    if role == "tool" {
                        "Tool result"
                    } else {
                        "Tool calls"
                    },
                    escape(&payload)
                ));
            } else if html {
                body.push_str(&format!(
                    "<h2>{}</h2><pre>{}</pre>\n",
                    escape(role),
                    escape(&content)
                ));
            } else {
                body.push_str(&format!("## {role}\n\n{content}\n\n"));
            }
        }
        if html {
            body.push_str("</body></html>\n");
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating export {}", path.display()))?;
        file.write_all(body.as_bytes())
            .context("writing session export")
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn fresh_id(&self) -> String {
        loop {
            let id = random_hex(4);
            if !self.by_id.contains_key(&id) {
                return id;
            }
        }
    }

    fn push(&mut self, kind: Kind) -> Result<String> {
        let parent = self.leaf.clone();
        self.push_under(parent, kind)
    }

    /// Appends an entry whose parent is `parent` (a rewind marker's parent is the rewind point, not the leaf).
    fn push_under(&mut self, parent: Option<String>, kind: Kind) -> Result<String> {
        let entry = Entry {
            id: self.fresh_id(),
            parent,
            ts: iso_now(),
            kind,
        };
        let line = entry_json(&entry, false).to_string();
        ensure!(
            line.len() < MAX_LINE,
            "session entry is {} bytes, over the {} byte line limit",
            line.len(),
            MAX_LINE
        );
        if let Some(file) = self.file.as_mut() {
            // One write_all on an O_APPEND descriptor: concurrent writers cannot interleave inside it.
            let mut buf = String::with_capacity(line.len() + 2);
            if self.pending_newline {
                buf.push('\n');
            }
            buf.push_str(&line);
            buf.push('\n');
            if let Err(e) = file.write_all(buf.as_bytes()) {
                self.pending_newline = true; // a partial line may be on disk; heal it on the next write
                return Err(e).context("appending to session");
            }
            self.pending_newline = false;
        }
        let id = entry.id.clone();
        self.by_id.insert(id.clone(), self.entries.len());
        self.leaf = Some(id.clone());
        self.entries.push(entry);
        Ok(id)
    }

    /// Appends a chat message (an object with a string `role`) as a child of the current leaf.
    pub fn append(&mut self, message: Value) -> Result<String> {
        ensure!(
            message.is_object() && message.get("role").is_some_and(Value::is_string),
            "session messages must be objects with a string `role`"
        );
        self.flush_runtime()?;
        if role_of(&message) == "user"
            && !self.entries.iter().any(|e| {
                matches!(&e.kind, Kind::Title(title) if !title.is_empty())
                    || matches!(&e.kind, Kind::Message(m) if role_of(m) == "user")
            })
        {
            let title = truncate_chars(
                &content_text(&message)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                TITLE_CHARS,
            );
            if !title.is_empty() {
                self.push(Kind::Title(title))?;
            }
        }
        let id = self.push(Kind::Message(message))?;
        self.runtime.anchor(Some(id.clone()));
        Ok(id)
    }

    /// Durable harness state is append-only but never sent as a chat role to providers.
    pub fn control_state(&self) -> Value {
        self.branch()
            .into_iter()
            .rev()
            .find_map(|i| match &self.entries[i].kind {
                Kind::Message(m) if m["role"] == "dial_state" => Some(m["state"].clone()),
                _ => None,
            })
            .unwrap_or_else(|| json!({}))
    }

    pub fn runtime(&self) -> crate::runtime::Tree {
        // Runtime children are anchored in this session's append-only branch tree.
        self.runtime.anchor_if_empty(self.id.clone());
        self.runtime.clone()
    }

    /// Active conversation nodes and mutable result children, with resolvable parent ids.
    pub fn execution_tree(&self) -> Vec<crate::runtime::Node> {
        use crate::runtime::{Node, Phase, ResultBody};
        let mut nodes = vec![Node {
            id: self.id.clone(),
            parent: None,
            kind: "session".into(),
            label: self.title(),
            result: ResultBody {
                text: String::new(),
                data: Some(json!({"model":self.model})),
                phase: Phase::Done,
                revision: 0,
            },
        }];
        let mut parent = Some(self.id.clone());
        for i in self.branch() {
            let entry = &self.entries[i];
            if let Kind::Message(message) = &entry.kind {
                nodes.push(Node {
                    id: entry.id.clone(),
                    parent,
                    kind: "message".into(),
                    label: role_of(message).into(),
                    result: ResultBody {
                        text: content_text(message),
                        data: Some(message.clone()),
                        phase: Phase::Done,
                        revision: 0,
                    },
                });
                parent = Some(entry.id.clone());
            }
        }
        nodes.extend(self.runtime.snapshots());
        nodes
    }

    /// Journal typed execution snapshots on the same active branch as messages. Streaming bodies
    /// stay in memory; request/tool boundaries persist changed snapshots without per-token disk I/O.
    pub fn flush_runtime(&mut self) -> Result<()> {
        for node in self.runtime.pending() {
            self.push(Kind::Runtime(node.clone()))?;
            self.runtime.persisted(&node);
        }
        Ok(())
    }
    pub fn flush_settled_runtime(&mut self) -> Result<()> {
        for node in self.runtime.pending_settled() {
            self.push(Kind::Runtime(node.clone()))?;
            self.runtime.persisted(&node);
        }
        Ok(())
    }

    fn restore_runtime(&mut self) {
        self.runtime = crate::runtime::Tree::default();
        // Never recycle result ids from abandoned branches.
        for entry in &self.entries {
            if let Kind::Runtime(node) = &entry.kind {
                self.runtime.reserve(&node.id);
            }
        }
        for i in self.branch() {
            if let Kind::Runtime(node) = &self.entries[i].kind {
                self.runtime.restore(node.clone());
            }
        }
        self.runtime.interrupt_open();
        let parent = self.branch().into_iter().rev().find_map(|i| {
            matches!(self.entries[i].kind, Kind::Message(_)).then(|| self.entries[i].id.clone())
        });
        self.runtime
            .anchor(parent.or_else(|| Some(self.id.clone())));
    }
    pub fn set_control_state(&mut self, state: Value) -> Result<()> {
        self.append(json!({"role":"dial_state","state":state}))
            .map(|_| ())
    }

    /// Entry indices from the root to the leaf.
    fn branch(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = self
            .leaf
            .as_deref()
            .and_then(|id| self.by_id.get(id))
            .copied();
        while let Some(i) = cur {
            if !seen.insert(i) {
                break;
            }
            out.push(i);
            cur = self.entries[i]
                .parent
                .as_deref()
                .and_then(|p| self.by_id.get(p))
                .copied();
        }
        out.reverse();
        out
    }

    /// (summary of the latest compaction on the branch, indices of the message entries still in view).
    fn view(&self, branch: &[usize]) -> (Option<&str>, Vec<usize>) {
        let is_message = |&&i: &&usize| matches!(&self.entries[i].kind, Kind::Message(m) if m["role"] != "dial_state");
        let Some(p) = branch
            .iter()
            .rposition(|&i| matches!(self.entries[i].kind, Kind::Compaction { .. }))
        else {
            return (None, branch.iter().filter(is_message).copied().collect());
        };
        let Kind::Compaction {
            summary,
            first_kept,
        } = &self.entries[branch[p]].kind
        else {
            return (None, Vec::new());
        };
        let start = first_kept
            .as_deref()
            .and_then(|f| branch[..p].iter().position(|&i| self.entries[i].id == f));
        let before = start.map(|s| &branch[s..p]).unwrap_or(&[]);
        let kept = before
            .iter()
            .chain(&branch[p + 1..])
            .filter(is_message)
            .copied()
            .collect();
        (Some(summary), kept)
    }

    /// The conversation as the model should see it: after a compaction, a summary message followed by
    /// the kept tail and everything since.
    pub fn messages(&self) -> Vec<Value> {
        let mut out = Vec::new();
        self.extend_messages(&mut out);
        out
    }

    /// Append directly into a request, avoiding a second history-sized vector.
    pub fn extend_messages(&self, out: &mut Vec<Value>) {
        let branch = self.branch();
        let (summary, kept) = self.view(&branch);
        out.reserve(kept.len() + usize::from(summary.is_some()));
        if let Some(summary) = summary {
            out.push(json!({
                "role": "user",
                "content": format!("<summary>\nSummary of the earlier conversation (older messages were compacted away):\n{summary}\n</summary>"),
            }));
        }
        for i in kept {
            if let Kind::Message(m) = &self.entries[i].kind
                && m["role"] != "dial_state"
            {
                out.push(m.clone());
            }
        }
    }
    /// Number of messages `messages()` would return.
    pub fn message_count(&self) -> usize {
        let branch = self.branch();
        let (summary, kept) = self.view(&branch);
        usize::from(summary.is_some()) + kept.iter().filter(|&&i| matches!(&self.entries[i].kind, Kind::Message(m) if m["role"] != "dial_state")).count()
    }

    /// Sum `weigh` over the messages of `messages()` from index `from` on, without building that vector.
    pub fn weigh_from(&self, from: usize, weigh: impl Fn(&Value) -> usize) -> usize {
        let branch = self.branch();
        let (summary, kept) = self.view(&branch);
        let (mut index, mut total) = (0usize, 0usize);
        if let Some(summary) = summary {
            if index >= from {
                total += weigh(&json!({"role": "user", "content": summary}));
            }
            index += 1;
        }
        for i in kept {
            if let Kind::Message(m) = &self.entries[i].kind {
                if m["role"] == "dial_state" {
                    continue;
                }
                if index >= from {
                    total += weigh(m);
                }
                index += 1;
            }
        }
        total
    }

    /// Complete a crashed/interrupted trailing call group without dropping its assistant message.
    pub fn repair_incomplete_tools(&mut self) -> Result<usize> {
        let branch = self.branch();
        let (_, kept) = self.view(&branch);
        let mut calls: Vec<String> = Vec::new();
        for i in kept {
            let Kind::Message(message) = &self.entries[i].kind else {
                continue;
            };
            match message["role"].as_str() {
                Some("assistant") => {
                    calls.clear();
                    if let Some(group) = message["tool_calls"].as_array() {
                        calls.extend(
                            group
                                .iter()
                                .filter_map(|call| call["id"].as_str().map(str::to_owned)),
                        );
                    }
                }
                Some("tool") => {
                    if let Some(id) = message["tool_call_id"].as_str() {
                        calls.retain(|pending| pending != id);
                    }
                }
                _ => {}
            }
        }
        let count = calls.len();
        for id in calls {
            self.append(crate::llm::tool_message(
                &id,
                "interrupted by the user before this ran",
            ))?;
        }
        Ok(count)
    }

    /// Replaces everything but the last `keep_last` messages with `summary`. The cut never starts on a
    /// tool result (its assistant tool call stays with it). Older entries remain in the file.
    pub fn compact(&mut self, summary: String, keep_last: usize) -> Result<()> {
        let branch = self.branch();
        let (_, visible) = self.view(&branch);
        let first_kept = if keep_last == 0 || visible.is_empty() {
            None
        } else {
            let mut idx = visible.len().saturating_sub(keep_last);
            while idx > 0
                && matches!(&self.entries[visible[idx]].kind, Kind::Message(m) if role_of(m) == "tool")
            {
                idx -= 1;
            }
            Some(self.entries[visible[idx]].id.clone())
        };
        self.push(Kind::Compaction {
            summary,
            first_kept,
        })
        .map(|_| ())
    }

    /// Moves the leaf back to just before the `user_turns`-th most recent user message (clamped to the
    /// oldest one in view). Nothing is deleted: the old branch stays in the file. Returns how many
    /// messages left the active conversation.
    pub fn rewind(&mut self, user_turns: usize) -> Result<usize> {
        if user_turns == 0 {
            return Ok(0);
        }
        let branch = self.branch();
        let (_, visible) = self.view(&branch);
        let users: Vec<usize> = (0..visible.len())
            .filter(|&n| matches!(&self.entries[visible[n]].kind, Kind::Message(m) if role_of(m) == "user"))
            .collect();
        let Some(&n) = users.iter().rev().nth(user_turns - 1).or(users.first()) else {
            return Ok(0);
        };
        let removed = visible.len() - n;
        let target = visible[n];
        let pos = branch
            .iter()
            .position(|&i| i == target)
            .context("rewind target not on the active branch")?;
        let new_leaf = pos
            .checked_sub(1)
            .map(|p| self.entries[branch[p]].id.clone());
        self.push_under(
            new_leaf,
            Kind::Meta {
                kind: "rewind".into(),
                removed,
            },
        )?;
        self.restore_runtime();
        Ok(removed)
    }

    /// A new session file holding the active branch (rewind markers dropped, ids preserved).
    pub fn fork(&self, layout: &Layout) -> Result<Session> {
        let mut s = Self::create_file(layout, &self.model, Some(&self.id))?;
        let mut kept: Vec<Entry> = Vec::new();
        for i in self.branch() {
            if matches!(self.entries[i].kind, Kind::Meta { .. }) {
                continue;
            }
            let mut e = self.entries[i].clone();
            if let Kind::Runtime(node) = &mut e.kind {
                if node.parent.as_deref() == Some(&self.id) {
                    node.parent = Some(s.id.clone());
                }
            }
            e.parent = kept.last().map(|p| p.id.clone());
            kept.push(e);
        }
        let mut body = String::new();
        for e in &kept {
            let line = entry_json(e, true).to_string();
            ensure!(line.len() < MAX_LINE, "session entry over the line limit");
            body.push_str(&line);
            body.push('\n');
        }
        if let Some(file) = s.file.as_mut() {
            file.write_all(body.as_bytes())
                .context("writing forked session")?;
        }
        for e in kept {
            s.by_id.insert(e.id.clone(), s.entries.len());
            s.leaf = Some(e.id.clone());
            s.entries.push(e);
        }
        s.restore_runtime();
        Ok(s)
    }

    /// Full-text search over every session of the project, best match first.
    pub fn search(layout: &Layout, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let dir = layout.sessions_dir();
        let terms = query_terms(query);
        if terms.is_empty() || !dir.is_dir() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 100);
        let mut conn = open_index(layout)?;
        sync_index(&mut conn, &dir)?;
        // Every term is a quoted FTS string, so no query syntax from the caller reaches the parser.
        // Prefer sessions containing all terms; fall back to any term.
        for joiner in [" ", " OR "] {
            let q = terms
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(joiner);
            let mut stmt = conn.prepare(
                "SELECT session_id, role, snippet(msgs, 0, '', '', '…', 24), ts FROM msgs WHERE msgs MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let hits = stmt
                .query_map(params![q, limit as i64], |r| {
                    Ok(SearchHit {
                        session_id: r.get(0)?,
                        role: r.get(1)?,
                        snippet: r.get(2)?,
                        when: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if !hits.is_empty() || terms.len() == 1 {
                return Ok(hits);
            }
        }
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------- search index

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .take(12)
        .map(|t| t.chars().take(64).collect())
        .collect()
}

fn open_index(layout: &Layout) -> Result<Connection> {
    private_dir(&layout.state_dir)?;
    let db = layout.state_dir.join("sessions.sqlite3");
    let conn = Connection::open(&db).with_context(|| format!("opening {}", db.display()))?;
    conn.busy_timeout(Duration::from_secs(10))?;
    let _ = crate::platform::fs::set_mode(&db, 0o600);
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS indexed(session_id TEXT PRIMARY KEY, bytes INTEGER NOT NULL);
         CREATE VIRTUAL TABLE IF NOT EXISTS msgs USING fts5(text, session_id UNINDEXED, role UNINDEXED, ts UNINDEXED, tokenize='porter unicode61');",
    )
    .context("creating the session search index (SQLite without FTS5?)")?;
    Ok(conn)
}

/// What of a message is searchable: user/assistant text plus the names of tools the assistant called,
/// and short tool results.
fn index_text(message: &Value) -> Option<(&str, String)> {
    let role = role_of(message);
    let mut text = content_text(message);
    match role {
        "user" => {}
        "assistant" => {
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    text.push_str(&format!("\n[tool: {name}]"));
                }
            }
        }
        "tool" if text.len() <= MAX_INDEXED_TOOL_BODY => {}
        _ => return None,
    }
    let text = text.trim();
    (!text.is_empty()).then(|| (role, text.chars().take(MAX_INDEXED_TEXT).collect()))
}

fn sync_index(conn: &mut Connection, dir: &Path) -> Result<()> {
    let files = session_files(dir)?;
    let known: HashMap<String, u64> = {
        let mut stmt = conn.prepare("SELECT session_id, bytes FROM indexed")?;
        stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut present = HashSet::new();
    for path in files {
        let Some(sid) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(file_id)
            .map(str::to_string)
        else {
            continue;
        };
        present.insert(sid.clone());
        let Ok(len) = fs::metadata(&path).map(|m| m.len()) else {
            continue;
        };
        if known.get(&sid) != Some(&len) {
            index_session(conn, &sid, &path, len)?;
        }
    }
    for gone in known.keys().filter(|k| !present.contains(*k)) {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM msgs WHERE session_id = ?1", [gone])?;
        tx.execute("DELETE FROM indexed WHERE session_id = ?1", [gone])?;
        tx.commit()?;
    }
    Ok(())
}

/// Indexes complete lines of `path` from the stored offset up to `len` (a half-written tail line is
/// left for the next sync). Serialised per database by an immediate transaction, so two processes
/// searching at once never index a line twice.
fn index_session(conn: &mut Connection, sid: &str, path: &Path, len: u64) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut offset = tx
        .query_row(
            "SELECT bytes FROM indexed WHERE session_id = ?1",
            [sid],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0) as u64;
    if len < offset {
        tx.execute("DELETE FROM msgs WHERE session_id = ?1", [sid])?; // file was replaced
        offset = 0;
    }
    if len > offset {
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return Ok(()),
        };
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(file.take(len - offset));
        let mut buf = Vec::new();
        while let Some(line) = read_bounded_line(&mut reader, &mut buf)? {
            if !line.terminated {
                break;
            }
            offset += line.consumed as u64;
            if line.oversized {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(&buf) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("message")
                || v.get("copied").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            let Some((role, text)) = v.get("message").and_then(index_text) else {
                continue;
            };
            let ts = v.get("ts").and_then(Value::as_str).unwrap_or("");
            tx.execute(
                "INSERT INTO msgs(text, session_id, role, ts) VALUES(?1, ?2, ?3, ?4)",
                params![text, sid, role, ts],
            )?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO indexed(session_id, bytes) VALUES(?1, ?2)",
        params![sid, offset as i64],
    )?;
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------- session_search tool

pub mod session_search_tool {
    use super::*;

    pub fn schema() -> Value {
        json!({"type": "function", "function": {
            "name": "session_search",
            "description": "Full-text search over this project's earlier conversations (user and assistant messages, tool names, short tool results). Use it to recall what was decided or done in a previous session. Returns the best matching snippets with session id and time.",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to look for; all words must match, falling back to any word."},
                    "limit": {"type": "integer", "description": "Maximum results (default 8, max 25)."}
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }})
    }

    pub fn call(layout: &Layout, args: &Value) -> ToolOutput {
        let err = |text: String| ToolOutput {
            data: None,
            text,
            is_error: true,
        };
        let Some(query) = args.get("query").and_then(Value::as_str) else {
            return err("session_search needs a `query` string".into());
        };
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(8)
            .clamp(1, 25) as usize;
        match Session::search(layout, query, limit) {
            Err(e) => err(format!("session search failed: {e:#}")),
            Ok(hits) if hits.is_empty() => ToolOutput {
                data: None,
                text: "no matching messages in earlier sessions".into(),
                is_error: false,
            },
            Ok(hits) => {
                let mut text = format!("{} result(s):\n", hits.len());
                for h in hits {
                    let snippet = h.snippet.split_whitespace().collect::<Vec<_>>().join(" ");
                    text.push_str(&format!(
                        "- [{}] session {} ({}): {}\n",
                        h.when, h.session_id, h.role, snippet
                    ));
                }
                ToolOutput {
                    data: None,
                    text,
                    is_error: false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
        layout: Layout,
    }

    fn env() -> Env {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
        Env {
            _home: home,
            _project: project,
            layout,
        }
    }

    fn user(t: &str) -> Value {
        json!({"role": "user", "content": t})
    }
    fn assistant(t: &str) -> Value {
        json!({"role": "assistant", "content": t})
    }
    fn new_session(e: &Env) -> Session {
        Session::create(&e.layout, SessionMeta { model: "m".into() }).unwrap()
    }
    fn contents(msgs: &[Value]) -> Vec<String> {
        msgs.iter().map(content_text).collect()
    }

    #[test]
    fn rename_persists_without_changing_messages_and_export_escapes_tools() {
        let e = env();
        let mut s = new_session(&e);
        s.append(user("first prompt")).unwrap();
        s.append(json!({"role":"assistant","content":"answer","tool_calls":[{"id":"one","function":{"name":"bash","arguments":"{}"}}]})).unwrap();
        s.append(json!({"role":"tool","tool_call_id":"one","content":"<script>alert(1)</script>"}))
            .unwrap();
        let messages = s.messages();
        s.rename("A named session").unwrap();
        let reopened = Session::open(s.path().unwrap()).unwrap();
        assert_eq!(reopened.title(), "A named session");
        assert_eq!(reopened.messages(), messages);
        let info = Session::list(&e.layout, usize::MAX).unwrap().remove(0);
        assert_eq!(info.title, "A named session");
        assert_eq!(info.first_message, "first prompt");
        assert_eq!(info.model, "m");
        let fork = reopened.fork(&e.layout).unwrap();
        let infos = Session::list_all(&e.layout, usize::MAX).unwrap();
        assert_eq!(
            infos
                .iter()
                .find(|i| i.id == fork.id())
                .unwrap()
                .parent_id
                .as_deref(),
            Some(s.id())
        );
        for html in [false, true] {
            let path = e
                .layout
                .project_root
                .join(if html { "out.html" } else { "out.md" });
            reopened.export(&path, html).unwrap();
            let output = fs::read_to_string(&path).unwrap();
            assert!(output.contains("A named session"));
            assert!(output.contains("first prompt"));
            assert!(output.contains("<details>"));
            assert!(output.contains("&lt;script&gt;"));
            assert!(!output.contains("<script>"));
            assert!(
                reopened.export(&path, html).is_err(),
                "exports must not overwrite existing files"
            );
        }
    }

    #[test]
    fn old_message_only_sessions_still_get_titles() {
        let e = env();
        let s = new_session(&e);
        let path = s.path().unwrap();
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{}", json!({"type":"message","id":"old","parent":null,"ts":"","message":user("old first message")})).unwrap();
        let mut loaded = Session::open(path).unwrap();
        assert_eq!(loaded.title(), "old first message");
        loaded.rename("new title").unwrap();
        assert_eq!(Session::open(path).unwrap().title(), "new title");
        assert_eq!(loaded.messages(), vec![user("old first message")]);
    }

    #[test]
    fn root_workers_round_trip_and_forks_reparent_to_the_new_session() {
        let e = env();
        let mut session = new_session(&e);
        let worker = session
            .runtime()
            .start("worker", "before first message", None);
        session.flush_runtime().unwrap();
        assert_eq!(worker.read(|n| n.parent.clone()), Some(session.id().into()));
        let loaded = Session::open(session.path().unwrap()).unwrap();
        let fresh = loaded.runtime().start("worker", "after reopen", None);
        assert_eq!(fresh.read(|n| n.parent.clone()), Some(loaded.id().into()));
        let fork = loaded.fork(&e.layout).unwrap();
        assert_eq!(
            fork.runtime().snapshots()[0].parent.as_deref(),
            Some(fork.id())
        );
    }

    #[test]
    fn runtime_journal_recovers_typed_results_and_rewind_prunes_only_the_active_view() {
        use crate::runtime::Phase;
        let e = env();
        let mut session = new_session(&e);
        let first = session.append(user("first")).unwrap();
        let tool = session.runtime().start("tool", "browser", None);
        tool.settle(
            "shown text".into(),
            Some(json!({"tabs":[{"id":7}]})),
            Phase::Done,
        );
        session.flush_runtime().unwrap();
        session.append(assistant("first answer")).unwrap();
        session.append(user("second")).unwrap();
        let worker = session.runtime().start("worker", "inspect", None);
        worker.settle(
            "working".into(),
            Some(json!({"status":"running"})),
            Phase::Running,
        );
        session.flush_runtime().unwrap();
        let path = session.path().unwrap().to_owned();
        let mut reopened = Session::open(&path).unwrap();
        let nodes = reopened.runtime().snapshots();
        assert_eq!(nodes[0].parent.as_deref(), Some(first.as_str()));
        assert_eq!(nodes[0].result.data.as_ref().unwrap()["tabs"][0]["id"], 7);
        assert_eq!(nodes[1].result.phase, Phase::Interrupted);
        assert_eq!(
            nodes[1].result.data.as_ref().unwrap()["status"],
            "interrupted"
        );
        let tree = reopened.execution_tree();
        for node in &tree {
            if let Some(parent) = &node.parent {
                assert!(tree.iter().any(|n| &n.id == parent), "orphan {}", node.id);
            }
        }
        let fork = reopened.fork(&e.layout).unwrap();
        assert_eq!(fork.runtime().snapshots().len(), 2);
        reopened.rewind(1).unwrap();
        assert_eq!(reopened.runtime().snapshots().len(), 1);
        let fresh = reopened.runtime().start("assistant", "new branch", None);
        assert_ne!(fresh.read(|n| n.id.clone()), worker.read(|n| n.id.clone()));
        assert_eq!(Session::open(&path).unwrap().runtime().snapshots().len(), 1);
        assert_eq!(
            reopened.messages().len(),
            2,
            "runtime snapshots are not provider messages"
        );
    }

    #[test]
    fn create_append_reopen_round_trip_with_private_permissions() {
        let e = env();
        let mut s = new_session(&e);
        s.append(user("hello")).unwrap();
        let call = json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{}"}}]});
        s.append(call.clone()).unwrap();
        s.append(json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}))
            .unwrap();
        let path = s.path().unwrap().to_path_buf();

        let header: Value =
            serde_json::from_str(fs::read_to_string(&path).unwrap().lines().next().unwrap())
                .unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["id"], s.id());
        assert_eq!(header["model"], "m");
        assert_eq!(header["cwd"], json!(e.layout.project_root));
        assert!(header["started"].as_str().unwrap().ends_with('Z'));

        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.id(), s.id());
        assert_eq!(reopened.messages(), s.messages());
        assert_eq!(reopened.messages()[1], call);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        // Appending after a reopen continues the same branch.
        let mut reopened = reopened;
        reopened.append(assistant("done")).unwrap();
        assert_eq!(Session::open(&path).unwrap().messages().len(), 4);
    }

    #[test]
    fn append_rejects_non_messages() {
        let e = env();
        let mut s = new_session(&e);
        assert!(s.append(json!("just a string")).is_err());
        assert!(s.append(json!({"content": "no role"})).is_err());
        assert!(s.messages().is_empty());
    }

    #[test]
    fn ephemeral_sessions_touch_no_disk() {
        let mut s = Session::ephemeral();
        s.append(user("x")).unwrap();
        s.compact("sum".into(), 0).unwrap();
        s.append(user("y")).unwrap();
        assert!(s.path().is_none());
        assert_eq!(contents(&s.messages()).len(), 2);
        assert!(s.messages()[0]["content"].as_str().unwrap().contains("sum"));
    }

    #[test]
    fn compaction_keeps_tail_summary_first_and_survives_reopen() {
        let e = env();
        let mut s = new_session(&e);
        for i in 0..4 {
            s.append(user(&format!("q{i}"))).unwrap();
            s.append(assistant(&format!("a{i}"))).unwrap();
        }
        s.compact("SUMMARY-ONE".into(), 3).unwrap();
        let m = s.messages();
        assert_eq!(m[0]["role"], "user");
        let head = m[0]["content"].as_str().unwrap();
        assert!(
            head.starts_with("<summary>") && head.contains("SUMMARY-ONE"),
            "{head}"
        );
        assert_eq!(contents(&m[1..]), ["a2", "q3", "a3"].map(String::from));

        s.append(user("after")).unwrap();
        let reopened = Session::open(s.path().unwrap()).unwrap();
        assert_eq!(reopened.messages(), s.messages());
        assert_eq!(
            contents(&reopened.messages()[1..]),
            ["a2", "q3", "a3", "after"].map(String::from)
        );

        // A second compaction replaces the first summary and counts only what is still in view.
        let mut s = reopened;
        s.compact("SUMMARY-TWO".into(), 1).unwrap();
        let m = s.messages();
        assert_eq!(m.len(), 2);
        assert!(m[0]["content"].as_str().unwrap().contains("SUMMARY-TWO"));
        assert!(!m[0]["content"].as_str().unwrap().contains("SUMMARY-ONE"));
        assert_eq!(content_text(&m[1]), "after");
    }

    #[test]
    fn compaction_never_starts_on_an_orphaned_tool_result() {
        let e = env();
        let mut s = new_session(&e);
        s.append(user("go")).unwrap();
        s.append(json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]})).unwrap();
        s.append(json!({"role": "tool", "tool_call_id": "c", "content": "out"}))
            .unwrap();
        s.append(assistant("finished")).unwrap();
        s.compact("S".into(), 2).unwrap(); // would cut exactly at the tool result
        let m = s.messages();
        assert_eq!(m[1]["role"], "assistant");
        assert!(m[1]["tool_calls"].is_array());
        assert_eq!(m[2]["role"], "tool");
        s.compact("S2".into(), 0).unwrap();
        assert_eq!(s.messages().len(), 1);
    }

    #[test]
    fn rewind_branches_without_deleting_history() {
        let e = env();
        let mut s = new_session(&e);
        for i in 1..=3 {
            s.append(user(&format!("u{i}"))).unwrap();
            s.append(assistant(&format!("a{i}"))).unwrap();
        }
        assert_eq!(s.rewind(0).unwrap(), 0);
        assert_eq!(s.rewind(1).unwrap(), 2);
        assert_eq!(
            contents(&s.messages()),
            ["u1", "a1", "u2", "a2"].map(String::from)
        );
        s.append(user("u3b")).unwrap();
        let path = s.path().unwrap().to_path_buf();

        let reopened = Session::open(&path).unwrap();
        assert_eq!(
            contents(&reopened.messages()),
            ["u1", "a1", "u2", "a2", "u3b"].map(String::from)
        );
        // The abandoned branch is still in the file.
        assert!(fs::read_to_string(&path).unwrap().contains("\"u3\""));

        let mut s = reopened;
        assert_eq!(s.rewind(2).unwrap(), 3); // u2 a2 u3b
        assert_eq!(contents(&s.messages()), ["u1", "a1"].map(String::from));
        // More turns than exist clamps to the start.
        assert_eq!(s.rewind(99).unwrap(), 2);
        assert!(s.messages().is_empty());
        s.append(user("fresh")).unwrap();
        assert_eq!(
            contents(&Session::open(&path).unwrap().messages()),
            ["fresh".to_string()]
        );
        assert_eq!(Session::open(&path).unwrap().rewind(1).unwrap(), 1);
    }

    #[test]
    fn fork_copies_only_the_active_branch_and_diverges() {
        let e = env();
        let mut s = new_session(&e);
        s.append(user("u1")).unwrap();
        s.append(assistant("a1")).unwrap();
        s.append(user("u2")).unwrap();
        s.rewind(1).unwrap();
        s.append(user("u2b")).unwrap();
        s.compact("SUM".into(), 2).unwrap();

        let mut f = s.fork(&e.layout).unwrap();
        assert_ne!(f.id(), s.id());
        assert_ne!(f.path(), s.path());
        assert_eq!(f.messages(), s.messages());
        let text = fs::read_to_string(f.path().unwrap()).unwrap();
        assert!(!text.contains("\"u2\"") && text.contains("forked_from"));
        f.append(user("only in fork")).unwrap();
        let reopened = Session::open(f.path().unwrap()).unwrap();
        assert_eq!(reopened.messages(), f.messages());
        assert_eq!(s.messages().len(), 3);
    }

    #[test]
    fn list_is_newest_first_with_titles_and_counts() {
        let e = env();
        assert!(Session::list(&e.layout, 10).unwrap().is_empty());
        assert!(Session::latest(&e.layout).unwrap().is_none());
        let mut ids = Vec::new();
        for i in 0..3 {
            let mut s = new_session(&e);
            s.append(user(&format!("topic   number {i}\nwith newline")))
                .unwrap();
            s.append(assistant("ok")).unwrap();
            ids.push(s.id().to_string());
        }
        let long = {
            let mut s = new_session(&e);
            s.append(user(&"x".repeat(500))).unwrap();
            s.id().to_string()
        };
        let list = Session::list(&e.layout, 10).unwrap();
        let order: Vec<_> = list.iter().map(|i| i.id.clone()).collect();
        assert_eq!(
            order,
            [long.clone(), ids[2].clone(), ids[1].clone(), ids[0].clone()]
        );
        assert_eq!(list[1].title, "topic number 2 with newline");
        assert_eq!(list[1].messages, 2);
        assert!(list[0].title.chars().count() <= TITLE_CHARS + 1 && list[0].title.ends_with('…'));
        assert!(list[0].started.ends_with('Z'));
        assert_eq!(Session::list(&e.layout, 2).unwrap().len(), 2);
        assert_eq!(Session::latest(&e.layout).unwrap().unwrap(), list[0].path);
    }

    #[test]
    fn truncated_or_corrupt_tail_is_skipped_and_healed_on_next_append() {
        let e = env();
        let mut s = new_session(&e);
        s.append(user("one")).unwrap();
        s.append(assistant("two")).unwrap();
        let path = s.path().unwrap().to_path_buf();
        drop(s);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(
            b"{\"type\":\"message\",\"id\":\"zz\",\"parent\":null,\"message\":{\"role\":\"us",
        )
        .unwrap();
        drop(f);

        let mut s = Session::open(&path).unwrap();
        assert_eq!(contents(&s.messages()), ["one", "two"].map(String::from));
        s.append(user("three")).unwrap();
        let s = Session::open(&path).unwrap();
        assert_eq!(
            contents(&s.messages()),
            ["one", "two", "three"].map(String::from)
        );

        // Garbage and blank lines in the middle are skipped too.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"\nnot json at all\n[1,2]\n").unwrap();
        drop(f);
        let mut s = Session::open(&path).unwrap();
        s.append(user("four")).unwrap();
        assert_eq!(Session::open(&path).unwrap().messages().len(), 4);
    }

    #[test]
    fn ids_from_disk_never_reach_paths_and_oversized_lines_are_skipped() {
        let e = env();
        let dir = e.layout.sessions_dir();
        private_dir(&dir).unwrap();
        let path = dir.join("20260101T000000000Z_abc123.jsonl");
        let huge = "y".repeat(MAX_LINE + 10);
        let body = format!(
            "{}\n{}\n{}\n",
            json!({"type": "session", "id": "../../etc/passwd", "started": "2026-01-01T00:00:00.000Z", "model": "m"}),
            json!({"type": "message", "id": "m1", "parent": null, "ts": "t", "message": {"role": "user", "content": huge}}),
            json!({"type": "message", "id": "m2", "parent": "m1", "ts": "t", "message": {"role": "user", "content": "small"}}),
        );
        fs::write(&path, body).unwrap();
        let s = Session::open(&path).unwrap();
        assert_eq!(s.id(), "abc123");
        // m1 was dropped as oversized, so m2's parent is missing and it becomes a root.
        assert_eq!(contents(&s.messages()), ["small".to_string()]);

        let mut s = s;
        let err = s.append(user(&"z".repeat(MAX_LINE))).unwrap_err();
        assert!(err.to_string().contains("line limit"), "{err}");
        assert_eq!(s.messages().len(), 1);
        assert!(Session::open(&dir.join("nope.jsonl")).is_err());
    }

    #[test]
    fn concurrent_writers_never_interleave_lines() {
        let e = env();
        let first = new_session(&e);
        let path = first.path().unwrap().to_path_buf();
        drop(first);
        let big = "p".repeat(100_000);
        let handles: Vec<_> = (0..2)
            .map(|w| {
                let (path, big) = (path.clone(), big.clone());
                std::thread::spawn(move || {
                    let mut s = Session::open(&path).unwrap();
                    for i in 0..60 {
                        s.append(user(&format!("w{w}-{i}-{big}"))).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let text = fs::read_to_string(&path).unwrap();
        let entries: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is intact JSON"))
            .collect();
        let messages: HashSet<_> = entries
            .iter()
            .filter(|e| e["type"] == "message")
            .map(|e| content_text(&e["message"]))
            .collect();
        let expected: HashSet<_> = (0..2)
            .flat_map(|w| {
                (0..60).map({
                    let big = &big;
                    move |i| format!("w{w}-{i}-{big}")
                })
            })
            .collect();
        assert_eq!(
            messages, expected,
            "every concurrent append keeps its full payload"
        );
    }

    #[test]
    fn search_ranks_stems_and_follows_new_appends() {
        let e = env();
        let mut a = new_session(&e);
        a.append(user("how do I configure the database migrations?"))
            .unwrap();
        a.append(assistant("Run the migration tool against postgres."))
            .unwrap();
        let mut b = new_session(&e);
        b.append(user("unrelated chat about sourdough bread"))
            .unwrap();
        b.append(json!({"role": "assistant", "content": null, "tool_calls": [{"id": "c", "type": "function", "function": {"name": "frobnicate", "arguments": "{\"secret\":\"zebra\"}"}}]})).unwrap();
        b.append(json!({"role": "tool", "tool_call_id": "c", "content": format!("kiwi {}", "filler ".repeat(400))})).unwrap();
        b.append(
            json!({"role": "tool", "tool_call_id": "c", "content": "small tool output mango"}),
        )
        .unwrap();

        let hits = Session::search(&e.layout, "migrations postgres", 5).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].session_id, a.id());
        assert_eq!(hits[0].role, "assistant"); // both words in one message ranks above one word
        assert!(hits[0].snippet.to_lowercase().contains("postgres"));
        assert!(hits[0].when.ends_with('Z'));
        // Stemming: "migrate" finds "migrations"/"migration".
        assert!(!Session::search(&e.layout, "migrate", 5).unwrap().is_empty());

        assert_eq!(
            Session::search(&e.layout, "frobnicate", 5).unwrap()[0].session_id,
            b.id()
        );
        assert!(
            Session::search(&e.layout, "zebra", 5).unwrap().is_empty(),
            "tool arguments are not indexed"
        );
        assert!(
            Session::search(&e.layout, "kiwi", 5).unwrap().is_empty(),
            "large tool bodies are not indexed"
        );
        assert_eq!(Session::search(&e.layout, "mango", 5).unwrap().len(), 1);
        // AND fails across messages -> falls back to OR.
        assert_eq!(
            Session::search(&e.layout, "sourdough postgres", 5)
                .unwrap()
                .len(),
            2
        );

        assert!(Session::search(&e.layout, "quokka", 5).unwrap().is_empty());
        b.append(user("now we discuss quokka habitats")).unwrap();
        assert_eq!(
            Session::search(&e.layout, "quokka", 5).unwrap()[0].session_id,
            b.id()
        );
        assert_eq!(
            Session::search(&e.layout, "migrations OR sourdough", 1)
                .unwrap()
                .len(),
            1
        );

        let db = e.layout.state_dir.join("sessions.sqlite3");
        assert_eq!(
            fs::metadata(db).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn search_queries_cannot_inject_fts_syntax() {
        let e = env();
        let mut s = new_session(&e);
        s.append(user("foo and bar")).unwrap();
        s.append(user("OR is a word 1 time")).unwrap();
        for q in [
            "foo\" OR 1",
            "*",
            "NEAR(",
            "",
            "   ",
            "\"",
            "text:foo",
            "foo AND NOT bar",
            "(((",
            "foo*",
            "^foo",
            "a:b",
            "\\",
            "-foo",
            "{foo}",
            "foo\0bar",
        ] {
            let r = Session::search(&e.layout, q, 5);
            assert!(r.is_ok(), "query {q:?} errored: {:?}", r.err());
        }
        assert!(Session::search(&e.layout, "", 5).unwrap().is_empty());
        assert!(Session::search(&e.layout, "*", 5).unwrap().is_empty());
        // Operators are plain words: "foo\" OR 1" is the terms foo, OR, 1.
        let hits = Session::search(&e.layout, "foo\" OR 1", 5).unwrap();
        assert!(!hits.is_empty());
        assert!(
            !Session::search(&e.layout, "nonexistentword AND foo", 5)
                .unwrap()
                .is_empty()
        ); // OR fallback, not an FTS AND
    }

    #[test]
    fn search_without_sessions_is_empty_and_forks_do_not_duplicate_hits() {
        let e = env();
        assert!(
            Session::search(&e.layout, "anything", 5)
                .unwrap()
                .is_empty()
        );
        assert!(!e.layout.state_dir.join("sessions.sqlite3").exists());
        let mut s = new_session(&e);
        s.append(user("the platypus problem")).unwrap();
        let mut f = s.fork(&e.layout).unwrap();
        assert_eq!(Session::search(&e.layout, "platypus", 10).unwrap().len(), 1);
        f.append(user("fork-only echidna")).unwrap();
        assert_eq!(
            Session::search(&e.layout, "echidna", 10).unwrap()[0].session_id,
            f.id()
        );
        assert_eq!(Session::search(&e.layout, "platypus", 10).unwrap().len(), 1);
        // Deleting a session file removes its hits.
        fs::remove_file(s.path().unwrap()).unwrap();
        assert!(
            Session::search(&e.layout, "platypus", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn search_tool_formats_hits_and_reports_bad_args() {
        let e = env();
        let mut s = new_session(&e);
        s.append(user("remember the wombat decision")).unwrap();
        let out = session_search_tool::call(&e.layout, &json!({"query": "wombat"}));
        assert!(
            !out.is_error && out.text.contains("wombat") && out.text.contains(s.id()),
            "{}",
            out.text
        );
        assert!(!session_search_tool::call(&e.layout, &json!({"query": "zzzz"})).is_error);
        assert!(session_search_tool::call(&e.layout, &json!({"limit": 3})).is_error);
        assert!(session_search_tool::call(&e.layout, &json!({"query": 7})).is_error);
        let schema = session_search_tool::schema();
        assert_eq!(schema["function"]["name"], "session_search");
        assert_eq!(
            schema["function"]["parameters"]["required"],
            json!(["query"])
        );
    }

    #[test]
    fn iso_formats_known_instants() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(951_782_400_000 + 1_234), "2000-02-29T00:00:01.234Z");
        assert_eq!(iso(1_790_000_000_000), "2026-09-21T14:13:20.000Z");
    }
}
