//! Host side of a collab room. Owns the relay socket, admits guests, and replicates the session log.
//!
//! Protocol: a guest sends `hello` with an optional write token. The host answers `welcome`
//! (carrying `readOnly`), streams the full log as `snapshot-chunk` frames to that peer, and then
//! broadcasts each new log entry as a sequence-numbered `entry` frame. Guest `prompt`/`abort`
//! frames are queued for the host agent, and only from writable guests while `allow_guest_steering`
//! is on. That queue is bounded: excess prompts get an explicit `error` reply, and aborts coalesce.
use super::crypto::{
    RoomKey, WRITE_TOKEN_BYTES, generate_room_key, generate_write_token, secrets_equal,
};
use super::protocol::{
    COLLAB_PROTO, Control, bounded_json, format_link, frame, generate_room_id,
    normalize_relay_origin, snapshot_chunks,
};
use super::registry::{self, HostRecord};
use super::socket::{Role, SnapshotCharge, Socket, SocketEvent, SocketOptions};
use crate::storage::session::Session;
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const NAME_MAX_CHARS: usize = 64;
/// Guest prompts that may wait for the host agent at once.
const MAX_QUEUED_PROMPTS: usize = 8;
/// Prompt text bytes that may wait for the host agent at once.
const MAX_QUEUED_PROMPT_BYTES: usize = 256 * 1024;
/// Largest single guest prompt.
const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// Why a guest request was not queued. The host replies to the guest with [`Refusal::message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    PromptTooLarge,
    Busy,
}

impl Refusal {
    fn message(&self) -> String {
        match self {
            Refusal::PromptTooLarge => {
                format!("prompt exceeds the {MAX_PROMPT_BYTES}-byte guest limit")
            }
            Refusal::Busy => "the host agent already has guest prompts queued; retry after the running turn finishes".into(),
        }
    }
}

/// Admit one guest request into the host's steering queue. Aborts coalesce into one pending abort
/// and never count against the prompt budget. Prompts are bounded by count, bytes and size.
fn queue_steer(queue: &mut Vec<Steer>, action: Steer) -> Result<(), Refusal> {
    match action {
        Steer::Abort => {
            if !queue.contains(&Steer::Abort) {
                queue.push(Steer::Abort);
            }
            Ok(())
        }
        Steer::Prompt(text) => {
            if text.len() > MAX_PROMPT_BYTES {
                return Err(Refusal::PromptTooLarge);
            }
            let (count, bytes) = queue
                .iter()
                .fold((0usize, 0usize), |(count, bytes), steer| match steer {
                    Steer::Prompt(queued) => (count + 1, bytes + queued.len()),
                    Steer::Abort => (count, bytes),
                });
            if count >= MAX_QUEUED_PROMPTS || bytes + text.len() > MAX_QUEUED_PROMPT_BYTES {
                return Err(Refusal::Busy);
            }
            queue.push(Steer::Prompt(text));
            Ok(())
        }
    }
}

/// Host-agent work requested by a guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Steer {
    Prompt(String),
    Abort,
}

struct GuestPeer {
    name: String,
    writable: bool,
}

pub struct Host {
    socket: Socket,
    link: String,
    /// `None` for a read-only room: no link carries a write token, so no peer can be granted write.
    write_token: Option<[u8; WRITE_TOKEN_BYTES]>,
    allow_steering: bool,
    /// Bounded JSON of every archived message, in order. This is the replicated log. Snapshot
    /// streams share this allocation; appends copy it only while a stream still reads the old one.
    entries: Arc<Vec<Arc<str>>>,
    guests: BTreeMap<u32, GuestPeer>,
    steering: Vec<Steer>,
    /// Local collab-directory record (user dir, record), kept current with the guest count.
    published: Option<(PathBuf, HostRecord)>,
}

/// True when `presented` (base64url, from `hello.writeToken`) matches the room's write token.
pub fn write_grant(expected: Option<&[u8; WRITE_TOKEN_BYTES]>, presented: Option<&str>) -> bool {
    match (
        expected,
        presented.and_then(|t| URL_SAFE_NO_PAD.decode(t).ok()),
    ) {
        (Some(expected), Some(token)) => secrets_equal(&token, expected),
        _ => false,
    }
}

