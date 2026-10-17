use super::*;

const T0: u64 = 1_700_000_000;

fn png(extra: &[u8]) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(extra);
    bytes
}

fn store() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn register_and_list_round_trip() {
    let home = store();
    let bytes = png(b"one");
    let entry = register_at(home.path(), "s1", ImageInput::Bytes(&bytes), None, T0).unwrap();
    assert_eq!(entry.sha256, sha256_hex(&bytes));
    assert_eq!(entry.mime, "image/png");
    assert_eq!(entry.bytes, bytes.len() as u64);
    let refs = list(home.path()).unwrap();
    assert_eq!(refs, vec![entry]);
    assert_eq!(refs[0].sessions, vec!["s1".to_owned()]);
}

#[test]
fn register_rejects_non_images() {
    let home = store();
    let err = register_at(home.path(), "s1", ImageInput::Bytes(b"hello"), None, T0).unwrap_err();
    assert!(err.to_string().contains("not a supported"));
    let err = register_at(
        home.path(),
        "s1",
        ImageInput::Bytes(b"hello"),
        Some("text/plain"),
        T0,
    )
    .unwrap_err();
    assert!(err.to_string().contains("not an image type"));
}

#[test]
fn register_rejects_mislabeled_png_bytes() {
    let home = store();
    let err = register_at(
        home.path(),
        "s1",
        ImageInput::Bytes(b"hello"),
        Some("image/png"),
        T0,
    )
    .unwrap_err();
    assert!(err.to_string().contains("does not match"));
    assert!(list(home.path()).unwrap().is_empty());
}

#[test]
fn resolve_rejects_unknown_sha() {
    let home = store();
    let bytes = png(b"known");
    let entry = register_at(home.path(), "s1", ImageInput::Bytes(&bytes), None, T0).unwrap();
    let unknown = "0".repeat(64);
    assert!(
        resolve(home.path(), &unknown)
            .unwrap_err()
            .to_string()
            .contains("unknown image")
    );
    assert!(resolve(home.path(), "../escape").is_err());
    assert!(resolve(home.path(), &entry.sha256.to_uppercase()).is_err());
    let path = resolve(home.path(), &entry.sha256).unwrap();
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn dedup_by_sha() {
    let home = store();
    let bytes = png(b"shared");
    register_at(home.path(), "s1", ImageInput::Bytes(&bytes), None, T0).unwrap();
    let second = register_at(home.path(), "s2", ImageInput::Bytes(&bytes), None, T0 + 5).unwrap();
    register_at(home.path(), "s1", ImageInput::Bytes(&bytes), None, T0 + 6).unwrap();
    let refs = list(home.path()).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].sessions, vec!["s1".to_owned(), "s2".to_owned()]);
    assert_eq!(refs[0].created, T0);
    assert_eq!(refs[0].touched, T0 + 6);
    assert_eq!(second.sha256, refs[0].sha256);
}

#[test]
fn doctor_detects_corrupted_blob() {
    let home = store();
    let bytes = png(b"sound");
    let entry = register_at(home.path(), "s1", ImageInput::Bytes(&bytes), None, T0).unwrap();
    let clean = verify(home.path()).unwrap();
    assert_eq!(clean.entries, 1);
    assert!(clean.problems.is_empty());
    fs::write(store_dir(home.path()).join(&entry.sha256), png(b"tampered")).unwrap();
    let report = verify(home.path()).unwrap();
    assert_eq!(report.problems.len(), 1);
    assert!(report.problems[0].contains("hashes to"));
    fs::remove_file(store_dir(home.path()).join(&entry.sha256)).unwrap();
    let report = verify(home.path()).unwrap();
    assert!(report.problems[0].contains("missing"));
}

#[test]
fn doctor_reports_orphan_blobs() {
    let home = store();
    let bytes = png(b"tracked");
    register_at(home.path(), "s1", ImageInput::Bytes(&bytes), None, T0).unwrap();
    let orphan = sha256_hex(b"untracked");
    fs::write(store_dir(home.path()).join(&orphan), b"untracked").unwrap();
    let report = verify(home.path()).unwrap();
    assert!(report.problems.is_empty());
    assert_eq!(report.orphans, vec![orphan]);
}

