//! Runtime-stream state: persisting, restoring and reclaiming in-flight stream records.

use super::*;

impl Session {
    pub fn runtime(&self) -> crate::runtime::Tree {
        // Runtime children are anchored in this session's append-only branch tree.
        self.runtime.anchor_if_empty(self.id.clone());
        self.runtime.clone()
    }

    /// Interrupted observations that have no equivalent committed assistant message after their
    /// anchor. They remain presentation records and never become provider instructions or calls.
    pub fn interrupted_streams(&self) -> Vec<crate::runtime::Body> {
        let mut bodies: Vec<_> = ["assistant", "reasoning"]
            .into_iter()
            .flat_map(|kind| self.runtime.bodies(kind))
            .filter(|body| {
                body.read(|node| {
                    if node.result.phase != crate::runtime::Phase::Interrupted
                        || node.result.text.is_empty()
                    {
                        return false;
                    }
                    let start = node
                        .parent
                        .as_ref()
                        .and_then(|parent| self.by_id.get(parent))
                        .and_then(|index| self.active.branch.iter().position(|i| i == index))
                        .map_or(0, |position| position + 1);
                    !self.active.branch[start..]
                        .iter()
                        .filter_map(|&index| match &self.entries[index].kind {
                            Kind::Message(message) => Some(message),
                            _ => None,
                        })
                        .take_while(|message| message["role"] != "user")
                        .any(|message| {
                            message["role"] == "assistant"
                                && content_text(message) == node.result.text
                        })
                })
            })
            .collect();
        bodies.sort_by_key(|body| {
            body.read(|node| {
                node.id
                    .strip_prefix("runtime-")
                    .and_then(|id| id.parse::<u64>().ok())
                    .unwrap_or(0)
            })
        });
        bodies
    }

    /// Active conversation nodes and mutable result children, with resolvable parent ids.
    pub fn execution_tree(&self) -> Vec<crate::runtime::Node> {
        use crate::runtime::{Node, Phase, ResultBody};
        let mut nodes = vec![Node {
            id: self.id.clone(),
            parent: None,
            kind: "session".into(),
            label: self.title(),
            presentation: None,
            call_id: None,
            result: ResultBody {
                execution: None,
                text: String::new(),
                data: Some(json!({"model":self.model})),
                phase: Phase::Completed,
                revision: 0,
            },
        }];
        let mut parent = Some(self.id.clone());
        for &i in self.branch() {
            let entry = &self.entries[i];
            if let Kind::Message(message) = &entry.kind {
                nodes.push(Node {
                    id: entry.id.clone(),
                    parent,
                    kind: "message".into(),
                    label: role_of(message).into(),
                    presentation: None,
                    call_id: None,
                    result: ResultBody {
                        execution: None,
                        text: content_text(message),
                        data: Some(message.clone()),
                        phase: Phase::Completed,
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
        let started = Instant::now();
        let result = self.flush_runtime_journaled();
        bump(&mut self.io.runtime_flush_attempts);
        add_elapsed(&mut self.io.runtime_flush_micros, started);
        if result.is_err() {
            bump(&mut self.io.runtime_flush_failures);
        }
        result
    }

    fn flush_runtime_journaled(&mut self) -> Result<()> {
        self.runtime
            .check_persistence()
            .map_err(anyhow::Error::msg)?;
        for node in self.runtime.pending_boundary_snapshots() {
            self.push(Kind::Runtime(Box::new(node.clone())))?;
            self.runtime.persisted(&node);
        }
        self.reclaim_streams()?;
        Ok(())
    }
    pub fn flush_settled_runtime(&mut self) -> Result<()> {
        for node in self.runtime.pending_settled() {
            self.push(Kind::Runtime(Box::new(node.clone())))?;
            self.runtime.persisted(&node);
        }
        self.reclaim_streams()?;
        Ok(())
    }

    fn reclaim_streams(&mut self) -> Result<()> {
        let Some(journal) = self
            .runtime
            .stream_writer()
            .filter(|writer| writer.needs_reclamation())
        else {
            return Ok(());
        };
        let Some(file) = &self.file else {
            return Ok(());
        };
        let started = Instant::now();
        let synced = crate::platform::fs::sync_file_data(file);
        bump(&mut self.io.runtime_reclaim_sync_attempts);
        add_elapsed(&mut self.io.runtime_reclaim_sync_micros, started);
        if synced.is_err() {
            bump(&mut self.io.runtime_reclaim_sync_failures);
        }
        synced.context("syncing runtime snapshots before stream reclamation")?;
        let mut acknowledged = std::collections::BTreeMap::<String, u64>::new();
        for entry in &self.entries {
            if let Kind::Runtime(node) = &entry.kind {
                let revision = acknowledged.entry(node.id.clone()).or_default();
                *revision = (*revision).max(node.result.revision);
            }
        }
        journal.checkpoint(&acknowledged)
    }

    pub(super) fn restore_runtime(&mut self) -> Result<()> {
        self.runtime = crate::runtime::Tree::default();
        // Never recycle result ids from abandoned branches.
        for entry in &self.entries {
            if let Kind::Runtime(node) = &entry.kind {
                self.runtime.reserve(&node.id);
            }
        }
        for &i in &self.active.branch {
            if let Kind::Runtime(node) = &self.entries[i].kind {
                self.runtime.restore((**node).clone());
            }
        }
        if let Some(path) = &self.path {
            let mut parents: HashSet<String> = self
                .active
                .branch
                .iter()
                .map(|&i| self.entries[i].id.clone())
                .chain(std::iter::once(self.id.clone()))
                .collect();
            let existing: HashMap<_, _> = self
                .runtime
                .snapshots()
                .into_iter()
                .map(|node| {
                    parents.insert(node.id.clone());
                    (node.id, node.result.revision)
                })
                .collect();
            let mut nodes = crate::runtime::stream_journal::recover(path)?;
            for node in &nodes {
                self.runtime.reserve(&node.id);
            }
            loop {
                let before = nodes.len();
                nodes.retain(|node| {
                    if !node
                        .parent
                        .as_ref()
                        .is_some_and(|parent| parents.contains(parent))
                    {
                        return true;
                    }
                    parents.insert(node.id.clone());
                    if existing
                        .get(&node.id)
                        .is_none_or(|revision| node.result.revision > *revision)
                    {
                        self.runtime.restore(node.clone());
                    }
                    false
                });
                if nodes.len() == before {
                    break;
                }
            }
        }
        self.runtime.interrupt_open();
        let parent = self.branch().iter().rev().find_map(|&i| {
            matches!(self.entries[i].kind, Kind::Message(_)).then(|| self.entries[i].id.clone())
        });
        self.runtime
            .anchor(parent.or_else(|| Some(self.id.clone())));
        if self.file.is_some()
            && let Some(path) = &self.path
        {
            self.runtime
                .journal(crate::runtime::stream_journal::Journal::open(path)?);
        }
        Ok(())
    }
}
