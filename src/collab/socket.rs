//! Client-side relay connection.
//!
//! One thread owns the WebSocket: it seals/opens AES-GCM frames, keeps a bounded FIFO send
//! queue, and reconnects with jittered exponential backoff on transient drops. Guests survive
//! the relay's host-drop room teardown (4001) while the host recreates the room; a host whose
//! connection dropped keeps retrying the relay's duplicate-host refusal (4009) inside a reclaim
//! window, because the relay may still hold the dead socket. Other fatal close codes and guest
//! decryption failures never reconnect. Hosts discard undecryptable guest frames. Every frame is
//! stamped with its sender role, epoch and sequence number; frames that are not from the peer role
//! or that repeat a sequence number inside the replay window are dropped before they reach the
//! host or guest.
use super::crypto::{RoomKey, SenderWindows, generate_epoch};
use super::protocol::{Control, MAX_FRAME_BYTES, pack_envelope, unpack_envelope};
use serde_json::Value;
use std::{
    collections::{HashSet, VecDeque},
    io::ErrorKind,
    net::{TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket,
    protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode},
    stream::MaybeTlsStream,
};

const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How long after a transient drop a host keeps retrying the relay's duplicate-host close.
pub const HOST_RECLAIM_WINDOW: Duration = Duration::from_secs(150);
const HOST_RECLAIM_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// A reclaim-time open stays provisional (not writable, not reported) this long.
const HOST_RECLAIM_CONFIRM: Duration = Duration::from_secs(1);
pub const MAX_PENDING_SENDS: usize = 256;
pub const MAX_PENDING_SEND_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETIRED_PEERS: usize = 256;
const READ_POLL: Duration = Duration::from_millis(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(20);
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
/// Per drain pass, so reads stay responsive while a large snapshot streams out.
const DRAIN_FRAMES_PER_PASS: usize = 64;
const DRAIN_BYTES_PER_PASS: usize = 1024 * 1024;
const EVENT_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Guest,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Host => "host",
            Role::Guest => "guest",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SocketOptions {
    /// `wss://host[:port]/r/<roomId>`: no query string.
    pub ws_url: String,
    pub role: Role,
    pub key: RoomKey,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SocketEvent {
    /// After every successful (re)connect.
    Open,
    /// A reconnect landed in a room the relay recreated: peer ids are reissued from 1.
    RoomRecreated,
    Frame {
        peer: u32,
        frame: Value,
    },
    Control(Control),
    /// `will_reconnect` distinguishes retries from terminal shutdown.
    Closed {
        reason: String,
        will_reconnect: bool,
    },
}

pub fn close_reason(code: u16) -> Option<&'static str> {
    match code {
        4001 => Some("room closed"),
        4004 => Some("no such room"),
        4009 => Some("a host is already connected for this room"),
        4029 => Some("room is full"),
        _ => None,
    }
}

/// Identity and serialized size of the shared log version a snapshot stream reads from. Queued
/// streams that read the same version are charged its bytes once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCharge {
    pub id: usize,
    pub bytes: usize,
}

/// Most snapshot bytes queued streams may reference at once. Each distinct log version counts once.
pub const MAX_RETAINED_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

enum Pending {
    One {
        target: u32,
        text: String,
        bytes: usize,
    },
    Batch {
        target: u32,
        charge: SnapshotCharge,
        frames: Box<dyn Iterator<Item = String> + Send>,
    },
}

impl Pending {
    fn target(&self) -> u32 {
        match self {
            Pending::One { target, .. } | Pending::Batch { target, .. } => *target,
        }
    }
    /// Bytes charged against the frame queue budget. Snapshot streams are charged via `SnapshotCharge`.
    fn bytes(&self) -> usize {
        match self {
            Pending::One { bytes, .. } => *bytes,
            Pending::Batch { .. } => 0,
        }
    }
}

#[derive(Default)]
struct Queue {
    items: VecDeque<Pending>,
    bytes: usize,
    /// Peers the relay retired. Targeted work is only admitted for peers absent from here.
    retired: VecDeque<u32>,
    retired_set: HashSet<u32>,
    overloaded: bool,
}

