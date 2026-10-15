//! Process-wide collab state. A process holds at most one room, either hosted or joined.
use super::{
    guest::Guest,
    host::{Host, Steer},
};
use crate::storage::session::Session;
use std::sync::Mutex;

pub enum Room {
    Host(Host),
    Guest(Guest),
}

static ROOM: Mutex<Option<Room>> = Mutex::new(None);

/// Run `f` with exclusive access to the room slot.
pub fn with<R>(f: impl FnOnce(&mut Option<Room>) -> R) -> R {
    let mut slot = ROOM
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut slot)
}

/// Process relay events for the active room. Returns transcript notices.
pub fn pump() -> Vec<String> {
    with(|slot| match slot {
        Some(Room::Host(host)) => host.pump(),
        Some(Room::Guest(guest)) => guest.pump(),
        None => Vec::new(),
    })
}

/// Publish archived messages added since the last sync. No-op unless hosting.
pub fn sync_host(session: &Session) {
    with(|slot| {
        if let Some(Room::Host(host)) = slot {
            host.publish_new(session);
        }
    });
}

/// Guest prompts and aborts awaiting the host agent. Empty unless hosting.
pub fn take_steering() -> Vec<Steer> {
    with(|slot| match slot {
        Some(Room::Host(host)) => host.take_steering(),
        _ => Vec::new(),
    })
}

/// Whether a guest abort is queued for the running turn. Prompts stay queued; false unless hosting.
pub fn take_aborts() -> bool {
    with(|slot| match slot {
        Some(Room::Host(host)) => host.take_aborts(),
        _ => false,
    })
}
