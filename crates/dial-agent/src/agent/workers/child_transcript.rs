//! Durable, redacted record of one child job's conversation.
//!
//! Each job writes `<state_dir>/subagents/<parent session>/<job>-<millis>.jsonl` outside the
//! workspace. The first line is a header; every archived message follows as one record, and a
//! `status` record closes the file. Lines are redacted and bounded, and readers only touch the
//! tail of a file so the viewer never loads a whole transcript.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LazyLock, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config::paths::Layout;
use crate::storage::redact::redact_value;
use crate::storage::subagent::safe_component;

/// Upper bound for one record; larger messages are replaced by a marker line.
pub const MAX_RECORD: usize = 256 * 1024;
/// Bytes read from the end of a transcript when the viewer loads it.
pub const TAIL_BYTES: u64 = 2 * 1024 * 1024;
/// Most in-flight tool calls one job exposes to live views; extra starts are not tracked.
const MAX_LIVE_TOOLS: usize = 64;
/// Longest line that can be a message record: a writer keeps at most `MAX_RECORD` bytes of
/// message plus a short envelope. Longer lines are never messages and are skipped unbuffered.
pub const MAX_LINE: usize = MAX_RECORD + 16 * 1024;
/// Largest header line a reader accepts; a larger first line is not a transcript header.
pub const MAX_HEADER: usize = 1024 * 1024;
/// Bytes read per [`MessageFollower::follow`] call. Always larger than [`MAX_LINE`].
pub const FOLLOW_CHUNK: usize = 4 << 20;
const _: () = assert!(FOLLOW_CHUNK > MAX_LINE);
/// Bytes kept from the start and the end of the consumed prefix to detect a rewrite in place.
const ANCHOR: usize = 256;

#[derive(Default)]
struct PendingAssistant {
    revision: u64,
    text: String,
    overflow: bool,
    tools: Vec<LiveTool>,
}

impl PendingAssistant {
    fn changed(&mut self) {
        static REVISION: AtomicU64 = AtomicU64::new(1);
        self.revision = REVISION.fetch_add(1, Ordering::Relaxed);
    }
    fn clear(&mut self) {
        self.text.clear();
        self.overflow = false;
        self.changed();
    }
}

type PendingRegistry = HashMap<PathBuf, Weak<Mutex<PendingAssistant>>>;
static PENDING: LazyLock<Mutex<PendingRegistry>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Process-local public prose, redacted as one cumulative value before crossing into
/// the UI. No raw fragments are written to disk, even when a credential spans tokens.
pub fn pending_assistant(path: &Path, since: u64) -> Option<(u64, String)> {
    let pending = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)?
        .upgrade()?;
    let pending = pending.lock().unwrap_or_else(|e| e.into_inner());
    if pending.revision == since {
        return None;
    }
    let revision = pending.revision;
    let mut value = json!({"text":pending.text});
    drop(pending);
    redact_value(&mut value);
    Some((revision, value["text"].as_str().unwrap_or("").to_owned()))
}

/// A tool call of a running job whose result is not yet archived. Live views show it until the
/// settled tool message is recorded; the archive itself never carries these entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTool {
    pub call_id: String,
    pub name: String,
    /// `false` once the tool finished, while its result still waits for the ordered archive write.
    pub running: bool,
}

/// Live tool calls of the job at `path`, in start order. `since` works as for [`pending_assistant`].
pub fn live_tools(path: &Path, since: u64) -> Option<(u64, Vec<LiveTool>)> {
    let pending = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)?
        .upgrade()?;
    let pending = pending.lock().unwrap_or_else(|e| e.into_inner());
    if pending.revision == since {
        return None;
    }
    Some((pending.revision, pending.tools.clone()))
}
/// Append-only writer for one child job.
pub struct ChildTranscript {
    path: PathBuf,
    file: Mutex<File>,
    pending: Arc<Mutex<PendingAssistant>>,
}

