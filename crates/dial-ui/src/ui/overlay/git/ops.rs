use super::GitOverlay;
use super::model::*;
use crate::git_panel::{AiSummary, Generated};
use std::sync::mpsc::TryRecvError;

impl GitOverlay {
    /// `s`, `u` and `space` in the diff: stage or unstage the hunk under the cursor, or the file.
    pub(super) fn stage_current(&mut self) {
        if self.commit.is_some() {
            return self.flash("The commit view is read-only", true);
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
        let (result, message) = match area {
            Area::Unstaged => (
                self.repo.apply_hunk(&shown.doc, index, true, false),
                "Staged hunk",
            ),
            Area::Staged => (
                self.repo.apply_hunk(&shown.doc, index, true, true),
                "Unstaged hunk",
            ),
            Area::Commit => return,
        };
        let Some(result) = result else {
            return;
        };
        self.finish(result, message);
    }

    /// `x`: discards the hunk under the cursor from the working tree (unstaged text changes only).
    pub(super) fn discard_hunk(&mut self) {
        if self.commit.is_some() {
            return self.flash("The commit view is read-only", true);
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
        let Some(result) = self.repo.apply_hunk(&shown.doc, index, false, true) else {
            return;
        };
        self.finish(result, "Discarded hunk");
    }

    /// `space` in the sidebar, or `s` on a file-level diff: stage an unstaged file or unstage a staged one.
    pub(super) fn toggle_file(&mut self) {
        if self.commit.is_some() {
            return self.flash("The commit view is read-only", true);
        }
        let Some(change) = self.selected().cloned() else {
            return;
        };
        let result = match change.area {
            Area::Commit => return,
            _ => self.repo.set_staged(&change),
        };
        let message = match change.area {
            Area::Unstaged => format!("Staged {}", change.path),
            _ => format!("Unstaged {}", change.path),
        };
        self.finish(result, message);
    }

    pub(super) fn stage_all(&mut self) {
        if self.commit.is_some() {
            return self.flash("The commit view is read-only", true);
        }
        let result = self.repo.stage_all();
        self.finish(result, "Staged all changes");
    }

    pub(super) fn unstage_all(&mut self) {
        if self.commit.is_some() {
            return self.flash("The commit view is read-only", true);
        }
        let result = self.repo.unstage_all();
        self.finish(result, "Unstaged all changes");
    }

    pub(super) fn discard_request(&mut self, pending: Option<Change>) {
        if self.commit.is_some() {
            return self.flash("The commit view is read-only", true);
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
        let result = self.repo.discard(&change);
        self.finish(result, format!("Discarded {}", change.path));
    }

    pub(super) fn commit(&mut self) {
        if self.commit.is_some() {
            return self.flash("The commit view is read-only", true);
        }
        let message = self.message.trim_end().to_owned();
        if message.trim().is_empty() && !self.amend {
            return self.flash("Write a commit message first", true);
        }
        match self.repo.commit(&message, self.amend) {
            Ok(output) => {
                let first = output.lines().next().unwrap_or("Committed").to_owned();
                self.message.clear();
                self.amend = false;
                self.editing = false;
                self.flash(first, false);
                self.sync();
            }
            Err(error) => self.flash(error, true),
        }
    }

    fn finish(&mut self, result: Result<(), String>, message: impl Into<String>) {
        match result {
            Ok(()) => {
                self.flash(message, false);
                self.sync();
            }
            Err(error) => self.flash(error, true),
        }
    }

    /// Ctrl-G in the composer: generates a message for the staged changes on a worker thread.
    /// Only runs on an empty composer, so typed text is never overwritten.
    pub(super) fn generate_message(&mut self) {
        if self.commit.is_some() {
            return self.flash("The commit view is read-only", true);
        }
        if !self.message.trim().is_empty() {
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
        if self.commit.is_some() {
            return self.flash("The commit view is read-only", true);
        }
        self.ai_prompt = Some(String::new());
    }

    /// Enter in the AI staging prompt: classifies the unstaged changes on a worker thread.
    pub(super) fn start_ai_stage(&mut self, instruction: String) {
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
    pub(super) fn poll_jobs(&mut self) {
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
        if generated.staged_all {
            self.sync();
        }
        if !self.message.trim().is_empty() {
            return self.flash("Generated message discarded: the composer has text", true);
        }
        self.message = generated.text;
        match generated.validation_error {
            Some(error) => self.flash(format!("Generated message needs a fix: {error}"), true),
            None if generated.staged_all => {
                self.flash("Staged all changes and generated a commit message", false)
            }
            None => self.flash("Generated a commit message", false),
        }
    }

    fn ai_staged(&mut self, result: Result<AiSummary, String>) {
        match result {
            Ok(summary) => {
                self.sync();
                self.flash(summary.message(), false);
            }
            Err(error) => self.flash(error, true),
        }
    }
}