impl Queue {
    fn discard_where(&mut self, mut matches: impl FnMut(&Pending) -> bool) -> usize {
        let before = self.items.len();
        let mut bytes = self.bytes;
        self.items.retain(|p| {
            let drop = matches(p);
            if drop {
                bytes -= p.bytes();
            }
            !drop
        });
        self.bytes = bytes;
        before - self.items.len()
    }
    fn retire(&mut self, peer: u32) {
        if self.retired_set.insert(peer) {
            self.retired.push_back(peer);
        }
        while self.retired.len() > MAX_RETIRED_PEERS {
            if let Some(old) = self.retired.pop_front() {
                self.retired_set.remove(&old);
            }
        }
        self.discard_where(|p| p.target() == peer);
    }
    fn reset_for_recreated_room(&mut self) {
        self.retired.clear();
        self.retired_set.clear();
        self.discard_where(|p| p.target() != 0);
    }
    /// Snapshot bytes referenced by queued streams, counting each shared log version once.
    fn snapshot_bytes(&self) -> usize {
        let mut seen = HashSet::new();
        self.items
            .iter()
            .filter_map(|p| match p {
                Pending::Batch { charge, .. } if seen.insert(charge.id) => Some(charge.bytes),
                _ => None,
            })
            .sum()
    }
    /// Admit one queued item or refuse it. Refused snapshots are not overload: only the frame count
    /// and frame byte budgets mark the queue overloaded.
    fn push(&mut self, pending: Pending) -> bool {
        let target = pending.target();
        if target != 0 && self.retired_set.contains(&target) {
            return false;
        }
        if let Pending::Batch { charge, .. } = &pending {
            // A peer that asks again supersedes its unfinished snapshot; its new welcome restarts it.
            self.discard_where(|p| matches!(p, Pending::Batch { target: t, .. } if *t == target));
            let shares_version = self
                .items
                .iter()
                .any(|p| matches!(p, Pending::Batch { charge: c, .. } if c.id == charge.id));
            if !shares_version && self.snapshot_bytes() + charge.bytes > MAX_RETAINED_SNAPSHOT_BYTES
            {
                return false;
            }
        }
        if self.items.len() >= MAX_PENDING_SENDS
            || self.bytes + pending.bytes() > MAX_PENDING_SEND_BYTES
        {
            self.overloaded = true;
            return false;
        }
        self.bytes += pending.bytes();
        self.items.push_back(pending);
        true
    }
}

struct Shared {
    queue: Mutex<Queue>,
    stop: AtomicBool,
    open: AtomicBool,
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Queue> {
    shared.queue.lock().unwrap_or_else(|e| e.into_inner())
}

/// Handle to a relay connection thread. Dropping it closes the connection.
pub struct Socket {
    shared: Arc<Shared>,
    events: Receiver<SocketEvent>,
}

impl Socket {
    pub fn connect(opts: SocketOptions) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            stop: AtomicBool::new(false),
            open: AtomicBool::new(false),
        });
        let (tx, events) = sync_channel(EVENT_CAPACITY);
        let epoch = generate_epoch().map_err(|e| std::io::Error::other(e.to_string()))?;
        let worker = shared.clone();
        thread::Builder::new()
            .name(format!("dial-collab-{}", opts.role.as_str()))
            .spawn(move || Worker::new(worker, opts, epoch, tx).run())?;
        Ok(Self { shared, events })
    }

    pub fn try_event(&self) -> Option<SocketEvent> {
        self.events.try_recv().ok()
    }

    /// Queue one frame for `target` (0 broadcasts). Returns false when refused.
    pub fn send(&self, frame: &Value, target: u32) -> bool {
        self.send_text(frame.to_string(), target)
    }

    /// Queue one pre-serialized frame, so a caller that already bounded the payload does not
    /// serialize it twice.
    pub fn send_text(&self, text: String, target: u32) -> bool {
        if self.shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        let bytes = text.len();
        self.enqueue(Pending::One {
            target,
            text,
            bytes,
        })
    }

    /// Keeps a snapshot contiguous with its welcome and ahead of later live traffic. Frames are
    /// pulled lazily as the transport drains. The stream reads shared log storage identified by
    /// `charge`, so queued snapshots do not each hold a copy of the history.
    pub fn send_batch(
        &self,
        frames: impl Iterator<Item = String> + Send + 'static,
        charge: SnapshotCharge,
        target: u32,
    ) -> bool {
        if self.shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        self.enqueue(Pending::Batch {
            target,
            charge,
            frames: Box::new(frames),
        })
    }

    fn enqueue(&self, pending: Pending) -> bool {
        lock(&self.shared).push(pending)
    }

    /// Discard everything still queued for a peer that left.
    pub fn drop_peer(&self, peer: u32) -> usize {
        if peer == 0 {
            return 0;
        }
        lock(&self.shared).discard_where(|p| p.target() == peer)
    }

    /// Intentional close: queued frames (e.g. a final `bye`) are flushed within a short
    /// deadline, then the socket closes with 1000 and never reconnects.
    pub fn close(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.close();
    }
}