impl ChildTranscript {
    /// Creates the transcript file and writes its header as the first line.
    pub fn create(layout: &Layout, parent_session: &str, job: &str, header: Value) -> Result<Self> {
        let dir = transcript_dir(layout, parent_session);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let path = dir.join(format!("{}-{millis}.jsonl", safe_component(job)));
        let mut options = OpenOptions::new();
        options.create_new(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        let transcript = Self {
            path,
            file: Mutex::new(file),
            pending: Arc::new(Mutex::new(PendingAssistant::default())),
        };
        let mut header = header;
        redact_value(&mut header);
        header["type"] = json!("header");
        transcript.write_line(&header)?;
        transcript
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .changed();
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(transcript.path.clone(), Arc::downgrade(&transcript.pending));
        Ok(transcript)
    }

    /// Appends one archived conversation message, redacted and bounded.
    pub fn record(&self, message: &Value) -> Result<()> {
        // Serialize assistant commit with process-local snapshot publication. The
        // authoritative message replaces the pending prose only after a successful write.
        if message["role"] == "assistant" {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            self.write_message(message)?;
            pending.clear();
            return Ok(());
        }
        self.write_message(message)?;
        if message["role"] == "tool"
            && let Some(id) = message["tool_call_id"].as_str()
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let before = pending.tools.len();
            pending.tools.retain(|tool| tool.call_id != id);
            if pending.tools.len() != before {
                pending.changed();
            }
        }
        Ok(())
    }

    fn write_message(&self, message: &Value) -> Result<()> {
        let mut value = message.clone();
        redact_value(&mut value);
        let line = serde_json::to_string(&value)?;
        if line.len() > MAX_RECORD {
            let marker = json!({
                "role": message["role"],
                "content": format!("[message truncated: {} bytes]", line.len()),
            });
            return self.write_line(&json!({"type": "message", "message": marker}));
        }
        self.write_line(&json!({"type": "message", "message": value}))
    }

    /// Accumulate public assistant prose in memory only. `None` discards a superseded
    /// attempt. Keep the whole bounded value for existing multiline/env redaction;
    /// overflow replaces all prose rather than retaining a cut credential prefix.
    pub fn assistant_stream(&self, text: Option<&str>) -> Result<()> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        match text {
            None => pending.clear(),
            Some(text) if !pending.overflow => {
                if pending.text.len().saturating_add(text.len()) > MAX_RECORD {
                    pending.text = "[assistant stream truncated: record limit exceeded]".into();
                    pending.overflow = true;
                } else {
                    pending.text.push_str(text);
                }
                pending.changed();
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// A tool call of this job started. Duplicate starts for one id are ignored.
    pub fn tool_started(&self, call_id: &str, name: &str) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.tools.len() >= MAX_LIVE_TOOLS
            || pending.tools.iter().any(|t| t.call_id == call_id)
        {
            return;
        }
        pending.tools.push(LiveTool {
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            running: true,
        });
        pending.changed();
    }

    /// A tool call of this job finished; its entry remains until the settled result is recorded.
    pub fn tool_finished(&self, call_id: &str) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tool) = pending
            .tools
            .iter_mut()
            .find(|t| t.running && t.call_id == call_id)
        {
            tool.running = false;
            pending.changed();
        }
    }

    fn flush_pending(&self) -> Result<()> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if !pending.text.is_empty() {
            self.write_message(&json!({"role":"assistant", "content":pending.text}))?;
            pending.clear();
        }
        Ok(())
    }

    pub fn execution_contract(&self, tools: &[String], model: &str) -> Result<()> {
        self.write_line(&json!({"type":"execution_contract", "tools":tools, "model":model}))
    }

    /// Records worker-only settings applied at a request boundary, so resume restores them.
    pub fn worker_settings(
        &self,
        settings: &crate::agent::workers::live_tasks::control::WorkerSettings,
    ) -> Result<()> {
        self.write_line(&json!({"type":"worker_settings", "model":settings.model, "thinking":settings.thinking}))
    }

    /// Closes the transcript with its final status.
    pub fn finish(&self, status: Value) -> Result<()> {
        self.flush_pending()?;
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if !pending.tools.is_empty() {
                pending.tools.clear();
                pending.changed();
            }
        }
        let mut status = status;
        redact_value(&mut status);
        let mut record = json!({"type": "status"});
        if let (Some(target), Some(source)) = (record.as_object_mut(), status.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        self.write_line(&record)
    }

    fn write_line(&self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        file.write_all(&line)
            .with_context(|| format!("writing {}", self.path.display()))?;
        file.flush()?;
        // Persist call intent before effects, and completed results before continuation.
        file.sync_data()?;
        Ok(())
    }
}

