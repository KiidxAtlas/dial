//! The in-file entry tree: the navigable view of one journal, entry labels and leaf moves.
//!
//! The journal is a tree whose active branch is the parent chain of the last entry. This module
//! lists the nodes a user can navigate to (user messages and labeled entries, abandoned branches
//! included), attaches short labels to entries, and moves the active leaf with the same
//! append-only marker a rewind writes, so no history is ever deleted or rewritten.

use super::*;
use anyhow::bail;

/// Longest label, in characters. Labels are short bookmarks, not notes.
pub const MAX_LABEL_CHARS: usize = 60;
/// Characters of an entry's text kept for its tree row.
const NODE_PREVIEW_CHARS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeNodeKind {
    User,
    Assistant,
    Summary,
    Other,
}

/// One navigable entry of the session tree, in display (depth-first) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub id: String,
    /// Indentation level: it grows only where the conversation forks.
    pub depth: usize,
    pub kind: TreeNodeKind,
    /// One-line preview of the entry's text.
    pub text: String,
    pub label: Option<String>,
    pub ts: String,
    /// The node lies on the active branch (the path the model currently sees).
    pub on_active_branch: bool,
    /// The nearest node at or above the active leaf: where the conversation stands now.
    pub current: bool,
    /// Navigable nodes directly below this one; more than one marks a fork.
    pub children: usize,
}

/// What a leaf move did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Navigation {
    /// False when the leaf was already at the requested place; nothing was appended.
    pub moved: bool,
    /// For a user message: its text, which the caller puts back in the composer to edit and resend.
    pub prefill: Option<String>,
}

impl Session {
    /// The latest label of each entry, in file order across every branch; an empty label clears.
    fn labels(&self) -> HashMap<&str, &str> {
        let mut labels = HashMap::new();
        for entry in &self.entries {
            if let Kind::Label { target, label } = &entry.kind {
                if label.is_empty() {
                    labels.remove(target.as_str());
                } else {
                    labels.insert(target.as_str(), label.as_str());
                }
            }
        }
        labels
    }

    /// The label on entry `id`, if any.
    pub fn label_of(&self, id: &str) -> Option<&str> {
        self.labels().get(id).copied()
    }

    /// Labels the entry the conversation currently ends at: the newest message or compaction on
    /// the active branch. An empty `label` clears it. Returns that entry's id.
    pub fn label_leaf(&mut self, label: &str) -> Result<String> {
        let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
        ensure!(
            label.chars().count() <= MAX_LABEL_CHARS,
            "a label is at most {MAX_LABEL_CHARS} characters"
        );
        let target = self
            .active
            .branch
            .iter()
            .rev()
            .map(|&i| &self.entries[i])
            .find(|entry| match &entry.kind {
                Kind::Message(m) => Self::is_safe_cut(m),
                Kind::Compaction { .. } => true,
                _ => false,
            })
            .map(|entry| entry.id.clone())
            .context("nothing to label yet: the conversation has no messages")?;
        let current = self.label_of(&target).unwrap_or("");
        if current != label {
            self.push(Kind::Label {
                target: target.clone(),
                label,
            })?;
        }
        Ok(target)
    }

    /// Where a leaf may rest without splitting a tool-call group: a user turn or a final answer.
    fn is_safe_cut(message: &Value) -> bool {
        match role_of(message) {
            "user" => true,
            "assistant" => message["tool_calls"].as_array().is_none_or(Vec::is_empty),
            _ => false,
        }
    }

    fn is_user_turn(message: &Value) -> bool {
        role_of(message) == "user"
            && is_conversation_message(message)
            && message["dial_compaction_summary"] != true
    }

