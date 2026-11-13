//! Rewinding, abandoning and summarizing branches, and forking sessions.

use super::*;

impl Session {
    /// Moves the leaf back to just before the `user_turns`-th most recent user message (clamped to the
    /// oldest one in view). Nothing is deleted: the old branch stays in the file. Returns how many
    /// messages left the active conversation.
    pub fn rewind(&mut self, user_turns: usize) -> Result<usize> {
        if let Some(removed) = self.rewind_context(user_turns)? {
            return Ok(removed);
        }
        let Some((target, removed)) = self.rewind_target(user_turns) else {
            return Ok(0);
        };
        let branch = self.branch();
        let pos = branch
            .iter()
            .position(|&i| i == target)
            .context("rewind target not on the active branch")?;
        let new_leaf = pos
            .checked_sub(1)
            .map(|p| self.entries[branch[p]].id.clone());
        self.push_rewind_marker(new_leaf, removed)?;
        Ok(removed)
    }

    /// The entry index a rewind of `user_turns` lands before, and how many visible messages it removes.
    /// None for a no-op: zero turns, or no user message in view.
    fn rewind_target(&self, user_turns: usize) -> Option<(usize, usize)> {
        if user_turns == 0 {
            return None;
        }
        let (_, visible) = self.view();
        let users: Vec<usize> = (0..visible.len())
            .filter(|&n| matches!(&self.entries[visible[n]].kind, Kind::Message(m) if role_of(m) == "user"))
            .collect();
        let &n = users.iter().rev().nth(user_turns - 1).or(users.first())?;
        Some((visible[n], visible.len() - n))
    }

    /// Checkpoint ids for precisely the active conversation range a rewind would abandon.
    /// Legacy messages, copied fork links and unavailable checkpoints must not be guessed.
    pub fn rewind_checkpoint_turns(&self, user_turns: usize) -> Result<Vec<u64>> {
        let target = if let Some(targets) = self.context_rewind_targets() {
            targets
                .iter()
                .rev()
                .nth(user_turns.saturating_sub(1))
                .or(targets.first())
                .and_then(|(id, _)| self.by_id.get(*id).copied())
        } else {
            self.rewind_target(user_turns).map(|(target, _)| target)
        };
        let Some(target) = target.filter(|_| user_turns > 0) else {
            return Ok(Vec::new());
        };
        let branch = self.branch();
        let pos = branch
            .iter()
            .position(|&i| i == target)
            .context("undo target is not on the active branch")?;
        let mut turns = Vec::new();
        for &i in &branch[pos..] {
            if let Kind::Message(message) = &self.entries[i].kind
                && role_of(message) == "user"
            {
                let link = &message["dial_checkpoint"];
                ensure!(
                    link["session"].as_str() == Some(self.id()),
                    "turn {} has no matching checkpoint session link (legacy or fork); conversation kept",
                    self.entries[i].id
                );
                let turn = link["turn"]
                    .as_u64()
                    .context("turn has no durable checkpoint link; conversation kept")?;
                ensure!(
                    turns.last().is_none_or(|previous| *previous < turn),
                    "checkpoint links are not ordered; conversation kept"
                );
                turns.push(turn);
            }
        }
        Ok(turns)
    }

    /// The messages a summarized rewind of `user_turns` would abandon: the current leaf (the fork point
    /// for the summary) and the active-branch messages from the rewind point onward, chronological.
    /// None when nothing is abandoned: a no-op, or a notes-backed context reset (which keeps no branch).
    pub fn rewind_abandonment(&self, user_turns: usize) -> Option<(String, Vec<Value>)> {
        if self.context_rewind_targets().is_some() {
            return None;
        }
        let (target, _) = self.rewind_target(user_turns)?;
        let branch = self.branch();
        let pos = branch.iter().position(|&i| i == target)?;
        let from = self.leaf.clone()?;
        let messages = branch[pos..]
            .iter()
            .filter_map(|&i| match &self.entries[i].kind {
                Kind::Message(m) => Some(m.clone()),
                _ => None,
            })
            .collect();
        Some((from, messages))
    }

    /// `rewind` that also leaves a branch-summary message on the new branch, so the model keeps what the
    /// abandoned segment did. `from` must still be the leaf that `rewind_abandonment` reported: the summary
    /// is computed while the session is not locked, and a moved leaf is rejected rather than misattributed.
    pub fn rewind_summarized(
        &mut self,
        user_turns: usize,
        from: &str,
        summary: &str,
        read_files: &[String],
        modified_files: &[String],
    ) -> Result<usize> {
        ensure!(
            self.leaf.as_deref() == Some(from),
            "session moved while summarizing the abandoned branch"
        );
        ensure!(
            self.context_rewind_targets().is_none(),
            "branch summaries do not apply to notes-backed context resets"
        );
        let removed = self.rewind(user_turns)?;
        if removed > 0 {
            self.append(Self::branch_summary_message(
                summary,
                from,
                read_files,
                modified_files,
            ))?;
        }
        Ok(removed)
    }