impl Host {
    /// Open a room for `session`. `grant_write == false` creates a read-only room.
    pub fn start(
        relay_url: &str,
        allow_steering: bool,
        grant_write: bool,
        messages: Vec<Value>,
    ) -> Result<Self> {
        ensure!(
            !relay_url.trim().is_empty(),
            "set [collab].relay_url before hosting"
        );
        let origin = normalize_relay_origin(relay_url)?;
        let room_id = generate_room_id()?;
        let key = generate_room_key()?;
        let write_token = if grant_write {
            Some(generate_write_token()?)
        } else {
            None
        };
        let link = format_link(
            relay_url,
            &room_id,
            &key,
            write_token.as_ref().map(|t| &t[..]),
        )?;
        let socket = Socket::connect(SocketOptions {
            ws_url: format!("{origin}/r/{room_id}"),
            role: Role::Host,
            key: RoomKey::new(&key)?,
        })?;
        let entries: Vec<Arc<str>> = messages
            .iter()
            .map(|message| Arc::from(bounded_json(message)))
            .collect();
        Ok(Self {
            socket,
            link,
            write_token,
            allow_steering,
            entries: Arc::new(entries),
            guests: BTreeMap::new(),
            steering: Vec::new(),
            published: None,
        })
    }

    pub fn link(&self) -> &str {
        &self.link
    }

    pub fn is_read_only(&self) -> bool {
        self.write_token.is_none()
    }

    pub fn guest_count(&self) -> usize {
        self.guests.len()
    }

    /// Append messages archived since the last publish and broadcast them to every peer. A rewind
    /// that shrank or rewrote the archive resets every guest's log, then replays the current one.
    pub fn publish_new(&mut self, session: &Session) {
        let messages = session.archived_messages();
        if self.diverged(&messages) {
            self.socket.send(&frame::reset(), 0);
            self.entries = Arc::default();
        }
        if messages.len() <= self.entries.len() {
            return;
        }
        for message in &messages[self.entries.len()..] {
            let json: Arc<str> = Arc::from(bounded_json(message));
            self.socket
                .send_text(frame::entry_json(self.entries.len(), &json), 0);
            Arc::make_mut(&mut self.entries).push(json);
        }
    }

    /// True when the archive no longer extends the published log: it shrank, or its last published
    /// entry changed. Comparing only the tail keeps this cheap for long sessions.
    fn diverged(&self, messages: &[Value]) -> bool {
        match self.entries.len().checked_sub(1) {
            None => false,
            Some(last) => messages
                .get(last)
                .is_none_or(|message| bounded_json(message).as_str() != &*self.entries[last]),
        }
    }

    /// Process pending relay events. Returns notices for the host transcript.
    pub fn pump(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        while let Some(event) = self.socket.try_event() {
            match event {
                SocketEvent::Frame {
                    peer, frame: msg, ..
                } => self.on_frame(peer, &msg, &mut notes),
                SocketEvent::Control(Control::PeerLeft(peer)) => {
                    if let Some(guest) = self.guests.remove(&peer) {
                        self.socket.drop_peer(peer);
                        notes.push(format!("{} left the collab session", guest.name));
                    }
                }
                SocketEvent::RoomRecreated => {
                    // The relay dropped every guest with the old room; they rejoin with new peer ids.
                    self.guests.clear();
                }
                SocketEvent::Closed {
                    reason,
                    will_reconnect,
                    ..
                } if !will_reconnect => {
                    notes.push(format!("Collab connection closed: {reason}"));
                }
                _ => {}
            }
        }
        self.refresh_registry();
        notes
    }

    /// Drain guest requests for the host agent, in arrival order.
    pub fn take_steering(&mut self) -> Vec<Steer> {
        std::mem::take(&mut self.steering)
    }

    /// Drain queued guest aborts for the running turn. Prompts stay queued for the main loop.
    /// Returns whether any abort was queued.
    pub fn take_aborts(&mut self) -> bool {
        let before = self.steering.len();
        self.steering.retain(|steer| *steer != Steer::Abort);
        self.steering.len() != before
    }

    pub fn stop(&self) {
        self.socket.send(&frame::bye("host stopped the session"), 0);
        self.socket.close();
        self.withdraw();
    }

