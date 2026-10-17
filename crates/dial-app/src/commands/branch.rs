//! Rewinds the session while preserving the abandoned branch as a summary message.
//!
//! The summary is produced by the same compaction summarizer the agent uses, so it is interruptible
//! through the app interrupt flag. Any failure falls back to a plain rewind and reports why.

use crate::agent::Agent;
use crate::agent::NullUi;
use crate::agent::compact::compaction;
use crate::app::App;
use crate::llm::Chat;
use crate::llm::provider::Backend;
use anyhow::Result;
use std::sync::atomic::Ordering;

/// Result of a rewind: how many messages were dropped, and an optional notice for the user.
pub(in crate::app) struct Rewound {
    pub dropped: usize,
    pub note: Option<String>,
}

/// Drops `user_turns` user turns from the session. When the abandoned branch has content, it is
/// summarized first and the summary is appended as a branch-summary message.
pub(in crate::app) fn rewind(
    app: &App,
    agent: &mut Agent<'_>,
    user_turns: usize,
) -> Result<Rewound> {
    let rewound = rewind_branch(app, agent, user_turns);
    crate::collab::room::sync_host(agent.session());
    rewound
}

fn rewind_branch(app: &App, agent: &mut Agent<'_>, user_turns: usize) -> Result<Rewound> {
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
    match summarize(app, agent, &messages) {
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
    app: &App,
    agent: &mut Agent<'_>,
    messages: &[serde_json::Value],
) -> Result<compaction::BranchSummary> {
    let snapshot = agent.compaction_snapshot(None)?;
    let interrupt = app.interrupt.clone();
    let mut backend = crate::agent::make_backend(&app.config)?;
    anyhow::ensure!(
        !backend.local_inference(),
        "branch summaries need a remote model; local inference is not supported"
    );
    let mut complete =
        |backend: &mut Backend, request: &Chat, _ui: &mut dyn crate::agent::Ui| -> Result<String> {
            let reply = backend.chat_with_control(
                None,
                request,
                snapshot.timeout_secs,
                None,
                &mut |_| {},
                interrupt.clone(),
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
        &app.interrupt,
        &mut ui,
        &mut complete,
    );
    app.interrupt.store(false, Ordering::SeqCst);
    summary
}