impl Drop for ChildTranscript {
    fn drop(&mut self) {
        // Recover interrupted prose through the same whole-message redaction boundary.
        let _ = self.flush_pending();
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.path);
    }
}

/// Directory holding transcripts for one parent session.
pub fn transcript_dir(layout: &Layout, parent_session: &str) -> PathBuf {
    crate::storage::subagent::transcript_dir(layout, parent_session)
}

/// Header and most recent status of one transcript, read without loading its messages.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptSummary {
    pub path: PathBuf,
    pub header: Value,
    pub status: Option<Value>,
}

/// Lists transcripts for a parent session, newest first.
pub fn scan(layout: &Layout, parent_session: &str) -> Vec<TranscriptSummary> {
    let Ok(entries) = fs::read_dir(transcript_dir(layout, parent_session)) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, TranscriptSummary)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            let header = read_first(&path)?;
            let status = read_status(&path);
            Some((
                modified,
                TranscriptSummary {
                    path,
                    header,
                    status,
                },
            ))
        })
        .collect();
    found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    found.into_iter().map(|(_, summary)| summary).collect()
}

/// Message records from the tail of a transcript, oldest first. Header and status are omitted.
pub fn messages(path: &Path) -> Result<Vec<Value>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let mut lines = bytes.split(|byte| *byte == b'\n');
    if start > 0 {
        // The first line may be cut mid-record; drop it.
        lines.next();
    }
    Ok(lines.filter_map(message_record).collect())
}

/// Every message record from the start of a transcript, oldest first. This is the index space of
/// `#<entry>` deep links: entry `i` is the `i`th message record in the file. It is read through
/// [`MessageFollower`], so live followers and deep links share one index space.
pub fn entries(path: &Path) -> Result<Vec<Value>> {
    let mut follower = MessageFollower::new(path);
    let mut out = Vec::new();
    loop {
        let progress = follower.follow(&mut |change| match change {
            Change::Reset => out.clear(),
            Change::Message(_, message) => out.push(message.clone()),
        })?;
        if progress == Follow::Unchanged {
            return Ok(out);
        }
    }
}

/// The message of one transcript line, if the line is a message record within [`MAX_LINE`].
fn message_record(line: &[u8]) -> Option<Value> {
    if line.len() > MAX_LINE {
        return None;
    }
    let record: Value = serde_json::from_slice(line).ok()?;
    (record["type"] == "message").then(|| record["message"].clone())
}

/// What one [`MessageFollower::follow`] call changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Follow {
    /// No complete record was consumed and nothing was reset.
    Unchanged,
    /// Complete records were consumed after the ones already emitted.
    Appended,
    /// The file was truncated, replaced or rewritten; a [`Change::Reset`] preceded the replay.
    Replaced,
}

/// One step of the message replay, in file order.
#[derive(Debug)]
pub enum Change<'a> {
    /// Discard every message emitted so far; the messages that follow replace them.
    Reset,
    /// Message record `index` (the `#entry` deep-link index) with its redacted value.
    Message(usize, &'a Value),
}

