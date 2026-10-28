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

#[cfg(unix)]
fn fake_gh(dir: &Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("gh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
fn fixture_share() -> EncryptedShare {
    EncryptedShare {
        bytes: b"ciphertext".to_vec(),
        key_fragment: "KEY".to_owned(),
    }
}

#[cfg(unix)]
#[test]
fn gist_fallback_returns_the_gist_url_with_the_key_fragment() {
    let dir = tempfile::tempdir().unwrap();
    let gh = fake_gh(
        dir.path(),
        "cat > /dev/null\necho https://gist.github.com/user/abc123",
    );
    let url = upload_gist_with(&gh, &fixture_share()).unwrap();
    assert_eq!(url, "https://gist.github.com/user/abc123#KEY");
}

#[cfg(unix)]
#[test]
fn gist_fallback_surfaces_gh_failure_output() {
    let dir = tempfile::tempdir().unwrap();
    let gh = fake_gh(
        dir.path(),
        "cat > /dev/null\necho not logged in >&2\nexit 1",
    );
    let error = upload_gist_with(&gh, &fixture_share()).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("gh gist create failed"), "{text}");
    assert!(text.contains("not logged in"), "{text}");
}

#[cfg(unix)]
#[test]
fn missing_gh_is_an_actionable_error() {
    let error = upload_gist_with("dial-no-such-gh-binary", &fixture_share()).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("GitHub CLI"), "{text}");
}
