//! `/commit`: generate a commit message for the working tree and commit it.
//!
//! The run goes through [`crate::commit::run::run_commit_command`] on a worker thread, so the
//! composer stays responsive while the model works. The worker never touches the terminal: when
//! the flow needs confirmation it sends a request over a channel, and the UI thread answers it with
//! the modal `Shell::ask` (polled from the event loop via [`poll`]).

use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};

use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use crate::commit::cli::{commit_help, parse_commit_args};
use crate::commit::run::{ProviderBackend, run_commit_command};
use anyhow::{Result, anyhow};

pub(super) const COMMANDS: &[super::CommandSpec] = &[super::CommandSpec {
    name: "commit",
    description: "Generate a commit message for the changes and commit them (--push, --dry-run, --legacy, --no-changelog, -c context, -m model)",
    args_hint: "[--push] [--dry-run] [--legacy] [--no-changelog] [-c <context>] [-m <model>]",
    subcommands: &[],
}];

pub(super) fn run(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    name: &str,
    args: &str,
) -> Option<Result<Step>> {
    if name != "commit" {
        return None;
    }
    Some(commit(shell, app, agent, args))
}

/// Messages from the commit worker to the UI thread.
pub(crate) enum CommitEvent {
    /// The worker needs a yes/no answer; `reply` receives it. A dropped reply means "no".
    Confirm {
        message: String,
        reply: SyncSender<bool>,
    },
    /// The run ended with this summary, or with the error text.
    Finished(Result<String, String>),
}

/// A `/commit` run in progress. Dropping it abandons the confirmation channel, so a worker still
/// waiting for an answer receives "no".
pub(crate) struct CommitJob {
    events: Receiver<CommitEvent>,
}

fn commit(shell: &mut Shell, app: &App, agent: &Agent<'_>, args: &str) -> Result<Step> {
    let tokens =
        shlex::split(args).ok_or_else(|| anyhow!("Unclosed quote in /commit arguments"))?;
    let parsed = match parse_commit_args(&tokens) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            shell.note("Commit", commit_help());
            return Ok(Step::Done);
        }
        Err(error) => {
            shell.note("Commit", error);
            return Ok(Step::Done);
        }
    };
    if shell.commit_job.is_some() {
        shell.note(
            "Commit",
            "A commit is already running; wait for it to finish.",
        );
        return Ok(Step::Done);
    }
    let root = agent.workspace().root().to_path_buf();
    let config = app.config.clone();
    let (events_tx, events_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut confirm = |message: &str| {
            let (reply, answer) = mpsc::sync_channel(1);
            let request = CommitEvent::Confirm {
                message: message.to_owned(),
                reply,
            };
            if events_tx.send(request).is_err() {
                return false;
            }
            answer.recv().unwrap_or(false)
        };
        let result = run_commit_command(&parsed, &root, &config, &ProviderBackend, &mut confirm)
            .map(|outcome| outcome.summary())
            .map_err(|error| error.to_string());
        let _ = events_tx.send(CommitEvent::Finished(result));
    });
    shell.commit_job = Some(CommitJob { events: events_rx });
    shell.note(
        "Commit",
        "Generating the commit message; the result appears here when it finishes.",
    );
    Ok(Step::Done)
}

/// Drain pending worker events from the event loop: answer confirmations on the UI thread and
/// report the finished run. Returns whether anything was handled.
pub(crate) fn poll(shell: &mut Shell) -> bool {
    let mut handled = false;
    loop {
        let Some(job) = shell.commit_job.as_ref() else {
            return handled;
        };
        match job.events.try_recv() {
            Ok(CommitEvent::Confirm { message, reply }) => {
                handled = true;
                // The split-plan question already ends with its own prompt; a generated message needs one.
                let body = if message.starts_with("Split commit plan") {
                    message.trim_end().to_owned()
                } else {
                    format!("{message}\n\nCommit with this message?")
                };
                let yes = shell.ask("Commit", &body, &[("Yes", 'y'), ("No", 'n')]) == Some(0);
                let _ = reply.send(yes);
            }
            Ok(CommitEvent::Finished(result)) => {
                shell.commit_job = None;
                match result {
                    Ok(summary) => shell.note("Commit", summary),
                    Err(error) => shell.note("Commit", error),
                }
                return true;
            }
            Err(TryRecvError::Empty) => return handled,
            Err(TryRecvError::Disconnected) => {
                shell.commit_job = None;
                shell.note("Commit", "The commit worker stopped unexpectedly.");
                return true;
            }
        }
    }
}