    /// A branch summary as a history message: the compaction frame (so `compaction_summary_text` and the
    /// file-operation carry-over recognize it) plus the fork point and file lists.
    pub fn branch_summary_message(
        summary: &str,
        from_id: &str,
        read_files: &[String],
        modified_files: &[String],
    ) -> Value {
        let mut message = Self::compaction_message(summary);
        message["dial_branch_summary"] = json!({
            "from_id": from_id,
            "read_files": read_files,
            "modified_files": modified_files,
        });
        message
    }

    /// A new session holding the ancestry of `target` (any entry, including one on an inactive
    /// preserved branch), or an empty ancestry when `target` is `None`. The source is only read: its
    /// journal, active leaf, and writer are untouched. A fork at an anchor imports persisted records
    /// only; live stream supplements belong to the source's worker and are never carried.
    pub fn fork_at(&self, layout: &Layout, target: Option<&str>) -> Result<Session> {
        let chain = match target {
            Some(id) => {
                ensure!(
                    self.by_id.contains_key(id),
                    "session {} has no entry {id}",
                    self.id
                );
                self.ancestry(Some(id))
            }
            None => Vec::new(),
        };
        self.fork_chain(layout, &chain, false)
    }

    /// A new session file holding the active branch (rewind markers dropped, ids preserved).
    pub fn fork(&self, layout: &Layout) -> Result<Session> {
        self.fork_chain(layout, self.branch(), true)
    }

    /// Creates the fork file for `chain` and fills it. Any failure after the header lands removes
    /// the new file so no half-written fork is listed.
    fn fork_chain(&self, layout: &Layout, chain: &[usize], live_root: bool) -> Result<Session> {
        let mut s = Self::create_file(layout, &self.model, Some(&self.id))?;
        if let Err(err) = self.fill_fork(&mut s, chain, live_root) {
            let _ = s.remove_file();
            return Err(err);
        }
        Ok(s)
    }

    fn fill_fork(&self, s: &mut Session, chain: &[usize], live_root: bool) -> Result<()> {
        // A fork resumes with the parent's roots; this rewrites the new header before the body lands.
        s.set_additional_dirs(&self.additional_dirs)?;
        let mut kept: Vec<Entry> = Vec::new();
        for &i in chain {
            if matches!(self.entries[i].kind, Kind::Meta { .. }) {
                continue;
            }
            let mut e = self.entries[i].clone();
            if let Kind::Runtime(node) = &mut e.kind
                && node.parent.as_deref() == Some(&self.id)
            {
                node.parent = Some(s.id.clone());
            }
            e.parent = kept.last().map(|p| p.id.clone());
            kept.push(e);
        }
        let mut body = String::new();
        for e in &kept {
            let line = entry_json(e, true).to_string();
            ensure!(line.len() < MAX_LINE, "session entry over the line limit");
            body.push_str(&line);
            body.push('\n');
        }
        if let Some(file) = s.file.as_mut() {
            file.write_all(body.as_bytes())
                .context("writing forked session")?;
        }
        for e in kept {
            s.by_id.insert(e.id.clone(), s.entries.len());
            s.leaf = Some(e.id.clone());
            s.observe_history_generation(&e.kind);
            s.entries.push(e);
        }
        s.rebuild_active();
        if live_root {
            self.carry_stream_supplement(s)?;
        }
        s.restore_runtime()?;
        Ok(())
    }

    /// Copies the stream supplement recorded for this branch into the fork's own stream journal.
    /// Frames are admitted by the ancestry rule `restore_runtime` applies, and a node parented on
    /// this session is re-parented to the fork. An oversized or unreadable source supplement is an
    /// error: a partial copy must not pass as the branch's history.
    fn carry_stream_supplement(&self, s: &Session) -> Result<()> {
        let Some(source) = self.path.as_deref() else {
            return Ok(());
        };
        let Some(writer) = s.runtime.stream_writer() else {
            return Ok(());
        };
        let mut known: HashSet<String> = self
            .branch()
            .iter()
            .map(|&i| self.entries[i].id.clone())
            .collect();
        known.insert(self.id.clone());
        let mut pending = crate::runtime::stream_journal::recover_complete(source)?;
        let mut admitted = Vec::new();
        loop {
            let before = pending.len();
            pending.retain(|node| {
                if node.parent.as_ref().is_some_and(|p| known.contains(p)) {
                    known.insert(node.id.clone());
                    admitted.push(node.clone());
                    false
                } else {
                    true
                }
            });
            if pending.len() == before {
                break;
            }
        }
        for mut node in admitted {
            if node.parent.as_deref() == Some(self.id.as_str()) {
                node.parent = Some(s.id.clone());
            }
            writer.snapshot(&node);
        }
        writer.check().map_err(anyhow::Error::msg)?;
        writer.sync()?;
        Ok(())
    }
}
