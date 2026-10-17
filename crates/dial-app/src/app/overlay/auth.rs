//! Sign-in and model-preparation jobs started from an overlay.

use crate::app::core::App;
use crate::config::settings_store::{Change, Scope};
use anyhow::{Result, ensure};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(in crate::app) struct PreparedModel {
    pub(in crate::app) changes: Vec<Change>,
    pub(in crate::app) role: String,
    pub(in crate::app) scope: Scope,
}

pub(crate) struct AuthJob {
    pub(in crate::app) label: &'static str,
    pub(in crate::app) receiver: std::sync::mpsc::Receiver<Result<String>>,
    pub(in crate::app) cancel: Arc<AtomicBool>,
    /// Progress lines a streaming job sends while it runs; `None` for plain jobs.
    pub(in crate::app) lines: Option<std::sync::mpsc::Receiver<String>>,
    /// Published only after preparation succeeds and the UI has not cancelled the job.
    pub(in crate::app) model: Option<PreparedModel>,
}

impl Drop for AuthJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

impl AuthJob {
    /// Progress lines received since the last call, tagged with the job label.
    pub(in crate::app) fn drain_lines(slot: &mut Option<Self>) -> Vec<(&'static str, String)> {
        let Some(job) = slot.as_ref() else {
            return Vec::new();
        };
        let Some(lines) = job.lines.as_ref() else {
            return Vec::new();
        };
        lines.try_iter().map(|line| (job.label, line)).collect()
    }

    pub(in crate::app) fn collect(
        slot: &mut Option<Self>,
        app: &App,
    ) -> Option<(&'static str, Result<String>)> {
        let result = slot.as_ref().and_then(|job| match job.receiver.try_recv() {
            Ok(result) => Some((job.label, result)),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(_) => Some((
                job.label,
                Err(anyhow::anyhow!(
                    "{} authentication worker stopped unexpectedly",
                    job.label
                )),
            )),
        });

        result.map(|(label, result)| {
            let job = slot.take().expect("completed background job");
            let result = result.and_then(|message| {
                ensure!(!job.cancel.load(Ordering::SeqCst), "operation cancelled");
                if let Some(model) = &job.model {
                    app.save_settings(model.scope, &model.changes, Some(&model.role))?;
                }
                Ok(message)
            });
            (label, result)
        })
    }
}
