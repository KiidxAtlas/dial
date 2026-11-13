//! Active-branch view, message projections, compaction, archives and exports.

use super::*;

const COMPACTION_FRAME_OPEN: &str =
    "<summary>\nSummary of the earlier conversation (older messages were compacted away):\n";
const COMPACTION_FRAME_CLOSE: &str = "\n</summary>";
/// Position in a caller-owned message projection. Reuse only with the same projection kind.
#[derive(Default)]
pub struct MessageCursor {
    session: String,
    branch_revision: u64,
    compaction: Option<String>,
    count: usize,
    tail: Option<String>,
}

/// Only newly appended messages, or the complete replacement after a branch/view change.
pub struct MessageDelta<'a> {
    pub reset: bool,
    pub messages: Vec<std::borrow::Cow<'a, Value>>,
}

impl Session {
    /// Export the active conversation followed by every subagent transcript this session spawned.
    /// Credentials are redacted and HTML embeds verified images (`storage::export`).
    pub fn export(&self, layout: &Layout, path: &Path, html: bool) -> Result<()> {
        let body = self.export_text(layout, html)?;
        let mut file =
            platform_fs::open_owner_only(OpenOptions::new().write(true).create_new(true), path)
                .with_context(|| format!("creating export {}", path.display()))?;
        file.write_all(body.as_bytes())
            .context("writing session export")
    }

    /// The rendered export document (see `export`).
    pub fn export_text(&self, layout: &Layout, html: bool) -> Result<String> {
        let messages: Vec<&Value> = self
            .branch()
            .iter()
            .filter_map(|&i| match &self.entries[i].kind {
                Kind::Message(message) if role_of(message) != "dial_state" => Some(message),
                _ => None,
            })
            .collect();
        let workers = crate::storage::subagent::workers(layout, self.id())?;
        Ok(crate::storage::export::render_with_workers(
            &self.title(),
            &messages,
            &workers,
            html,
        ))
    }

    /// The share snapshot for the default scope: a session header, then the active branch in order.
    /// Abandoned branches, rewind markers and internal records (`dial_state` messages and session
    /// meta) stay local. Only the explicit whole-journal diagnostic mode shares the raw journal.
    pub fn share_records(&self) -> Vec<Value> {
        let mut records = vec![json!({
            "type": "session",
            "id": self.id,
            "started": self.started,
            "model": self.model,
            "version": env!("CARGO_PKG_VERSION"),
            "schema_version": migrate::CURRENT_SCHEMA_VERSION,
        })];
        for &i in self.branch() {
            let entry = &self.entries[i];
            match &entry.kind {
                Kind::Meta { .. } => {}
                Kind::Message(message) if role_of(message) == "dial_state" => {}
                _ => records.push(entry_json(entry, false)),
            }
        }
        records
    }

    /// Visible user messages that `rewind` can branch from, oldest first: (entry id, text).
    /// The n-th entry from the end is `rewind(n)`'s target.
    pub fn branchable_user_messages(&self) -> Vec<(String, String)> {
        let (_, visible) = self.view();
        visible
            .iter()
            .filter_map(|&i| match &self.entries[i].kind {
                Kind::Message(message) if role_of(message) == "user" => {
                    Some((self.entries[i].id.clone(), content_text(message)))
                }
                _ => None,
            })
            .collect()
    }

