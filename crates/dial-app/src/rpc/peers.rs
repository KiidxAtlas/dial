//! Native controller access to the active agent's existing coordination identity.
use super::coded;
use crate::{
    agent::{Agent, PeerSender},
    peer_board::Peers,
};
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Mutex;

/// No binding while moving/rebuilding/switching. A bound agent may itself have coordination off.
#[derive(Default)]
pub(super) struct PeerControl(Mutex<Option<(String, Option<PeerSender>)>>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List<'a> {
    session_id: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send<'a> {
    session_id: &'a str,
    to: &'a str,
    text: &'a str,
}

impl PeerControl {
    pub(super) fn bind(&self, agent: &mut Agent<'_>) {
        let binding = (agent.session().id().to_owned(), agent.peer_sender());
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(binding);
    }

    /// Called on admission, before the actor can replace the session, project, or coordination policy.
    pub(super) fn suspend(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn with_sender(
        &self,
        session_id: &str,
        op: impl FnOnce(&PeerSender) -> Result<Value>,
    ) -> Result<Value> {
        if session_id.is_empty() {
            return Err(coded(ErrorCode::InvalidParams, "session_id is required"));
        }
        // Never retain a cloned sender across the identity check and the operation.
        let state = self
            .0
            .lock()
            .map_err(|_| coded(ErrorCode::Failed, "peer controller unavailable"))?;
        let (active, sender) = state
            .as_ref()
            .ok_or_else(|| coded(ErrorCode::Busy, "session is changing"))?;
        if session_id != active {
            return Err(coded(
                ErrorCode::Conflict,
                "active session changed; refresh the peer board",
            ));
        }
        op(sender.as_ref().ok_or_else(|| {
            coded(
                ErrorCode::Refused,
                "project coordination is disabled or unavailable",
            )
        })?)
    }

    pub(super) fn list(&self, params: &Value) -> Result<Value> {
        let args = List::deserialize(params).map_err(|e| coded(ErrorCode::InvalidParams, e))?;
        self.with_sender(args.session_id, |sender| {
            let mut board = json!(Peers::from_board(sender.board()?, &sender.messages()?));
            board["session_id"] = json!(args.session_id);
            Ok(board)
        })
    }

    pub(super) fn send(&self, params: &Value) -> Result<Value> {
        let args = Send::deserialize(params).map_err(|e| coded(ErrorCode::InvalidParams, e))?;
        self.with_sender(args.session_id, |sender| {
            // Exact live instance, input bounds, and queue admission are all checked by send_letter.
            sender
                .send(args.to, args.text, true)
                .map_err(|e| coded(ErrorCode::Refused, e))?;
            Ok(json!({"session_id":args.session_id,"to":args.to,"queued":true}))
        })
    }
}
