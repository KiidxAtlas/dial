//! Sessions: an append-only JSONL tree per conversation, plus full-text search across all sessions
//! of a project.
//!
//! File layout (`<sessions_dir>/<ts>_<id>.jsonl`): the first line is a header
//! `{type:"session", id, started, cwd, model, version, schema_version}`; every further line is an entry
//! `{type:"message"|"compaction"|"meta", id, parent, ts, ...}`. The active branch is the parent
//! chain of the *last* entry in the file, so branching (rewind) is just another appended line and
//! no history is ever deleted.
//!
//! Active branch/message indices are derived once on load or rewind and extended on ordinary
//! appends. Readers share those indices; message bodies and control state remain in the entries.
//!
//! Search keeps an FTS5 index in `<state_dir>/sessions.sqlite3`. The index is derived data: each
//! search first catches up on every session file from the byte offset it last indexed, so it also
//! picks up lines written by other processes and survives a crash between append and indexing.
//!
//! Load-time recovery: a torn final line (interrupted append) is tolerated; malformed or oversized
//! interior lines, and entries whose parent is missing, are skipped and reported through
//! `Session::take_recovery_diagnostics`. Loading never rewrites or truncates the file.
mod context;
mod exploration;
mod journal;
mod lifecycle;
mod listing;
mod message;
mod metrics;
mod relocate;
mod rewind;
mod runtime_state;
mod search;
pub mod session_search_tool;
mod transcript;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::platform::fs as platform_fs;

use super::{session_pins, side_thread};
use crate::config::paths::Layout;
use crate::runtime::tool_output::ToolOutput;
pub use context::{ContextNotesRevision, MAX_CONTEXT_NOTES_BYTES};
use context::{context_notes_text, ensure_closed_context_groups, valid_context_bootstrap};
pub use exploration::ExplorationRewind;
use journal::{Claimed, MAX_LINE, busy_session_error, entry_json, read_bounded_line};
pub use lifecycle::Resumed;
pub use listing::{SessionInfo, latest_conversation};
use listing::{TITLE_CHARS, artifact_root, file_id, session_files, valid_id};
pub use message::{TURN_DRAFT_KEY, typed_text};
use message::{
    apply_control_entry, collect_strings, content_text, control_entry_field,
    is_conversation_message, is_original_record, role_of, set_string, truncate_chars,
};
pub use metrics::SessionIoMetrics;
use metrics::{add_elapsed, bump};
use relocate::sync_dir;
pub use relocate::{ensure_side_thread_free, relocate_side_thread};
pub use search::SearchHit;
pub use transcript::{MessageCursor, MessageDelta};

// Reached by the test suite through `super::*`.
#[cfg(test)]
use journal::claim_writer;
#[cfg(test)]
use listing::PICKER_PREVIEW_CHARS;
#[cfg(test)]
use relocate::{CreatedCompanions, discard_moved_copy};

/// Text budget kept inline for each oversized message field before the artifact pointer.
const OVERSIZED_INLINE_TEXT: usize = 256 * 1024;
/// A successful append is not necessarily a durable acknowledgement if syncing failed.
#[derive(Debug, Clone)]
pub struct JournalCommit {
    pub entry_id: String,
    pub durability_error: Option<String>,
}

pub struct SessionMeta {
    pub model: String,
}

/// Header fields carried over from a foreign session. `None` keeps Dial's defaults.
#[derive(Debug, Clone, Default)]
pub struct ImportOrigin {
    /// Source timestamp, written verbatim as the header's `started`.
    pub started: Option<String>,
    /// Source working directory, written as the header's `cwd`.
    pub cwd: Option<PathBuf>,
}

/// Why a journal line or link was not loaded as written. Loading never repairs the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalIssueKind {
    /// The final line has no newline and does not parse: an interrupted append, tolerated.
    TornTail,
    /// A complete line that is not valid JSON, is a duplicate, or is a malformed known record.
    MalformedInterior,
    /// A complete line over the size limit; its content was not loaded.
    OversizedInterior,
    /// An entry names a parent absent from the file; history above that link is unreachable.
    MissingParent,
}

/// One load-time finding. `line` is the 1-based physical line in the journal file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct JournalDiagnostic {
    pub kind: JournalIssueKind,
    pub line: usize,
    pub entry_id: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone)]
