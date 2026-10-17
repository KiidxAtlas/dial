//! AES-256-GCM sealing for collab frames.
//!
//! The room key lives only in the link fragment; the relay sees opaque bytes.
//! Sealed layout: `[12B IV][ciphertext+16B tag]`, byte-compatible with WebCrypto
//! `AES-GCM` so the browser viewer opens what the host seals.
use anyhow::{Result, anyhow, ensure};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::collections::{HashMap, VecDeque};

/// AES-256-GCM room key; the seal key for every collab frame.
pub const ROOM_KEY_BYTES: usize = 32;
/// Random write token appended to the room key in full links (`base64url(key ∥ token)`).
pub const WRITE_TOKEN_BYTES: usize = 16;
/// Random room id bytes (base64url-encoded in links and relay paths).
pub const ROOM_ID_BYTES: usize = 16;
const TAG_LEN: usize = 16;

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    SystemRandom::new()
        .fill(&mut out)
        .map_err(|_| anyhow!("system random source unavailable"))?;
    Ok(out)
}

pub fn generate_room_key() -> Result<[u8; ROOM_KEY_BYTES]> {
    random_bytes()
}

pub fn generate_write_token() -> Result<[u8; WRITE_TOKEN_BYTES]> {
    random_bytes()
}

/// A room key ready for sealing and opening. Cloning shares nothing mutable.
#[derive(Clone)]
pub struct RoomKey {
    raw: [u8; ROOM_KEY_BYTES],
}

impl std::fmt::Debug for RoomKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RoomKey(..)")
    }
}

impl RoomKey {
    pub fn new(raw: &[u8]) -> Result<Self> {
        ensure!(
            raw.len() == ROOM_KEY_BYTES,
            "room key must be {ROOM_KEY_BYTES} bytes, got {}",
            raw.len()
        );
        let mut key = [0u8; ROOM_KEY_BYTES];
        key.copy_from_slice(raw);
        Ok(Self { raw: key })
    }

    fn aead(&self) -> LessSafeKey {
        // Length is checked in `new`, so construction cannot fail.
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.raw).expect("32-byte AES-256 key"))
    }

    /// Seal plaintext with a fresh random IV.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let iv: [u8; NONCE_LEN] = random_bytes()?;
        let mut out = Vec::with_capacity(NONCE_LEN + plaintext.len() + TAG_LEN);
        out.extend_from_slice(&iv);
        out.extend_from_slice(plaintext);
        let tag = self
            .aead()
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(iv),
                Aad::empty(),
                &mut out[NONCE_LEN..],
            )
            .map_err(|_| anyhow!("collab frame sealing failed"))?;
        out.extend_from_slice(tag.as_ref());
        Ok(out)
    }

    /// Inverse of [`RoomKey::seal`]. Fails on authentication failure or malformed input.
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            sealed.len() >= NONCE_LEN + TAG_LEN,
            "sealed frame too short"
        );
        let mut iv = [0u8; NONCE_LEN];
        iv.copy_from_slice(&sealed[..NONCE_LEN]);
        let mut body = sealed[NONCE_LEN..].to_vec();
        let plain_len = self
            .aead()
            .open_in_place(Nonce::assume_unique_for_key(iv), Aad::empty(), &mut body)
            .map_err(|_| anyhow!("bad room key or corrupted frame"))?
            .len();
        body.truncate(plain_len);
        Ok(body)
    }
}

/// Constant-time equality for secrets of public length.
pub fn secrets_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Anti-replay window for one sender epoch. Each sequence number is accepted at most once and only
/// while it lies within [`REPLAY_WINDOW`] of the highest number seen; older numbers are rejected.
#[derive(Debug, Default, Clone)]
pub struct ReplayWindow {
    started: bool,
    highest: u64,
    /// Bit `i` set means sequence `highest - i` was accepted.
    seen: u64,
}

/// Sequence numbers tracked behind the highest accepted one.
pub const REPLAY_WINDOW: u64 = 64;

impl ReplayWindow {
    /// Returns true exactly once per sequence number inside the window. Sequence 0 is never valid.
    pub fn accept(&mut self, seq: u64) -> bool {
        if seq == 0 {
            return false;
        }
        if !self.started {
            self.started = true;
            self.highest = seq;
            self.seen = 1;
            return true;
        }
        if seq > self.highest {
            let shift = seq - self.highest;
            self.seen = if shift >= REPLAY_WINDOW {
                0
            } else {
                self.seen << shift
            };
            self.seen |= 1;
            self.highest = seq;
            return true;
        }
        let back = self.highest - seq;
        if back >= REPLAY_WINDOW {
            return false;
        }
        let bit = 1u64 << back;
        if self.seen & bit != 0 {
            return false;
        }
        self.seen |= bit;
        true
    }
}

/// Most sender epochs tracked at once. The oldest epoch is forgotten first.
pub const MAX_TRACKED_SENDERS: usize = 1024;

/// Replay windows keyed by the sender's random epoch. Epochs persist for the lifetime of the
/// socket owner, so a frame captured from an earlier epoch is still rejected after a newer one
/// has been seen.
#[derive(Debug, Default)]
pub struct SenderWindows {
    order: VecDeque<String>,
    windows: HashMap<String, ReplayWindow>,
}

impl SenderWindows {
    /// Accept `(epoch, seq)` once. Authentication must happen before this check.
    pub fn accept(&mut self, epoch: &str, seq: u64) -> bool {
        if !self.windows.contains_key(epoch) {
            if self.windows.len() >= MAX_TRACKED_SENDERS
                && let Some(old) = self.order.pop_front()
            {
                self.windows.remove(&old);
            }
            self.order.push_back(epoch.to_owned());
            self.windows
                .insert(epoch.to_owned(), ReplayWindow::default());
        }
        self.windows
            .get_mut(epoch)
            .is_some_and(|window| window.accept(seq))
    }
}

/// Random per-socket epoch (base64url) stamped on every frame and checked by receivers.
pub fn generate_epoch() -> Result<String> {
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        random_bytes::<12>()?,
    ))
}

#[cfg(test)]
mod tests;
