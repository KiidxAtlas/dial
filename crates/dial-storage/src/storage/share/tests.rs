use super::*;
#[test]
fn encryption_roundtrip_redacts_secrets() {
    let source = br#"{"messages":[{"text":"token sk-ant-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH1234567890"}]}"#;
    let encrypted = encrypt_transcript(source).unwrap();
    assert!(!encrypted.bytes.windows(7).any(|w| w == b"sk-ant-"));
    let decoded: Value =
        serde_json::from_slice(&decrypt_share(&encrypted.bytes, &encrypted.key_fragment).unwrap())
            .unwrap();
    assert!(!decoded.to_string().contains("sk-ant-"));
    assert!(decrypt_share(&encrypted.bytes, &URL_SAFE_NO_PAD.encode([1u8; 32])).is_err());
}
