//! Connection-owned side conversations over immutable actor snapshots.
use super::{Sender, coded, workflows};
use crate::{agent::Agent, config::paths::Layout, side::SideThreadState};
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    side: SideThreadState,
    session: Option<String>,
    snapshot: Option<Arc<crate::agent::modes::btw::SideSnapshot>>,
    published: Option<(Option<String>, u64)>,
    closed: bool,
}

#[derive(Default)]
pub(super) struct SideControl(Mutex<State>);

impl SideControl {
    /// Withdraws the snapshot before an actor mutation. Running side answers retain their own
    /// immutable context; polling must continue to drain them while a replacement is prepared.
    pub(super) fn suspend(&self) -> Result<()> {
        self.0
            .lock()
            .map_err(|_| coded(ErrorCode::Failed, "side owner poisoned"))?
            .snapshot = None;
        Ok(())
    }

    /// Fence new side inference before relocating companion files. An active writer must settle
    /// first; controller polling may otherwise save a streamed prefix back to the source path.
    pub(super) fn prepare_move(&self) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| coded(ErrorCode::Failed, "side owner poisoned"))?;
        if state.side.busy() {
            return Err(coded(
                ErrorCode::Busy,
                "a side answer is running; move after it finishes or cancellation settles",
            ));
        }
        state.snapshot = None;
        Ok(())
    }

    pub(super) fn bind(&self, agent: &Agent<'_>, prompt: Option<&str>) -> Result<()> {
        {
            let mut state = self
                .0
                .lock()
                .map_err(|_| coded(ErrorCode::Failed, "side owner poisoned"))?;
            state.snapshot = None;
            if state.closed {
                return Ok(());
            }
            if let Err(error) = state.side.bind(agent.session()) {
                // A failed relocation/load must not leave the previous conversation accessible.
                state.side = SideThreadState::default();
                state.session = None;
                return Err(error);
            }
            state.session = Some(agent.session().id().to_owned());
        }
        // Projection can copy a large conversation. Do not hold the controller lock while doing it.
        let snapshot = Arc::new(agent.side_snapshot(prompt)?);
        let mut state = self
            .0
            .lock()
            .map_err(|_| coded(ErrorCode::Failed, "side owner poisoned"))?;
        if !state.closed {
            state.snapshot = Some(snapshot);
        }
        Ok(())
    }

    pub(super) fn handle(&self, params: &Value) -> Result<Value> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| coded(ErrorCode::Failed, "side owner poisoned"))?;
        if state.closed {
            return Err(coded(ErrorCode::Refused, "side conversations are closed"));
        }
        let snapshot = state.snapshot.clone();
        workflows::side(&mut state.side, snapshot.as_deref(), params)
    }

    pub(super) fn promote(
        &self,
        agent: &Agent<'_>,
        layout: &Layout,
        params: &Value,
    ) -> Result<Value> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| coded(ErrorCode::Failed, "side owner poisoned"))?;
        if state.closed {
            return Err(coded(ErrorCode::Refused, "side conversations are closed"));
        }
        workflows::promote(&mut state.side, agent, layout, params)
    }

    fn next_event_locked(state: &mut State) -> Option<Value> {
        let session = state.session.clone();
        let notes = state.side.drain(session.as_deref());
        let revision = state.side.revision();
        // Identity and revision are checked under the same bind/publication lock. Idle polls
        // must not copy or serialize an unchanged conversation's entire side history.
        let publication = (session.clone(), revision);
        if state.published.as_ref() == Some(&publication) && notes.is_empty() {
            return None;
        }
        let projection = workflows::side_projection(&state.side);
        state.published = Some(publication);
        Some(
            json!({"event":"side_state","session_id":session,"request_id":null,"state":projection,
            "notices":notes.into_iter().map(|note| json!({"title":note.title,"text":note.text})).collect::<Vec<_>>()}),
        )
    }

    pub(super) fn poll(&self, out: &Sender) -> Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| coded(ErrorCode::Failed, "side owner poisoned"))?;
        // Serialize publication with binding, not merely event construction. Otherwise an event
        // copied from the old session could be published after the new session is bound.
        if let Some(event) = Self::next_event_locked(&mut state) {
            out.emit(event);
        }
        Ok(())
    }

    #[cfg(test)]
    fn next_event(&self) -> Result<Option<Value>> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| coded(ErrorCode::Failed, "side owner poisoned"))?;
        Ok(Self::next_event_locked(&mut state))
    }

    /// Disconnect is terminal even if the actor is still unwinding a main turn.
    pub(super) fn cancel(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.closed = true;
            state.snapshot = None;
            state.side.cancel();
        }
    }
}

#[cfg(test)]
mod tests;
