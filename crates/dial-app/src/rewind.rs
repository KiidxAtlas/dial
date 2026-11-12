//! Frontend-neutral rewind and undo. The terminal (`/undo`, `/branch`, `/retry`) and `dial rpc`
//! (`undo`) call these owners, so validation order, the session-change gate, checkpoint restore
//! and the branch summary behave identically for every client.
//!
//! A rewind preserves the abandoned branch as a summary produced by the compaction summarizer;
//! the summary is interruptible through `interrupt`. Any summary failure falls back to a plain
//! rewind and reports why.

use crate::agent::Agent;
use crate::agent::NullUi;
use crate::agent::compact::compaction;
use crate::config::types::Config;
use crate::llm::Chat;
use crate::llm::provider::Backend;
use crate::storage::checkpoint::RollbackReport;
use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Result of a rewind: how many messages were dropped, and an optional notice for the user.
pub(crate) struct Rewound {
    pub dropped: usize,
    pub note: Option<String>,
}

/// One-shot gate admission, obtained before undo can restore files.
pub(crate) struct RewindApproval {
    user_turns: usize,
}

/// Runs the `session_before_branch` gate for dropping `user_turns` user turns.
pub(crate) fn preflight_rewind(agent: &Agent<'_>, user_turns: usize) -> Result<RewindApproval> {
    agent
        .gate_session_change(
            "session_before_branch",
            serde_json::json!({"reason":"branch","user_turns":user_turns}),
        )
        .map_err(anyhow::Error::msg)?;
    Ok(RewindApproval { user_turns })
}

/// Drops `user_turns` user turns from the session. When the abandoned branch has content, it is
/// summarized first and the summary is appended as a branch-summary message.
pub(crate) fn rewind(
    config: &Config,
    interrupt: &Arc<AtomicBool>,
    agent: &mut Agent<'_>,
    user_turns: usize,
) -> Result<Rewound> {
    let approval = preflight_rewind(agent, user_turns)?;
    rewind_approved(config, interrupt, agent, approval)
}

/// Performs a rewind the gate already admitted.
pub(crate) fn rewind_approved(
    config: &Config,
    interrupt: &Arc<AtomicBool>,
    agent: &mut Agent<'_>,
    approval: RewindApproval,
) -> Result<Rewound> {
    let user_turns = approval.user_turns;
    let rewound = rewind_branch(config, interrupt, agent, user_turns);
    crate::collab::room::sync_host(agent.session());
    rewound
}

/// Why an undo did not start. Nothing on disk or in the conversation changed.
#[derive(Debug)]
pub(crate) enum UndoRefusal {
    /// The requested range is not valid for this conversation.
    Range(anyhow::Error),
    /// The `session_before_branch` gate vetoed the rewind.
    Gate(anyhow::Error),
}

/// Outcome of an undo that started. Files are restored first; the conversation is rewound only
/// when every file was restored, so a partial restore keeps the conversation for inspection.
#[derive(Debug, Default)]
pub(crate) struct UndoOutcome {
    /// Checkpoint restore report, when the undone turns touched files.
    pub files: Option<RollbackReport>,
    /// Restore failure; the conversation was kept.
    pub rollback_error: Option<String>,
    /// Messages dropped from the conversation, when it was rewound.
    pub dropped: Option<usize>,
    /// Branch-summary notice from the rewind.
    pub note: Option<String>,
    /// Rewind failure after files were restored.
    pub rewind_error: Option<String>,
    /// Checkpoint bookkeeping failure after a successful rewind.
    pub cleanup_error: Option<String>,
}

impl UndoOutcome {
    /// Whether the conversation was left unchanged.
    pub(crate) fn conversation_kept(&self) -> bool {
        self.dropped.is_none()
    }
}

