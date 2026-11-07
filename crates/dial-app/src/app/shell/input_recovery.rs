//! Private session-scoped unsent input. Restoring never starts work.
use super::Shell;
use crate::app::core::App;
use crate::config::paths::Layout;
use crate::platform::{atomic, fs};
use crate::ui::composer::recovery::RecoverySnapshot;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod images;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    project: PathBuf,
    session: String,
    pending: Vec<RecoverySnapshot>,
    current: RecoverySnapshot,
}

struct Store {
    path: PathBuf,
    _lease: File,
    record: Record,
}

impl Store {
    fn open(layout: &Layout, session: &str) -> Result<Self> {
        Self::open_inner(layout, session, false)
    }

    fn open_inner(layout: &Layout, session: &str, reset: bool) -> Result<Self> {
        ensure!(
            !session.is_empty()
                && session.len() <= 128
                && session
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid recovery session identity"
        );
        let dir = layout.state_dir.join("input-recovery");
        atomic::private_dir(&dir)?;
        let lock_path = dir.join(format!("{session}.lock"));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        let lease = fs::open_nofollow_nonblocking(&options, &lock_path)?;
        atomic::check_file(&lease)?;
        fs::try_lock_journal(&lease, true)
            .context("another Dial process owns this session's input recovery")?;
        let path = dir.join(format!("{session}.json"));
        let mut record = match if reset {
            None
        } else {
            atomic::read_private(&path)?
        } {
            Some(bytes) => serde_json::from_slice::<Record>(&bytes)
                .context("input recovery record is corrupt; kept unchanged")?,
            None => Record {
                version: 1,
                project: layout.project_root.clone(),
                session: session.into(),
                pending: Vec::new(),
                current: RecoverySnapshot::default(),
            },
        };
        ensure!(
            record.version == 1
                && record.project == layout.project_root
                && record.session == session,
            "input recovery identity/version mismatch; kept unchanged"
        );
        ensure!(
            record.pending.len() <= 4,
            "too many retained input recovery records"
        );
        record.current.validate()?;
        for snapshot in &record.pending {
            snapshot.validate()?;
        }
        images::hydrate(&mut record, &path)?;
        if !record.current.is_empty() {
            ensure!(
                record.pending.len() < 4,
                "input recovery is full; saved records kept unchanged"
            );
            record.pending.push(std::mem::take(&mut record.current));
        }
        Ok(Self {
            path,
            _lease: lease,
            record,
        })
    }

    fn publish(&mut self, record: Record) -> Result<()> {
        let bytes = images::prepare(&record, &self.path)?;
        atomic::write_private(&self.path, &bytes)?;
        self.record = record;
        Ok(())
    }

    fn checkpoint(&mut self, snapshot: &RecoverySnapshot) -> Result<()> {
        if self.record.current == *snapshot {
            return Ok(());
        }
        let mut next = self.record.clone();
        next.current = snapshot.clone();
        self.publish(next)
    }
}

#[derive(Default)]
pub(super) struct InputRecovery {
    layout: Option<Layout>,
    session: String,
    store: Option<Arc<Mutex<Store>>>,
    checked: Option<Instant>,
    error: Option<String>,
    epoch: Arc<AtomicU64>,
    save_job: Option<Receiver<std::result::Result<(), String>>>,
}

/// A review is bound to the writer lease, session, saved records and exact current input.
pub(super) struct InputPreview {
    store: Arc<Mutex<Store>>,
    session: String,
    pending: Vec<RecoverySnapshot>,
    current: RecoverySnapshot,
}

impl InputPreview {
    pub(super) fn text(&self) -> String {
        let saved = self
            .pending
            .first()
            .map_or_else(|| "[no saved input]".into(), RecoverySnapshot::preview_text);
        format!(
            "Session: {}\nSaved records: {} (oldest shown first)\n\nSAVED INPUT\n{}\n\nCURRENT INPUT\n{}\n\nRestore held refuses nonempty current input. Replace reviewed input overwrites only the current input shown here. Discard removes all saved records, not current input. Nothing here sends a request. PageUp/PageDown scroll; Keep current or Esc cancels.",
            self.session,
            self.pending.len(),
            saved,
            self.current.preview_text()
        )
    }
}

