//! Sign-in and model-preparation jobs started from an overlay.

use crate::app::core::App;
use crate::config::settings_store::{Change, Scope};
use anyhow::{Context, Result, ensure};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Settings to save once a background model download succeeds; nothing is written before then.
pub(in crate::app) struct PreparedModel {
    pub(in crate::app) changes: Vec<Change>,
    /// The model role the changes assign; `None` for a plain setting such as a draft model.
    pub(in crate::app) role: Option<String>,
    pub(in crate::app) scope: Scope,
    /// Runtime/model paths discovered by voice preparation, committed together with the selector.
    pub(in crate::app) voice: Option<PreparedVoice>,
    pub(in crate::app) review: Option<crate::config::settings_store::ModelConfigReview>,
}

pub(in crate::app) struct PreparedVoice {
    pub(in crate::app) original: crate::config::voice::VoiceConfig,
    pub(in crate::app) changes: std::sync::mpsc::Receiver<Vec<Change>>,
}

/// One historical setup failure, already redacted and bounded for session presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct SetupFailure {
    pub label: String,
    pub detail: String,
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

    #[cfg(test)]
    pub(in crate::app) fn collect(
        slot: &mut Option<Self>,
        app: &App,
    ) -> Option<(&'static str, Result<String>)> {
        Self::collect_with_failure(slot, app, None)
    }

    /// All interactive collectors share the same completion and failure-projection boundary.
    pub(in crate::app) fn collect_retained(
        slot: &mut Option<Self>,
        app: &App,
        last_failure: &mut Option<SetupFailure>,
    ) -> Option<(&'static str, Result<String>)> {
        Self::collect_with_failure(slot, app, Some(last_failure))
    }

    fn collect_with_failure(
        slot: &mut Option<Self>,
        app: &App,
        last_failure: Option<&mut Option<SetupFailure>>,
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
                    let mut changes = model.changes.clone();
                    if let Some(prepared) = &model.voice {
                        ensure!(app.reload()?.voice == prepared.original,
                            "Voice settings changed during setup; select the backend again to use the prepared installation");
                        changes.extend(
                            prepared
                                .changes
                                .try_recv()
                                .context("setup did not provide prepared settings")?,
                        );
                    }
                    if label == "Voice" {
                        let proposed = crate::config::settings_store::preview_config(
                            &app.layout,
                            model.scope,
                            &changes,
                            &app.overlay,
                        )?;
                        for change in &model.changes {
                            if let Change::Set {
                                path,
                                value: toml::Value::String(value),
                            } = change
                            {
                                crate::setup::voice::ready(&proposed, path, value)?;
                            }
                        }
                    }
                    if let Some(review) = &model.review {
                        app.save_reviewed_model_settings(review)?;
                    } else {
                        app.save_settings(model.scope, &changes, model.role.as_deref())?;
                    }
                }
                Ok(message)
            });
            if !job.cancel.load(Ordering::SeqCst)
                && (job.model.is_some() || matches!(label, "Setup" | "Model" | "Voice"))
                && let Err(error) = &result
                && let Some(last_failure) = last_failure
            {
                // OAuth/sign-in jobs are deliberately excluded: arbitrary provider responses
                // are not suitable retained diagnostics. Redact before truncating credentials.
                let detail = crate::storage::redact::redact_text(&format!("{error:#}")).into_owned();
                *last_failure = Some(SetupFailure {
                    label: crate::ui::text::one_line(&crate::storage::redact::redact_text(label)).chars().take(128).collect(),
                    detail: crate::ui::text::one_line(&detail).chars().take(2048).collect(),
                });
            }
            (label, result)
        })
    }
}

#[cfg(test)]
mod tests;
