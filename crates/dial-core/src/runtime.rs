//! Shared execution state. Views observe result bodies; snapshots are journal records, never model text.
pub mod arguments;
pub mod cancellation;
pub mod clock;
pub mod evidence;
pub mod execution;
pub mod fingerprint;
pub mod integration;
pub mod json_transport;
pub mod presentation;
pub mod process;
pub mod schema;
pub mod stream_journal;
pub mod structural;
pub mod template;
pub mod tool_output;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Running,
    #[serde(alias = "done")]
    Completed,
    Blocked,
    NeedsInput,
    Incomplete,
    Failed,
    #[serde(alias = "cancelled", alias = "canceled")]
    Interrupted,
}

impl Phase {
    pub fn from_status(status: &str) -> Self {
        match status {
            "running" | "pending" => Self::Running,
            "done" | "completed" => Self::Completed,
            "blocked" => Self::Blocked,
            "needs_input" => Self::NeedsInput,
            "incomplete" => Self::Incomplete,
            "interrupted" | "cancelled" | "canceled" => Self::Interrupted,
            _ => Self::Failed,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::NeedsInput => "needs input",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
    pub fn is_running(self) -> bool {
        self == Self::Running
    }
    pub fn is_failure(self) -> bool {
        self == Self::Failed
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<execution::Execution>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    pub phase: Phase,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub parent: Option<String>,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub result: ResultBody,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<presentation::Presentation>,
}

/// Wall-clock milliseconds since the Unix epoch, stamped on a running body's last progress.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

/// Worker shards expose only this UTF-8 tail; complete answers live in result/artifact records.
pub(super) const WORKER_TAIL_LIMIT: usize = 16 * 1024;

/// Live text a running tool body retains. Interfaces stream tool output through their own bounded
/// buffers and the settled result replaces this text, so the body keeps a prefix-stable head
/// (byte-cursor deltas stay valid) instead of growing without bound with a command's output.
pub(super) const TOOL_STREAM_LIMIT: usize = 1024 * 1024;
/// Appended once when a running tool body stops retaining live output.
pub(super) const TOOL_STREAM_MARKER: &str =
    "\n[further live output omitted; the tool result reports the outcome]\n";
/// Text a snapshot keeps when the whole node would exceed the stream journal's row limit.
const OVERSIZED_TEXT_TAIL: usize = 64 * 1024;

/// The part of `text` a running tool body may still append: everything, a cut prefix followed by
/// [`TOOL_STREAM_MARKER`], or nothing once the marker has been written.
fn tool_stream_chunk<'a>(retained: usize, text: &'a str) -> Option<std::borrow::Cow<'a, str>> {
    let reserve = TOOL_STREAM_LIMIT - TOOL_STREAM_MARKER.len();
    if retained >= reserve {
        return None;
    }
    let room = reserve - retained;
    if text.len() <= room {
        return Some(std::borrow::Cow::Borrowed(text));
    }
    let keep = text.floor_char_boundary(room);
    Some(std::borrow::Cow::Owned(format!(
        "{}{TOOL_STREAM_MARKER}",
        &text[..keep]
    )))
}

/// Shrinks a node whose snapshot the journal refused as oversized: its text keeps a bounded tail
/// and oversized structured data is replaced by a note, so the update can still be published.
fn shrink_for_journal(node: &mut Node) {
    if node.result.text.len() > OVERSIZED_TEXT_TAIL {
        let kept = tail(&node.result.text, OVERSIZED_TEXT_TAIL).to_owned();
        node.result.text =
            format!("[earlier text omitted: exceeded the runtime journal row limit]\n{kept}");
    }
    let oversized_data = node
        .result
        .data
        .as_ref()
        .and_then(|data| serde_json::to_vec(data).ok())
        .is_some_and(|bytes| bytes.len() > OVERSIZED_TEXT_TAIL);
    if oversized_data {
        node.result.data = Some(serde_json::json!({
            "omitted": "result data exceeded the runtime journal row limit"
        }));
    }
}

/// Drop complete leading characters until at most `limit` bytes remain.
fn tail(text: &str, limit: usize) -> &str {
    let mut cut = text.len().saturating_sub(limit);
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    &text[cut..]
}

/// Apply the same bounded-tail operation to live state and recovered journal frames.
fn append_tail(target: &mut String, text: &str, retain_bytes: usize) {
    let cut = target.len() - tail(target, retain_bytes).len();
    target.drain(..cut);
    target.push_str(text);
}

/// Node ids whose live revision may differ from the last journal-accepted publication. Body
/// writers insert while holding their own node lock; this is a leaf lock and is never held while
/// locking a body, so it cannot invert the Tree -> Body order.
type Dirty = Arc<Mutex<BTreeSet<String>>>;

/// Leaf-lock insert. The `contains` probe avoids allocating an id for every append or progress
/// update of a body that is already dirty.
fn mark_dirty(dirty: &Dirty, id: &str) {
    let mut set = dirty.lock().unwrap_or_else(|e| e.into_inner());
    if !set.contains(id) {
        set.insert(id.to_owned());
    }
}

fn unmark_dirty(dirty: &Dirty, id: &str) {
    dirty.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
}

/// Whether the journal has accepted this exact live revision. Callers hold the body's lock.
fn is_persisted(persisted: &BTreeMap<String, u64>, node: &Node) -> bool {
    persisted.get(&node.id) == Some(&node.result.revision)
}

#[derive(Debug, Clone)]
pub struct Body(Arc<Mutex<Node>>, Option<stream_journal::Journal>, Dirty);
impl Body {
    /// Commit a metadata snapshot before exposing it through any live body handle.
    fn update(&self, edit: impl FnOnce(&mut Node) -> bool) {
        let mut node = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(journal) = &self.1 {
            if journal.check().is_err() {
                return;
            }
            let mut next = node.clone();
            if !edit(&mut next) {
                return;
            }
            next.result.revision = next.result.revision.saturating_add(1);
            if !journal.snapshot(&next) {
                // One oversized update must not stop the session's journal: publish a bounded
                // version of it instead. If even that is refused, the live body stays unchanged.
                shrink_for_journal(&mut next);
                if !journal.snapshot(&next) {
                    return;
                }
            }
            if journal.check().is_ok() {
                *node = next;
                mark_dirty(&self.2, &node.id);
            }
        } else if edit(&mut node) {
            node.result.revision = node.result.revision.saturating_add(1);
            mark_dirty(&self.2, &node.id);
        }
    }
    pub fn execution(&self, execution: execution::Execution) {
        self.update(|node| {
            node.result.execution = Some(execution);
            true
        });
    }
    /// Whether two handles observe the same live body, regardless of their journal ids.
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn progress(&self, phase: &str, steps: u32) {
        self.update(|node| {
            if node.result.phase != Phase::Running {
                return false;
            }
            let data = node
                .result
                .data
                .get_or_insert_with(|| serde_json::json!({}));
            if !data.is_object() {
                *data = serde_json::json!({});
            }
            data["phase"] = serde_json::json!(phase);
            data["steps"] = serde_json::json!(steps);
            data["updated_ms"] = serde_json::json!(now_ms());
            true
        });
    }
    /// A worker started a tool: the phase names it, and the running tool count grows.
    pub fn tool_started(&self, name: &str, steps: u32) {
        self.update(|node| {
            if node.result.phase != Phase::Running {
                return false;
            }
            let data = node
                .result
                .data
                .get_or_insert_with(|| serde_json::json!({}));
            if !data.is_object() {
                *data = serde_json::json!({});
            }
            let tools = data["tools"].as_u64().unwrap_or(0) + 1;
            data["phase"] = serde_json::json!(format!("tool: {name}"));
            data["current_tool"] = serde_json::json!(name);
            data["tools"] = serde_json::json!(tools);
            data["steps"] = serde_json::json!(steps);
            data["updated_ms"] = serde_json::json!(now_ms());
            true
        });
    }
    pub fn read<R>(&self, read: impl FnOnce(&Node) -> R) -> R {
        read(&self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
    pub fn snapshot(&self) -> Node {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn revision(&self) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .result
            .revision
    }
    pub fn append(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut node = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if node.result.phase != Phase::Running {
            return;
        }
        let revision = node.result.revision.saturating_add(1);
        let retain_bytes =
            (node.kind == "worker").then(|| WORKER_TAIL_LIMIT.saturating_sub(text.len()));
        let chunk;
        let text = if retain_bytes.is_some() {
            tail(text, WORKER_TAIL_LIMIT)
        } else if node.kind == "tool" {
            let Some(bounded) = tool_stream_chunk(node.result.text.len(), text) else {
                return;
            };
            chunk = bounded;
            &chunk
        } else {
            text
        };
        if let Some(journal) = &self.1
            && (!journal.text_tail(&node.id, revision, text, retain_bytes)
                || journal.check().is_err())
        {
            return;
        }
        if let Some(retain_bytes) = retain_bytes {
            append_tail(&mut node.result.text, text, retain_bytes);
        } else {
            node.result.text.push_str(text);
        }
        node.result.revision = revision;
        mark_dirty(&self.2, &node.id);
    }
    pub fn correlate(&self, call_id: &str) {
        self.update(|node| {
            node.call_id = Some(call_id.into());
            true
        });
    }
    pub fn present(&self, presentation: presentation::Presentation) {
        self.update(|node| {
            node.presentation = Some(presentation);
            true
        });
    }
    pub fn settle(&self, text: String, mut data: Option<Value>, phase: Phase) {
        self.update(|node| {
            if phase == Phase::Running && node.result.phase != Phase::Running {
                return false;
            }
            let phase = if node.result.phase == Phase::Interrupted {
                if phase != Phase::Interrupted
                    && node.kind == "worker"
                    && let Some(value) = data.as_mut().filter(|value| value.is_object())
                {
                    value["status"] = serde_json::json!("interrupted");
                    value["phase"] = serde_json::json!("interrupted");
                }
                Phase::Interrupted
            } else {
                phase
            };
            node.result.text = text;
            node.result.data = data;
            node.result.phase = phase;
            true
        });
    }
    fn interrupt(&self) {
        self.update(|node| {
            if node.result.phase != Phase::Running {
                return false;
            }
            if let Some(value) = node.result.data.as_mut().filter(|v| v.is_object()) {
                value["status"] = serde_json::json!("interrupted");
                value["phase"] = serde_json::json!("interrupted");
            }
            node.result.phase = Phase::Interrupted;
            true
        });
    }
}

#[derive(Debug, Default)]
struct Inner {
    nodes: BTreeMap<String, Body>,
    dirty: Dirty,
    next: u64,
    persisted: BTreeMap<String, u64>,
    anchor: Option<String>,
    journal: Option<stream_journal::Journal>,
}

/// One handle shared by the controller, terminal, RPC clients and test observers.
#[derive(Debug, Clone, Default)]
pub struct Tree(Arc<Mutex<Inner>>);
impl Tree {
    pub fn journal(&self, journal: stream_journal::Journal) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).journal = Some(journal);
    }
    pub fn check_persistence(&self) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .journal
            .as_ref()
            .map_or(Ok(()), |journal| journal.check())
    }
    pub fn stream_writer(&self) -> Option<stream_journal::Journal> {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .journal
            .clone()
    }
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn owns(&self, body: &Body) -> bool {
        let tree = self.0.lock().unwrap_or_else(|e| e.into_inner());
        body.read(|node| {
            tree.nodes
                .get(&node.id)
                .is_some_and(|current| Arc::ptr_eq(&current.0, &body.0))
        })
    }
    pub fn anchor(&self, parent: Option<String>) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).anchor = parent;
    }
    pub fn anchor_if_empty(&self, parent: String) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .anchor
            .get_or_insert(parent);
    }
    pub fn reserve(&self, id: &str) {
        if let Some(n) = id
            .strip_prefix("runtime-")
            .and_then(|n| n.parse::<u64>().ok())
        {
            let mut tree = self.0.lock().unwrap_or_else(|e| e.into_inner());
            tree.next = tree.next.max(n);
        }
    }
    pub fn bodies(&self, kind: &str) -> Vec<Body> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .nodes
            .values()
            .filter(|b| b.read(|n| n.kind == kind))
            .cloned()
            .collect()
    }
    pub fn start(&self, kind: &str, label: &str, parent: Option<String>) -> Body {
        let mut tree = self.0.lock().unwrap_or_else(|e| e.into_inner());
        tree.next += 1;
        let id = format!("runtime-{}", tree.next);
        let body = Body(
            Arc::new(Mutex::new(Node {
                id: id.clone(),
                parent: parent.or_else(|| tree.anchor.clone()),
                kind: kind.into(),
                label: label.into(),
                call_id: None,
                presentation: None,
                result: ResultBody {
                    execution: None,
                    text: String::new(),
                    data: None,
                    phase: Phase::Running,
                    revision: 0,
                },
            })),
            tree.journal.clone(),
            tree.dirty.clone(),
        );
        if let Some(journal) = &body.1 {
            journal.snapshot(&body.snapshot());
        }
        if body
            .1
            .as_ref()
            .is_none_or(|journal| journal.check().is_ok())
        {
            mark_dirty(&tree.dirty, &id);
            tree.nodes.insert(id, body.clone());
        }
        body
    }
    pub fn restore(&self, node: Node) {
        let mut tree = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = node
            .id
            .strip_prefix("runtime-")
            .and_then(|n| n.parse::<u64>().ok())
        {
            tree.next = tree.next.max(n);
        }
        tree.persisted.insert(node.id.clone(), node.result.revision);
        let journal = tree.journal.clone();
        let dirty = tree.dirty.clone();
        unmark_dirty(&dirty, &node.id);
        tree.nodes.insert(
            node.id.clone(),
            Body(Arc::new(Mutex::new(node)), journal, dirty),
        );
    }
    pub fn snapshots(&self) -> Vec<Node> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .nodes
            .values()
            .map(Body::snapshot)
            .collect()
    }
    pub fn pending(&self) -> Vec<Node> {
        self.pending_where(false)
    }
    pub fn pending_settled(&self) -> Vec<Node> {
        self.pending_where(true)
    }
    /// Visits only bodies whose revision may be unpersisted, not every node. A clean body leaves
    /// the dirty set while holding its own lock, so a concurrent change re-marks it atomically.
    fn pending_where(&self, settled_only: bool) -> Vec<Node> {
        let tree = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<String> = tree
            .dirty
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        let mut pending = Vec::new();
        for id in ids {
            let Some(body) = tree.nodes.get(&id) else {
                unmark_dirty(&tree.dirty, &id);
                continue;
            };
            body.read(|n| {
                if is_persisted(&tree.persisted, n) {
                    unmark_dirty(&tree.dirty, &n.id);
                } else if !settled_only || n.result.phase != Phase::Running {
                    pending.push(n.clone());
                }
            });
        }
        pending
    }
    /// Acknowledge only the exact live snapshot. An older publication, including one from a
    /// replaced body at the same revision, must not clear a newer or different result.
    pub fn persisted(&self, node: &Node) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Inner {
            nodes,
            persisted,
            dirty,
            ..
        } = &mut *guard;
        if let Some(body) = nodes.get(&node.id) {
            body.read(|current| {
                if current == node {
                    persisted.insert(node.id.clone(), node.result.revision);
                    unmark_dirty(dirty, &node.id);
                }
            });
        }
    }
    /// Interrupts every running body. Each body is settled under its own lock, so a concurrent
    /// `append` or `settle` is never overwritten by a stale snapshot.
    pub fn interrupt_open(&self) {
        let bodies: Vec<Body> = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .nodes
            .values()
            .cloned()
            .collect();
        for body in bodies {
            body.interrupt();
        }
    }
}

#[cfg(test)]
mod tests;
