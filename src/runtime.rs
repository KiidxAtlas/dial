//! Shared execution state. Views observe result bodies; snapshots are journal records, never model text.
pub mod arguments;
pub mod cancellation;
pub mod clock;
pub mod execution;
pub mod fingerprint;
pub(crate) mod json_transport;
pub mod presentation;
pub mod process;
pub mod schema;
pub(crate) mod stream_journal;
pub(crate) mod structural;
pub mod template;
pub mod tool_output;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
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

#[derive(Debug, Clone)]
pub struct Body(Arc<Mutex<Node>>, Option<stream_journal::Journal>);
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
            journal.snapshot(&next);
            if journal.check().is_ok() {
                *node = next;
            }
        } else if edit(&mut node) {
            node.result.revision = node.result.revision.saturating_add(1);
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
        if self.read(|node| node.kind == "worker") {
            self.update(|node| {
                if node.result.phase != Phase::Running {
                    return false;
                }
                node.result.text.push_str(text);
                // Worker shards show a bounded tail; final answers live in result/artifact records.
                const LIMIT: usize = 16 * 1024;
                if node.result.text.len() > LIMIT {
                    let mut cut = node.result.text.len() - LIMIT;
                    while !node.result.text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    node.result.text.drain(..cut);
                }
                true
            });
            return;
        }
        let mut node = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if node.result.phase != Phase::Running {
            return;
        }
        let revision = node.result.revision.saturating_add(1);
        if let Some(journal) = &self.1 {
            journal.text(&node.id, revision, text);
            if journal.check().is_err() {
                return;
            }
        }
        node.result.text.push_str(text);
        node.result.revision = revision;
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
    next: u64,
    persisted: BTreeMap<String, u64>,
    anchor: Option<String>,
    journal: Option<stream_journal::Journal>,
}

/// One handle shared by the controller, terminal, RPC clients and test observers.
#[derive(Debug, Clone, Default)]
pub struct Tree(Arc<Mutex<Inner>>);
impl Tree {
    pub(crate) fn journal(&self, journal: stream_journal::Journal) {
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
    pub(crate) fn stream_writer(&self) -> Option<stream_journal::Journal> {
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
        );
        if let Some(journal) = &body.1 {
            journal.snapshot(&body.snapshot());
        }
        if body
            .1
            .as_ref()
            .is_none_or(|journal| journal.check().is_ok())
        {
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
        tree.nodes
            .insert(node.id.clone(), Body(Arc::new(Mutex::new(node)), journal));
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
    fn pending_where(&self, settled_only: bool) -> Vec<Node> {
        let tree = self.0.lock().unwrap_or_else(|e| e.into_inner());
        tree.nodes
            .values()
            .filter(|b| {
                b.read(|n| {
                    (!settled_only || n.result.phase != Phase::Running)
                        && tree.persisted.get(&n.id) != Some(&n.result.revision)
                })
            })
            .map(Body::snapshot)
            .collect()
    }
    pub fn persisted(&self, node: &Node) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .persisted
            .insert(node.id.clone(), node.result.revision);
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