#[derive(Clone, Copy)]
pub(super) enum InputReviewChoice {
    Keep,
    RestoreHeld,
    ReplaceReviewed,
    Discard,
}

impl Shell {
    /// Must run before changing an agent/session/project, while the old editor still owns its key.
    pub(in crate::app) fn prepare_input_session_change(&mut self) -> Result<()> {
        if self.input_recovery.session.is_empty() {
            ensure!(
                self.composer.recovery_snapshot()?.is_empty(),
                "input recovery is not bound yet; keep this session open until unsent input can be saved"
            );
            return Ok(());
        }
        self.checkpoint_input(true);
        if let Some(error) = &self.input_recovery.error {
            anyhow::bail!(
                "Cannot leave this session: unsent input was not saved: {error}. Resolve the recovery error first; your draft and queue are unchanged."
            );
        }
        ensure!(
            self.input_recovery.store.is_some(),
            "Cannot leave this session: input recovery is unavailable"
        );
        Ok(())
    }

    /// Defensive fallback for identity owners; a failed save never clears the old input.
    pub(in crate::app) fn recovery_session_changed(&mut self, _id: &str) -> bool {
        if let Err(error) = self.prepare_input_session_change() {
            self.note("Input recovery", format!("{error:#}"));
            return false;
        }
        self.composer.clear_recovery_input();
        self.composer.set_recovery_sink(None);
        self.input_recovery.store = None;
        self.input_recovery.session.clear();
        self.input_recovery.checked = None;
        true
    }

    pub(in crate::app) fn poll_input_recovery(&mut self, app: &App) -> bool {
        if self.session_id.is_empty() {
            return false;
        }
        let different_project = self
            .input_recovery
            .layout
            .as_ref()
            .is_some_and(|layout| layout.project_root != app.layout.project_root);
        if different_project && !self.recovery_session_changed("") {
            return true;
        }
        self.input_recovery.layout = Some(app.layout.clone());
        if self.input_recovery.session != self.session_id {
            self.input_recovery.session = self.session_id.clone();
            match Store::open(&app.layout, &self.session_id) {
                Ok(store) => {
                    let count = store.record.pending.len();
                    let store = Arc::new(Mutex::new(store));
                    let sink = store.clone();
                    let epoch = self.input_recovery.epoch.clone();
                    self.composer
                        .set_recovery_sink(Some(Arc::new(move |snapshot| {
                            epoch.fetch_add(1, Ordering::AcqRel);
                            sink.lock()
                                .map_err(|_| anyhow::anyhow!("input recovery lock poisoned"))?
                                .checkpoint(snapshot)
                        })));
                    self.input_recovery.store = Some(store);
                    self.input_recovery.error = None;
                    if count > 0 {
                        self.note("Recover input", format!("{count} saved unsent draft/queue record(s) for this session. /draft previews saved and current input before you choose. Restored requests stay held until you send or /draft resume."));
                    }
                }
                Err(error) => {
                    // No writer lease means no writes or discard, but input remains usable.
                    self.input_recovery.error = Some(format!("{error:#}"));
                    self.note(
                        "Input recovery",
                        format!("Recovery unavailable: {error:#}. Saved input was not changed."),
                    );
                    // Fail closed at execution boundaries: an older record must not resurrect a send.
                    self.composer.set_recovery_sink(Some(Arc::new(|_| {
                        anyhow::bail!(
                            "recovery unavailable; resolve the recovery error before sending"
                        )
                    })));
                }
            }
            self.input_recovery.checked = Some(Instant::now());
            return true;
        }
        self.checkpoint_input(false)
    }

