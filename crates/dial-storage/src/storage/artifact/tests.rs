use super::*;
#[test]
fn missing_artifacts_are_explicit_in_both_paging_modes() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let uri = format!("artifact://{}", "a".repeat(64));
    for error in [
        store.retrieve(&uri, 1, 1).unwrap_err(),
        store.retrieve_bytes(&uri, 0, 1).unwrap_err(),
    ] {
        assert!(
            error
                .to_string()
                .contains("unavailable in this project's retained store")
        );
        assert!(error.to_string().contains(&uri));
        assert!(!error.to_string().contains(dir.path().to_str().unwrap()));
    }
}

#[test]
fn ranged_and_immutable() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let uri = store.retain("a\nb\nc\n").unwrap();
    assert_eq!(uri, store.retain("a\nb\nc\n").unwrap());
    assert!(store.retrieve(&uri, 2, 1).unwrap().contains("2:b\n"));
    assert!(store.retrieve("artifact://../x", 1, 1).is_err());
}
fn capped(max: u64, head: u64) -> (tempfile::TempDir, ArtifactStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path())
        .unwrap()
        .with_limits(&ArtifactConfig {
            max_bytes: max,
            head_bytes: head,
        });
    (dir, store)
}

#[test]
fn capped_spool_keeps_head_marker_and_tail_and_hashes_stored_bytes() {
    let (_dir, store) = capped(64, 16);
    let mut spool = store.spool().unwrap();
    let stream: Vec<u8> = (0..1000u32).map(|n| b'a' + (n % 26) as u8).collect();
    for chunk in stream.chunks(7) {
        spool.write_all(chunk).unwrap();
    }
    assert_eq!(spool.elided_bytes(), 1000 - 64);
    let tail_path = spool.path.with_extension("tail");
    assert!(tail_path.exists());
    let uri = spool.finish().unwrap();
    assert!(!tail_path.exists(), "the tail ring is private scratch");
    let stored = fs::read(store.path(&uri).unwrap()).unwrap();
    assert!(stored.starts_with(&stream[..16]));
    assert!(stored.ends_with(&stream[1000 - 48..]));
    let text = String::from_utf8_lossy(&stored);
    assert!(
        text.contains(
            "[ARTIFACT TRUNCATED: kept first 16B + last 48B of 1000B; 936B elided from the middle]"
        ),
        "{text}"
    );
    assert_eq!(uri, format!("artifact://{:x}", Sha256::digest(&stored)));
}

#[test]
fn output_within_the_cap_stays_verbatim_even_past_the_head() {
    let (_dir, store) = capped(64, 16);
    let mut spool = store.spool().unwrap();
    let stream: Vec<u8> = (0..60u8).collect();
    spool.write_all(&stream).unwrap();
    assert_eq!(spool.elided_bytes(), 0);
    let uri = spool.finish().unwrap();
    assert_eq!(fs::read(store.path(&uri).unwrap()).unwrap(), stream);
}

#[test]
fn unbounded_and_head_only_limits() {
    let (_dir, store) = capped(0, 0);
    let mut spool = store.spool().unwrap();
    spool.write_all(&[7u8; 5000]).unwrap();
    let uri = spool.finish().unwrap();
    assert_eq!(fs::read(store.path(&uri).unwrap()).unwrap().len(), 5000);
    let (_dir, store) = capped(10, 10);
    let mut spool = store.spool().unwrap();
    spool.write_all(b"0123456789abcdef").unwrap();
    let uri = spool.finish().unwrap();
    let stored = String::from_utf8(fs::read(store.path(&uri).unwrap()).unwrap()).unwrap();
    assert!(
        stored.starts_with(
            "0123456789\n[ARTIFACT TRUNCATED: kept first 10B + last 0B of 16B; 6B elided"
        ),
        "{stored}"
    );
}

#[test]
fn abandoned_spool_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let spool = store.spool().unwrap();
    let path = spool.path.clone();
    drop(spool);
    assert!(!path.exists());
}