/// Undoes the last `turns` user turns: validates the range, runs the gate, calls `admitted`
/// (for caller-side feedback recording), restores the checkpointed files, then rewinds the
/// conversation and finalizes the checkpoints.
pub(crate) fn undo(
    config: &Config,
    interrupt: &Arc<AtomicBool>,
    agent: &mut Agent<'_>,
    turns: usize,
    admitted: impl FnOnce(),
) -> std::result::Result<UndoOutcome, UndoRefusal> {
    // Resolve and validate the exact conversation range before any filesystem mutation.
    let checkpoint_turns = agent
        .session()
        .rewind_checkpoint_turns(turns)
        .map_err(UndoRefusal::Range)?;
    let approval = preflight_rewind(agent, turns).map_err(UndoRefusal::Gate)?;
    admitted();
    let checkpoint_session = agent.session().id().to_owned();
    let mut outcome = UndoOutcome::default();
    if !checkpoint_turns.is_empty() {
        let result = agent
            .checkpoints()
            .context("checkpoints unavailable; conversation kept")
            .and_then(|checkpoints| {
                checkpoints.rollback_conversation(agent.session().id(), &checkpoint_turns)
            });
        match result {
            Ok(report) => {
                let incomplete = !report.warnings.is_empty();
                outcome.files = Some(report);
                if incomplete {
                    return Ok(outcome);
                }
            }
            Err(error) => {
                outcome.rollback_error = Some(format!("{error:#}"));
                return Ok(outcome);
            }
        }
    }
    agent.cancel_background_compaction();
    match rewind_approved(config, interrupt, agent, approval) {
        Ok(rewound) => {
            outcome.dropped = Some(rewound.dropped);
            outcome.note = rewound.note;
            agent.record_feedback(crate::telemetry::FeedbackKind::Undo, turns as u64);
            if let Some(checkpoints) = agent.checkpoints()
                && let Err(error) = checkpoints
                    .finalize_conversation_rollback(&checkpoint_session, &checkpoint_turns)
            {
                outcome.cleanup_error = Some(format!("{error:#}"));
            }
        }
        Err(error) => outcome.rewind_error = Some(format!("{error:#}")),
    }
    Ok(outcome)
}

fn rewind_branch(
    config: &Config,
    interrupt: &Arc<AtomicBool>,
    agent: &mut Agent<'_>,
    user_turns: usize,
) -> Result<Rewound> {
    let Some((from, messages)) = agent.session().rewind_abandonment(user_turns) else {
        let dropped = agent.session_mut().rewind(user_turns)?;
        return Ok(Rewound {
            dropped,
            note: None,
        });
    };
    if messages.is_empty() {
        let dropped = agent.session_mut().rewind(user_turns)?;
        return Ok(Rewound {
            dropped,
            note: None,
        });
    }
    match summarize(config, interrupt, agent, &messages) {
        Ok(summary) => {
            match agent.session_mut().rewind_summarized(
                user_turns,
                &from,
                &summary.summary,
                &summary.read_files,
                &summary.modified_files,
            ) {
                Ok(dropped) => Ok(Rewound {
                    dropped,
                    note: None,
                }),
                Err(error) => plain(
                    agent,
                    user_turns,
                    format!("branch summary not saved: {error:#}"),
                ),
            }
        }
        Err(error) => plain(
            agent,
            user_turns,
            format!("rewound without a branch summary: {error:#}"),
        ),
    }
}

fn plain(agent: &mut Agent<'_>, user_turns: usize, note: String) -> Result<Rewound> {
    let dropped = agent.session_mut().rewind(user_turns)?;
    Ok(Rewound {
        dropped,
        note: Some(note),
    })
}

fn summarize(
    config: &Config,
    interrupt: &Arc<AtomicBool>,
    agent: &mut Agent<'_>,
    messages: &[serde_json::Value],
) -> Result<compaction::BranchSummary> {
    let snapshot = agent.compaction_snapshot(None)?;
    let mut backend = crate::agent::make_backend(config)?;
    anyhow::ensure!(
        !backend.local_inference(),
        "branch summaries need a remote model; local inference is not supported"
    );
    let control = interrupt.clone();
    let mut complete =
        |backend: &mut Backend, request: &Chat, _ui: &mut dyn crate::agent::Ui| -> Result<String> {
            let reply = backend.chat_with_control(
                None,
                request,
                snapshot.timeout_secs,
                None,
                &mut |_| {},
                control.clone(),
            )?;
            Ok(reply.text)
        };
    let mut ui = NullUi;
    let summary = compaction::summarize_branch(
        &mut backend,
        messages,
        &snapshot.policy,
        &snapshot.context(),
        "",
        interrupt,
        &mut ui,
        &mut complete,
    );
    interrupt.store(false, Ordering::SeqCst);
    summary
}
