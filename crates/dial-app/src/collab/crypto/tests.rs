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
        0x53, 0x0f, 0x8a, 0xfb, 0xc7, 0x45, 0x36, 0xb9, 0xa9, 0x63, 0xb4, 0xf1, 0xc4, 0xcb, 0x73,
        0x8b,
    ]);
    assert_eq!(key.open(&sealed).unwrap(), b"");
}

#[test]
fn secret_comparison() {
    assert!(secrets_equal(b"abc", b"abc"));
    assert!(!secrets_equal(b"abc", b"abd"));
    assert!(!secrets_equal(b"abc", b"ab"));
}

#[test]
fn replay_window_accepts_each_sequence_once_within_window() {
    let mut window = ReplayWindow::default();
    assert!(!window.accept(0));
    assert!(window.accept(5));
    assert!(!window.accept(5), "identical sequence replayed");
    assert!(window.accept(3), "late frame inside the window is fresh");
    assert!(!window.accept(3));
    assert!(window.accept(70));
    assert!(!window.accept(5), "sequence fell out of the window");
    assert!(window.accept(71));
    assert!(!window.accept(71));
}

#[test]
fn sender_windows_reject_old_epoch_replays_after_newer_epoch() {
    let mut senders = SenderWindows::default();
    assert!(senders.accept("epoch-a", 1));
    assert!(senders.accept("epoch-a", 2));
    assert!(senders.accept("epoch-b", 1));
    assert!(
        !senders.accept("epoch-a", 1),
        "captured old-epoch frame replayed"
    );
    assert!(!senders.accept("epoch-b", 1), "identical frame replayed");
    assert!(senders.accept("epoch-a", 3));
    assert!(!generate_epoch().unwrap().is_empty());
}

#[test]
fn identical_ciphertext_replay_is_caught_by_sequence_not_iv() {
    let key = RoomKey::new(&[4u8; 32]).unwrap();
    let sealed = key.seal(br#"{"t":"prompt","transportSeq":9}"#).unwrap();
    let mut windows = SenderWindows::default();
    assert!(windows.accept("sender", 9));
    // The relay can redeliver the same ciphertext; decryption still succeeds, so the
    // sequence check is what rejects it.
    assert_eq!(
        key.open(&sealed).unwrap(),
        br#"{"t":"prompt","transportSeq":9}"#
    );
    assert!(!windows.accept("sender", 9));
}
