//! Import of foreign Claude Code and Codex transcripts into Dial sessions (`--from-claude` / `--from-codex`).
//!
//! `convert_claude` and `convert_codex` are pure: they map raw JSONL records onto Dial's
//! OpenAI-style message list and count every dropped content kind in `Transcript::skipped`.
//! `persist` writes the result through the public `Session` API.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::paths::Layout;
use crate::storage::session::{ImportOrigin, Session, SessionMeta};

/// Summary used when a Codex `compacted` record carries no message.
const CODEX_COMPACTION_FALLBACK: &str = "Context compacted by Codex.";

/// Codex events that carry tool activity Dial cannot represent; they are counted, not imported.
const CODEX_DROPPED_EVENTS: &[&str] = &[
    "dynamic_tool_call_request",
    "dynamic_tool_call_response",
    "web_search_end",
    "mcp_tool_call_end",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Claude,
    Codex,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    /// Suffix of the `--from-<flag>` argument.
    pub fn flag(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// A Dial-side compaction: messages before `at` are replaced by `summary`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    pub summary: String,
    pub at: usize,
}

#[derive(Debug, Default, PartialEq)]
pub struct Transcript {
    pub source_id: Option<String>,
    pub source_cwd: Option<String>,
    /// Source start timestamp (ISO-8601), carried into the Dial header as `started`.
    pub source_started: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub messages: Vec<Value>,
    pub compaction: Option<Compaction>,
    /// Content that was not imported, keyed by kind, with counts.
    pub skipped: BTreeMap<String, usize>,
}

impl Transcript {
    fn skip(&mut self, what: &str) {
        self.skip_n(what, 1);
    }

    fn skip_n(&mut self, what: &str, n: usize) {
        if n > 0 {
            *self.skipped.entry(what.to_string()).or_default() += n;
        }
    }

    /// Human-readable list of dropped content, e.g. `"2 thinking, 1 image"`. Empty when nothing was dropped.
    pub fn skipped_summary(&self) -> String {
        self.skipped
            .iter()
            .map(|(kind, count)| format!("{count} {kind}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ---------------------------------------------------------------------------
// Claude Code transcripts
// ---------------------------------------------------------------------------

/// Converts Claude Code records. Only the active branch is imported: the chain of `parentUuid`
/// links ending at the last record in the file (Claude appends each message to the current leaf).
pub fn convert_claude(records: &[Value]) -> Transcript {
    let mut t = Transcript {
        source_id: records
            .iter()
            .find_map(|r| r["sessionId"].as_str())
            .map(str::to_string),
        source_cwd: records
            .iter()
            .find_map(|r| r["cwd"].as_str())
            .map(str::to_string),
        source_started: records
            .iter()
            .find_map(|r| r["timestamp"].as_str())
            .map(str::to_string),
        title: claude_title(records),
        ..Transcript::default()
    };

    let by_uuid: HashMap<&str, &Value> = records
        .iter()
        .filter_map(|r| Some((r["uuid"].as_str()?, r)))
        .collect();
    let mut chain: Vec<&Value> = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = records.iter().rev().find_map(|r| r["uuid"].as_str());
    while let Some(id) = cursor {
        if !seen.insert(id) {
            break;
        }
        let Some(&rec) = by_uuid.get(id) else { break };
        chain.push(rec);
        cursor = rec["parentUuid"].as_str();
    }
    chain.reverse();

    // Consecutive assistant records sharing a message id are one reply split per content block.
    let mut open: Option<(String, usize)> = None;
    for rec in chain {
        let kind = rec["type"].as_str().unwrap_or_default();
        if kind != "user" && kind != "assistant" {
            continue;
        }
        if rec["isMeta"] == true {
            continue;
        }
        if rec["isSidechain"] == true {
            t.skip("sidechain record");
            continue;
        }
        if rec["isApiErrorMessage"] == true {
            t.skip("API error record");
            continue;
        }
        let msg = &rec["message"];

        if kind == "user" {
            open = None;
            match &msg["content"] {
                Value::String(s) => push_text(&mut t, "user", s),
                Value::Array(blocks) => {
                    let mut text = Vec::new();
                    for b in blocks {
                        match b["type"].as_str() {
                            Some("text") => {
                                text.push(b["text"].as_str().unwrap_or_default().to_string())
                            }
                            Some("tool_result") => {
                                flush_user(&mut t, &mut text);
                                let content = claude_result_text(&b["content"], &mut t);
                                let mut m = json!({
                                    "role": "tool",
                                    "tool_call_id": b["tool_use_id"].as_str().unwrap_or_default(),
                                    "content": content,
                                });
                                if b["is_error"] == true {
                                    m["is_error"] = json!(true);
                                }
                                t.messages.push(m);
                            }
                            Some(other) => t.skip(other),
                            None => t.skip("unknown block"),
                        }
                    }
                    flush_user(&mut t, &mut text);
                }
                _ => {}
            }
            continue;
        }

        if let Some(model) = msg["model"]
            .as_str()
            .filter(|m| !m.is_empty() && *m != "<synthetic>")
        {
            t.model = Some(model.to_string());
        }
        let id = msg["id"].as_str().map(str::to_string);
        let idx = match (&open, &id) {
            (Some((o, at)), Some(i)) if o == i => *at,
            _ => {
                t.messages
                    .push(json!({"role": "assistant", "content": null}));
                let at = t.messages.len() - 1;
                open = id.clone().map(|i| (i, at));
                at
            }
        };

        let blocks: Vec<Value> = match &msg["content"] {
            Value::String(s) => vec![json!({"type": "text", "text": s})],
            Value::Array(a) => a.clone(),
            _ => Vec::new(),
        };
        for b in &blocks {
            match b["type"].as_str() {
                Some("text") => {
                    append_text(&mut t.messages[idx], b["text"].as_str().unwrap_or_default())
                }
                Some("tool_use") => {
                    let arguments =
                        serde_json::to_string(&b["input"]).unwrap_or_else(|_| "{}".to_string());
                    push_tool_call(
                        &mut t.messages[idx],
                        json!({
                            "id": b["id"].clone(),
                            "type": "function",
                            "function": {"name": b["name"].clone(), "arguments": arguments},
                        }),
                    );
                }
                Some(other) => t.skip(other),
                None => t.skip("unknown block"),
            }
        }
        // A freshly pushed message is always the last element; drop it if nothing survived.
        if t.messages[idx]["content"].is_null() && t.messages[idx]["tool_calls"].is_null() {
            t.messages.remove(idx);
            open = None;
        }
    }
    t
}

fn claude_title(records: &[Value]) -> Option<String> {
    let last = |kind: &str, key: &str| {
        records
            .iter()
            .rev()
            .filter(|r| r["type"] == kind)
            .find_map(|r| r[key].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    last("custom-title", "customTitle").or_else(|| last("ai-title", "aiTitle"))
}

fn claude_result_text(content: &Value, t: &mut Transcript) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for b in blocks {
                match b["type"].as_str() {
                    Some("text") => parts.push(b["text"].as_str().unwrap_or_default().to_string()),
                    Some(other) => t.skip(other),
                    None => t.skip("unknown block"),
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn flush_user(t: &mut Transcript, text: &mut Vec<String>) {
    if !text.is_empty() {
        let joined = text.join("\n\n");
        text.clear();
        push_text(t, "user", &joined);
    }
}

// ---------------------------------------------------------------------------
// Codex rollouts
// ---------------------------------------------------------------------------

/// Converts Codex rollout records. Event messages duplicating a `response_item` message are dropped;
/// `thread_rolled_back` removes the last N user turns; `compacted` becomes a Dial compaction.
pub fn convert_codex(records: &[Value]) -> Transcript {
    let mut t = Transcript::default();
    let canonical: HashSet<(String, String)> = records
        .iter()
        .filter(|r| r["type"] == "response_item" && r["payload"]["type"] == "message")
        .map(|r| {
            (
                r["payload"]["role"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                codex_text(&r["payload"]["content"]),
            )
        })
        .collect();

    for r in records {
        let p = &r["payload"];
        match r["type"].as_str().unwrap_or_default() {
            "session_meta" => {
                if t.source_id.is_none() {
                    t.source_id = p["id"].as_str().map(str::to_string);
                }
                if t.source_cwd.is_none() {
                    t.source_cwd = p["cwd"].as_str().map(str::to_string);
                }
                if t.source_started.is_none() {
                    t.source_started = r["timestamp"]
                        .as_str()
                        .or_else(|| p["timestamp"].as_str())
                        .map(str::to_string);
                }
            }
            "turn_context" => {
                if let Some(model) = p["model"].as_str().filter(|m| !m.is_empty()) {
                    t.model = Some(model.to_string());
                }
            }
            "compacted" => {
                let summary = p["message"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(CODEX_COMPACTION_FALLBACK);
                t.compaction = Some(Compaction {
                    summary: summary.to_string(),
                    at: t.messages.len(),
                });
            }
            "response_item" => codex_response_item(&mut t, p),
            "event_msg" => codex_event(&mut t, p, &canonical),
            _ => {}
        }
    }
    if t.compaction.as_ref().is_some_and(|c| c.at == 0) {
        t.compaction = None;
        t.skip("compaction marker before first message");
    }
    t
}

fn codex_response_item(t: &mut Transcript, p: &Value) {
    match p["type"].as_str().unwrap_or_default() {
        "message" => {
            let role = p["role"].as_str().unwrap_or_default();
            t.skip_n("image", count_images(&p["content"]));
            match role {
                "user" | "assistant" => push_text(t, role, &codex_text(&p["content"])),
                other => t.skip(&format!("{other} message")),
            }
        }
        "reasoning" => t.skip("reasoning"),
        kind @ ("function_call" | "custom_tool_call") => {
            let args = if kind == "function_call" {
                &p["arguments"]
            } else {
                &p["input"]
            };
            push_call(
                t,
                p["call_id"].as_str().unwrap_or_default(),
                p["name"].as_str().unwrap_or_default(),
                value_text(args),
            );
        }
        "function_call_output" | "custom_tool_call_output" => {
            t.messages.push(json!({
                "role": "tool",
                "tool_call_id": p["call_id"].as_str().unwrap_or_default(),
                "content": output_text(&p["output"]),
            }));
        }
        other => t.skip(&format!("response item {other}")),
    }
}

fn codex_event(t: &mut Transcript, p: &Value, canonical: &HashSet<(String, String)>) {
    match p["type"].as_str().unwrap_or_default() {
        kind @ ("user_message" | "agent_message") => {
            let role = if kind == "user_message" {
                "user"
            } else {
                "assistant"
            };
            let text = p["message"].as_str().unwrap_or_default();
            if !canonical.contains(&(role.to_string(), text.to_string())) {
                push_text(t, role, text);
            }
        }
        "agent_reasoning" => t.skip("reasoning"),
        "thread_name_updated" => {
            if let Some(name) = p["thread_name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                t.title = Some(name.to_string());
            }
        }
        "thread_rolled_back" => rollback(t, p["num_turns"].as_u64().unwrap_or(0) as usize),
        other if CODEX_DROPPED_EVENTS.contains(&other) => t.skip(other),
        _ => {}
    }
}

/// Removes the last `turns` user turns and everything after the earliest of them.
fn rollback(t: &mut Transcript, turns: usize) {
    if turns == 0 {
        return;
    }
    let mut remaining = turns;
    let mut cut = 0;
    for (i, m) in t.messages.iter().enumerate().rev() {
        if m["role"] == "user" {
            if remaining == 1 {
                cut = i;
                break;
            }
            remaining -= 1;
        }
    }
    t.messages.truncate(cut);
    if t.compaction.as_ref().is_some_and(|c| c.at > cut) {
        t.compaction = None;
    }
}

fn codex_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn count_images(content: &Value) -> usize {
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|p| matches!(p["type"].as_str(), Some("input_image" | "image")))
                .count()
        })
        .unwrap_or(0)
}

fn output_text(output: &Value) -> String {
    match output {
        Value::String(s) => s.clone(),
        Value::Object(_) if output["content"].is_string() => {
            output["content"].as_str().unwrap_or_default().to_string()
        }
        other => other.to_string(),
    }
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn push_call(t: &mut Transcript, id: &str, name: &str, arguments: String) {
    let call = json!({
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    });
    match t.messages.last_mut() {
        Some(last) if last["role"] == "assistant" => push_tool_call(last, call),
        _ => t
            .messages
            .push(json!({"role": "assistant", "content": null, "tool_calls": [call]})),
    }
}

// ---------------------------------------------------------------------------
// Shared message helpers
// ---------------------------------------------------------------------------

fn push_text(t: &mut Transcript, role: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    t.messages.push(json!({"role": role, "content": text}));
}

fn append_text(msg: &mut Value, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let joined = match msg["content"].as_str() {
        Some(prev) => format!("{prev}\n\n{text}"),
        None => text.to_string(),
    };
    msg["content"] = Value::String(joined);
}

fn push_tool_call(msg: &mut Value, call: Value) {
    if !msg["tool_calls"].is_array() {
        msg["tool_calls"] = json!([]);
    }
    if let Some(calls) = msg["tool_calls"].as_array_mut() {
        calls.push(call);
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// A foreign transcript found on disk.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub id: String,
    pub title: Option<String>,
    pub modified: SystemTime,
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// `CLAUDE_CONFIG_DIR`, or `~/.claude`.
pub fn claude_root() -> PathBuf {
    env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home_dir().join(".claude"))
}

/// `CODEX_HOME`, or `~/.codex`.
pub fn codex_root() -> PathBuf {
    env_dir("CODEX_HOME").unwrap_or_else(|| home_dir().join(".codex"))
}

/// Reads a JSONL file, returning parsed records and the number of malformed lines.
fn read_records(path: &Path) -> Result<(Vec<Value>, usize)> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut records = Vec::new();
    let mut malformed = 0;
    // Per-line decoding: one torn or non-UTF-8 line must not discard the whole transcript.
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
    {
        match serde_json::from_slice::<Value>(line) {
            Ok(v) => records.push(v),
            Err(_) => malformed += 1,
        }
    }
    Ok((records, malformed))
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            walk_jsonl(&path, out);
        } else if path.extension().is_some_and(|x| x == "jsonl") {
            out.push(path);
        }
    }
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn newest_first(mut list: Vec<Candidate>) -> Vec<Candidate> {
    list.sort_by_key(|candidate| std::cmp::Reverse(candidate.modified));
    list
}

/// Claude transcripts under `<root>/projects/**` whose recorded cwd equals `cwd`, newest first.
pub fn list_claude(root: &Path, cwd: &str) -> Vec<Candidate> {
    let mut files = Vec::new();
    for dir in ["projects", ".projects"] {
        walk_jsonl(&root.join(dir), &mut files);
    }
    let mut out = Vec::new();
    for path in files {
        let Ok((records, _)) = read_records(&path) else {
            continue;
        };
        if records.iter().find_map(|r| r["cwd"].as_str()) != Some(cwd) {
            continue;
        }
        let id = records
            .iter()
            .find_map(|r| r["sessionId"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| file_stem(&path));
        out.push(Candidate {
            title: claude_title(&records),
            modified: modified(&path),
            id,
            path,
        });
    }
    newest_first(out)
}

/// Codex rollouts under `<root>/sessions/**` and `<root>/archived_sessions/**` whose `session_meta`
/// cwd equals `cwd`, newest first. Titles come from `session_index.jsonl`.
pub fn list_codex(root: &Path, cwd: &str) -> Vec<Candidate> {
    let index = codex_index(root);
    let mut files = Vec::new();
    for dir in ["sessions", "archived_sessions"] {
        walk_jsonl(&root.join(dir), &mut files);
    }
    let mut out = Vec::new();
    for path in files {
        let Ok((records, _)) = read_records(&path) else {
            continue;
        };
        let Some(meta) = records.iter().find(|r| r["type"] == "session_meta") else {
            continue;
        };
        if meta["payload"]["cwd"].as_str() != Some(cwd) {
            continue;
        }
        let id = meta["payload"]["id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| file_stem(&path));
        let title = index.get(&id).cloned().or_else(|| codex_title(&records));
        out.push(Candidate {
            modified: modified(&path),
            title,
            id,
            path,
        });
    }
    newest_first(out)
}

fn codex_title(records: &[Value]) -> Option<String> {
    records
        .iter()
        .rev()
        .filter(|r| r["type"] == "event_msg" && r["payload"]["type"] == "thread_name_updated")
        .find_map(|r| r["payload"]["thread_name"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn codex_index(root: &Path) -> HashMap<String, String> {
    let Ok((records, _)) = read_records(&root.join("session_index.jsonl")) else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for r in records {
        if let (Some(id), Some(name)) = (
            r["id"].as_str(),
            r["thread_name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ) {
            map.insert(id.to_string(), name.to_string());
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// Outcome of importing one transcript file.
pub struct Imported {
    pub session: Session,
    pub source_id: Option<String>,
    /// Dropped content, as produced by `Transcript::skipped_summary`. Empty when nothing was dropped.
    pub skipped: String,
}

/// Reads a foreign transcript file, converts it, and persists it as a new Dial session under `layout`.
pub fn import_file(layout: &Layout, source: Source, path: &Path) -> Result<Imported> {
    let (records, malformed) = read_records(path)?;
    let mut t = match source {
        Source::Claude => convert_claude(&records),
        Source::Codex => convert_codex(&records),
    };
    t.skip_n("malformed line", malformed);
    if source == Source::Codex
        && t.title.is_none()
        && let Some(id) = &t.source_id
    {
        t.title = codex_index(&codex_root()).remove(id);
    }
    let source_id = t.source_id.clone();
    let skipped = t.skipped_summary();
    let session = persist(layout, t)?;
    Ok(Imported {
        session,
        source_id,
        skipped,
    })
}

/// Writes a converted transcript through the public `Session` API. The Dial header records the
/// source model, start time, and working directory.
pub fn persist(layout: &Layout, t: Transcript) -> Result<Session> {
    let Transcript {
        title,
        model,
        source_started,
        source_cwd,
        messages,
        compaction,
        ..
    } = t;
    let origin = ImportOrigin {
        started: source_started,
        cwd: source_cwd.map(PathBuf::from),
    };
    let mut session = Session::create_imported(
        layout,
        SessionMeta {
            model: model.unwrap_or_default(),
        },
        &origin,
    )?;
    if let Err(error) = fill_imported(&mut session, messages, compaction, title) {
        // A half-written import is not a usable session; remove it rather than leave it listed.
        let _ = session.remove_file();
        return Err(error);
    }
    Ok(session)
}

fn fill_imported(
    session: &mut Session,
    messages: Vec<Value>,
    compaction: Option<Compaction>,
    title: Option<String>,
) -> Result<()> {
    let total = messages.len();
    for message in messages {
        session.append(message)?;
    }
    if let Some(c) = compaction {
        session.compact(c.summary, total - c.at)?;
    }
    if let Some(title) = title {
        session.rename(&title)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
