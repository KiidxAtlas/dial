//! Named shell sessions handed to a background job. While the job runs, the session is marked busy
//! in the workspace; when the job settles, the state it published is applied on the next use.

use crate::embedded_shell::state::{Exchange, State};
use crate::tools::Workspace;
use std::collections::BTreeMap;

/// A named session's entry while a background job holds it, or the outcome that job left behind.
#[derive(Debug)]
pub(crate) enum SessionBusy {
    /// The session belongs to this background job until it settles.
    Running { job: String },
    /// The job settled: `Some` publishes its state, `None` discards the session.
    Settled(Option<State>),
}

/// The claim a foreground command carries into the background table. Dropping an unsettled claim
/// (a kill, or a job that was removed) discards the session's state.
pub(crate) struct SessionClaim {
    workspace: Workspace,
    name: String,
    exchange: Exchange,
    marked: bool,
    settled: bool,
}

impl SessionClaim {
    pub(crate) fn new(workspace: &Workspace, name: &str, exchange: Exchange) -> Self {
        Self {
            workspace: workspace.clone(),
            name: name.to_owned(),
            exchange,
            marked: false,
            settled: false,
        }
    }

    /// Marks the session busy for `job`. Called while the foreground guard still excludes others.
    pub(crate) fn mark(&mut self, job: &str) {
        self.marked = true;
        self.set(SessionBusy::Running {
            job: job.to_owned(),
        });
    }

    /// Ends the claim. A clean exit publishes the state the shell wrote; anything else discards it.
    /// The state is applied on the session's next use, so this never takes the session map lock.
    pub(crate) fn settle(mut self, clean: bool) {
        let state = clean
            .then(|| read_published(&self.workspace, &self.exchange))
            .and_then(Result::ok);
        self.set(SessionBusy::Settled(state));
        self.settled = true;
    }

    fn set(&self, value: SessionBusy) {
        busy_map(&self.workspace).insert(self.name.clone(), value);
    }
}

impl Drop for SessionClaim {
    fn drop(&mut self) {
        if self.marked && !self.settled {
            self.set(SessionBusy::Settled(None));
        }
    }
}

/// Admits a foreground command to named session `name`: refuses it while a background job holds
/// the session, and first applies the state of a job that settled since the session was last used.
pub(crate) fn admit(
    ws: &Workspace,
    sessions: &mut BTreeMap<String, State>,
    name: &str,
) -> Result<(), String> {
    let mut busy = busy_map(ws);
    if let Some(SessionBusy::Running { job }) = busy.get(name) {
        return Err(format!(
            "session busy in background job {job}; wait or kill it"
        ));
    }
    match busy.remove(name) {
        Some(SessionBusy::Settled(Some(state))) => {
            sessions.insert(name.to_owned(), state);
        }
        Some(SessionBusy::Settled(None)) => {
            sessions.remove(name);
        }
        Some(SessionBusy::Running { .. }) | None => {}
    }
    Ok(())
}

/// The state a completed foreground or background command published, if it is usable: its cwd must
/// stay inside the workspace.
pub(crate) fn read_published(ws: &Workspace, exchange: &Exchange) -> Result<State, String> {
    exchange.read().and_then(|state| {
        if !state.completed {
            return Err("shell did not publish a completed state exchange".into());
        }
        let cwd = state
            .cwd
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if cwd.to_str().is_none_or(|cwd| ws.resolve(cwd).is_err()) || !cwd.is_dir() {
            return Err("shell state cwd is outside this workspace".into());
        }
        Ok(state)
    })
}

fn busy_map(ws: &Workspace) -> std::sync::MutexGuard<'_, BTreeMap<String, SessionBusy>> {
    ws.shell_busy.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests;
