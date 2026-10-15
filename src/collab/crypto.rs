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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_uses_fresh_ivs() {
        let key = RoomKey::new(&generate_room_key().unwrap()).unwrap();
        let first = key.seal(b"{\"t\":\"hello\"}").unwrap();
        let second = key.seal(b"{\"t\":\"hello\"}").unwrap();
        assert_ne!(first, second, "IV must be random per frame");
        assert_eq!(first.len(), NONCE_LEN + 13 + TAG_LEN);
        assert_eq!(key.open(&first).unwrap(), b"{\"t\":\"hello\"}");
        assert_eq!(key.open(&second).unwrap(), b"{\"t\":\"hello\"}");
        let empty = key.seal(b"").unwrap();
        assert_eq!(key.open(&empty).unwrap(), b"");
    }

    #[test]
    fn open_rejects_wrong_key_tampering_and_short_input() {
        let key = RoomKey::new(&[7u8; 32]).unwrap();
        let other = RoomKey::new(&[8u8; 32]).unwrap();
        let mut sealed = key.seal(b"secret transcript").unwrap();
        assert!(other.open(&sealed).is_err());
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(key.open(&sealed).is_err());
        assert!(key.open(&[0u8; 12]).is_err());
        assert!(key.open(&[0u8; 20]).is_err());
        assert!(RoomKey::new(&[0u8; 16]).is_err());
    }

    #[test]
    fn known_vector_matches_webcrypto_layout() {
        // NIST GCM test case 13 (256-bit zero key, zero IV, empty plaintext): tag only.
        let key = RoomKey::new(&[0u8; 32]).unwrap();
        let mut sealed = vec![0u8; NONCE_LEN];
        sealed.extend_from_slice(&[
            0x53, 0x0f, 0x8a, 0xfb, 0xc7, 0x45, 0x36, 0xb9, 0xa9, 0x63, 0xb4, 0xf1, 0xc4, 0xcb,
            0x73, 0x8b,
        ]);
        assert_eq!(key.open(&sealed).unwrap(), b"");
    }

    #[test]
    fn secret_comparison() {
        assert!(secrets_equal(b"abc", b"abc"));
        assert!(!secrets_equal(b"abc", b"abd"));
        assert!(!secrets_equal(b"abc", b"ab"));
    }
}
