//! Local WebSocket relay. Payloads remain opaque; only the peer-id envelope is rewritten.
use super::protocol::{MAX_FRAME_BYTES, rewrite_envelope_peer, unpack_envelope, valid_room_id};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    collections::HashMap,
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    Message, accept_hdr_with_config,
    handshake::server::{Request, Response},
    protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode},
};

const QUEUED_FRAMES: usize = 16;
const POLL: Duration = Duration::from_millis(20);
/// Relay pings each peer this often; browsers answer with pongs and Dial clients ping every 20s.
const PING_EVERY: Duration = Duration::from_secs(20);
/// A peer that sends nothing (not even a pong) for this long is dropped.
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
struct Connection {
    outgoing: SyncSender<Message>,
    closing: AtomicBool,
}
type Peer = Arc<Connection>;
#[derive(Default)]
struct Room {
    host: Option<Peer>,
    guests: HashMap<u32, Peer>,
}
type Rooms = Arc<Mutex<HashMap<String, Room>>>;

/// Serve an HTTP/WebSocket relay at `bind`. Rooms are memory-only and disappear when empty.
pub fn serve(bind: SocketAddr) -> Result<()> {
    let listener =
        TcpListener::bind(bind).with_context(|| format!("binding collab relay at {bind}"))?;
    eprintln!(
        "Dial collaboration relay listening on {}",
        listener.local_addr()?
    );
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
    let next_peer = Arc::new(AtomicU32::new(1));
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("collab accept failed: {error}");
                continue;
            }
        };
        let rooms = rooms.clone();
        let next_peer = next_peer.clone();
        thread::Builder::new()
            .name("dial-collab-peer".into())
            .spawn(move || {
                if let Err(error) = peer(stream, rooms, next_peer) {
                    eprintln!("collab peer ended: {error:#}");
                }
            })?;
    }
    bail!("collab relay listener stopped")
}

fn send(peer: &Peer, message: Message) {
    if peer.outgoing.try_send(message).is_err() {
        // Disconnect the slow recipient, not the host broadcasting to other guests.
        peer.closing.store(true, Ordering::Release);
    }
}

