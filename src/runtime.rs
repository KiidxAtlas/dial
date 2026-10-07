//! Shared execution state. Views observe result bodies; snapshots are journal records, never model text.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Running,
    Done,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultBody {
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
    pub result: ResultBody,
}

#[derive(Debug, Clone)]
pub struct Body(Arc<Mutex<Node>>);
impl Body {
    pub fn progress(&self, phase: &str, steps: u32) {
        let mut node = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if node.result.phase != Phase::Running {
            return;
        }
        let data = node
            .result
            .data
            .get_or_insert_with(|| serde_json::json!({}));
        data["phase"] = serde_json::json!(phase);
        data["steps"] = serde_json::json!(steps);
        node.result.revision = node.result.revision.saturating_add(1);
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
        let mut node = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if node.result.phase != Phase::Running || text.is_empty() {
            return;
        }
        node.result.text.push_str(text);
        // Worker shards show a bounded tail; their final answers live in result/artifact records.
        const LIMIT: usize = 16 * 1024;
        if node.kind == "worker" && node.result.text.len() > LIMIT {
            let mut cut = node.result.text.len() - LIMIT;
            while !node.result.text.is_char_boundary(cut) {
                cut += 1;
            }
            node.result.text.drain(..cut);
        }
        node.result.revision = node.result.revision.saturating_add(1);
    }
    pub fn settle(&self, text: String, data: Option<Value>, phase: Phase) {
        let mut node = self.0.lock().unwrap_or_else(|e| e.into_inner());
        node.result.text = text;
        node.result.data = data;
        node.result.phase = phase;
        node.result.revision = node.result.revision.saturating_add(1);
    }
}

#[derive(Debug, Default)]
struct Inner {
    nodes: BTreeMap<String, Body>,
    next: u64,
    persisted: BTreeMap<String, u64>,
    anchor: Option<String>,
}

/// One handle shared by the controller, terminal, RPC clients and test observers.
#[derive(Debug, Clone, Default)]
pub struct Tree(Arc<Mutex<Inner>>);
impl Tree {
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
        let body = Body(Arc::new(Mutex::new(Node {
            id: id.clone(),
            parent: parent.or_else(|| tree.anchor.clone()),
            kind: kind.into(),
            label: label.into(),
            result: ResultBody {
                text: String::new(),
                data: None,
                phase: Phase::Running,
                revision: 0,
            },
        })));
        tree.nodes.insert(id, body.clone());
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
        tree.nodes
            .insert(node.id.clone(), Body(Arc::new(Mutex::new(node))));
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
    pub fn interrupt_open(&self) {
        for body in self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .nodes
            .values()
        {
            let node = body.snapshot();
            if node.result.phase == Phase::Running {
                let mut data = node.result.data;
                if let Some(value) = data.as_mut().filter(|v| v.is_object()) {
                    value["status"] = serde_json::json!("interrupted");
                    value["phase"] = serde_json::json!("interrupted");
                }
                body.settle(node.result.text, data, Phase::Interrupted);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn views_share_one_mutating_body_and_settlement_retains_structured_objects() {
        let tree = Tree::default();
        let body = tree.start("tool", "task", None);
        let view = body.clone();
        body.append("live");
        assert_eq!(view.snapshot().result.text, "live");
        body.settle(
            "summary".into(),
            Some(serde_json::json!({"jobs":[{"id":"worker-1","status":"done"}]})),
            Phase::Done,
        );
        let node = view.snapshot();
        assert_eq!(node.result.data.unwrap()["jobs"][0]["id"], "worker-1");
        body.append("late");
        assert_eq!(view.snapshot().result.text, "summary");
        tree.persisted(&view.snapshot());
        assert!(tree.pending().is_empty());
    }
}