    /// Navigable nodes in depth-first order, oldest sibling first. A node is a real user message
    /// or any labeled entry; abandoned branches are included.
    pub fn tree(&self) -> Vec<TreeNode> {
        let labels = self.labels();
        let is_node: Vec<bool> = self
            .entries
            .iter()
            .map(|entry| {
                labels.contains_key(entry.id.as_str())
                    || matches!(&entry.kind, Kind::Message(m) if Self::is_user_turn(m))
            })
            .collect();
        // Nearest node strictly above each entry. Parents precede children in an intact file; a
        // forward or missing link ends the chain, as it does for the active branch.
        let mut above: Vec<Option<usize>> = vec![None; self.entries.len()];
        for (i, entry) in self.entries.iter().enumerate() {
            above[i] = entry
                .parent
                .as_deref()
                .and_then(|p| self.by_id.get(p))
                .copied()
                .filter(|&p| p < i)
                .and_then(|p| if is_node[p] { Some(p) } else { above[p] });
        }
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); self.entries.len()];
        let mut roots = Vec::new();
        for i in (0..self.entries.len()).filter(|&i| is_node[i]) {
            match above[i] {
                Some(parent) => children[parent].push(i),
                None => roots.push(i),
            }
        }
        let on_branch: HashSet<usize> = self.active.branch.iter().copied().collect();
        let current = self
            .active
            .branch
            .iter()
            .rev()
            .copied()
            .find(|&i| is_node[i]);
        let mut out = Vec::new();
        let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&i| (i, 0)).collect();
        while let Some((i, depth)) = stack.pop() {
            let entry = &self.entries[i];
            let (kind, text) = match &entry.kind {
                Kind::Message(m) if role_of(m) == "user" => (TreeNodeKind::User, content_text(m)),
                Kind::Message(m) => (TreeNodeKind::Assistant, content_text(m)),
                Kind::Compaction { summary, .. } => (TreeNodeKind::Summary, summary.clone()),
                _ => (TreeNodeKind::Other, String::new()),
            };
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let forks = children[i].len() > 1;
            out.push(TreeNode {
                id: entry.id.clone(),
                depth,
                kind,
                text: truncate_chars(&text, NODE_PREVIEW_CHARS),
                label: labels.get(entry.id.as_str()).map(|l| (*l).to_owned()),
                ts: entry.ts.clone(),
                on_active_branch: on_branch.contains(&i),
                current: current == Some(i),
                children: children[i].len(),
            });
            let child_depth = depth + usize::from(forks);
            stack.extend(children[i].iter().rev().map(|&c| (c, child_depth)));
        }
        out
    }

    /// Moves the active leaf to node `target` of [`Session::tree`]. A user message is entered from
    /// just before it, like a rewind, and its text comes back as `prefill`; any other node becomes
    /// the leaf itself. The move is an appended marker whose parent is the new leaf: the branch
    /// being left stays in the file and reachable from the tree.
    pub fn navigate_to(&mut self, target: &str) -> Result<Navigation> {
        let index = *self
            .by_id
            .get(target)
            .with_context(|| format!("entry {target} is not in this session"))?;
        let (new_leaf, prefill) = match &self.entries[index].kind {
            Kind::Message(m) if Self::is_user_turn(m) => {
                (self.entries[index].parent.clone(), Some(content_text(m)))
            }
            Kind::Message(m) if role_of(m) == "dial_state" => {
                bail!("entry {target} is internal state, not a conversation turn")
            }
            Kind::Message(m) if Self::is_safe_cut(m) => (Some(target.to_owned()), None),
            Kind::Compaction { .. } => (Some(target.to_owned()), None),
            Kind::Message(_) => {
                bail!("entry {target} is inside a tool-call group; pick a message or answer")
            }
            _ => bail!("entry {target} is not a navigable conversation entry"),
        };
        // Messages on the branch being left, below its common ancestor with the destination.
        let destination: HashSet<usize> = self.ancestry(new_leaf.as_deref()).into_iter().collect();
        let leaving = self
            .active
            .branch
            .iter()
            .filter(|i| !destination.contains(i))
            .filter(|&&i| matches!(&self.entries[i].kind, Kind::Message(m) if role_of(m) != "dial_state"))
            .count();
        // Already there: the destination is on the active branch and nothing conversational
        // follows it (labels, state and markers do not count).
        let on_branch = new_leaf
            .as_deref()
            .is_none_or(|leaf| self.contains_branch_leaf(leaf));
        if leaving == 0 && on_branch {
            return Ok(Navigation {
                moved: false,
                prefill,
            });
        }
        self.commit_leaf_move(new_leaf, "navigate", leaving.max(1))?;
        Ok(Navigation {
            moved: true,
            prefill,
        })
    }

    /// Entry indices from the root down to `leaf`, following parent links (cycle-safe).
    pub(super) fn ancestry(&self, leaf: Option<&str>) -> Vec<usize> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = leaf.and_then(|id| self.by_id.get(id)).copied();
        while let Some(i) = cur {
            if !seen.insert(i) {
                break;
            }
            chain.push(i);
            cur = self.entries[i]
                .parent
                .as_deref()
                .and_then(|p| self.by_id.get(p))
                .copied();
        }
        chain.reverse();
        chain
    }
}
