//! Guest side of a collab room. Joins from a link and replicates the host's conversation log
//! through a typed [`Replica`]. Prompts and aborts go to the host only when the host grants write
//! access in `welcome`; a read-only guest refuses them, and no guest prompt ever runs on the local
//! agent.
use super::crypto::RoomKey;
use super::protocol::{Control, frame, parse_link};
use super::socket::{Role, Socket, SocketEvent, SocketOptions};
use anyhow::Result;
use serde_json::{Value, json};

/// Replicated entries shown in the terminal after a snapshot or a long catch-up.
const PREVIEW_ENTRIES: usize = 20;
/// Characters of replicated text shown per entry.
const PREVIEW_CHARS: usize = 240;

/// Typed replica of the host's conversation log.
///
/// Frames are applied in wire order. `welcome` opens a snapshot that is staged until its `final`
/// chunk, so a partial snapshot never replaces a complete replica. `entry` frames carry the index
/// they occupy in the host log: an index below the replica length is a duplicate and is dropped,
/// and an index beyond it is a gap that is reported rather than papered over. `reset` clears the
/// log, and the host then replays it from index 0.
#[derive(Debug, Default)]
pub struct Replica {
    entries: Vec<Value>,
    staging: Option<Vec<Value>>,
    /// Set once a snapshot or reset established the log, so live entries may apply.
    live: bool,
}

/// What one frame did to the replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Nothing visible changed: a staged chunk, a duplicate, or a frame that is not replication.
    None,
    /// A completed snapshot replaced the log; the value is its entry count.
    Snapshot(usize),
    /// One entry was appended at this index.
    Appended(usize),
    /// The host cleared its log; the replay follows.
    Reset,
    /// An entry arrived beyond the replica's end and was not applied.
    Gap { expected: usize, received: usize },
}

impl Replica {
    pub fn entries(&self) -> &[Value] {
        &self.entries
    }

    /// Apply one decrypted host frame. Frames that are not replication return [`Change::None`].
    pub fn apply(&mut self, msg: &Value) -> Change {
        match msg["t"].as_str() {
            Some("welcome") => {
                self.staging = Some(Vec::new());
                Change::None
            }
            Some("snapshot-chunk") => self.apply_chunk(msg),
            Some("entry") => self.apply_entry(msg),
            Some("reset") => {
                self.entries.clear();
                self.staging = None;
                self.live = true;
                Change::Reset
            }
            _ => Change::None,
        }
    }

    fn apply_chunk(&mut self, msg: &Value) -> Change {
        let Some(staging) = self.staging.as_mut() else {
            // A chunk without a welcome is stale: its snapshot was superseded or reset away.
            return Change::None;
        };
        staging.extend(msg["entries"].as_array().into_iter().flatten().cloned());
        if msg["final"].as_bool() != Some(true) {
            return Change::None;
        }
        let Some(done) = self.staging.take() else {
            return Change::None;
        };
        let count = done.len();
        self.entries = done;
        self.live = true;
        Change::Snapshot(count)
    }

    fn apply_entry(&mut self, msg: &Value) -> Change {
        let seq = msg["seq"]
            .as_u64()
            .and_then(|seq| usize::try_from(seq).ok());
        let (Some(seq), Some(entry)) = (seq, msg.get("entry")) else {
            return Change::None;
        };
        let log = match self.staging.as_mut() {
            Some(staging) => staging,
            None if self.live => &mut self.entries,
            // Before any snapshot the host's coming snapshot carries this entry.
            None => return Change::None,
        };
        let expected = log.len();
        if seq < expected {
            return Change::None;
        }
        if seq > expected {
            return Change::Gap {
                expected,
                received: seq,
            };
        }
        log.push(entry.clone());
        Change::Appended(expected)
    }
}

pub struct Guest {
    socket: Socket,
    read_only: bool,
    welcomed: bool,
    ended: bool,
    hello: Value,
    replica: Replica,
    /// Replica entries already shown in the terminal, so each one is projected exactly once.
    projected: usize,
}

impl Guest {
    /// Connect to the room named by `link`. `default_relay` is used only for bare links.
    pub fn join(default_relay: &str, link: &str, name: &str) -> Result<Self> {
        let parsed = parse_link(link, default_relay)?;
        let socket = Socket::connect(SocketOptions {
            ws_url: parsed.ws_url.clone(),
            role: Role::Guest,
            key: RoomKey::new(&parsed.key)?,
        })?;
        let hello = frame::hello(name, parsed.write_token.as_ref().map(|t| &t[..]));
        socket.send(&hello, 0);
        Ok(Self {
            socket,
            hello,
            read_only: true,
            welcomed: false,
            ended: false,
            replica: Replica::default(),
            projected: 0,
        })
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Conversation entries currently replicated from the host.
    pub fn replicated_entries(&self) -> usize {
        self.replica.entries().len()
    }

    /// Route a prompt to the host agent. Refused unless the host granted write access, so a
    /// read-only terminal never falls back to running the prompt locally.
    pub fn send_prompt(&self, text: &str) -> Result<(), &'static str> {
        self.send_request(json!({"t": "prompt", "text": text}))
    }