    /// Publish this room to the local collab directory so `collab list` can show it. Metadata
    /// only: the link and keys are never written.
    pub fn publish(
        &mut self,
        user_dir: &Path,
        cwd: &str,
        session_id: &str,
        model: &str,
    ) -> Result<()> {
        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let proc = crate::platform::procinfo::ProcId::current()
            .ok_or_else(|| anyhow::anyhow!("cannot record host process identity"))?;
        let record = HostRecord {
            id: format!("{}-{}", std::process::id(), rand::random::<u32>()),
            pid: std::process::id(),
            proc,
            session_id: session_id.to_owned(),
            cwd: cwd.to_owned(),
            model: model.to_owned(),
            read_only: self.is_read_only(),
            guests: self.guest_count(),
            started_ms,
        };
        registry::write(user_dir, &record)?;
        self.published = Some((user_dir.to_path_buf(), record));
        Ok(())
    }

    /// Rewrite the published record when the guest count changed. A failed write must not break
    /// the room, so the count simply stays stale until the next change.
    fn refresh_registry(&mut self) {
        let guests = self.guest_count();
        if let Some((user_dir, record)) = self.published.as_mut()
            && record.guests != guests
        {
            record.guests = guests;
            let _ = registry::write(user_dir, record);
        }
    }

    /// Remove this room's record from the local collab directory.
    fn withdraw(&self) {
        if let Some((user_dir, record)) = &self.published {
            registry::remove(&registry::dir(user_dir).join(format!("{}.json", record.id)));
        }
    }

    fn on_frame(&mut self, peer: u32, msg: &Value, notes: &mut Vec<String>) {
        match msg["t"].as_str() {
            Some("hello") => self.admit(peer, msg, notes),
            Some("prompt") => {
                if let Some(text) = msg["text"]
                    .as_str()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    self.steer(peer, Steer::Prompt(text.to_owned()))
                }
            }
            Some("abort") => self.steer(peer, Steer::Abort),
            _ => {}
        }
    }

    fn admit(&mut self, peer: u32, msg: &Value, notes: &mut Vec<String>) {
        if msg["proto"].as_u64() != Some(COLLAB_PROTO) {
            self.socket.send(
                &frame::error(&format!("protocol mismatch: host speaks v{COLLAB_PROTO}")),
                peer,
            );
            return;
        }
        let name = msg["name"]
            .as_str()
            .map(|n| n.trim().chars().take(NAME_MAX_CHARS).collect::<String>())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("guest-{peer}"));
        let writable = write_grant(self.write_token.as_ref(), msg["writeToken"].as_str());
        self.socket.send(
            &json!({
                "t": "welcome",
                "proto": COLLAB_PROTO,
                "readOnly": !writable,
                "entryCount": self.entries.len(),
            }),
            peer,
        );
        let charge = SnapshotCharge {
            id: Arc::as_ptr(&self.entries) as usize,
            bytes: self.entries.iter().map(|entry| entry.len()).sum(),
        };
        if !self
            .socket
            .send_batch(snapshot_chunks(self.entries.clone()), charge, peer)
        {
            self.socket.send(
                &frame::error(
                    "the session history could not be queued for this guest; rejoin to retry",
                ),
                peer,
            );
        }
        let access = if writable { "" } else { " (read-only)" };
        let note = format!("{name} joined the collab session{access}");
        self.socket.send(&frame::notice(&note), 0);
        notes.push(note);
        self.guests.insert(peer, GuestPeer { name, writable });
    }

    fn steer(&mut self, peer: u32, action: Steer) {
        let Some(guest) = self.guests.get(&peer) else {
            return;
        };
        let writable = guest.writable;
        let label = match action {
            Steer::Prompt(_) => "prompt",
            Steer::Abort => "abort",
        };
        if !writable {
            self.socket.send(
                &frame::error(&format!("{label} is disabled on a read-only link")),
                peer,
            );
            return;
        }
        if !self.allow_steering {
            self.socket.send(
                &frame::error("guest steering is disabled on this host"),
                peer,
            );
            return;
        }
        if let Err(refusal) = queue_steer(&mut self.steering, action) {
            self.socket.send(&frame::error(&refusal.message()), peer);
        }
    }
}

#[cfg(test)]
mod tests;
