//! Native RPC session administration served by the controller: `list_sessions`, `delete_session`
//! and `pin_session`. They read or remove journals of the launched project only and never touch
//! the active session, so they stay available while a turn runs. Opening and resuming sessions
//! remain actor work (`resume`, `new_session`, `fork_session`).
use super::coded;
use crate::config::paths::Layout;
use crate::storage::{session::Session, session_pins};
use anyhow::Result;
use dial_protocol::ErrorCode;
use serde_json::{Value, json};

fn invalid(message: impl std::fmt::Display) -> anyhow::Error {
    coded(ErrorCode::InvalidParams, message)
}

/// One page of this project's sessions in picker order (pinned first, then latest activity).
/// Empty sessions (no user request) are omitted unless `include_empty` is true.
pub(super) fn list(layout: &Layout, current: Option<&str>, params: &Value) -> Result<Value> {
    if !(params.is_null() || params.is_object()) {
        return Err(invalid("params must be an object"));
    }
    let offset = match params.get("offset").filter(|value| !value.is_null()) {
        Some(value) => usize::try_from(
            value
                .as_u64()
                .ok_or_else(|| invalid("offset must be an unsigned integer"))?,
        )
        .map_err(|_| invalid("session offset cannot be represented on this platform"))?,
        None => 0,
    };
    let limit = match params.get("limit").filter(|value| !value.is_null()) {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| invalid("limit must be an unsigned integer"))?,
        None => super::MESSAGES_PAGE_DEFAULT,
    }
    .clamp(1, super::MESSAGES_PAGE_MAX) as usize;
    let include_empty = match params.get("include_empty").filter(|value| !value.is_null()) {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid("include_empty must be a boolean"))?,
        None => false,
    };
    let pinned = session_pins::load(&layout.user_dir)?;
    let mut sessions = Session::list_by_activity(layout, usize::MAX)?;
    sessions.retain(|session| {
        include_empty || session.is_conversation() || Some(session.id.as_str()) == current
    });
    let sessions = session_pins::sort_pinned_first(sessions, &pinned, |s| &s.id);
    if offset > sessions.len() {
        return Err(invalid("session offset is past the end"));
    }
    let end = offset + limit.min(sessions.len() - offset);
    let page: Vec<Value> = sessions[offset..end]
        .iter()
        .map(|s| {
            json!({
                "session_id": s.id,
                "title": s.title,
                "started": s.started,
                "updated": s.updated,
                "messages": s.messages,
                "user_messages": s.user_messages,
                "model": s.model,
                "parent_id": s.parent_id,
                "first_message": s.first_message,
                "last_message": s.last_message,
                "last_answer": s.last_answer,
                "pinned": pinned.contains(&s.id),
                "current": Some(s.id.as_str()) == current,
                "open_elsewhere": Some(s.id.as_str()) != current && Session::open_elsewhere(&s.path),
            })
        })
        .collect();
    Ok(json!({
        "sessions": page,
        "offset": offset,
        "total": sessions.len(),
        "next_offset": (end < sessions.len()).then_some(end),
    }))
}

/// The one journal of this project whose id is exactly `wanted`.
fn find(layout: &Layout, wanted: &str) -> Result<crate::storage::session::SessionInfo> {
    if wanted.is_empty() {
        return Err(invalid("session_id must not be empty"));
    }
    Session::find_prefix(layout, wanted)?
        .into_iter()
        .find(|session| session.id == wanted)
        .ok_or_else(|| {
            coded(
                ErrorCode::NotFound,
                "session id not found in launched project",
            )
        })
}

/// Deletes another session of this project: its journal, stream supplement, side thread and pin.
/// The active session is refused, and so is a journal another writer holds.
pub(super) fn delete(layout: &Layout, current: Option<&str>, params: &Value) -> Result<Value> {
    let wanted = params["session_id"]
        .as_str()
        .ok_or_else(|| invalid("delete_session requires params.session_id"))?;
    if Some(wanted) == current {
        return Err(invalid(
            "cannot delete the active session; switch to another session first",
        ));
    }
    let info = find(layout, wanted)?;
    if Session::open_elsewhere(&info.path) {
        return Err(coded(
            ErrorCode::Busy,
            "session is open in another process; close it there first",
        ));
    }
    Session::open(&info.path)?.remove_file()?;
    // The pin is dropped only after the journal is gone, as the session picker does.
    session_pins::unpin(&layout.user_dir, wanted)?;
    Ok(json!({"session_id": wanted, "deleted": true}))
}

/// Pins or unpins a session of this project. Idempotent.
pub(super) fn pin(layout: &Layout, params: &Value) -> Result<Value> {
    let wanted = params["session_id"]
        .as_str()
        .ok_or_else(|| invalid("pin_session requires params.session_id"))?;
    let pinned = params["pinned"]
        .as_bool()
        .ok_or_else(|| invalid("pin_session requires boolean params.pinned"))?;
    find(layout, wanted)?;
    let changed = session_pins::set(&layout.user_dir, wanted, pinned)?;
    Ok(json!({"session_id": wanted, "pinned": pinned, "changed": changed}))
}

#[cfg(test)]
mod tests;
