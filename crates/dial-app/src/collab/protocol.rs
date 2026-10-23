//! Collab wire protocol.
//!
//! Relay contract:
//! - `GET /r/<roomId>?role=host|guest` upgrades to a WebSocket.
//! - Binary frames carry the envelope `[4B uint32 BE peerId][sealed payload]`.
//!   Host→relay: peerId 0 broadcasts, N targets guest N. Guest→relay: always 0;
//!   the relay rewrites it to the sender's id.
//! - TEXT control frames (unencrypted, no session data): relay→host
//!   `{"t":"peer-joined"|"peer-left","peer":N}`, relay→guest `{"t":"room-closed"}`.
//!
//! Sealed payloads are JSON frames (see [`frame`] helpers). Every sealed frame is stamped by its
//! socket with `role` (`host`|`guest`), a random per-socket `epoch` and a monotonic `transportSeq`,
//! which receivers check with a replay window before acting (`super::crypto::SenderWindows`).
//! Entry `seq` remains the zero-based log index, independent of transport/control frames and resets.
//! Version 1003 requires `transportSeq`; legacy ambiguous `seq` stamps are not accepted. Clients
//! speaking other protocol versions are refused at `hello` instead of silently misrendering.
use super::crypto::{ROOM_ID_BYTES, ROOM_KEY_BYTES, WRITE_TOKEN_BYTES, random_bytes};
use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::sync::Arc;

/// Wire protocol version carried in `hello`/`welcome`; the host rejects mismatches.
pub const COLLAB_PROTO: u64 = 1003;
/// Plaintext envelope prefix length.
pub const ENVELOPE_HEADER_LENGTH: usize = 4;
/// Upper bound for one replicated entry/node payload before shrinking.
pub const MAX_REPLICATED_PAYLOAD_BYTES: usize = 256 * 1024;
/// Snapshot chunks are bounded by this many serialized bytes (at least one entry each).
pub const SNAPSHOT_CHUNK_BYTES: usize = 512 * 1024;
/// Relay per-message ceiling. Hosts never produce larger frames.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub fn pack_envelope(peer: u32, sealed: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENVELOPE_HEADER_LENGTH + sealed.len());
    out.extend_from_slice(&peer.to_be_bytes());
    out.extend_from_slice(sealed);
    out
}

pub fn unpack_envelope(data: &[u8]) -> Option<(u32, &[u8])> {
    if data.len() < ENVELOPE_HEADER_LENGTH {
        return None;
    }
    let peer = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    Some((peer, &data[ENVELOPE_HEADER_LENGTH..]))
}

/// Rewrite the peer id in place without copying the payload.
pub fn rewrite_envelope_peer(data: &mut [u8], peer: u32) -> bool {
    if data.len() < ENVELOPE_HEADER_LENGTH {
        return false;
    }
    data[..ENVELOPE_HEADER_LENGTH].copy_from_slice(&peer.to_be_bytes());
    true
}

pub fn generate_room_id() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(random_bytes::<ROOM_ID_BYTES>()?))
}

pub fn valid_room_id(id: &str) -> bool {
    (10..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_local_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Normalize a relay base URL (ws/wss/http/https) into a ws/wss origin.
pub fn normalize_relay_origin(relay_url: &str) -> Result<String> {
    let url = url::Url::parse(relay_url.trim())
        .map_err(|_| anyhow::anyhow!("invalid relay URL: {relay_url}"))?;
    let scheme = match url.scheme() {
        "wss" | "https" => "wss",
        "ws" | "http" => "ws",
        other => bail!("unsupported relay URL scheme: {other}:"),
    };
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("relay URL has no host: {relay_url}"))?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if scheme == "ws" && !is_local_host(&host) {
        bail!("relay link must be wss:// (plain ws:// is only allowed for localhost)");
    }
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!("{scheme}://{host}{port}"))
}

