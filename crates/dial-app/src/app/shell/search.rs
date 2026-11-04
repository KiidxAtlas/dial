//! `/search`: past-session full-text search off the interactive path.
//!
//! `Session::search` syncs and queries the SQLite index, which can take long on a large project.
//! It runs on one background worker, the same `mpsc` job shape `/commit` uses
//! (`commands/commit.rs`). At most one search runs at a time and at most one newer request waits
//! behind it; a newer request replaces the waiting one.
//!
//! Request identity is a context revision. The context is the project state directory plus the
//! session id. Each change bumps the revision: a change seen by `sync` at a request or poll, or an
//! explicit `invalidate` made where the shell changes its session. The explicit bump is what keeps
//! a switch away and back between two polls from reviving an older request. A result publishes
//! only when it answers the latest request and the revision has not moved since that request
//! started. A running worker cannot be interrupted, so a stale one finishes in the background and
//! its result is discarded.

use crate::app::display::short;
use crate::app::shell::Shell;
use crate::config::paths::Layout;
use crate::storage::session::{SearchHit, Session};
use anyhow::anyhow;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// How many hits `/search` lists.
const SEARCH_LIMIT: usize = 8;

/// The search a worker runs. Production uses `Session::search`; tests inject results.
type Runner = Arc<dyn Fn(&Layout, &str) -> anyhow::Result<Vec<SearchHit>> + Send + Sync>;

/// A finished search that still belongs to the latest request in the current context.
pub(in crate::app) struct Outcome {
    pub(in crate::app) query: String,
    pub(in crate::app) result: anyhow::Result<Vec<SearchHit>>,
}

struct Request {
    id: u64,
    revision: u64,
    layout: Layout,
    query: String,
}

struct Running {
    id: u64,
    revision: u64,
    query: String,
    events: Receiver<anyhow::Result<Vec<SearchHit>>>,
}

/// Latest-request search state: at most one running worker and one waiting request.
pub(in crate::app) struct Search {
    next_id: u64,
    latest: u64,
    /// Bumped on every project or session change; a request is valid only for the revision it saw.
    revision: u64,
    /// The `(state_dir, session id)` the revision last described.
    context: Option<(PathBuf, String)>,
    running: Option<Running>,
    queued: Option<Request>,
    runner: Runner,
}

impl Default for Search {
    fn default() -> Self {
        Self::with_runner(Arc::new(|layout, query| {
            Session::search(layout, query, SEARCH_LIMIT)
        }))
    }
}

impl Search {
    fn with_runner(runner: Runner) -> Self {
        Self {
            next_id: 0,
            latest: 0,
            revision: 0,
            context: None,
            running: None,
            queued: None,
            runner,
        }
    }

    /// Whether a request is running or waiting. Keeps the event loop polling at its tick rate.
    pub(in crate::app) fn is_pending(&self) -> bool {
        self.running.is_some() || self.queued.is_some()
    }

    /// Marks the context changed now, so requests started before this point are stale even if the
    /// shell switches back before the next poll.
    pub(in crate::app) fn invalidate(&mut self) {
        self.revision += 1;
    }

    /// Records the context a request or poll sees; a difference counts as a change.
    fn sync(&mut self, layout: &Layout, session: &str) {
        let context = (layout.state_dir.clone(), session.to_owned());
        if self.context.as_ref() != Some(&context) {
            self.context = Some(context);
            self.invalidate();
        }
    }

    /// Makes `query` the latest request in the current context and returns at once.
    pub(in crate::app) fn request(&mut self, layout: &Layout, session: &str, query: &str) {
        self.sync(layout, session);
        self.next_id += 1;
        self.latest = self.next_id;
        // Replaces any waiting request: only the latest query is worth running.
        self.queued = Some(Request {
            id: self.latest,
            revision: self.revision,
            layout: layout.clone(),
            query: query.to_owned(),
        });
        if self.running.is_none() {
            self.start_queued();
        }
    }

    /// Collects finished searches without blocking. Returns only results that still matter.
    pub(in crate::app) fn poll(&mut self, layout: &Layout, session: &str) -> Vec<Outcome> {
        self.sync(layout, session);
        let mut outcomes = Vec::new();
        loop {
            let Some(run) = self.running.take() else {
                if self.start_queued() {
                    continue;
                }
                break;
            };
            match run.events.try_recv() {
                Ok(result) => outcomes.extend(self.finish(run, result)),
                Err(TryRecvError::Empty) => {
                    self.running = Some(run);
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    let result = Err(anyhow!("the search worker stopped unexpectedly"));
                    outcomes.extend(self.finish(run, result));
                }
            }
        }
        outcomes
    }

    /// Starts the waiting request unless the context changed after it was made. A stale request
    /// is dropped rather than run against another session's or project's state.
    fn start_queued(&mut self) -> bool {
        let Some(request) = self.queued.take() else {
            return false;
        };
        if request.revision != self.revision {
            return false;
        }
        let (events_tx, events_rx) = mpsc::channel();
        let runner = Arc::clone(&self.runner);
        let layout = request.layout;
        let query = request.query.clone();
        std::thread::spawn(move || {
            let _ = events_tx.send(runner(&layout, &query));
        });
        self.running = Some(Running {
            id: request.id,
            revision: request.revision,
            query: request.query,
            events: events_rx,
        });
        true
    }

    /// Publishes a result only when it answers the latest request and the context has not changed
    /// since that request started.
    fn finish(&self, run: Running, result: anyhow::Result<Vec<SearchHit>>) -> Option<Outcome> {
        (run.id == self.latest && run.revision == self.revision).then_some(Outcome {
            query: run.query,
            result,
        })
    }

    /// Waits for every running and waiting request to finish. Tests only: the shell never blocks.
    #[cfg(test)]
    fn settle(&mut self, layout: &Layout, session: &str) -> Vec<Outcome> {
        self.sync(layout, session);
        let mut outcomes = Vec::new();
        loop {
            match self.running.take() {
                Some(run) => {
                    let result = run
                        .events
                        .recv()
                        .unwrap_or_else(|_| Err(anyhow!("the search worker stopped unexpectedly")));
                    outcomes.extend(self.finish(run, result));
                }
                None if !self.start_queued() => break,
                None => {}
            }
        }
        outcomes
    }
}

/// The `/search` transcript text for a finished search. Errors stay visible with their full chain.
pub(super) fn search_text(query: &str, result: &anyhow::Result<Vec<SearchHit>>) -> String {
    match result {
        Ok(hits) if hits.is_empty() => format!("No past messages match \"{query}\"."),
        Ok(hits) => hits
            .iter()
            .map(|hit| format!("{} [{}] {}", short(&hit.session_id), hit.role, hit.snippet))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format!("error: {error:#}"),
    }
}

impl Shell {
    /// Records the session the shell now shows. A real change invalidates pending searches, even
    /// when the shell later switches back.
    pub(in crate::app) fn set_session_id(&mut self, id: &str) {
        if self.session_id != id {
            self.session_id = id.to_owned();
            self.search.invalidate();
        }
    }

    /// Starts a background search for `query` in the current project and session.
    pub(super) fn start_search(&mut self, layout: &Layout, query: &str) {
        self.search.request(layout, &self.session_id, query);
    }

    /// Notes finished searches. Returns whether anything was published.
    pub(super) fn poll_search(&mut self, layout: &Layout) -> bool {
        let outcomes = self.search.poll(layout, &self.session_id);
        let handled = !outcomes.is_empty();
        for outcome in outcomes {
            let text = search_text(&outcome.query, &outcome.result);
            self.note("Search", text);
        }
        handled
    }
}

#[cfg(test)]
mod tests;