    /// Full immutable conversation evidence on the active branch, before compaction view cuts.
    pub fn archived_messages(&self) -> Vec<Value> {
        self.active
            .branch
            .iter()
            .filter_map(|&i| match &self.entries[i].kind {
                Kind::Message(message) if role_of(message) != "dial_state" => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// Incrementally read an archive (`archived`) or model-visible history without cloning old
    /// bodies. Immutable entry ids validate the tail; branch rebuilds and compaction invalidate
    /// the appropriate projection. Control/runtime-only appends leave both projections intact.
    pub fn message_delta<'a>(
        &'a self,
        cursor: &mut MessageCursor,
        archived: bool,
    ) -> MessageDelta<'a> {
        use std::borrow::Cow;
        let indices = if archived {
            &self.active.archived
        } else {
            &self.active.messages
        };
        let compaction = (!archived)
            .then_some(self.active.compaction)
            .flatten()
            .map(|i| self.entries[i].id.clone());
        let reset = cursor.session != self.id
            || cursor.branch_revision != self.active.revision
            || cursor.compaction != compaction
            || cursor.count > indices.len()
            || cursor.count.checked_sub(1).is_some_and(|last| {
                cursor.tail.as_deref() != Some(self.entries[indices[last]].id.as_str())
            });
        let mut messages = Vec::new();
        if reset && !archived {
            if let Some(prefix) = self.compaction_prefix() {
                messages.extend(prefix.iter().map(Cow::Borrowed));
            } else if let Some(summary) = self.view().0 {
                messages.push(Cow::Owned(Self::compaction_message(summary)));
            }
        }
        for &i in &indices[if reset { 0 } else { cursor.count }..] {
            if let Kind::Message(message) = &self.entries[i].kind {
                messages.push(Cow::Borrowed(message));
            }
        }
        cursor.session.clone_from(&self.id);
        cursor.branch_revision = self.active.revision;
        cursor.compaction = compaction;
        cursor.count = indices.len();
        cursor.tail = indices.last().map(|&i| self.entries[i].id.clone());
        MessageDelta { reset, messages }
    }

    /// Original conversation input/output only: synthetic compaction/reset prefixes and
    /// potentially large tool evidence are excluded before allocating cloned messages.
    pub fn archived_user_assistant_messages(&self) -> Vec<Value> {
        self.active
            .branch
            .iter()
            .filter_map(|&i| match &self.entries[i].kind {
                Kind::Message(message) if is_conversation_message(message) => {
                    let original = message
                        .get("dial_child_original_response")
                        .filter(|original| matches!(role_of(original), "user" | "assistant"))
                        .unwrap_or(message);
                    // Memory observes what the user typed, not the drafted prompt.
                    if let Some(typed) = typed_text(original) {
                        let mut typed_message = original.clone();
                        typed_message["content"] = json!(typed);
                        if let Some(object) = typed_message.as_object_mut() {
                            object.remove(TURN_DRAFT_KEY);
                        }
                        return Some(typed_message);
                    }
                    Some(original.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Count authentic paired native requests on this branch after the turn's predecessor.
    /// No projected copies or header-shaped tool output can create a receipt.
    pub fn paired_tool_calls_since(&self, predecessor: Option<&str>) -> usize {
        let start = match predecessor {
            Some(id) => match self
                .active
                .branch
                .iter()
                .position(|&index| self.entries[index].id == id)
            {
                Some(index) => index + 1,
                None => return 0,
            },
            None => 0,
        };
        let mut owner: Option<&Value> = None;
        let mut seen = std::collections::HashSet::new();
        let mut count = 0;
        for &index in &self.active.branch[start..] {
            let Kind::Message(message) = &self.entries[index].kind else {
                continue;
            };
            if role_of(message) == "dial_state" {
                continue;
            }
            if role_of(message) != "tool" {
                owner = (role_of(message) == "assistant").then_some(message);
                seen.clear();
                continue;
            }
            let Some(id) = message["tool_call_id"].as_str() else {
                continue;
            };
            let Some(calls) = owner.and_then(|message| message["tool_calls"].as_array()) else {
                continue;
            };
            if calls
                .iter()
                .filter(|call| call["id"].as_str() == Some(id))
                .count()
                == 1
                && seen.insert(id)
            {
                count += 1;
            }
        }
        count
    }

    /// Entry indices from the root to the leaf, shared by all history readers.
    pub(super) fn branch(&self) -> &[usize] {
        &self.active.branch
    }

    /// Rebuild only after loading or changing branches. Keep the same missing-parent and
    /// cycle behavior as walking the persisted tree directly.
    pub(super) fn rebuild_active(&mut self) {
        self.active.revision += 1;
        self.active.branch.clear();
        self.active.control = None;
        self.active.control_state = Value::Null;
        self.active.extendable = true;
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
            self.active.branch.push(i);
            let parent = self.entries[i]
                .parent
                .as_deref()
                .and_then(|p| self.by_id.get(p))
                .copied();
            if self.entries[i].parent.is_some() && !parent.is_some_and(|p| p < i) {
                self.active.extendable = false;
            }
            cur = parent;
        }
        self.active.branch.reverse();
        self.active.archived = self
            .active
            .branch
            .iter()
            .copied()
            .filter(
                |&i| matches!(&self.entries[i].kind, Kind::Message(m) if m["role"] != "dial_state"),
            )
            .collect();
        self.active.control = self.active.branch.iter().rev().copied().find(
            |&i| matches!(&self.entries[i].kind, Kind::Message(m) if m["role"] == "dial_state"),
        );
        let mut control_state = Value::Null;
        for &i in &self.active.branch {
            if let Kind::Message(m) = &self.entries[i].kind
                && m["role"] == "dial_state"
            {
                apply_control_entry(&mut control_state, m);
            }
        }
        self.active.control_state = control_state;
        self.active.maintenance_revision = self
            .active
            .branch
            .iter()
            .rev()
            .copied()
            .find(|&index| !matches!(&self.entries[index].kind, Kind::Runtime(_)));
        self.rebuild_view();
    }

    /// Recompute the visible indices when a compaction changes the kept range.
    pub(super) fn rebuild_view(&mut self) {
        self.active.messages.clear();
        self.active.compaction = None;
        let branch = &self.active.branch;
        let is_message = |&&i: &&usize| matches!(&self.entries[i].kind, Kind::Message(m) if m["role"] != "dial_state");
        let Some(p) = branch.iter().rposition(|&i| {
            matches!(
                self.entries[i].kind,
                Kind::Compaction { .. } | Kind::ContextReset { .. }
            )
        }) else {
            self.active
                .messages
                .extend(branch.iter().filter(is_message).copied());
            return;
        };
        self.active.compaction = Some(branch[p]);
        let start = match &self.entries[branch[p]].kind {
            Kind::Compaction { first_kept, .. } => first_kept
                .as_deref()
                .and_then(|f| branch[..p].iter().position(|&i| self.entries[i].id == f)),
            Kind::ContextReset { .. } => None,
            _ => return,
        };
        let before = start.map(|s| &branch[s..p]).unwrap_or(&[]);
        self.active.messages.extend(
            before
                .iter()
                .chain(&branch[p + 1..])
                .filter(is_message)
                .copied(),
        );
    }

    /// Latest compaction summary and visible message indices, without cloning either.
    pub(super) fn view(&self) -> (Option<&str>, &[usize]) {
        let summary = self
            .active
            .compaction
            .and_then(|i| match &self.entries[i].kind {
                Kind::Compaction { summary, .. } => Some(summary.as_str()),
                _ => None,
            });
        (summary, &self.active.messages)
    }

    /// The conversation as the model should see it: after a compaction, a summary message followed by
    /// the kept tail and everything since.
    pub fn messages(&self) -> Vec<Value> {
        let mut out = Vec::new();
        self.extend_messages(&mut out);
        out
    }

    /// Conversation projection with journal timestamps for presentation, not provider fields.
    /// Synthetic compaction messages keep no timestamp unless one was explicitly retained.
    pub fn timestamped_messages(&self) -> Vec<Value> {
        let mut messages = self.messages();
        let offset = messages.len() - self.active.messages.len();
        for (message, &index) in messages[offset..].iter_mut().zip(&self.active.messages) {
            if let Some(object) = message.as_object_mut() {
                object.insert("dial_timestamp".into(), json!(self.entries[index].ts));
            }
        }
        messages
    }

    /// Canonical compaction replay framing, shared with atomic request previews.
    pub fn compaction_message(summary: &str) -> Value {
        json!({
            "role": "user",
            "content": format!("{COMPACTION_FRAME_OPEN}{summary}{COMPACTION_FRAME_CLOSE}"),
            "dial_compaction_summary": true,
        })
    }

    /// The stored summary inside a `compaction_message` (including legacy messages without the marker).
    pub fn compaction_summary_text(message: &Value) -> Option<&str> {
        if message["role"] != "user" {
            return None;
        }
        message["content"]
            .as_str()?
            .strip_prefix(COMPACTION_FRAME_OPEN)?
            .strip_suffix(COMPACTION_FRAME_CLOSE)
    }

    pub(super) fn compaction_prefix(&self) -> Option<&[Value]> {
        self.active
            .compaction
            .and_then(|i| match &self.entries[i].kind {
                Kind::Compaction { prefix, .. } => prefix.as_deref(),
                Kind::ContextReset { bootstrap, .. } => Some(bootstrap),
                _ => None,
            })
    }

    pub fn latest_compaction_summary(&self) -> Option<&str> {
        self.view().0
    }

    /// Append directly into a request, avoiding a second history-sized vector.
    pub fn extend_messages(&self, out: &mut Vec<Value>) {
        let (summary, kept) = self.view();
        let prefix = self.compaction_prefix();
        out.reserve(kept.len() + prefix.map_or(usize::from(summary.is_some()), <[Value]>::len));
        if let Some(prefix) = prefix {
            out.extend_from_slice(prefix);
        } else if let Some(summary) = summary {
            out.push(Self::compaction_message(summary));
        }
        for &i in kept {
            if let Kind::Message(m) = &self.entries[i].kind {
                out.push(m.clone());
            }
        }
    }
    /// Number of messages `messages()` would return.
    pub fn message_count(&self) -> usize {
        self.active.messages.len()
            + self.compaction_prefix().map_or(
                usize::from(self.active.compaction.is_some()),
                <[Value]>::len,
            )
    }

    /// Sum `weigh` over the messages of `messages()` from index `from` on, without building that vector.
    pub fn weigh_from(&self, from: usize, weigh: impl Fn(&Value) -> usize) -> usize {
        let (summary, kept) = self.view();
        let mut total = 0usize;
        let prefix = self.compaction_prefix();
        let prefix_len = prefix.map_or(usize::from(summary.is_some()), <[Value]>::len);
        if let Some(prefix) = prefix {
            total += prefix.iter().skip(from).map(&weigh).sum::<usize>();
        } else if let Some(summary) = summary
            && from == 0
        {
            total += weigh(&Self::compaction_message(summary));
        }
        let skip = from.saturating_sub(prefix_len);
        for &i in kept.iter().skip(skip) {
            if let Kind::Message(m) = &self.entries[i].kind {
                total += weigh(m);
            }
        }
        total
    }

    /// Complete a crashed/interrupted trailing call group without dropping its assistant message.
    pub fn repair_incomplete_tools(&mut self) -> Result<usize> {
        let (_, kept) = self.view();
        let mut calls: Vec<String> = Vec::new();
        for &i in kept {
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
            let mut message = crate::llm::tool_message(
                &id,
                "completion unknown: interrupted before a durable tool result was recorded; inspect current files and process state before retrying this call",
            );
            message["dial_effects"] = serde_json::json!("unknown");
            message["dial_execution_status"] = serde_json::json!("aborted");
            self.append(message)?;
        }
        Ok(count)
    }

    /// Replaces everything but the last `keep_last` messages with `summary`. The cut never starts on a
    /// tool result (its assistant tool call stays with it). Older entries remain in the file.
    pub fn compact(&mut self, summary: String, keep_last: usize) -> Result<()> {
        self.compact_with_prefix(summary, None, keep_last)
            .map(|_| ())
    }

    /// Append one atomic view replacement; original source messages are never rewritten.
    pub fn compact_with_prefix(
        &mut self,
        summary: String,
        prefix: Option<Vec<Value>>,
        keep_last: usize,
    ) -> Result<JournalCommit> {
        if let Some(prefix) = &prefix {
            ensure!(
                !prefix.is_empty()
                    && prefix.iter().all(|m| matches!(
                        m["role"].as_str(),
                        Some("user" | "assistant" | "tool")
                    ) && m["dial_transient"] != true
                        && m["dial_portable_rules"] != true),
                "compaction prefix must contain nonempty conversation messages, not system/transient controls"
            );
            ensure!(
                crate::storage::transcript::complete_boundary(prefix, prefix.len()),
                "compaction prefix contains an incomplete or orphan tool-call group"
            );
        }
        let (_, visible) = self.view();
        let mut idx = visible.len().saturating_sub(keep_last);
        let mut pending = std::collections::HashMap::new();
        let mut group_start = 0usize;
        for (at, &entry) in visible.iter().enumerate() {
            let Kind::Message(message) = &self.entries[entry].kind else {
                continue;
            };
            if message["role"] == "assistant" {
                if pending.is_empty() {
                    group_start = at;
                }
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let id = call["id"].as_str().context("tool call has no ID")?;
                    ensure!(
                        pending.insert(id, group_start).is_none(),
                        "duplicate pending tool-call ID"
                    );
                }
            } else if message["role"] == "tool" {
                let id = message["tool_call_id"]
                    .as_str()
                    .context("tool result has no call ID")?;
                let start = pending
                    .remove(id)
                    .context("orphan tool result cannot be compacted")?;
                if keep_last != 0 && at >= idx {
                    idx = idx.min(start);
                }
            }
        }
        ensure!(
            pending.is_empty(),
            "incomplete tool-call group cannot be compacted"
        );
        let first_kept =
            (keep_last != 0 && idx < visible.len()).then(|| self.entries[visible[idx]].id.clone());
        let mut control = self.control_state();
        ensure!(
            control.is_object(),
            "compaction provider control state is not an object; no history replacement committed"
        );
        let generation = control["provider_identity"]["history_generation"]
            .as_u64()
            .unwrap_or(0)
            .checked_add(1)
            .context("provider history generation exhausted; no compaction committed")?;
        if !control["provider_identity"].is_object() {
            control["provider_identity"] = json!({});
        }
        control["provider_identity"]["history_generation"] = json!(generation);
        if self.owns_journal() {
            self.append_context_batch(
                self.leaf.clone(),
                vec![
                    Kind::Message(json!({"role":"dial_state","state":control})),
                    Kind::Compaction {
                        summary,
                        first_kept,
                        prefix,
                    },
                ],
            )
        } else {
            ensure!(
                self.path.is_none(),
                "read-only session cannot commit compaction"
            );
            self.push(Kind::Message(json!({"role":"dial_state","state":control})))?;
            let entry_id = self.push(Kind::Compaction {
                summary,
                first_kept,
                prefix,
            })?;
            Ok(JournalCommit {
                entry_id,
                durability_error: None,
            })
        }
    }
}
