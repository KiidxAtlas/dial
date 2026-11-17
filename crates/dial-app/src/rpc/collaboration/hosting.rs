//! Connection-owned adapter over the existing collaboration host. Secrets leave this module only
//! in the explicitly confirmed invite operation; room status remains metadata-only.
use super::{Config, Layout, Result, Value, coded, json, room};
use crate::collab::host::Host;
use crate::storage::session::Session;
use dial_protocol::ErrorCode;
use serde::Deserialize;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct State {
    epoch: u64,
    pending: bool,
    owned: Option<Owned>,
}
struct Owned {
    room_id: String,
    session_id: String,
}
static STATE: Mutex<State> = Mutex::new(State {
    epoch: 0,
    pending: false,
    owned: None,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    confirm: bool,
    #[serde(default = "read_only")]
    read_only: bool,
}
fn read_only() -> bool {
    true
}

fn request(params: &Value) -> Result<Start> {
    let request: Start = serde_json::from_value(params.clone()).map_err(|_| {
        coded(
            ErrorCode::InvalidParams,
            "expected confirm and optional read_only booleans",
        )
    })?;
    if !request.confirm {
        return Err(coded(
            ErrorCode::InvalidParams,
            "hosting shares the session archive; confirm must be true",
        ));
    }
    Ok(request)
}

pub(crate) fn start(
    config: &Config,
    layout: &Layout,
    session: &Session,
    params: &Value,
    cancel: &AtomicBool,
) -> Result<Value> {
    let request = request(params)?;
    if config.collab.relay_url.trim().is_empty() {
        return Err(coded(
            ErrorCode::InvalidParams,
            "configure collab.relay_url before hosting",
        ));
    }
    let epoch = {
        let mut state = state();
        if cancel.load(Ordering::SeqCst) {
            return Err(coded(ErrorCode::Conflict, "hosting cancelled"));
        }
        if state.pending || state.owned.is_some() || room::with(|slot| slot.is_some()) {
            return Err(coded(
                ErrorCode::Busy,
                "another collaboration room is active",
            ));
        }
        state.pending = true;
        state.epoch
    };
    // Socket setup and archive serialization never hold either lifecycle mutex. A stop or
    // disconnect invalidates this epoch, even when the connection has not finished opening.
    let opened = (|| {
        let mut host = Host::start(
            &config.collab.relay_url,
            config.collab.allow_guest_steering,
            !request.read_only,
            session.archived_messages(),
        )?;
        if let Err(error) = host.publish(
            &layout.user_dir,
            &layout.project_root.display().to_string(),
            session.id(),
            &config.model,
        ) {
            host.stop();
            return Err(error);
        }
        Ok(host)
    })();
    let mut state = state();
    // An invalidated older start must not clear a newer reservation.
    if state.epoch == epoch {
        state.pending = false;
    }
    let host = opened?;
    if cancel.load(Ordering::SeqCst) || state.epoch != epoch {
        host.stop();
        return Err(coded(ErrorCode::Conflict, "hosting cancelled"));
    }
    room::with(|slot| {
        if slot.is_some() {
            host.stop();
            return Err(coded(
                ErrorCode::Busy,
                "another collaboration room is active",
            ));
        }
        state.owned = Some(Owned {
            room_id: host.room_id().to_owned(),
            session_id: session.id().to_owned(),
        });
        *slot = Some(room::Room::Host(Box::new(host)));
        Ok(json!({"hosting":true,"read_only":request.read_only,"session_id":session.id()}))
    })
}

/// Applies only to the room installed by this RPC connection. Never stops a guest or a TUI host.
pub(crate) fn stop() -> Result<Value> {
    let mut state = state();
    let pending = state.pending;
    state.epoch = state.epoch.wrapping_add(1);
    state.pending = false;
    let owned = state.owned.take();
    let host = room::with(|slot| match (&*slot, &owned) {
        (Some(room::Room::Host(host)), Some(owned)) if host.room_id() == owned.room_id => {
            slot.take()
        }
        _ => None,
    });
    drop(state);
    if let Some(room::Room::Host(host)) = host {
        host.stop();
    } else if !pending {
        return Err(coded(
            ErrorCode::NotPending,
            "this connection is not hosting",
        ));
    }
    Ok(json!({"stopped":true}))
}

pub(crate) fn disconnect() {
    let _ = stop();
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invite {
    confirm: bool,
}

/// Deliberately separate from start/status: the returned link is a bearer capability. A writable
/// link grants admission and steering only under the host owner's existing configured gates.
pub(crate) fn invite(params: &Value) -> Result<Value> {
    let request: Invite = serde_json::from_value(params.clone())
        .map_err(|_| coded(ErrorCode::InvalidParams, "expected confirm boolean"))?;
    if !request.confirm {
        return Err(coded(
            ErrorCode::InvalidParams,
            "invite discloses room access; confirm must be true",
        ));
    }
    let state = state();
    room::with(|slot| match (slot, &state.owned) {
        (Some(room::Room::Host(host)), Some(owned)) if host.room_id() == owned.room_id => {
            Ok(json!({"link":host.link(),"read_only":host.is_read_only()}))
        }
        _ => Err(coded(
            ErrorCode::NotPending,
            "this connection is not hosting",
        )),
    })
}

/// Never replicate a replacement session into a room invited to the old session. Also drops
/// queued steering with that room before the replacement session can run it.
pub(crate) fn sync(session: &Session) {
    let mut state = state();
    let Some(owned) = &state.owned else {
        return;
    };
    let obsolete = owned.session_id != session.id();
    let removed = room::with(|slot| match slot {
        Some(room::Room::Host(host)) if host.room_id() == owned.room_id => {
            if obsolete {
                slot.take()
            } else {
                host.publish_new(session);
                None
            }
        }
        _ => None,
    });
    if obsolete {
        state.owned = None;
    }
    drop(state);
    if let Some(room::Room::Host(host)) = removed {
        host.stop();
    }
}

pub(crate) fn take_prompt() -> Option<String> {
    let state = state();
    room::with(|slot| match (slot, &state.owned) {
        (Some(room::Room::Host(host)), Some(owned)) if host.room_id() == owned.room_id => {
            host.take_prompt()
        }
        _ => None,
    })
}

pub(crate) fn take_abort() -> bool {
    let state = state();
    room::with(|slot| match (slot, &state.owned) {
        (Some(room::Room::Host(host)), Some(owned)) if host.room_id() == owned.room_id => {
            host.take_aborts()
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests;
