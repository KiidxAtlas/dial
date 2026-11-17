use super::GitOverlay;
use super::model::*;
use crate::git_panel::{AiSummary, Generated};
use std::sync::mpsc::TryRecvError;

impl GitOverlay {
    /// `s`, `u` and `space` in the diff: stage or unstage the hunk under the cursor, or the file.
    pub(super) fn stage_current(&mut self) {
        if !self.mutation_ready() {
            return;
        }
        if self.shown.is_none() {
            return self.flash("Diff unavailable; cannot stage a hunk", true);
        }
        let hunk = if self.view == View::File || !self.hunk_ops_allowed() {
            None
        } else {
            self.rows.get(self.cursor_row).and_then(|row| row.hunk)
        };
        let Some(change) = self.selected().cloned() else {
            return;
        };
        match hunk {
            Some(index) => self.stage_hunk(index, change.area),
            None => self.toggle_file(),
        }
    }

    fn stage_hunk(&mut self, index: usize, area: Area) {
        let Some(shown) = self.shown.as_ref() else {
            return;
        };
        let doc = shown.doc.clone();
        let (reverse, message) = match area {
            Area::Unstaged => (false, "Staged hunk"),
            Area::Staged => (true, "Unstaged hunk"),
            Area::Commit => return,
        };
        self.start_mutation(move |repo| {
            repo.apply_hunk(&doc, index, true, reverse)
                .ok_or_else(|| "Hunk unavailable".to_owned())??;
            Ok((message.to_owned(), false))
        });
    }

    /// `x`: discards the hunk under the cursor from the working tree (unstaged text changes only).
    pub(super) fn discard_hunk(&mut self) {
        if !self.mutation_ready() {
            return;
        }
        let Some(change) = self.selected().cloned() else {
            return;
        };
        if change.area != Area::Unstaged
            || matches!(change.kind, Kind::Untracked | Kind::Conflicted)
        {
            return self.flash("Only unstaged text hunks can be discarded", true);
        }
        let hunk = if self.view == View::File || !self.hunk_ops_allowed() {
            None
        } else {
            self.rows.get(self.cursor_row).and_then(|row| row.hunk)
        };
        let Some(index) = hunk else {
            return self.flash("Move onto a hunk to discard it", true);
        };
        let Some(shown) = self.shown.as_ref() else {
            return;
        };
        let doc = shown.doc.clone();
        let signature = format!("{}:{:?}:{index}:{doc:?}", self.reads.generation, change);
        if !self.confirm_patch(signature, "Press x again to discard this hunk") {
            return;
        }
        self.start_mutation(move |repo| {
            repo.apply_hunk(&doc, index, false, true)
                .ok_or_else(|| "Hunk unavailable".to_owned())??;
            Ok(("Discarded hunk".to_owned(), false))
        });
    }

    /// `space` in the sidebar, or `s` on a file-level diff: stage an unstaged file or unstage a staged one.
    pub(super) fn toggle_file(&mut self) {
        if !self.mutation_ready() {
            return;
        }
        let Some(change) = self.selected().cloned() else {
            return;
        };
        if change.area == Area::Commit {
            return;
        }
        let message = if change.area == Area::Unstaged {
            format!("Staged {}", change.path)
        } else {
            format!("Unstaged {}", change.path)
        };
        self.start_mutation(move |repo| {
            repo.set_staged(&change)?;
            Ok((message, false))
        });
    }

    pub(super) fn stage_all(&mut self) {
        if !self.mutation_ready() {
            return;
        }
        self.start_mutation(|repo| {
            repo.stage_all()?;
            Ok(("Staged all changes".to_owned(), false))
        });
    }

    pub(super) fn unstage_all(&mut self) {
        if !self.mutation_ready() {
            return;
        }
        self.start_mutation(|repo| {
            repo.unstage_all()?;
            Ok(("Unstaged all changes".to_owned(), false))
        });
    }

    pub(super) fn discard_request(&mut self, pending: Option<Change>) {
        if !self.mutation_ready() {
            return;
        }
        let Some(change) = self.selected().cloned() else {
            return;
        };
        if pending.as_ref() != Some(&change) {
            self.flash(
                format!("Press delete again to discard {}", change.path),
                true,
            );
            self.confirm = Some(change);
            return;
        }
        self.start_mutation(move |repo| {
            repo.discard(&change)?;
            Ok((format!("Discarded {}", change.path), false))
        });
    }

    pub(super) fn commit(&mut self) {
        if !self.mutation_ready() {
            return;
        }
        let message = self.message.text.trim_end().to_owned();
        if message.trim().is_empty() && !self.amend {
            return self.flash("Write a commit message first", true);
        }
        self.commit_draft = Some(self.message.text.clone());
        let amend = self.amend;
        self.start_mutation(move |repo| {
            let output = repo.commit(&message, amend)?;
            Ok((
                output.lines().next().unwrap_or("Committed").to_owned(),
                true,
            ))
        });
    }