enum Ended {
    Stopped,
    Fatal(String),
    Closed { code: Option<u16>, reason: String },
}

struct Worker {
    shared: Arc<Shared>,
    opts: SocketOptions,
    tx: SyncSender<SocketEvent>,
    /// Random per-socket epoch stamped on every sealed frame. Kept across reconnects.
    epoch: String,
    /// Sequence number of the last sealed frame. Monotonic across reconnects.
    send_seq: u64,
    /// Replay windows for frames received from the peer role.
    windows: SenderWindows,
    attempt: u32,
    rejoining: bool,
    retry_missing_room: bool,
    reclaim_until: Option<Instant>,
    reclaim_attempt: u32,
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

impl Worker {
    fn new(
        shared: Arc<Shared>,
        opts: SocketOptions,
        epoch: String,
        tx: SyncSender<SocketEvent>,
    ) -> Self {
        Self {
            shared,
            opts,
            tx,
            epoch,
            send_seq: 0,
            windows: SenderWindows::default(),
            attempt: 0,
            rejoining: false,
            retry_missing_room: false,
            reclaim_until: None,
            reclaim_attempt: 0,
        }
    }

    fn stopped(&self) -> bool {
        self.shared.stop.load(Ordering::SeqCst)
    }

    /// Deliver an event; false when the owner is gone.
    fn emit(&self, event: SocketEvent) -> bool {
        let mut event = event;
        loop {
            match self.tx.try_send(event) {
                Ok(()) => return true,
                Err(TrySendError::Disconnected(_)) => return false,
                Err(TrySendError::Full(back)) => {
                    // Backpressure: the owner is slow. Stop reading until it drains.
                    if self.stopped() {
                        return false;
                    }
                    event = back;
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn run(mut self) {
        loop {
            if self.stopped() {
                return;
            }
            let ended = match self.open_socket() {
                Ok((ws, probe)) => self.session(ws, probe),
                Err(Connect::Fatal(reason)) => Ended::Fatal(reason),
                Err(Connect::Transient(reason)) => Ended::Closed { code: None, reason },
            };
            self.shared.open.store(false, Ordering::SeqCst);
            let (delay, reason) = match ended {
                Ended::Stopped => return,
                Ended::Fatal(reason) => {
                    self.fail(&reason);
                    return;
                }
                Ended::Closed { code, reason } => match self.handle_close(code, reason) {
                    Some(next) => next,
                    None => return,
                },
            };
            if !self.emit(SocketEvent::Closed {
                reason,
                will_reconnect: true,
            }) {
                return;
            }
            if !self.sleep(delay) {
                return;
            }
        }
    }

    fn fail(&self, reason: &str) {
        // Deliver the terminal event before raising `stop`: `emit` gives up on a full channel once
        // `stop` is set, so the owner would silently miss the close. Bounded, so an owner that holds
        // the handle but never drains cannot pin this worker.
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut event = SocketEvent::Closed {
            reason: reason.to_owned(),
            will_reconnect: false,
        };
        loop {
            match self.tx.try_send(event) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => break,
                Err(TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    event = back;
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
        self.shared.stop.store(true, Ordering::SeqCst);
        let mut queue = lock(&self.shared);
        queue.items.clear();
        queue.bytes = 0;
    }

    /// Handles a closed connection: Some((delay, reason)) to retry, None when terminal.
    fn handle_close(&mut self, code: Option<u16>, reason: String) -> Option<(Duration, String)> {
        let now = Instant::now();
        if code == Some(4009) && self.reclaim_until.is_some_and(|until| now < until) {
            self.rejoining = true;
            let delay = backoff(self.reclaim_attempt, HOST_RECLAIM_BACKOFF_MAX);
            self.reclaim_attempt += 1;
            return Some((
                delay,
                "the relay still holds this host's previous connection".into(),
            ));
        }
        let fatal = code.and_then(close_reason);
        let close_reason = fatal.map(str::to_owned).unwrap_or(reason);
        let retry_room = self.opts.role == Role::Guest
            && (code == Some(4001) || (code == Some(4004) && self.retry_missing_room));
        if retry_room {
            self.retry_missing_room = true;
            self.rejoining = true;
            let delay = backoff(self.attempt, BACKOFF_MAX);
            self.attempt += 1;
            return Some((delay, close_reason));
        }
        if let Some(fatal) = fatal {
            self.fail(fatal);
            return None;
        }
        self.rejoining = true;
        if self.opts.role == Role::Host {
            self.reclaim_until = Some(now + HOST_RECLAIM_WINDOW);
            self.reclaim_attempt = 0;
        }
        let delay = backoff(self.attempt, BACKOFF_MAX);
        self.attempt += 1;
        Some((delay, close_reason))
    }

    fn sleep(&self, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        while Instant::now() < until {
            if self.stopped() {
                return false;
            }
            thread::sleep(
                Duration::from_millis(50).min(until.saturating_duration_since(Instant::now())),
            );
        }
        !self.stopped()
    }

    fn open_socket(&self) -> Result<(Ws, TcpStream), Connect> {
        let url = format!("{}?role={}", self.opts.ws_url, self.opts.role.as_str());
        let parsed =
            url::Url::parse(&url).map_err(|e| Connect::Fatal(format!("invalid relay URL: {e}")))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| Connect::Fatal("relay URL has no host".into()))?
            .trim_matches(['[', ']'])
            .to_owned();
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| Connect::Fatal("relay URL has no port".into()))?;
        let addrs: Vec<_> = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| Connect::Transient(format!("cannot resolve relay {host}: {e}")))?
            .collect();
        let mut last = None;
        let mut tcp = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    tcp = Some(stream);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let tcp = tcp.ok_or_else(|| {
            Connect::Transient(match last {
                Some(e) => format!("cannot reach relay {host}:{port}: {e}"),
                None => format!("relay {host} resolved to no addresses"),
            })
        })?;
        let _ = tcp.set_nodelay(true);
        tcp.set_read_timeout(Some(CONNECT_TIMEOUT))
            .and_then(|_| tcp.set_write_timeout(Some(CONNECT_TIMEOUT)))
            .map_err(|e| Connect::Transient(e.to_string()))?;
        let probe = tcp
            .try_clone()
            .map_err(|e| Connect::Transient(e.to_string()))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        match tungstenite::client_tls_with_config(url.as_str(), tcp, Some(config), None) {
            Ok((ws, _)) => {
                probe
                    .set_read_timeout(Some(READ_POLL))
                    .map_err(|e| Connect::Transient(e.to_string()))?;
                Ok((ws, probe))
            }
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))) => {
                let status = response.status();
                let message = format!("relay refused the connection (HTTP {status})");
                if status.is_client_error() {
                    Err(Connect::Fatal(message))
                } else {
                    Err(Connect::Transient(message))
                }
            }
            Err(tungstenite::HandshakeError::Failure(error)) => Err(Connect::Transient(format!(
                "relay handshake failed: {error}"
            ))),
            Err(tungstenite::HandshakeError::Interrupted(_)) => {
                Err(Connect::Transient("relay handshake timed out".into()))
            }
        }
    }