/// Incremental reader of the message records of one transcript. It keeps a byte offset, the file
/// identity and two short byte anchors, never the messages, so the caller owns replayed memory.
///
/// Only complete newline-terminated lines are consumed; a torn tail waits for its newline. A line
/// longer than [`MAX_LINE`] is skipped without being buffered. A file that was truncated, replaced
/// by another inode, or rewritten (its first or last consumed bytes differ) is replayed from the
/// start after a [`Change::Reset`]. An I/O error makes the next successful call begin with that
/// reset too, so a caller that drops its state on `Err` never mixes two versions of the file.
#[derive(Debug, Default)]
pub struct MessageFollower {
    path: PathBuf,
    /// Bytes consumed; always the end of a complete line or of a skipped oversized line.
    offset: u64,
    /// Device and inode of the file the offset refers to.
    identity: Option<(u64, u64)>,
    /// First bytes of the file as consumed, to catch an in-place rewrite at the start.
    head: Vec<u8>,
    /// Last bytes consumed, to catch an in-place rewrite at the end.
    anchor: Vec<u8>,
    /// Message records emitted since the last reset: the next absolute index.
    messages: usize,
    /// Inside an oversized line whose bytes are dropped up to its newline.
    discarding: bool,
    /// Set by an error; the next call resets the caller before replaying.
    reset_pending: bool,
    /// Bytes read from the file, including verification reads.
    bytes_read: u64,
}