    /// One in-flight mutation, no unbounded queue; input and rendering never run Git.
    pub(super) fn start_mutation(
        &mut self,
        operation: impl FnOnce(crate::git_panel::Repo) -> Result<(String, bool), String>
        + Send
        + 'static,
    ) {
        if self.mutation.is_some() {
            return;
        }
        self.mutation = Some(self.repo.spawn_job("Git write", operation));
        self.confirm = None;
        self.discard_patch = None;
        self.flash("Git operation in progress…", false);
    }

    pub(super) fn confirm_patch(&mut self, signature: String, prompt: &str) -> bool {
        if self.discard_patch.take().as_ref() == Some(&signature) {
            return true;
        }
        self.discard_patch = Some(signature);
        self.flash(prompt, true);
        false
    }

    /// Ctrl-G in the composer: generates a message for the staged changes on a worker thread.
    /// Only runs on an empty composer, so typed text is never overwritten.
    pub(super) fn generate_message(&mut self) {
        if !self.mutation_ready() {
            return;
        }
        if !self.message.text.trim().is_empty() {
            return self.flash("Clear the message to generate one", true);
        }
        if self.generating.is_some() {
            return self.flash("Already generating a commit message", true);
        }
        let receiver = self.repo.spawn_generate();
        self.generating = Some(receiver);
        self.flash("Generating commit message…", false);
    }

    /// `A` in the sidebar: opens the prompt that describes what to stage with AI.
    pub(super) fn open_ai_prompt(&mut self) {
        if !self.mutation_ready() {
            return;
        }
        self.ai_prompt = Some(crate::ui::editor::Editor::default());
    }

    /// Enter in the AI staging prompt: classifies the unstaged changes on a worker thread.
    pub(super) fn start_ai_stage(&mut self, instruction: String) {
        if !self.mutation_ready() {
            return;
        }
        if self.ai_job.is_some() {
            return self.flash("AI staging is already running", true);
        }
        let changes: Vec<Change> = self
            .changes
            .iter()
            .filter(|change| change.area == Area::Unstaged && change.kind != Kind::Conflicted)
            .cloned()
            .collect();
        if changes.is_empty() {
            return self.flash("No unstaged changes to stage", true);
        }
        let receiver = self.repo.spawn_ai_stage(instruction, changes);
        self.ai_job = Some(receiver);
        self.flash("Classifying unstaged changes with AI…", false);
    }

    /// Applies finished background jobs. Called from `tick` every frame.
    /// Observe shared lifecycle without consuming another panel's result or retrying its operation.
    pub(super) fn poll_repo_job(&mut self) {
        let notice = self.repo.job_notice();
        if notice == self.repo_job {
            return;
        }
        let local = self.mutation.is_some() || self.ai_job.is_some() || self.generating.is_some();
        self.repo_job = notice;
        if !local
            && let Some(notice) = &self.repo_job
            && !notice.running
        {
            let (message, error) = (notice.message.clone(), notice.error);
            self.show_repo_outcome = true;
            self.sync();
            self.flash(message, error);
        }
    }

    pub(super) fn poll_jobs(&mut self) {
        self.poll_repo_job();
        match self.mutation.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(result)) => {
                self.mutation = None;
                match result {
                    Ok((message, committed)) => {
                        if committed
                            && self.commit_draft.as_deref() == Some(self.message.text.as_str())
                        {
                            self.message = crate::ui::editor::Editor::default();
                            self.amend = false;
                            self.editing = false;
                        }
                        self.sync();
                        self.flash(message, false);
                    }
                    Err(error) => {
                        self.sync();
                        self.flash(error, true);
                    }
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.mutation = None;
                self.sync();
                self.flash(
                    "Git operation stopped unexpectedly; inspect repository state",
                    true,
                );
            }
            _ => {}
        }
        let generated = self.generating.as_ref().map(|receiver| receiver.try_recv());
        match generated {
            Some(Ok(result)) => {
                self.generating = None;
                self.generated(result);
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.generating = None;
                self.flash("Commit message generation stopped unexpectedly", true);
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        let staged = self.ai_job.as_ref().map(|receiver| receiver.try_recv());
        match staged {
            Some(Ok(result)) => {
                self.ai_job = None;
                self.ai_staged(result);
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.ai_job = None;
                self.flash("AI staging stopped unexpectedly", true);
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn generated(&mut self, result: Result<Generated, String>) {
        let generated = match result {
            Ok(generated) => generated,
            Err(error) => return self.flash(error, true),
        };
        if !self.message.text.trim().is_empty() {
            return self.flash("Generated message discarded: the composer has text", true);
        }
        self.message.set(&generated.text);
        match generated.validation_error {
            Some(error) => self.flash(format!("Generated message needs a fix: {error}"), true),
            None => self.flash("Generated a commit message", false),
        }
    }

    fn ai_staged(&mut self, result: Result<AiSummary, String>) {
        self.sync();
        match result {
            Ok(summary) => self.flash(summary.message(), false),
            Err(error) => self.flash(error, true),
        }
    }
}