    fn commit_open(&mut self) -> bool {
        if !self.retry_missing_room {
            self.attempt = 0;
        }
        if self.rejoining {
            self.rejoining = false;
            lock(&self.shared).reset_for_recreated_room();
            if !self.emit(SocketEvent::RoomRecreated) {
                return false;
            }
        }
        self.shared.open.store(true, Ordering::SeqCst);
        self.emit(SocketEvent::Open)
    }

    fn session(&mut self, mut ws: Ws, probe: TcpStream) -> Ended {
        let mut provisional = (self.rejoining
            && self.reclaim_until.is_some_and(|u| Instant::now() < u))
        .then(|| Instant::now() + HOST_RECLAIM_CONFIRM);
        if provisional.is_none() && !self.commit_open() {
            return Ended::Stopped;
        }
        let mut last_rx = Instant::now();
        let mut last_ping = Instant::now();
        let mut close_code: Option<u16> = None;
        let mut close_text = String::new();
        loop {
            if self.stopped() {
                if provisional.is_none() {
                    let _ = probe.set_write_timeout(Some(Duration::from_secs(2)));
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < deadline {
                        match self.drain(&mut ws) {
                            Ok(true) => continue,
                            _ => break,
                        }
                    }
                }
                let _ = ws.close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "".into(),
                }));
                let _ = ws.flush();
                return Ended::Stopped;
            }
            match ws.read() {
                Ok(message) => {
                    last_rx = Instant::now();
                    if provisional.take().is_some()
                        && !matches!(message, Message::Close(_))
                        && !self.commit_open()
                    {
                        return Ended::Stopped;
                    }
                    match message {
                        Message::Binary(bytes) => {
                            if let Some(ended) = self.handle_binary(&bytes) {
                                let _ = ws.close(None);
                                let _ = ws.flush();
                                return ended;
                            }
                        }
                        Message::Text(text) => {
                            if let Some(control) = Control::parse(text.as_str()) {
                                if let Control::PeerLeft(peer) = control {
                                    lock(&self.shared).retire(peer);
                                }
                                if !self.emit(SocketEvent::Control(control)) {
                                    return Ended::Stopped;
                                }
                            }
                        }
                        Message::Close(frame) => {
                            if let Some(frame) = frame {
                                close_code = Some(u16::from(frame.code));
                                close_text = frame.reason.as_str().to_owned();
                            }
                        }
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    }
                }
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ended::Closed {
                        code: close_code,
                        reason: closed_reason(close_code, &close_text),
                    };
                }
                Err(error) => {
                    return Ended::Closed {
                        code: close_code,
                        reason: if close_code.is_some() {
                            closed_reason(close_code, &close_text)
                        } else {
                            format!("connection lost: {error}")
                        },
                    };
                }
            }
            if let Some(until) = provisional
                && Instant::now() >= until
            {
                provisional = None;
                if !self.commit_open() {
                    return Ended::Stopped;
                }
            }
            if close_code.is_some() {
                // Wait for tungstenite to finish the closing handshake, but never past the silence limit.
                if last_rx.elapsed() >= SILENCE_LIMIT {
                    return Ended::Closed {
                        code: close_code,
                        reason: closed_reason(close_code, &close_text),
                    };
                }
                continue;
            }
            if provisional.is_none()
                && let Err(error) = self.drain(&mut ws)
            {
                return error;
            }
            if last_ping.elapsed() >= PING_EVERY {
                last_ping = Instant::now();
                if ws.send(Message::Ping(Vec::new().into())).is_err() {
                    return Ended::Closed {
                        code: None,
                        reason: "connection lost while pinging relay".into(),
                    };
                }
            }
            if last_rx.elapsed() >= SILENCE_LIMIT {
                return Ended::Closed {
                    code: None,
                    reason: "relay stopped responding".into(),
                };
            }
        }
    }

    /// Returns Some when the frame ends the session.
    fn handle_binary(&mut self, bytes: &[u8]) -> Option<Ended> {
        let (peer, payload) = unpack_envelope(bytes)?;
        let frame = self
            .opts
            .key
            .open(payload)
            .ok()
            .and_then(|plain| serde_json::from_slice::<Value>(&plain).ok())
            .filter(Value::is_object);
        let Some(frame) = frame else {
            return match self.opts.role {
                Role::Host => None,
                Role::Guest => Some(Ended::Fatal("bad key or corrupted frame".into())),
            };
        };
        if !accept_sequenced(&mut self.windows, self.opts.role, &frame) {
            return None;
        }
        self.retry_missing_room = false;
        self.attempt = 0;
        if !self.emit(SocketEvent::Frame { peer, frame }) {
            return Some(Ended::Stopped);
        }
        None
    }

    /// Write up to one pass of queued frames. Ok(true) when more work remains.
    fn drain(&mut self, ws: &mut Ws) -> Result<bool, Ended> {
        let mut frames = 0;
        let mut bytes = 0;
        while frames < DRAIN_FRAMES_PER_PASS && bytes < DRAIN_BYTES_PER_PASS {
            let next = {
                let mut queue = lock(&self.shared);
                if queue.overloaded {
                    let recovery = match self.opts.role {
                        Role::Host => "restart sharing and rejoin",
                        Role::Guest => "rejoin",
                    };
                    return Err(Ended::Fatal(format!(
                        "collab send backlog exceeded its limit; {recovery} to resync and check whether pending commands ran before retrying"
                    )));
                }
                let Some(head) = queue.items.front_mut() else {
                    return Ok(false);
                };
                match head {
                    Pending::One { .. } => {
                        let Some(Pending::One {
                            target,
                            text,
                            bytes,
                        }) = queue.items.pop_front()
                        else {
                            unreachable!("front was One");
                        };
                        queue.bytes -= bytes;
                        (target, text)
                    }
                    Pending::Batch { target, frames, .. } => {
                        let target = *target;
                        match frames.next() {
                            Some(text) => (target, text),
                            None => {
                                queue.items.pop_front();
                                continue;
                            }
                        }
                    }
                }
            };
            let (target, text) = next;
            self.send_seq += 1;
            let Some(stamped) = stamp_frame(self.opts.role, &self.epoch, self.send_seq, &text)
            else {
                return Err(Ended::Fatal("could not stamp collab frame".into()));
            };
            let sealed = match self.opts.key.seal(stamped.as_bytes()) {
                Ok(sealed) => sealed,
                Err(error) => {
                    return Err(Ended::Fatal(format!(
                        "could not seal collab frame: {error}"
                    )));
                }
            };
            let envelope = pack_envelope(target, &sealed);
            bytes += envelope.len();
            frames += 1;
            if let Err(error) = ws.send(Message::Binary(envelope.into())) {
                return Err(Ended::Closed {
                    code: None,
                    reason: format!("connection lost: {error}"),
                });
            }
        }
        Ok(!lock(&self.shared).items.is_empty())
    }
}

