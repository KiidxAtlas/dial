//! Exploration checkpoints: control entries that close exploration groups and rewind to an anchor.

use super::*;

/// One validated exploration rewind: the checkpoint it anchors to and the report branch it appends.
pub struct ExplorationRewind<'a> {
    pub owner_id: &'a str,
    pub expected_leaf: &'a str,
    pub anchor: &'a str,
    pub report: Value,
    pub siblings: Vec<Value>,
    pub control: Value,
    pub digest: String,
}

impl Session {
    pub fn exploration_timestamp() -> String {
        iso_now()
    }

    fn exploration_replay(&self) -> impl Iterator<Item = &Value> {
        self.compaction_prefix().into_iter().flatten().chain(
            self.active.messages.iter().filter_map(|&index| {
                if let Kind::Message(message) = &self.entries[index].kind {
                    Some(message)
                } else {
                    None
                }
            }),
        )
    }

    pub fn ensure_exploration_groups_closed(&self) -> Result<()> {
        ensure_closed_context_groups(self.exploration_replay())
    }

    pub fn has_exploration_result(&self, call_id: &str) -> bool {
        self.exploration_replay().any(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == call_id
                && message["dial_exploration_result"] == true
        })
    }

    pub fn completed_exploration_checkpoint(&self, anchor: &str) -> bool {
        self.exploration_replay().any(|message| {
            message["dial_rewind_report"] == true
                && message["dial_exploration_proof"]["checkpointEntryId"] == anchor
        })
    }

    pub fn commit_exploration_control(
        &mut self,
        owner_id: &str,
        expected_leaf: &str,
        state: Value,
    ) -> Result<JournalCommit> {
        ensure!(
            self.id == owner_id && self.branch_leaf() == Some(expected_leaf),
            "checkpoint owner or branch changed; no activation committed"
        );
        self.ensure_exploration_groups_closed()?;
        self.append_context_entry(Kind::Message(json!({"role":"dial_state","state":state})))
    }

    /// Validate the authentic checkpoint replay without copying it or inventing portable
    /// substitutes for signed/encrypted provider history. Branch ancestry retains that replay.
    pub fn ensure_exploration_anchor_closed(&self, anchor: &str) -> Result<()> {
        let at = self
            .active
            .branch
            .iter()
            .position(|&index| self.entries[index].id == anchor)
            .context("investigation checkpoint is not on the owning active branch")?;
        let branch = &self.active.branch[..=at];
        let boundary = branch.iter().rposition(|&index| {
            matches!(
                self.entries[index].kind,
                Kind::Compaction { .. } | Kind::ContextReset { .. }
            )
        });
        let (prefix, before, after): (&[Value], &[usize], &[usize]) = match boundary {
            None => (&[], &[], branch),
            Some(at) => match &self.entries[branch[at]].kind {
                Kind::Compaction {
                    prefix, first_kept, ..
                } => {
                    let start = first_kept.as_deref().and_then(|id| {
                        branch[..at]
                            .iter()
                            .position(|&index| self.entries[index].id == id)
                    });
                    (
                        prefix.as_deref().unwrap_or(&[]),
                        start.map(|start| &branch[start..at]).unwrap_or(&[]),
                        &branch[at + 1..],
                    )
                }
                Kind::ContextReset { bootstrap, .. } => (bootstrap, &[], &branch[at + 1..]),
                _ => unreachable!(),
            },
        };
        ensure_closed_context_groups(prefix.iter().chain(before.iter().chain(after).filter_map(
            |&index| {
                if let Kind::Message(message) = &self.entries[index].kind
                    && message["role"] != "dial_state"
                {
                    Some(message)
                } else {
                    None
                }
            },
        )))
    }

    /// Append a new report branch under the checkpoint; the discarded exploration remains
    /// immutable and searchable. Current same-owner notebook, controls and real runtime
    /// outcomes are carried, not rolled back to obsolete approvals/tasks at the checkpoint.
    pub fn rewind_exploration(&mut self, rewind: ExplorationRewind<'_>) -> Result<JournalCommit> {
        let ExplorationRewind {
            owner_id,
            expected_leaf,
            anchor,
            report,
            siblings,
            control,
            digest,
        } = rewind;
        ensure!(
            self.owns_journal() && self.id == owner_id && self.branch_leaf() == Some(expected_leaf),
            "rewind owner or branch changed; no report branch committed"
        );
        ensure!(
            report["dial_rewind_report"] == true
                && report["dial_exploration_proof"]["checkpointEntryId"] == anchor
                && report["dial_exploration_proof"]["digest"] == digest,
            "rewind report has no matching checkpoint proof"
        );
        ensure!(
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "rewind requires its validated branch digest"
        );
        self.ensure_exploration_groups_closed()?;
        let anchor_at = self
            .active
            .branch
            .iter()
            .position(|&index| self.entries[index].id == anchor)
            .context("rewind checkpoint is no longer an active ancestor")?;
        self.ensure_exploration_anchor_closed(anchor)?;
        ensure!(
            report["role"] == "user" && report["dial_transient"] != true,
            "rewind report must be an ordinary authenticated investigation message"
        );
        ensure!(
            siblings.is_empty() || valid_context_bootstrap(&siblings),
            "rewind must retain complete authentic terminal sibling groups"
        );
        ensure!(
            control.is_object(),
            "rewind control state is not an object; no report branch committed"
        );
        let mut staged_control = control.clone();
        staged_control["exploration"] = self.control_state()["exploration"].clone();
        let mut kinds = vec![Kind::Message(
            json!({"role":"dial_state","state":staged_control}),
        )];
        kinds.extend(
            self.active.branch[anchor_at + 1..]
                .iter()
                .filter_map(|&index| match &self.entries[index].kind {
                    Kind::Runtime(node) => Some(Kind::Runtime(node.clone())),
                    Kind::Custom { custom_type, data } => Some(Kind::Custom {
                        custom_type: custom_type.clone(),
                        data: data.clone(),
                    }),
                    _ => None,
                }),
        );
        kinds.push(Kind::Message(report));
        kinds.extend(siblings.into_iter().map(Kind::Message));
        kinds.push(Kind::Message(json!({"role":"dial_state","state":control})));
        self.append_context_batch(Some(anchor.to_owned()), kinds)
    }
}