#[test]
fn purge_dry_run_leaves_files() {
    let home = store();
    let bytes = png(b"stale");
    let entry = register_at(home.path(), "s1", ImageInput::Bytes(&bytes), None, T0).unwrap();
    let later = T0 + IMAGE_TTL_SECS + 1;
    let report = purge_at(home.path(), false, false, later).unwrap();
    assert!(!report.applied);
    assert_eq!(report.selected.len(), 1);
    assert_eq!(report.deleted, 0);
    assert!(store_dir(home.path()).join(&entry.sha256).is_file());
    assert_eq!(list(home.path()).unwrap().len(), 1);
}

#[test]
fn purge_apply_removes_expired_only() {
    let home = store();
    let old = png(b"old");
    let fresh = png(b"fresh");
    let old_entry = register_at(home.path(), "s1", ImageInput::Bytes(&old), None, T0).unwrap();
    let fresh_entry = register_at(
        home.path(),
        "s1",
        ImageInput::Bytes(&fresh),
        None,
        T0 + IMAGE_TTL_SECS,
    )
    .unwrap();
    let later = T0 + IMAGE_TTL_SECS + 10;
    let report = purge_at(home.path(), false, true, later).unwrap();
    assert!(report.applied);
    assert_eq!(report.deleted, 1);
    assert!(report.errors.is_empty());
    assert!(!store_dir(home.path()).join(&old_entry.sha256).exists());
    assert!(store_dir(home.path()).join(&fresh_entry.sha256).is_file());
    let remaining = list(home.path()).unwrap();
    assert_eq!(remaining, vec![fresh_entry.clone()]);

    let report = purge_at(home.path(), true, true, later).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(list(home.path()).unwrap().is_empty());
    assert!(!store_dir(home.path()).join(&fresh_entry.sha256).exists());
}

#[test]
fn scan_sessions_finds_image_blocks() {
    let home = store();
    let sessions = home.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let bytes = png(b"transcript");
    let b64 = STANDARD.encode(&bytes);
    let line_block = json!({
        "type": "message",
        "message": {"role": "user", "content": [
            {"type": "text", "text": "look"},
            {"type": "image", "mimeType": "image/png", "data": b64}
        ]}
    });
    let line_url = json!({
        "type": "message",
        "message": {"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{b64}")}}
        ]}
    });
    let line_text = json!({"type": "message", "message": {"content": "no images here"}});
    let file = sessions.join("2026-01-01_abc123.jsonl");
    fs::write(&file, format!("{line_text}\n{line_block}\n{line_url}\n")).unwrap();
    let found = scan_sessions_at(home.path(), &sessions, T0).unwrap();
    assert_eq!(found, 1, "the same image in one transcript counts once");
    let refs = list(home.path()).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].sha256, sha256_hex(&bytes));
    assert_eq!(refs[0].sessions, vec!["abc123".to_owned()]);
}

#[test]
fn scan_all_sessions_walks_projects() {
    let home = store();
    let sessions = home
        .path()
        .join("projects")
        .join("demo-1234")
        .join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let bytes = png(b"project");
    let line = json!({"type": "image", "mimeType": "image/png", "data": STANDARD.encode(&bytes)});
    fs::write(sessions.join("x_s9.jsonl"), format!("{line}\n")).unwrap();
    assert_eq!(scan_all_sessions(home.path()).unwrap(), 1);
    assert_eq!(
        list(home.path()).unwrap()[0].sessions,
        vec!["s9".to_owned()]
    );
}

#[test]
fn scan_all_sessions_includes_archived_transcripts() {
    let home = store();
    let archive = home
        .path()
        .join("projects")
        .join("demo-1234")
        .join("sessions")
        .join("archive");
    fs::create_dir_all(&archive).unwrap();
    let bytes = png(b"archived");
    let line = json!({"type": "image", "mimeType": "image/png", "data": STANDARD.encode(&bytes)});
    fs::write(archive.join("x_s7.jsonl"), format!("{line}\n")).unwrap();
    assert_eq!(scan_all_sessions(home.path()).unwrap(), 1);
    assert_eq!(
        list(home.path()).unwrap()[0].sessions,
        vec!["s7".to_owned()]
    );
}

#[test]
// Windows has no file mode bits; this test asserts the Unix mode, so it is Unix-only.
#[cfg(unix)]
fn store_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let home = store();
    let bytes = png(b"private");
    let entry = register_at(home.path(), "s1", ImageInput::Bytes(&bytes), None, T0).unwrap();
    let mode = fs::metadata(store_dir(home.path()).join(&entry.sha256))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0);
}
