//! Guest side of a collab room. Joins from a link and mirrors the host's log. Writes are sent only
//! when the host grants write access in `welcome`; otherwise the guest is read-only.
use super::crypto::RoomKey;
use super::protocol::{Control, frame, parse_link};
use super::socket::{Role, Socket, SocketEvent, SocketOptions};
use anyhow::Result;
use serde_json::Value;

pub struct Guest {
    socket: Socket,
    read_only: bool,
    welcomed: bool,
    ended: bool,
    hello: Value,
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
        })
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn ended(&self) -> bool {
        self.ended
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
}