enum Kind {
    Message(Value),
    Runtime(Box<crate::runtime::Node>),
    Compaction {
        summary: String,
        first_kept: Option<String>,
        prefix: Option<Vec<Value>>,
    },
    Custom {
        custom_type: String,
        data: Value,
    },
    ContextReset {
        bootstrap: Vec<Value>,
        notes: Option<Value>,
        digest: String,
    },
    Meta {
        kind: String,
        removed: usize,
    },
    Title(String),
    /// A short user label on entry `target`; the latest label in file order wins, empty clears.
    Label {
        target: String,
        label: String,
    },
}

#[derive(Debug, Clone)]
struct Entry {
    id: String,
    parent: Option<String>,
    ts: String,
    kind: Kind,
}

/// Derived entry indices only: messages and durable state always come from `entries`.
/// Ordinary appends extend this view; branch changes and loaded trees rebuild it.
#[derive(Default)]
struct ActiveView {
    branch: Vec<usize>,
    messages: Vec<usize>,
    archived: Vec<usize>,
    revision: u64,
    compaction: Option<usize>,
    control: Option<usize>,
    /// Control state merged along the branch: a `state` entry replaces it, a `patch` entry
    /// overwrites only the top-level keys it names (null removes one).
    control_state: Value,
    maintenance_revision: Option<usize>,
    // Corrupt/forward/missing parent links may change meaning when an id is added.
    extendable: bool,
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
    active: ActiveView,
    /// Namespace epochs are session-monotonic; all other control data remains branch-local.
    history_generation_floor: u64,
    /// Workspace roots added beyond the project root; persisted as the header's `additional_dirs`.
    additional_dirs: Vec<PathBuf>,
    /// Load-time findings about this journal; `take_recovery_diagnostics` drains them.
    recovery: Vec<JournalDiagnostic>,
    /// Process-local write/sync measurements; never journaled and never read by session logic.
    io: SessionIoMetrics,
}

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

/// Current wall-clock time in the same UTC format as session entry timestamps.
pub fn timestamp_now() -> String {
    iso_now()
}

pub(super) fn random_hex(bytes: usize) -> String {
    (0..bytes)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect()
}

impl Session {
    /// Model recorded in the session header.
    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Workspace roots added beyond the project root, as recorded in the session header.
    pub fn additional_dirs(&self) -> &[PathBuf] {
        &self.additional_dirs
    }

    /// Replaces the header's `additional_dirs`. The header is rewritten atomically (temp file,
    /// exclusive lock, rename) and this session's writer is reopened on the new file. Ephemeral
    /// sessions keep the value in memory only. Read-only snapshots are refused.
    pub fn set_additional_dirs(&mut self, dirs: &[PathBuf]) -> Result<()> {
        self.set_additional_dirs_with(dirs, sync_dir)
    }