/// Render the shareable link `host[:port]/r/<roomId>.<key>` (`ws://…` kept for localhost).
/// Full links append the write token to the key; view links carry the bare key.
pub fn format_link(
    relay_url: &str,
    room_id: &str,
    key: &[u8],
    write_token: Option<&[u8]>,
) -> Result<String> {
    let origin = normalize_relay_origin(relay_url)?;
    let mut secret = key.to_vec();
    if let Some(token) = write_token {
        secret.extend_from_slice(token);
    }
    let key_text = URL_SAFE_NO_PAD.encode(&secret);
    let compact = origin.strip_prefix("wss://").unwrap_or(&origin);
    Ok(format!("{compact}/r/{room_id}.{key_text}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLink {
    /// `wss://host[:port]/r/<roomId>`: no query, no fragment, no key.
    pub ws_url: String,
    pub room_id: String,
    pub key: [u8; ROOM_KEY_BYTES],
    /// Present for full links; absent for read-only (view) links.
    pub write_token: Option<[u8; WRITE_TOKEN_BYTES]>,
}

/// Parse any collab link form: full `wss://…/r/<id>.<key>`, scheme-less `host/r/…`, a browser
/// deep link `https://…/#<link>`, legacy `#`-joined keys, or bare `<id>.<key>` resolved
/// against `default_relay` (Dial has no public default relay).
pub fn parse_link(link: &str, default_relay: &str) -> Result<ParsedLink> {
    let mut text = link.trim().replace("%23", "#");
    if let Some((id, key)) = bare_link(&text) {
        let relay = default_relay.trim();
        ensure!(
            !relay.is_empty(),
            "bare collab link needs a relay; set collab.relay_url or paste the full link"
        );
        text = format!("{}/r/{id}.{key}", normalize_relay_origin(relay)?);
    } else if !text.contains("://") {
        text = format!("wss://{text}");
    }
    let url = url::Url::parse(&text).map_err(|_| anyhow::anyhow!("invalid collab link: {link}"))?;
    let fragment = url.fragment().unwrap_or("");
    if matches!(url.scheme(), "http" | "https")
        && !fragment.is_empty()
        && let Ok(parsed) = parse_link(fragment, default_relay)
    {
        return Ok(parsed);
    }
    let origin = normalize_relay_origin(url.as_str())?;
    let path = url.path();
    let Some(rest) = path.strip_prefix("/r/") else {
        if !fragment.is_empty() && !matches!(url.scheme(), "http" | "https") {
            return parse_link(fragment, default_relay);
        }
        bail!("collab link must contain a /r/<roomId> path");
    };
    let (room_id, key_text) = match rest.split_once('.') {
        Some((id, key)) => (id, key),
        None => (rest, fragment),
    };
    ensure!(valid_room_id(room_id), "collab link has an invalid room id");
    ensure!(
        !key_text.is_empty(),
        "collab link is missing the <key> part"
    );
    let secret = key_text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        .then(|| URL_SAFE_NO_PAD.decode(key_text).ok())
        .flatten();
    let Some(secret) = secret
        .filter(|s| s.len() == ROOM_KEY_BYTES || s.len() == ROOM_KEY_BYTES + WRITE_TOKEN_BYTES)
    else {
        bail!("collab link key must be 32 (view) or 48 (full) base64url bytes");
    };
    let mut key = [0u8; ROOM_KEY_BYTES];
    key.copy_from_slice(&secret[..ROOM_KEY_BYTES]);
    let write_token = (secret.len() > ROOM_KEY_BYTES).then(|| {
        let mut token = [0u8; WRITE_TOKEN_BYTES];
        token.copy_from_slice(&secret[ROOM_KEY_BYTES..]);
        token
    });
    Ok(ParsedLink {
        ws_url: format!("{origin}/r/{room_id}"),
        room_id: room_id.to_owned(),
        key,
        write_token,
    })
}

fn bare_link(text: &str) -> Option<(&str, &str)> {
    let at = text.find(['.', '#'])?;
    let (id, key) = (&text[..at], &text[at + 1..]);
    let b64 = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    (valid_room_id(id) && b64(key)).then_some((id, key))
}

// ---------------------------------------------------------------------------
// Frames (JSON inside the seal)
// ---------------------------------------------------------------------------

/// Relay control message (TEXT JSON, unencrypted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    PeerJoined(u32),
    PeerLeft(u32),
    RoomClosed,
}

impl Control {
    pub fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let peer = || value["peer"].as_u64().and_then(|p| u32::try_from(p).ok());
        match value["t"].as_str()? {
            "peer-joined" => Some(Self::PeerJoined(peer()?)),
            "peer-left" => Some(Self::PeerLeft(peer()?)),
            "room-closed" => Some(Self::RoomClosed),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn encode(&self) -> String {
        match self {
            Self::PeerJoined(peer) => json!({"t":"peer-joined","peer":peer}).to_string(),
            Self::PeerLeft(peer) => json!({"t":"peer-left","peer":peer}).to_string(),
            Self::RoomClosed => json!({"t":"room-closed"}).to_string(),
        }
    }
}

/// Frame constructors. Frames are JSON objects tagged by `t`.
pub mod frame {
    use super::*;

    pub fn hello(name: &str, write_token: Option<&[u8]>) -> Value {
        let mut frame = json!({"t":"hello","proto":COLLAB_PROTO,"name":name});
        if let Some(token) = write_token {
            frame["writeToken"] = json!(URL_SAFE_NO_PAD.encode(token));
        }
        frame
    }
    pub fn notice(message: &str) -> Value {
        json!({"t":"notice","message":message})
    }
    /// `{"t":"reset"}`: guests clear their replicated log before the host replays it.
    pub fn reset() -> Value {
        json!({"t":"reset"})
    }
    pub fn error(message: &str) -> Value {
        json!({"t":"error","message":message})
    }
    pub fn bye(reason: &str) -> Value {
        json!({"t":"bye","reason":reason})
    }
    /// `{"t":"entry","seq":N,"entry":…}`: `seq` is the entry's index in the host log, so receivers
    /// drop duplicates and detect gaps. The entry is embedded from its JSON without re-serializing.
    pub fn entry_json(seq: usize, entry_json: &str) -> String {
        format!("{{\"t\":\"entry\",\"seq\":{seq},\"entry\":{entry_json}}}")
    }
    /// `{"t":"snapshot-chunk","entries":[…],"final":…}` from each entry's JSON.
    pub fn snapshot_chunk(entries: &[&str], last: bool) -> String {
        format!(
            "{{\"t\":\"snapshot-chunk\",\"entries\":[{}],\"final\":{last}}}",
            entries.join(",")
        )
    }
}

/// Byte-bounded snapshot chunking over shared, immutable log storage. Chunks are produced lazily
/// from a cursor, so a queued snapshot holds one `Arc` instead of a copy of the history. Every chunk
/// carries at least one entry; an empty snapshot still yields one `final` chunk so guests never
/// wait for a missing terminator.
pub fn snapshot_chunks(entries: Arc<Vec<Arc<str>>>) -> impl Iterator<Item = String> + Send {
    let mut next = 0usize;
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let mut batch: Vec<&str> = Vec::new();
        let mut bytes = 0usize;
        while let Some(entry) = entries.get(next) {
            if !batch.is_empty() && bytes + entry.len() > SNAPSHOT_CHUNK_BYTES {
                break;
            }
            bytes += entry.len();
            batch.push(&**entry);
            next += 1;
        }
        let last = next >= entries.len();
        done = last;
        Some(frame::snapshot_chunk(&batch, last))
    })
}