enum Connect {
    Fatal(String),
    Transient(String),
}

/// Stamp a frame object with its sender role, socket epoch and sequence number before sealing.
/// Frames are JSON objects built by this crate, so the stamp is spliced after the opening brace.
fn stamp_frame(role: Role, epoch: &str, seq: u64, text: &str) -> Option<String> {
    let rest = text.strip_prefix('{')?;
    let separator = if rest.trim_start() == "}" { "" } else { "," };
    Some(format!(
        "{{\"role\":\"{}\",\"epoch\":\"{epoch}\",\"seq\":{seq}{separator}{rest}",
        role.as_str()
    ))
}

/// Authenticate a decrypted frame's sender. Only the peer role is accepted, and each
/// `(epoch, seq)` pair at most once inside the replay window. Callers must have opened the frame.
fn accept_sequenced(windows: &mut SenderWindows, local: Role, frame: &Value) -> bool {
    let sender = match local {
        Role::Host => Role::Guest,
        Role::Guest => Role::Host,
    };
    if frame["role"].as_str() != Some(sender.as_str()) {
        return false;
    }
    match (frame["epoch"].as_str(), frame["seq"].as_u64()) {
        (Some(epoch), Some(seq)) => windows.accept(epoch, seq),
        _ => false,
    }
}

