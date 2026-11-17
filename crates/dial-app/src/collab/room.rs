//! Process-wide collab state. A process holds at most one room, either hosted or joined.
use super::{
    guest::Guest,
    host::{Host, Steer},
};
use crate::storage::session::Session;
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

static JOINING: AtomicBool = AtomicBool::new(false);
static JOIN_EPOCH: AtomicU64 = AtomicU64::new(0);

/// A lifecycle refusal callers can map without parsing terminal notices.
#[derive(Debug)]
pub enum Refused {
    Busy,
    NotGuest,
    Cancelled,
}
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Busy => "another collaboration room or join is active",
            Self::NotGuest => "not joined as a guest",
            Self::Cancelled => "collaboration join cancelled",
        })
    }
}
impl std::error::Error for Refused {}

pub enum Room {
    Host(Box<Host>),
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

/// Whether prompts currently belong to a remote host rather than the local agent.
pub fn is_guest() -> bool {
    with(|slot| matches!(slot, Some(Room::Guest(_))))
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

/// Route a local prompt while this process is a joined guest. `None` when this process is not a
/// guest, so the local agent handles the prompt. `Some` means the prompt belongs to the host; it
/// is either sent or refused, and the local agent never runs it.
pub fn route_guest_prompt(text: &str) -> Option<Result<(), &'static str>> {
    with(|slot| match slot {
        Some(Room::Guest(guest)) => Some(guest.send_prompt(text)),
        _ => None,
    })
}

/// Ask the host to abort its running turn. `None` unless this process is a joined guest.
pub fn send_guest_abort() -> Option<Result<(), &'static str>> {
    with(|slot| match slot {
        Some(Room::Guest(guest)) => Some(guest.send_abort()),
        _ => None,
    })
}

/// Join using the same socket and admission handshake as every frontend. Network setup never
/// holds the room mutex; a concurrent leave invalidates the eventual installation.
pub fn join(relay: &str, link: &str, name: &str, cancel: &AtomicBool) -> anyhow::Result<()> {
    if cancel.load(Ordering::SeqCst) {
        return Err(Refused::Cancelled.into());
    }
    // Reservation and epoch capture share the leave lock: a leave that sees the reservation
    // must invalidate this exact epoch, never an epoch the join captures afterward.
    let epoch = with(|slot| {
        if slot.is_some() || JOINING.swap(true, Ordering::SeqCst) {
            Err(Refused::Busy)
        } else {
            Ok(JOIN_EPOCH.load(Ordering::SeqCst))
        }
    })?;
    struct Joining;
    impl Drop for Joining {
        fn drop(&mut self) {
            JOINING.store(false, Ordering::SeqCst);
        }
    }
    let _joining = Joining;
    let guest = Guest::join(relay, link, name)?;
    with(|slot| {
        if cancel.load(Ordering::SeqCst) || epoch != JOIN_EPOCH.load(Ordering::SeqCst) {
            guest.leave();
            return Err(Refused::Cancelled.into());
        }
        if slot.is_some() {
            guest.leave();
            return Err(Refused::Busy.into());
        }
        *slot = Some(Room::Guest(guest));
        Ok(())
    })
}

/// Leave a guest room, or invalidate an in-flight join. Never stops an unrelated hosted room.
pub fn leave() -> Result<(), Refused> {
    let guest = with(|slot| {
        JOIN_EPOCH.fetch_add(1, Ordering::SeqCst);
        match slot {
            Some(Room::Guest(_)) => Ok(slot.take()),
            None if JOINING.load(Ordering::SeqCst) => Ok(None),
            _ => Err(Refused::NotGuest),
        }
    })?;
    if let Some(Room::Guest(guest)) = guest {
        guest.leave();
    }
    Ok(())
}

/// Structured peer state after processing queued relay frames. Does not expose room links/keys.
pub fn status() -> Value {
    let notices = pump();
    with(|slot| match slot {
        None => {
            json!({"role": "none", "joining": JOINING.load(Ordering::SeqCst), "notices": notices})
        }
        Some(Room::Host(host)) => {
            json!({"role": "host", "read_only": host.is_read_only(), "peers": host.peers(), "notices": notices})
        }
        Some(Room::Guest(guest)) => {
            json!({"role": "guest", "read_only": guest.is_read_only(), "admitted": guest.welcomed(), "ended": guest.ended(), "replicated_entries": guest.replicated_entries(), "notices": notices})
        }
    })
}