    /// `set_additional_dirs` with the directory sync injected so tests can fail it after the rename.
    fn set_additional_dirs_with(
        &mut self,
        dirs: &[PathBuf],
        sync: impl FnOnce(&Path) -> io::Result<()>,
    ) -> Result<()> {
        if self.additional_dirs == dirs {
            return Ok(());
        }
        let Some(path) = self.path.clone() else {
            self.additional_dirs = dirs.to_vec();
            return Ok(());
        };
        ensure!(
            self.file.is_some(),
            "read-only session snapshot cannot change its workspace roots"
        );
        let mut src_reader = BufReader::new(
            File::open(&path).with_context(|| format!("reading {}", path.display()))?,
        );
        let mut first = Vec::new();
        src_reader
            .read_until(b'\n', &mut first)
            .with_context(|| format!("reading {}", path.display()))?;
        // A file without a newline is all header; any torn or non-UTF-8 tail is copied verbatim.
        if first.last() == Some(&b'\n') {
            first.pop();
        }
        let mut header: Value = serde_json::from_slice(&first).context("parsing session header")?;
        let object = header
            .as_object_mut()
            .context("session header is not an object")?;
        if dirs.is_empty() {
            object.remove("additional_dirs");
        } else {
            let names = dirs
                .iter()
                .map(|d| {
                    d.to_str()
                        .map(str::to_owned)
                        .context("workspace root is not valid UTF-8")
                })
                .collect::<Result<Vec<_>>>()?;
            object.insert("additional_dirs".to_owned(), json!(names));
        }
        let dir = path.parent().context("session path has no parent")?;
        let name = path
            .file_name()
            .context("session path has no file name")?
            .to_string_lossy();
        let tmp = dir.join(format!(".{}.{name}.tmp", random_hex(8)));
        // Read access as in `create_file_at`: Windows cannot lock an append-only handle.
        let mut out = platform_fs::open_owner_only(
            OpenOptions::new().read(true).append(true).create_new(true),
            &tmp,
        )
        .with_context(|| format!("creating {}", tmp.display()))?;
        let written = (|| -> io::Result<u64> {
            platform_fs::try_lock_journal(&out, true).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("claiming rewritten session writer: {error}"),
                )
            })?;
            out.write_all(header.to_string().as_bytes())?;
            out.write_all(b"\n")?;
            let copied = io::copy(&mut src_reader, &mut out)?;
            crate::platform::fs::sync_file(&out)?;
            Ok(copied)
        })();
        drop(src_reader);
        let copied = match written.and_then(|copied| fs::rename(&tmp, &path).map(|()| copied)) {
            Ok(copied) => copied,
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                return Err(err).with_context(|| format!("rewriting {}", path.display()));
            }
        };
        // The rename published the header, so the writer and roots follow it before the directory
        // sync. A sync failure leaves them applied and reports only durability as uncertain.
        self.file = Some(out);
        self.pending_newline = self.pending_newline && copied > 0;
        self.additional_dirs = dirs.to_vec();
        sync(dir).with_context(|| {
            format!(
                "workspace roots applied to {}, but durability is uncertain",
                path.display()
            )
        })?;
        Ok(())
    }

    /// Append metadata; never rewrite conversation history.
    pub fn rename(&mut self, title: &str) -> Result<()> {
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        ensure!(!title.is_empty(), "a session title is required");
        self.push(Kind::Title(title)).map(|_| ())
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// A snapshot identity for background maintenance. Control/runtime appends also invalidate it.
    pub fn branch_leaf(&self) -> Option<&str> {
        self.active
            .branch
            .last()
            .map(|&i| self.entries[i].id.as_str())
    }

    /// Runtime progress does not change memory input; actual history and control changes do.
    pub fn memory_revision(&self) -> Option<&str> {
        self.active
            .maintenance_revision
            .map(|index| self.entries[index].id.as_str())
    }

    pub fn contains_branch_leaf(&self, leaf: &str) -> bool {
        self.active
            .branch
            .iter()
            .any(|&i| self.entries[i].id == leaf)
    }

    /// Ephemeral children and inspection snapshots have no authority over a durable notebook.
    pub fn owns_journal(&self) -> bool {
        self.path.is_some() && self.file.is_some()
    }

    /// Appends a non-model record, such as a training-turn capture. Model context never reads
    /// these entries; they exist for replay and display only.
    pub fn append_custom_entry(
        &mut self,
        custom_type: &str,
        data: serde_json::Value,
    ) -> Result<JournalCommit> {
        anyhow::ensure!(
            !custom_type.is_empty() && custom_type.len() <= 64,
            "custom entry type must be 1-64 bytes"
        );
        self.append_context_entry(Kind::Custom {
            custom_type: custom_type.to_owned(),
            data,
        })
    }

    /// Records of one custom type on the active branch, oldest first. Each is paired with its
    /// position in `messages()`: the number of messages that precede it on the branch, so a replay
    /// can place it between the messages it followed. Records never enter model context.
    pub fn custom_records(&self, custom_type: &str) -> Vec<(usize, serde_json::Value)> {
        let kept: std::collections::HashSet<usize> = self.active.messages.iter().copied().collect();
        let prefix = self.message_count() - self.active.messages.len();
        let mut seen = 0;
        let mut out = Vec::new();
        for &i in &self.active.branch {
            match &self.entries[i].kind {
                Kind::Message(_) if kept.contains(&i) => seen += 1,
                Kind::Custom {
                    custom_type: t,
                    data,
                } if t == custom_type => {
                    out.push((prefix + seen, data.clone()));
                }
                _ => {}
            }
        }
        out
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
    /// Keep persisted messages below the bounded JSONL line size while preserving the complete
    /// original string in the session's artifact store.
    fn fit_oversized_message(&self, message: &Value, original_len: usize) -> Result<Value> {
        let mut fitted = message.clone();
        let path = self
            .path
            .as_ref()
            .context("oversized message requires a durable session")?;
        let artifact_root = artifact_root(path)?;
        let artifacts = crate::storage::artifact::ArtifactStore::new(&artifact_root)?;
        let mut borrowed = Vec::new();
        collect_strings(&fitted, &mut Vec::new(), &mut borrowed);
        let mut candidates: Vec<_> = borrowed
            .into_iter()
            .map(|(pointer, text)| (pointer, text.to_owned()))
            .collect();
        candidates.sort_by_key(|(_, text)| std::cmp::Reverse(text.len()));
        for (pointer, text) in candidates {
            if text.len() <= OVERSIZED_INLINE_TEXT {
                continue;
            }
            let reference = artifacts.retain(&text)?;
            let prefix_end = text
                .char_indices()
                .take_while(|(index, _)| *index <= OVERSIZED_INLINE_TEXT)
                .map(|(index, ch)| index + ch.len_utf8())
                .last()
                .unwrap_or(0)
                .min(text.len());
            let suffix_start = text
                .char_indices()
                .find(|(index, _)| *index >= text.len().saturating_sub(OVERSIZED_INLINE_TEXT))
                .map_or(text.len(), |(index, _)| index);
            let replacement = format!(
                "{}\n[SESSION MESSAGE TRUNCATED: full content stored as artifact {reference}; original entry was {original_len} bytes]\n{}",
                &text[..prefix_end],
                &text[suffix_start..]
            );
            set_string(&mut fitted, &pointer, replacement);
            if serde_json::to_vec(&fitted).is_ok_and(|bytes| bytes.len() < MAX_LINE) {
                return Ok(fitted);
            }
        }
        anyhow::bail!("oversized session message has no truncatable string fields")
    }

    /// Appends an entry as a child of the current leaf.
    fn push(&mut self, kind: Kind) -> Result<String> {
        self.push_under(self.leaf.clone(), kind)
    }
    /// Bounds one journal record. Oversized messages and compaction records keep their complete
    /// strings in the artifact store; the journal keeps a UTF-8-safe prefix and suffix with a marker.
    /// Other kinds are returned unchanged and still fail the line-limit check at the append site.
    fn fit_kind_for_journal(&self, kind: Kind, line_len: usize) -> Result<Kind> {
        match kind {
            Kind::Message(message) => Ok(Kind::Message(
                self.fit_oversized_message(&message, line_len)?,
            )),
            Kind::Compaction {
                summary,
                first_kept,
                prefix,
            } => {
                let wrapper = json!({"summary": summary, "prefix": prefix});
                let fitted = self.fit_oversized_message(&wrapper, line_len)?;
                Ok(Kind::Compaction {
                    summary: fitted["summary"].as_str().unwrap_or_default().to_owned(),
                    first_kept,
                    prefix: fitted["prefix"].as_array().cloned(),
                })
            }
            other => Ok(other),
        }
    }
    /// Commits a real rewind: a provider-generation bump (so shown recall cannot suppress facts the
    /// rewind removed from context) and the rewind marker, then rebuilds runtime. Callers guarantee
    /// `removed > 0`; a no-op rewind never reaches here.
    fn push_rewind_marker(&mut self, new_leaf: Option<String>, removed: usize) -> Result<()> {
        ensure!(
            new_leaf
                .as_ref()
                .is_none_or(|leaf| self.contains_branch_leaf(leaf)),
            "rewind parent is outside the active branch"
        );
        self.commit_leaf_move(new_leaf, "rewind", removed)
    }
    /// Moves the leaf to `new_leaf` (any entry, or the root for `None`) by appending a control
    /// snapshot of that point and a `kind` marker beneath it. `removed` must be positive.
    fn commit_leaf_move(
        &mut self,
        new_leaf: Option<String>,
        kind: &str,
        removed: usize,
    ) -> Result<()> {
        if removed == 0 {
            return Ok(());
        }
        let generation = self.control_state()["provider_identity"]["history_generation"]
            .as_u64()
            .unwrap_or(0)
            .checked_add(1)
            .context("provider history generation exhausted; no rewind committed")?;
        // Restore the target branch's state, not obligations from the discarded turn.
        let target_path = self.ancestry(new_leaf.as_deref());
        ensure!(
            new_leaf.is_none() || !target_path.is_empty(),
            "branch point {} is not in this session",
            new_leaf.as_deref().unwrap_or("")
        );
        let mut control = json!({});
        for &i in &target_path {
            if let Kind::Message(message) = &self.entries[i].kind
                && message["role"] == "dial_state"
            {
                apply_control_entry(&mut control, message);
            }
        }
        if control.is_null() {
            control = json!({});
        }
        ensure!(
            control.is_object(),
            "rewind control state is not an object; no rewind committed"
        );
        if !control["provider_identity"].is_object() {
            control["provider_identity"] = json!({});
        }
        control["provider_identity"]["history_generation"] = json!(generation);
        let meta = Kind::Meta {
            kind: kind.to_owned(),
            removed,
        };
        if self.owns_journal() {
            // Generation precedes the marker so no reader sees the shortened window under the old key.
            self.append_context_batch(
                new_leaf,
                vec![
                    Kind::Message(json!({"role":"dial_state","state":control})),
                    meta,
                ],
            )?;
        } else {
            ensure!(
                self.path.is_none(),
                "read-only session cannot commit rewind"
            );
            let control_id = self.push_under(
                new_leaf,
                Kind::Message(json!({"role":"dial_state","state":control})),
            )?;
            self.push_under(Some(control_id), meta)?;
        }
        self.restore_runtime()
    }
    /// Appends an entry whose parent is `parent` (a rewind marker's parent is the rewind point, not the leaf).
    fn push_under(&mut self, parent: Option<String>, kind: Kind) -> Result<String> {
        let extends_active = parent == self.leaf && self.active.extendable;
        let mut entry = Entry {
            id: self.fresh_id(),
            parent,
            ts: iso_now(),
            kind,
        };
        let mut line = entry_json(&entry, false).to_string();
        ensure!(
            self.path.is_none() || self.file.is_some(),
            "read-only session snapshot cannot be modified; claim writer ownership with Session::open"
        );
        if self.file.is_some() && line.len() >= MAX_LINE {
            entry.kind = self.fit_kind_for_journal(entry.kind.clone(), line.len())?;
            line = entry_json(&entry, false).to_string();
        }
        ensure!(
            self.file.is_none() || line.len() < MAX_LINE,
            "session entry is {} bytes, over the {} byte line limit",
            line.len(),
            MAX_LINE
        );
        if let Some(file) = self.file.as_mut() {
            // Writer ownership excludes divergent in-memory leaves and interleaved append syscalls.
            let mut buf = String::with_capacity(line.len() + 2);
            if self.pending_newline {
                buf.push('\n');
            }
            buf.push_str(&line);
            buf.push('\n');
            if let Err(e) = file.write_all(buf.as_bytes()) {
                self.pending_newline = true;
                self.file = None;
                return Err(e).context("appending to session; a partial journal entry may exist; reopen and reclaim the canonical journal before continuing, no rollback or retry is implied");
            }
            self.pending_newline = false;
        }
        let id = entry.id.clone();
        self.by_id.insert(id.clone(), self.entries.len());
        self.leaf = Some(id.clone());
        self.observe_history_generation(&entry.kind);
        self.entries.push(entry);
        if extends_active {
            let i = self.entries.len() - 1;
            self.active.branch.push(i);
            if !matches!(&self.entries[i].kind, Kind::Runtime(_)) {
                self.active.maintenance_revision = Some(i);
            }
            match &self.entries[i].kind {
                Kind::Message(m) if m["role"] == "dial_state" => {
                    self.active.control = Some(i);
                    apply_control_entry(&mut self.active.control_state, m);
                }
                Kind::Message(_) => {
                    self.active.messages.push(i);
                    self.active.archived.push(i);
                }
                Kind::Compaction { .. } | Kind::ContextReset { .. } => self.rebuild_view(),
                _ => {}
            }
        } else {
            self.rebuild_active();
        }
        Ok(id)
    }

    /// Appends a chat message (an object with a string `role`) as a child of the current leaf.
    pub fn append(&mut self, mut message: Value) -> Result<String> {
        ensure!(
            message.is_object() && message.get("role").is_some_and(Value::is_string),
            "session messages must be objects with a string `role`"
        );
        if message.get("dial_images").is_some()
            && let Some(path) = &self.path
        {
            let root = artifact_root(path)?;
            let store = crate::storage::artifact::ArtifactStore::new(&root)?;
            crate::storage::media::retain_message(&mut message, &store);
        } else if self.path.is_none() {
            crate::storage::media::retain_volatile_message(&mut message);
        }
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
        let state = if self.active.control_state.is_null() {
            json!({})
        } else {
            self.active.control_state.clone()
        };
        self.with_history_generation_floor(state)
    }

    pub fn context_history_generation(&self) -> u64 {
        self.history_generation_floor
    }

    fn observe_history_generation(&mut self, kind: &Kind) {
        if let Kind::Message(message) = kind
            && message["role"] == "dial_state"
        {
            self.history_generation_floor = self.history_generation_floor.max(
                control_entry_field(message, "provider_identity")
                    .and_then(|identity| identity["history_generation"].as_u64())
                    .unwrap_or(0),
            );
        }
    }

    fn with_history_generation_floor(&self, mut state: Value) -> Value {
        if self.history_generation_floor == 0
            || state["provider_identity"]["history_generation"]
                .as_u64()
                .unwrap_or(0)
                >= self.history_generation_floor
        {
            return state;
        }
        if !state.is_object() {
            state = json!({});
        }
        let identity = &mut state["provider_identity"];
        if !identity.is_object() {
            *identity = json!({});
        }
        identity["history_generation"] = json!(self.history_generation_floor);
        state
    }

    /// Derived from durable state, never injected as a new user instruction or verification proof.
    pub fn resume_summary(&self) -> String {
        let state = self.control_state();
        let mut lines = vec!["Recorded session state (checks have not been rerun):".to_string()];
        if let Some(tasks) = state["todo"].as_array() {
            for task in tasks.iter().take(16) {
                lines.push(format!(
                    "{}: {}",
                    task["status"].as_str().unwrap_or("unknown"),
                    task["content"].as_str().unwrap_or("unnamed task")
                ));
            }
            if tasks.len() > 16 {
                lines.push(format!("{} more tasks; use /todo", tasks.len() - 16));
            }
        }
        if let Some(checks) = state["verification_results"].as_array() {
            for check in checks.iter().take(12) {
                lines.push(format!(
                    "Recorded check {}: {}",
                    check["name"].as_str().unwrap_or("unnamed"),
                    check["status"].as_str().unwrap_or("unverified")
                ));
            }
            if checks.len() > 12 {
                lines.push(format!("{} more recorded checks", checks.len() - 12));
            }
        }
        lines.push("Completed tool work is not replayed. Inspect checkpoint changes with /review; resume the open task rather than repeating completed work.".to_string());
        truncate_chars(&lines.join("\n"), 12 * 1024)
    }

    /// Journals only the top-level keys that changed since the merged branch state. A full
    /// snapshot every step made these entries 70-95% of a session file; readers fold both forms.
    pub fn set_control_state(&mut self, state: Value) -> Result<()> {
        let started = Instant::now();
        let result = self.publish_control_state(state);
        bump(&mut self.io.control_publication_attempts);
        let micros = add_elapsed(&mut self.io.control_publication_micros, started);
        match result {
            Ok(equal) => {
                if equal {
                    bump(&mut self.io.equal_control_publications);
                    self.io.equal_control_publication_micros = self
                        .io
                        .equal_control_publication_micros
                        .saturating_add(micros);
                }
                Ok(())
            }
            Err(error) => {
                bump(&mut self.io.control_publication_failures);
                Err(error)
            }
        }
    }

    /// Appends the `dial_state` record; returns whether its patch was empty (state already equal).
    fn publish_control_state(&mut self, state: Value) -> Result<bool> {
        let state = self.with_history_generation_floor(state);
        let mut message = json!({"role":"dial_state"});
        let mut equal = false;
        match (self.active.control_state.as_object(), state.as_object()) {
            (Some(previous), Some(next)) if self.active.extendable => {
                let mut patch = serde_json::Map::new();
                for (key, value) in next {
                    if previous.get(key) != Some(value) {
                        patch.insert(key.clone(), value.clone());
                    }
                }
                for key in previous.keys() {
                    if !next.contains_key(key) {
                        patch.insert(key.clone(), Value::Null);
                    }
                }
                equal = patch.is_empty();
                if equal {
                    // Runtime changes still cross the persistence boundary; identical control
                    // data does not create a new context node or invalidate memory authority.
                    self.flush_runtime()?;
                    return Ok(true);
                }
                message["patch"] = Value::Object(patch);
            }
            _ => message["state"] = state,
        }
        self.append(message).map(|_| equal)
    }
}

mod index;
mod migrate;
pub mod pack;
mod transfers;
mod tree;
pub use tree::{MAX_LABEL_CHARS, Navigation, TreeNode, TreeNodeKind};
#[cfg(test)]
mod tests;

impl crate::llm::provider::BoundSession for Session {
    fn id(&self) -> &str {
        Session::id(self)
    }
    fn messages(&self) -> Vec<serde_json::Value> {
        Session::messages(self)
    }
    fn control_state(&self) -> serde_json::Value {
        Session::control_state(self)
    }
    fn set_control_state(&mut self, state: serde_json::Value) -> Result<()> {
        Session::set_control_state(self, state)
    }
}