fn closed_reason(code: Option<u16>, text: &str) -> String {
    match code {
        Some(code) => close_reason(code).map(str::to_owned).unwrap_or_else(|| {
            if text.is_empty() {
                format!("connection lost (code {code})")
            } else {
                text.to_owned()
            }
        }),
        None => "connection lost".into(),
    }
}

/// Jittered exponential backoff: base·2^attempt capped at `max`, ±25%.
fn backoff(attempt: u32, max: Duration) -> Duration {
    let base = BACKOFF_BASE
        .saturating_mul(1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX))
        .min(max);
    let jitter = 0.75 + rand::random::<f64>() * 0.5;
    base.mul_f64(jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps_with_jitter() {
        for attempt in 0..40 {
            let delay = backoff(attempt, BACKOFF_MAX);
            assert!(delay <= BACKOFF_MAX.mul_f64(1.25));
            assert!(delay >= Duration::from_millis(750));
        }
        assert!(backoff(10, HOST_RECLAIM_BACKOFF_MAX) <= HOST_RECLAIM_BACKOFF_MAX.mul_f64(1.25));
    }

    #[test]
    fn queue_retires_peers_and_drops_their_work() {
        let mut queue = Queue::default();
        queue.items.push_back(Pending::One {
            target: 2,
            text: "a".into(),
            bytes: 1,
        });
        queue.items.push_back(Pending::One {
            target: 0,
            text: "b".into(),
            bytes: 1,
        });
        queue.items.push_back(Pending::Batch {
            target: 2,
            charge: SnapshotCharge { id: 1, bytes: 0 },
            frames: Box::new(std::iter::empty()),
        });
        queue.bytes = 2;
        queue.retire(2);
        assert_eq!(queue.items.len(), 1);
        assert_eq!(queue.bytes, 1);
        assert!(queue.retired_set.contains(&2));
        for peer in 100..(100 + MAX_RETIRED_PEERS as u32 + 10) {
            queue.retire(peer);
        }
        assert!(queue.retired_set.len() <= MAX_RETIRED_PEERS);
        queue.items.push_back(Pending::One {
            target: 5,
            text: "c".into(),
            bytes: 1,
        });
        queue.bytes += 1;
        queue.reset_for_recreated_room();
        assert!(queue.retired_set.is_empty());
        assert_eq!(
            queue.items.len(),
            1,
            "broadcast work survives a room recreation"
        );
    }

    #[test]
    fn snapshot_streams_coalesce_per_peer_and_share_versions() {
        let stream = |id: usize, bytes: usize| Pending::Batch {
            target: 4,
            charge: SnapshotCharge { id, bytes },
            frames: Box::new(std::iter::empty()),
        };
        let mut queue = Queue::default();
        assert!(queue.push(stream(1, 100)));
        // The same peer asking again supersedes its unfinished snapshot instead of stacking one.
        assert!(queue.push(stream(1, 100)));
        assert_eq!(queue.items.len(), 1);
        // A second stream over the same log version is charged its bytes once.
        let other_peer = Pending::Batch {
            target: 5,
            charge: SnapshotCharge { id: 1, bytes: 100 },
            frames: Box::new(std::iter::empty()),
        };
        assert!(queue.push(other_peer));
        assert_eq!(queue.snapshot_bytes(), 100);
        assert_eq!(
            queue.bytes, 0,
            "snapshot streams are not frame-budget bytes"
        );
        // Distinct versions accumulate and are refused once the retained budget is exhausted.
        assert!(!queue.push(Pending::Batch {
            target: 6,
            charge: SnapshotCharge {
                id: 2,
                bytes: MAX_RETAINED_SNAPSHOT_BYTES,
            },
            frames: Box::new(std::iter::empty()),
        }));
        assert!(
            !queue.overloaded,
            "a refused snapshot is not a frame overload"
        );
    }

    #[test]
    fn frames_are_stamped_and_sequence_checked() {
        let stamped =
            stamp_frame(Role::Guest, "epoch", 7, r#"{"t":"prompt","text":"hi"}"#).unwrap();
        let value: Value = serde_json::from_str(&stamped).unwrap();
        assert_eq!(value["role"], "guest");
        assert_eq!(value["epoch"], "epoch");
        assert_eq!(value["seq"], 7);
        assert_eq!(value["t"], "prompt");
        let empty: Value =
            serde_json::from_str(&stamp_frame(Role::Host, "e", 1, "{}").unwrap()).unwrap();
        assert_eq!(empty["seq"], 1);
        assert!(stamp_frame(Role::Host, "e", 1, "[]").is_none());

        let mut windows = SenderWindows::default();
        // A host accepts guest frames once; the identical ciphertext replayed later is rejected.
        assert!(accept_sequenced(&mut windows, Role::Host, &value));
        assert!(!accept_sequenced(&mut windows, Role::Host, &value));
        // A frame reflected back from the other role is refused even when it is fresh.
        let reflected = serde_json::json!({"role": "host", "epoch": "h", "seq": 1, "t": "prompt"});
        assert!(!accept_sequenced(&mut windows, Role::Host, &reflected));
        assert!(accept_sequenced(&mut windows, Role::Guest, &reflected));
    }

    #[test]
    fn close_codes_map_to_reasons() {
        assert_eq!(
            close_reason(4009),
            Some("a host is already connected for this room")
        );
        assert_eq!(close_reason(1006), None);
        assert_eq!(closed_reason(Some(4001), ""), "room closed");
        assert_eq!(closed_reason(Some(1011), ""), "connection lost (code 1011)");
    }
}