fn close_frame(code: u16, reason: &str) -> CloseFrame {
    CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    }
}
struct Membership {
    rooms: Rooms,
    room: String,
    host: bool,
    id: u32,
    peer: Peer,
}
impl Drop for Membership {
    fn drop(&mut self) {
        let mut all = self.rooms.lock().unwrap_or_else(|p| p.into_inner());
        let Some(room) = all.get_mut(&self.room) else {
            return;
        };
        if self.host {
            if room
                .host
                .as_ref()
                .is_some_and(|host| Arc::ptr_eq(host, &self.peer))
            {
                room.host = None;
                for guest in room.guests.values() {
                    send(
                        guest,
                        Message::Text(json!({"t":"room-closed"}).to_string().into()),
                    );
                    send(
                        guest,
                        Message::Close(Some(close_frame(4001, "host disconnected"))),
                    );
                }
                room.guests.clear();
            }
        } else if room
            .guests
            .get(&self.id)
            .is_some_and(|guest| Arc::ptr_eq(guest, &self.peer))
        {
            room.guests.remove(&self.id);
            if let Some(host) = &room.host {
                send(
                    host,
                    Message::Text(json!({"t":"peer-left","peer":self.id}).to_string().into()),
                );
            }
        }
        if room.host.is_none() && room.guests.is_empty() {
            all.remove(&self.room);
        }
    }
}
fn would_block(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted))
}
fn peer(stream: TcpStream, rooms: Rooms, next_peer: Arc<AtomicU32>) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let captured = Arc::new(Mutex::new(None::<(String, bool)>));
    let record = captured.clone();
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_FRAME_BYTES);
    config.max_frame_size = Some(MAX_FRAME_BYTES);
    config.write_buffer_size = 0;
    config.max_write_buffer_size = MAX_FRAME_BYTES.saturating_mul(2).saturating_add(1024);
    let mut socket = accept_hdr_with_config(
        stream,
        move |request: &Request, response: Response| {
            let id = request.uri().path().strip_prefix("/r/").unwrap_or("");
            let role = request
                .uri()
                .query()
                .unwrap_or("")
                .split('&')
                .find_map(|part| part.strip_prefix("role="))
                .unwrap_or("");
            if valid_room_id(id) && matches!(role, "host" | "guest") {
                *record.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some((id.to_owned(), role == "host"));
                Ok(response)
            } else {
                let mut error = tungstenite::handshake::server::ErrorResponse::new(Some(
                    "invalid relay path or role".into(),
                ));
                *error.status_mut() = tungstenite::http::StatusCode::BAD_REQUEST;
                Err(error)
            }
        },
        Some(config),
    )?;
    let (room_id, host) = captured
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .context("relay handshake omitted room")?;
    socket.get_mut().set_read_timeout(None)?;
    socket.get_mut().set_write_timeout(None)?;
    socket.get_mut().set_nonblocking(true)?;
    let (outgoing, incoming) = mpsc::sync_channel(QUEUED_FRAMES);
    let connection = Arc::new(Connection {
        outgoing,
        closing: AtomicBool::new(false),
    });
    let id = if host {
        0
    } else {
        next_peer
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| anyhow::anyhow!("relay peer IDs exhausted"))?
    };
    let refused: Option<(u16, &str)> = {
        let mut all = rooms.lock().unwrap_or_else(|p| p.into_inner());
        if host {
            let room = all.entry(room_id.clone()).or_default();
            if room.host.is_some() {
                Some((4009, "room already has a host"))
            } else {
                room.host = Some(connection.clone());
                None
            }
        } else {
            match all.get_mut(&room_id) {
                Some(room) => match room.host.as_ref() {
                    Some(room_host) => {
                        room.guests.insert(id, connection.clone());
                        send(
                            room_host,
                            Message::Text(json!({"t":"peer-joined","peer":id}).to_string().into()),
                        );
                        None
                    }
                    None => Some((4004, "room has no host")),
                },
                None => Some((4004, "room has no host")),
            }
        }
    };
    if let Some((code, reason)) = refused {
        // Refuse after the handshake so the client receives the close code it retries on. The
        // socket is nonblocking here; a WouldBlock would drop the close frame, so write it blocking.
        let stream = socket.get_mut();
        stream.set_nonblocking(false)?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let _ = socket.close(Some(close_frame(code, reason)));
        let _ = socket.flush();
        return Ok(());
    }
    let _membership = Membership {
        rooms: rooms.clone(),
        room: room_id.clone(),
        host,
        id,
        peer: connection.clone(),
    };
    let mut closing = false;
    let mut close_deadline = None;
    let mut blocked_since = None;
    let mut last_rx = Instant::now();
    let mut last_ping = Instant::now();
    loop {
        if connection.closing.load(Ordering::Acquire) {
            bail!("relay recipient exceeded its outbound queue");
        }
        if close_deadline.is_some_and(|end| Instant::now() >= end) {
            break;
        }
        match socket.flush() {
            Ok(()) => {
                blocked_since = None;
                if closing {
                    break;
                }
            }
            Err(error) if would_block(&error) => {
                let since = *blocked_since.get_or_insert_with(Instant::now);
                ensure!(
                    since.elapsed() < Duration::from_secs(10),
                    "relay recipient stopped reading"
                );
                // Still read incoming control frames; do not enqueue more output until flushed.
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(error) => return Err(error.into()),
        }
        if blocked_since.is_none() && !closing {
            for _ in 0..QUEUED_FRAMES {
                let message = match incoming.try_recv() {
                    Ok(message) => message,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                };
                closing = matches!(message, Message::Close(_));
                if closing {
                    close_deadline = Some(Instant::now() + Duration::from_secs(1));
                }
                match socket.send(message) {
                    Ok(()) => {}
                    Err(error) if would_block(&error) => {
                        blocked_since.get_or_insert_with(Instant::now);
                        break;
                    }
                    Err(
                        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed,
                    ) => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
                if closing {
                    break;
                }
            }
        }
        let read = socket.read();
        if read.is_ok() {
            last_rx = Instant::now();
        }
        match read {
            Ok(Message::Binary(payload)) => {
                ensure!(
                    payload.len() <= MAX_FRAME_BYTES,
                    "relay frame exceeds size limit"
                );
                if let Some((target, _)) = unpack_envelope(&payload) {
                    let payload = if host {
                        payload
                    } else {
                        let mut owned = payload
                            .try_into_mut()
                            .unwrap_or_else(|shared| (&shared[..]).into());
                        rewrite_envelope_peer(&mut owned, id);
                        owned.freeze()
                    };
                    let targets: Vec<Peer> = {
                        let all = rooms.lock().unwrap_or_else(|p| p.into_inner());
                        let Some(room) = all.get(&room_id) else {
                            break;
                        };
                        if host {
                            if target == 0 {
                                room.guests.values().cloned().collect()
                            } else {
                                room.guests.get(&target).cloned().into_iter().collect()
                            }
                        } else {
                            room.host.iter().cloned().collect()
                        }
                    };
                    for target in targets {
                        send(&target, Message::Binary(payload.clone()));
                    }
                }
            }
            Ok(Message::Close(_)) => {
                // Tungstenite queued the close response; flush it before releasing membership.
                closing = true;
                close_deadline = Some(Instant::now() + Duration::from_secs(1));
            }
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_)) => {}
            Err(error) if would_block(&error) => {
                if last_ping.elapsed() >= PING_EVERY {
                    last_ping = Instant::now();
                    send(&connection, Message::Ping(Vec::new().into()));
                }
                ensure!(
                    last_rx.elapsed() < SILENCE_LIMIT,
                    "relay peer stopped responding"
                );
                thread::sleep(POLL);
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