/// Serialize a replicated value under [`MAX_REPLICATED_PAYLOAD_BYTES`]. Oversized values first
/// lose inline image data, then have long strings clipped (head and tail kept); a value that
/// still does not fit ships as a bounded placeholder so a snapshot train always terminates.
pub fn bounded_json(value: &Value) -> String {
    bounded_json_with(value, MAX_REPLICATED_PAYLOAD_BYTES)
}

pub fn bounded_json_with(value: &Value, max: usize) -> String {
    let text = value.to_string();
    if text.len() <= max {
        return text;
    }
    let mut copy = value.clone();
    strip_images(&mut copy);
    let text = copy.to_string();
    if text.len() <= max {
        return text;
    }
    for limit in [16 * 1024, 4 * 1024, 1024, 256] {
        let mut clipped = copy.clone();
        clip_strings(&mut clipped, limit);
        let text = clipped.to_string();
        if text.len() <= max {
            return text;
        }
    }
    let mut placeholder = json!({"type":"placeholder","note":"entry too large to replicate"});
    for key in ["type", "id", "parent", "ts"] {
        if let Some(v) = value.get(key).filter(|v| v.is_string() || v.is_null()) {
            placeholder[if key == "type" { "originalType" } else { key }] = v.clone();
        }
    }
    let text = placeholder.to_string();
    if text.len() <= max {
        text
    } else {
        json!({"type":"placeholder","note":"entry too large to replicate"}).to_string()
    }
}

fn strip_images(value: &mut Value) {
    match value {
        Value::String(s) if s.starts_with("data:image/") && s.len() > 512 => {
            *s = "[image omitted from collab replica]".into();
        }
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("image")
                && let Some(Value::String(data)) = map.get_mut("data")
                && data.len() > 512
            {
                *data = String::new();
            }
            map.values_mut().for_each(strip_images);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_images),
        _ => {}
    }
}

fn clip_strings(value: &mut Value, limit: usize) {
    match value {
        Value::String(s) if s.len() > limit => *s = clip_middle(s, limit),
        Value::Object(map) => map.values_mut().for_each(|v| clip_strings(v, limit)),
        Value::Array(items) => items.iter_mut().for_each(|v| clip_strings(v, limit)),
        _ => {}
    }
}

/// Keep the head and tail of `text` within about `limit` bytes, on char boundaries.
pub fn clip_middle(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let half = limit / 2;
    let mut head = half.min(text.len());
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len().saturating_sub(half);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n…[{} bytes clipped for collab]…\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

#[cfg(test)]
mod tests;