    /// At most one background timer write exists. Execution barriers invalidate older snapshots
    /// before taking the writer mutex, so a delayed save cannot resurrect already-consumed input.
    pub(in crate::app) fn checkpoint_input(&mut self, force: bool) -> bool {
        if force {
            self.input_recovery.epoch.fetch_add(1, Ordering::AcqRel);
        }
        let mut changed = false;
        if let Some(job) = self.input_recovery.save_job.take() {
            let result = if force {
                job.recv()
                    .unwrap_or_else(|_| Err("input recovery writer stopped".into()))
            } else {
                match job.try_recv() {
                    Ok(result) => result,
                    Err(TryRecvError::Empty) => {
                        self.input_recovery.save_job = Some(job);
                        return false;
                    }
                    Err(TryRecvError::Disconnected) => Err("input recovery writer stopped".into()),
                }
            };
            changed |= self.input_recovery_result(result.err());
        }
        if !force
            && self
                .input_recovery
                .checked
                .is_some_and(|at| at.elapsed() < Duration::from_millis(750))
        {
            return changed;
        }
        self.input_recovery.checked = Some(Instant::now());
        let Some(store) = self.input_recovery.store.clone() else {
            return changed;
        };
        let snapshot = match self.composer.recovery_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return self.input_recovery_result(Some(format!("{error:#}"))) || changed,
        };
        if force {
            let result = store
                .lock()
                .map_err(|_| anyhow::anyhow!("input recovery lock poisoned"))
                .and_then(|mut store| store.checkpoint(&snapshot));
            return self.input_recovery_result(result.err().map(|error| format!("{error:#}")))
                || changed;
        }
        let epoch = self.input_recovery.epoch.clone();
        let expected = epoch.load(Ordering::Acquire);
        let (tx, rx) = mpsc::channel();
        self.input_recovery.save_job = Some(rx);
        std::thread::spawn(move || {
            let result = store
                .lock()
                .map_err(|_| anyhow::anyhow!("input recovery lock poisoned"))
                .and_then(|mut store| {
                    if epoch.load(Ordering::Acquire) != expected {
                        return Ok(());
                    }
                    store.checkpoint(&snapshot)
                });
            let _ = tx.send(result.map_err(|error| format!("{error:#}")));
        });
        changed
    }

    fn input_recovery_result(&mut self, error: Option<String>) -> bool {
        if error == self.input_recovery.error {
            return false;
        }
        if let Some(error) = &error {
            self.note(
                "Input recovery",
                format!("Could not save unsent input: {error}"),
            );
        }
        self.input_recovery.error = error;
        true
    }

    /// Returns true for the /draft command family. Explicit replace is the only overwrite path.
    pub(in crate::app) fn input_recovery_command(&mut self, command: &str) -> bool {
        let mut words = command.split_whitespace();
        if words.next() != Some("/draft") {
            return false;
        }
        let action = words.next().unwrap_or("status");
        if words.next().is_some() {
            self.note(
                "Input recovery",
                "Use /draft restore, replace, discard or resume.",
            );
            return true;
        }
        let result = self.input_recovery_action(action);
        match result {
            Ok(text) => self.note("Input recovery", text),
            Err(error) => self.note("Input recovery", format!("{error:#}")),
        }
        true
    }

    /// Call only after an explicit history-return action and after binding the destination session.
    /// Unlike replacement, this never overwrites a newer destination draft or queue.
    pub(in crate::app) fn restore_input_recovery_if_empty(&mut self) -> bool {
        if !self
            .composer
            .recovery_snapshot()
            .is_ok_and(|snapshot| snapshot.is_empty())
        {
            return false;
        }
        match self.input_recovery_action("restore") {
            Ok(text) => {
                self.note("Input recovery", text);
                true
            }
            Err(error) => {
                self.note(
                    "Input recovery",
                    format!("Could not restore input: {error:#}"),
                );
                false
            }
        }
    }

    pub(super) fn preview_input_recovery(&self) -> Result<InputPreview> {
        let store = self
            .input_recovery
            .store
            .clone()
            .context("recovery is unavailable for this session; saved input was not changed")?;
        let pending = {
            let writer = store
                .lock()
                .map_err(|_| anyhow::anyhow!("input recovery lock poisoned"))?;
            ensure!(
                writer.record.session == self.session_id,
                "recovery session changed; reopen /draft"
            );
            writer.record.pending.clone()
        };
        Ok(InputPreview {
            store,
            session: self.session_id.clone(),
            pending,
            current: self.composer.recovery_snapshot()?,
        })
    }

    pub(super) fn apply_input_review(
        &mut self,
        preview: InputPreview,
        choice: InputReviewChoice,
    ) -> Result<String> {
        if matches!(choice, InputReviewChoice::Keep) {
            return Ok("Current input and saved records kept unchanged.".into());
        }
        ensure!(
            self.session_id == preview.session
                && self
                    .input_recovery
                    .store
                    .as_ref()
                    .is_some_and(|store| Arc::ptr_eq(store, &preview.store)),
            "recovery session changed; reopen /draft to review again"
        );
        ensure!(
            self.composer.recovery_snapshot()? == preview.current,
            "current input changed after preview; reopen /draft to review again"
        );
        self.input_recovery.epoch.fetch_add(1, Ordering::AcqRel);
        let mut store = preview
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("input recovery lock poisoned"))?;
        ensure!(
            store.record.pending == preview.pending,
            "saved input changed after preview; reopen /draft to review again"
        );
        if matches!(choice, InputReviewChoice::Discard) {
            let mut next = store.record.clone();
            next.pending.clear();
            next.current = preview.current;
            store.publish(next)?;
            return Ok("Saved recovery records discarded; current editor and queue kept.".into());
        }
        let snapshot = preview
            .pending
            .first()
            .cloned()
            .context("no saved input for this session")?;
        self.composer.restore_recovery(
            snapshot,
            matches!(choice, InputReviewChoice::ReplaceReviewed),
        )?;
        let result = (|| {
            let mut next = store.record.clone();
            next.pending.remove(0);
            next.current = self.composer.recovery_snapshot()?;
            store.publish(next)
        })();
        if let Err(error) = result {
            self.composer.restore_recovery(preview.current, true)?;
            return Err(error);
        }
        Ok("Input restored and held. Review it, then Enter to send or /draft resume to release the queue.".into())
    }

    fn input_recovery_action(&mut self, action: &str) -> Result<String> {
        self.input_recovery.epoch.fetch_add(1, Ordering::AcqRel);
        if action == "discard" && self.input_recovery.store.is_none() {
            let layout = self
                .input_recovery
                .layout
                .as_ref()
                .context("recovery has not been initialized")?;
            let mut store = Store::open_inner(layout, &self.session_id, true)?;
            store.publish(store.record.clone())?;
            let store = Arc::new(Mutex::new(store));
            let sink = store.clone();
            let epoch = self.input_recovery.epoch.clone();
            self.composer
                .set_recovery_sink(Some(Arc::new(move |snapshot| {
                    epoch.fetch_add(1, Ordering::AcqRel);
                    sink.lock()
                        .map_err(|_| anyhow::anyhow!("input recovery lock poisoned"))?
                        .checkpoint(snapshot)
                })));
            self.input_recovery.store = Some(store);
            self.input_recovery.error = None;
        }
        if action == "resume" {
            self.composer.resume_recovered_queue();
            return Ok("Recovered queue released by your explicit request.".into());
        }
        if action == "restore" {
            let preview = self.preview_input_recovery()?;
            return self.apply_input_review(preview, InputReviewChoice::RestoreHeld);
        }
        let store = self
            .input_recovery
            .store
            .clone()
            .context("recovery is unavailable for this session")?;
        let mut store = store
            .lock()
            .map_err(|_| anyhow::anyhow!("input recovery lock poisoned"))?;
        match action {
            "status" => Ok(format!(
                "{} saved input record(s). /draft opens a read-only preview with Restore held, Keep current, Replace reviewed input and Discard saved choices. /draft resume releases a held queue.",
                store.record.pending.len()
            )),
            "discard" => {
                let mut next = store.record.clone();
                next.pending.clear();
                next.current = self.composer.recovery_snapshot()?;
                store.publish(next)?;
                Ok("Saved recovery records discarded; current editor and queue kept.".into())
            }
            "replace" => anyhow::bail!("Replacement requires a fresh review; open /draft replace."),
            _ => anyhow::bail!("Use /draft restore, replace, discard or resume."),
        }
    }
}

#[cfg(test)]
mod tests;