    /// Ask the host to abort its running turn. Refused unless the host granted write access.
    pub fn send_abort(&self) -> Result<(), &'static str> {
        self.send_request(json!({"t": "abort"}))
    }

    fn send_request(&self, request: Value) -> Result<(), &'static str> {
        if self.ended {
            return Err("The collab session has ended; /leave to exit");
        }
        if self.read_only {
            return Err(
                "This collab link is read-only, so the request was not sent and nothing ran locally",
            );
        }
        if self.socket.send(&request, 0) {
            Ok(())
        } else {
            Err("The host connection is closing or backlogged; the request was not sent")
        }
    }

    /// Process pending relay events. Returns notices for the guest transcript.
    pub fn pump(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        while let Some(event) = self.socket.try_event() {
            match event {
                SocketEvent::Frame { frame: msg, .. } => self.on_frame(&msg, &mut notes),
                SocketEvent::Control(Control::RoomClosed) => {
                    self.ended = true;
                    notes.push("The host closed the collab room".to_owned());
                }
                SocketEvent::RoomRecreated => {
                    self.welcomed = false;
                    self.socket.send(&self.hello, 0);
                }
                SocketEvent::Closed {
                    reason,
                    will_reconnect,
                    ..
                } if !will_reconnect => {
                    self.ended = true;
                    notes.push(format!("Collab connection closed: {reason}"));
                }
                _ => {}
            }
        }
        notes
    }

    /// Disconnect. Dropping the guest also closes the socket.
    pub fn leave(&self) {
        self.socket.close();
    }

    fn on_frame(&mut self, msg: &Value, notes: &mut Vec<String>) {
        if matches!(
            msg["t"].as_str(),
            Some("welcome" | "snapshot-chunk" | "entry" | "reset")
        ) {
            let change = self.replica.apply(msg);
            self.project(change, notes);
        }
        match msg["t"].as_str() {
            Some("welcome") => {
                // Missing grant data defaults to read-only.
                self.read_only = msg["readOnly"].as_bool().unwrap_or(true);
                self.welcomed = true;
                self.ended = false;
            }
            Some("notice") => notes.push(msg["message"].as_str().unwrap_or_default().to_owned()),
            Some("error") => notes.push(format!(
                "Host refused: {}",
                msg["message"].as_str().unwrap_or_default()
            )),
            Some("bye") => {
                // The host stopped on purpose: stop reconnecting to a room that will not return.
                self.ended = true;
                self.socket.close();
                notes.push(format!(
                    "Host ended the session: {}",
                    msg["reason"].as_str().unwrap_or_default()
                ));
            }
            _ => {}
        }
    }

    /// Terminal projection of one replica change. Each replicated entry is shown once; a reset
    /// clears that record so the host's replay is shown again.
    fn project(&mut self, change: Change, notes: &mut Vec<String>) {
        match change {
            Change::None => {}
            Change::Snapshot(count) => {
                if count < self.projected {
                    notes.push(
                        "The host conversation changed while you were away; showing it again"
                            .to_owned(),
                    );
                    self.projected = 0;
                }
                notes.push(format!(
                    "Replicated {count} conversation entr{} from the host",
                    if count == 1 { "y" } else { "ies" }
                ));
                self.show_from(self.projected, notes);
            }
            Change::Appended(index) => self.show_from(index, notes),
            Change::Reset => {
                self.projected = 0;
                notes.push("The host rewound the conversation; replaying it".to_owned());
            }
            Change::Gap { expected, received } => notes.push(format!(
                "Collab replica is missing host entries {expected} to {received}; rejoin to resync"
            )),
        }
    }

    /// Show replicated entries from `from` onward, collapsing older ones beyond the preview.
    fn show_from(&mut self, from: usize, notes: &mut Vec<String>) {
        let total = self.replica.entries().len();
        let start = from.min(total);
        let hidden = (total - start).saturating_sub(PREVIEW_ENTRIES);
        if hidden > 0 {
            notes.push(format!("{hidden} earlier conversation entries not shown"));
        }
        for entry in &self.replica.entries()[start + hidden..] {
            notes.push(describe(entry));
        }
        self.projected = total;
    }
}

/// One-line terminal summary of a replicated message.
fn describe(entry: &Value) -> String {
    let role = entry["role"].as_str().unwrap_or("entry");
    let text = match &entry["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return format!("{role}: (no text)");
    }
    let mut preview: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        preview.push('…');
    }
    format!("{role}: {preview}")
}

#[cfg(test)]
mod tests;