impl MessageFollower {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }

    /// Message records emitted since the last reset.
    pub fn message_count(&self) -> usize {
        self.messages
    }

    /// Bytes read from the file so far, including the checks that detect a rewrite.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Emits what was appended since the last call, at most [`FOLLOW_CHUNK`] bytes per call.
    pub fn follow(&mut self, sink: &mut impl FnMut(Change<'_>)) -> Result<Follow> {
        let outcome = self.advance(sink);
        if outcome.is_err() {
            self.reset_pending = true;
        }
        outcome
    }

    fn advance(&mut self, sink: &mut impl FnMut(Change<'_>)) -> Result<Follow> {
        let mut file =
            File::open(&self.path).with_context(|| format!("opening {}", self.path.display()))?;
        let meta = file
            .metadata()
            .with_context(|| format!("reading {}", self.path.display()))?;
        let len = meta.len();
        let identity = file_identity(&meta);
        let mut replaced = false;
        if self.reset_pending || self.is_rewritten(&mut file, identity, len)? {
            sink(Change::Reset);
            self.restart();
            replaced = true;
        }
        let mut consumed = 0;
        if self.offset < len {
            let end = len.min(self.offset + FOLLOW_CHUNK as u64);
            let chunk = self.read_span(&mut file, self.offset, end)?;
            consumed = self.emit_lines(&chunk, sink);
            if consumed > 0 {
                self.commit(&mut file, identity, &chunk[..consumed])?;
            }
        }
        Ok(if replaced {
            Follow::Replaced
        } else if consumed > 0 {
            Follow::Appended
        } else {
            Follow::Unchanged
        })
    }

    /// Emits the records of the complete lines in `chunk`; returns the bytes they consumed.
    fn emit_lines(&mut self, chunk: &[u8], sink: &mut impl FnMut(Change<'_>)) -> usize {
        let mut pos = 0;
        while pos < chunk.len() {
            let rest = &chunk[pos..];
            match rest.iter().position(|byte| *byte == b'\n') {
                Some(newline) => {
                    if !self.discarding
                        && let Some(message) = message_record(&rest[..newline])
                    {
                        sink(Change::Message(self.messages, &message));
                        self.messages += 1;
                    }
                    self.discarding = false;
                    pos += newline + 1;
                }
                None if self.discarding || rest.len() > MAX_LINE => {
                    self.discarding = true;
                    pos = chunk.len();
                }
                None => break,
            }
        }
        pos
    }

    fn commit(&mut self, file: &mut File, identity: (u64, u64), consumed: &[u8]) -> Result<()> {
        self.offset += consumed.len() as u64;
        self.identity = Some(identity);
        self.anchor
            .extend_from_slice(&consumed[consumed.len().saturating_sub(ANCHOR)..]);
        let excess = self.anchor.len().saturating_sub(ANCHOR);
        self.anchor.drain(..excess);
        if self.head.len() < ANCHOR {
            self.head = self.read_span(file, 0, self.offset.min(ANCHOR as u64))?;
        }
        Ok(())
    }

    fn is_rewritten(&mut self, file: &mut File, identity: (u64, u64), len: u64) -> Result<bool> {
        if self.offset == 0 {
            return Ok(false);
        }
        if self.identity != Some(identity) || len < self.offset {
            return Ok(true);
        }
        let head = self.read_span(file, 0, self.head.len() as u64)?;
        if head != self.head {
            return Ok(true);
        }
        let start = self.offset - self.anchor.len() as u64;
        let anchor = self.read_span(file, start, self.offset)?;
        Ok(anchor != self.anchor)
    }

    fn restart(&mut self) {
        self.offset = 0;
        self.identity = None;
        self.head.clear();
        self.anchor.clear();
        self.messages = 0;
        self.discarding = false;
        self.reset_pending = false;
    }

    fn read_span(&mut self, file: &mut File, start: u64, end: u64) -> Result<Vec<u8>> {
        let wanted = end.saturating_sub(start);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity(wanted as usize);
        Read::by_ref(file).take(wanted).read_to_end(&mut bytes)?;
        self.bytes_read += bytes.len() as u64;
        Ok(bytes)
    }
}

#[cfg(unix)]
fn file_identity(meta: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_identity(_: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn read_first(path: &Path) -> Option<Value> {
    let file = File::open(path).ok()?;
    let mut line = Vec::new();
    BufReader::new(file.take(MAX_HEADER as u64 + 1))
        .read_until(b'\n', &mut line)
        .ok()?;
    if line.len() > MAX_HEADER {
        return None;
    }
    let value: Value = serde_json::from_slice(&line).ok()?;
    (value["type"] == "header").then_some(value)
}

fn read_status(path: &Path) -> Option<Value> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(4096);
    file.seek(SeekFrom::Start(start)).ok()?;
    // Bytes, not text: the window may begin inside a multi-byte character of an earlier line.
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    bytes
        .split(|byte| *byte == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(|record| record["type"] == "status")
}

/// One readable line per archived message (`role: text`). Tool and other structured blocks are
/// summarized by kind so the hub shows what happened without dumping raw payloads.
pub fn format_lines(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| {
            let presentation = message.get("dial_tool_presentation");
            let view = presentation.and_then(|value| {
                serde_json::from_value::<crate::runtime::presentation::Presentation>(
                    value["view"].clone(),
                )
                .ok()
            });
            if let Some(view) = view {
                let phase = presentation
                    .and_then(|value| {
                        serde_json::from_value::<crate::runtime::Phase>(value["phase"].clone()).ok()
                    })
                    .unwrap_or(crate::runtime::Phase::Failed);
                let glyph = match phase {
                    crate::runtime::Phase::Completed => "✓",
                    crate::runtime::Phase::Failed | crate::runtime::Phase::Interrupted => "✗",
                    _ => "!",
                };
                let mut facts = view.facts;
                if phase != crate::runtime::Phase::Completed {
                    facts.push(phase.label().into());
                }
                let mut rows = vec![format!(
                    "{glyph} {:<7} {}{}",
                    view.label,
                    view.target,
                    if facts.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", facts.join(" · "))
                    }
                )];
                let evidence = message["dial_child_tool_result"]["text"]
                    .as_str()
                    .or_else(|| message["content"].as_str())
                    .unwrap_or_default();
                rows.extend(evidence.lines().map(|line| format!("   {line}")));
                rows
            } else {
                vec![format_message(message)]
            }
        })
        .collect()
}

fn format_message(message: &Value) -> String {
    let role = message["role"].as_str().unwrap_or("message");
    let body = match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(block_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    format!("{role}: {}", body.replace(['\n', '\r'], " "))
}

fn block_text(block: &Value) -> String {
    match block["type"].as_str() {
        Some("text" | "output_text") | None => {
            block["text"].as_str().unwrap_or_default().to_owned()
        }
        Some(kind) => match block["name"].as_str() {
            Some(name) if !name.is_empty() => format!("[{kind} {name}]"),
            _ => format!("[{kind}]"),
        },
    }
}

#[cfg(test)]
mod tests;
