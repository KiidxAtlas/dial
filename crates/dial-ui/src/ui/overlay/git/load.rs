//! Bounded, coalescing repository reads. The input thread only accepts finished snapshots.
use super::{GitOverlay, model::*, tree::Entry, view};
use crate::{
    git_panel::{Repo, Watch},
    ui::images::ImageProtocol,
};
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Default)]
pub(super) struct Reads {
    status: Option<Receiver<Result<Status, String>>>,
    pub(super) status_pending: bool,
    diff: Option<Receiver<(u64, Result<Shown, String>)>>,
    pub(super) pending_diff: Option<DiffRequest>,
    pub(super) generation: u64,
    pub(super) landing: Option<bool>,
}

impl Reads {
    pub(super) fn busy(&self) -> bool {
        self.status.is_some()
            || self.status_pending
            || self.diff.is_some()
            || self.pending_diff.is_some()
    }
}

struct Status {
    repo: Repo,
    commit: Option<Commit>,
    changes: Vec<Change>,
    branch: Option<String>,
    head: Option<(String, String)>,
    recent: Result<Vec<(String, String)>, String>,
    watch: Option<Watch>,
    watch_error: Option<String>,
}

pub(super) struct DiffRequest {
    pub(super) repo: Repo,
    pub(super) change: Change,
    pub(super) revision: Option<String>,
    pub(super) view: View,
    pub(super) whitespace: view::Whitespace,
    pub(super) protocol: Option<ImageProtocol>,
    pub(super) generation: u64,
}

impl DiffRequest {
    fn run(&self) -> Result<Shown, String> {
        let doc = self.git_diff(CONTEXT)?;
        let binary = if view::is_binary_doc(&doc) {
            Some(view::load_binary(
                &self.repo,
                &self.change,
                self.revision.as_deref(),
                self.protocol,
            )?)
        } else {
            None
        };
        let full = if self.view == View::File && binary.is_none() {
            Some(self.git_diff(FULL_CONTEXT)?)
        } else {
            None
        };
        Ok(Shown {
            change: self.change.clone(),
            doc,
            full,
            binary,
        })
    }

    fn git_diff(&self, context: &str) -> Result<Doc, String> {
        if self.change.kind == Kind::Untracked {
            return self.repo.untracked_doc(&self.change.path);
        }
        let mut args = match &self.revision {
            Some(sha) => vec![
                "show",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                "--format=",
                context,
                sha,
            ],
            None if self.change.area == Area::Staged => vec![
                "diff",
                "--cached",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                context,
            ],
            None => vec![
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                context,
            ],
        };
        args.extend(self.whitespace.git_args().iter().copied());
        args.extend(["--", self.change.path.as_str()]);
        self.repo.diff(&args)
    }
}

fn status(repo: Repo, revision: Option<String>, watch: bool) -> Result<Status, String> {
    let repo = repo.at_repository_root()?;
    let (commit, mut changes) = match revision {
        Some(rev) => {
            let sha = repo.resolve(&rev)?;
            (Some(repo.commit_info(&sha)?), repo.commit_files(&sha)?)
        }
        None => (None, repo.status()?),
    };
    sort_changes(&mut changes);
    let (branch, head) = if commit.is_none() {
        repo.branch_and_head()
    } else {
        (None, None)
    };
    let recent = repo.recent_commits();
    let (watch, watch_error) = if watch && commit.is_none() {
        match repo.watch() {
            Ok(watch) => (Some(watch), None),
            Err(error) => (None, Some(error)),
        }
    } else {
        (None, None)
    };
    Ok(Status {
        repo,
        commit,
        changes,
        branch,
        head,
        recent,
        watch,
        watch_error,
    })
}

/// Disconnection is a visible failure, not a permanently pending panel.
fn receive<T>(slot: &mut Option<Receiver<T>>, wait: bool) -> Option<Result<T, String>> {
    let receiver = slot.as_ref()?;
    let result = if wait {
        receiver
            .recv_timeout(std::time::Duration::from_secs(15))
            .map_err(|error| error.to_string())
    } else {
        match receiver.try_recv() {
            Ok(value) => Ok(value),
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("repository read worker stopped".to_owned()),
        }
    };
    *slot = None;
    Some(result)
}

impl GitOverlay {
    pub(super) fn start_status(&mut self) {
        if self.reads.status.is_some() || !self.reads.status_pending {
            return;
        }
        self.reads.status_pending = false;
        let repo = self.repo.clone();
        let revision = self.revision.clone();
        let watch = self.loading || self.watch.is_none();
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("git-panel-status".to_owned())
            .spawn(move || {
                let _ = tx.send(status(repo, revision, watch));
            }) {
            Ok(_) => self.reads.status = Some(rx),
            Err(error) => {
                self.loading = true;
                self.flash(format!("starting repository read: {error}"), true);
            }
        }
    }

    pub(super) fn start_diff(&mut self) {
        if self.reads.diff.is_some() {
            return;
        }
        let Some(request) = self.reads.pending_diff.take() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("git-panel-diff".to_owned())
            .spawn(move || {
                let _ = tx.send((request.generation, request.run()));
            }) {
            Ok(_) => self.reads.diff = Some(rx),
            Err(error) => self.flash(format!("starting diff read: {error}"), true),
        }
    }

    pub(super) fn poll_reads(&mut self) {
        self.poll_reads_inner(false);
    }

    /// Never let a shortcut fall back from an unavailable hunk to a whole-file mutation.
    pub(super) fn mutation_ready(&mut self) -> bool {
        if self.revision.is_some() {
            self.flash("The commit view is read-only", true);
            return false;
        }
        if self.loading || self.reads.busy() {
            self.flash("Repository read pending; wait before changing files", true);
            return false;
        }
        true
    }
    fn poll_reads_inner(&mut self, wait: bool) {
        if let Some(result) = receive(&mut self.reads.status, wait) {
            // A refresh requested after this snapshot started supersedes it.
            if !self.reads.status_pending {
                match result.and_then(|value| value) {
                    Ok(next) => self.accept_status(next),
                    Err(error) => {
                        self.loading = true;
                        self.flash(error, true);
                    }
                }
            }
            self.start_status();
        }
        if let Some(result) = receive(&mut self.reads.diff, wait) {
            match result {
                Ok((generation, result)) if generation == self.reads.generation => match result {
                    Ok(shown) => {
                        self.shown = Some(shown);
                        self.rebuild_rows();
                        if let Some(backwards) = self.reads.landing.take() {
                            self.cursor_row = if backwards {
                                self.hunk_rows().last()
                            } else {
                                self.hunk_rows().next()
                            }
                            .unwrap_or(0);
                            self.sync_hunk();
                        }
                    }
                    Err(error) => self.flash(error, true),
                },
                Err(error) => self.flash(error, true),
                _ => {} // Selection/view/whitespace changed while the worker was reading.
            }
            self.start_diff();
        }
    }

    fn accept_status(&mut self, next: Status) {
        let initial = self.loading;
        let key = self.selected().cloned();
        self.repo = next.repo;
        self.commit = next.commit;
        self.branch = next.branch;
        self.head = next.head;
        self.recent = next.recent;
        self.log_cursor = self
            .log_cursor
            .min(self.recent.as_ref().map_or(0, Vec::len).saturating_sub(1));
        self.changes = next.changes;
        if self.watch.is_none() {
            self.watch = next.watch;
            self.watch_error = next.watch_error;
        }
        self.loading = false;
        let entries = self.entries();
        self.cursor = if initial {
            self.first_file_row()
        } else {
            key.and_then(|key| entries.iter().position(|entry| matches!(entry, Entry::File { index, .. } if self.changes[*index] == key)))
                .unwrap_or_else(|| self.cursor.min(entries.len().saturating_sub(1)))
        };
        self.load_shown();
        if initial {
            self.status = None;
        }
        if let Some(error) = &self.watch_error {
            self.flash(Self::watch_notice(error), true);
        }
    }

    #[cfg(test)]
    pub(super) fn wait_reads(&mut self) {
        while self.reads.status.is_some() || self.reads.diff.is_some() {
            self.poll_reads_inner(true);
        }
    }
}

#[cfg(test)]
mod tests;
