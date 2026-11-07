use super::*;

fn layout(dir: &tempfile::TempDir) -> Layout {
    let root = dir.path().join("project");
    fs::create_dir_all(&root).unwrap();
    Layout::with_user_dir(&root, dir.path().join("private")).unwrap()
}

#[test]
fn independent_future_cursors_drain_completion_and_reject_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(&dir);
    let mut capture = Capture::start(&layout, "web").unwrap();
    capture.append(b"old error\n").unwrap();
    let mut first = Reader::attach(&layout, "web").unwrap();
    let mut second = Reader::attach(&layout, "web").unwrap();
    assert!(Capture::start(&layout, "web").is_err());
    capture.append(b"new error\n").unwrap();
    assert_eq!(first.poll(4).unwrap(), (b"new ".to_vec(), false));
    assert_eq!(second.poll(100).unwrap(), (b"new error\n".to_vec(), false));
    capture.finish(None).unwrap();
    drop(capture);
    assert_eq!(first.poll(100).unwrap(), (b"error\n".to_vec(), true));
    assert!(Reader::attach(&layout, "web").is_err());
    let _replacement = Capture::start(&layout, "web").unwrap();
    assert!(
        first
            .poll(100)
            .unwrap_err()
            .to_string()
            .contains("restarted")
    );
}

#[test]
fn bounded_output_reports_loss_and_storage_or_lifetime_failure() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(&dir);
    let mut capture = Capture::start(&layout, "web").unwrap();
    let mut reader = Reader::attach(&layout, "web").unwrap();
    capture.append(&vec![b'x'; CAPACITY + 1]).unwrap();
    assert_eq!(capture.snapshot.bytes.len(), CAPACITY);
    assert!(
        reader
            .poll(4096)
            .unwrap_err()
            .to_string()
            .contains("overrun")
    );
    let mut reader = Reader::attach(&layout, "web").unwrap();
    drop(capture); // same PID is still live, but the lifetime lock is not held
    assert!(
        reader
            .poll(4096)
            .unwrap_err()
            .to_string()
            .contains("disappeared")
    );
    assert!(Reader::attach(&layout, "web").is_err());
    let mut capture = Capture::start(&layout, "web").unwrap();
    let mut reader = Reader::attach(&layout, "web").unwrap();
    capture.finish(Some("disk capture failure")).unwrap();
    assert!(
        reader
            .poll(4096)
            .unwrap_err()
            .to_string()
            .contains("disk capture failure")
    );
    fs::remove_file(&capture.path).unwrap();
    assert!(reader.poll(4096).is_err());
}

#[test]
fn names_projects_and_private_paths_are_confined() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(&dir);
    for name in ["", ".", "..", "../web", "/tmp/web", "a b", "🦀"] {
        assert!(Capture::start(&layout, name).is_err(), "{name}");
    }
    let _capture = Capture::start(&layout, "web").unwrap();
    let other = dir.path().join("other");
    fs::create_dir(&other).unwrap();
    let other = Layout::with_user_dir(&other, layout.user_dir.clone()).unwrap();
    assert!(Reader::attach(&other, "web").is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        assert_eq!(
            fs::metadata(layout.state_dir.join("runs/web.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let path = layout.state_dir.join("runs/evil.json");
        symlink(layout.state_dir.join("runs/web.json"), &path).unwrap();
        assert!(Reader::attach(&layout, "evil").is_err());
    }
}

#[test]
fn slot_reuse_does_not_revive_a_disappeared_generation() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(&dir);
    let capture = Capture::start(&layout, "old").unwrap();
    let path = capture.path.clone();
    let snapshot = fs::read(&path).unwrap();
    let mut reader = Reader::attach(&layout, "old").unwrap();
    let slot = capture.snapshot.slot;
    drop(capture);
    let replacement = Capture::start(&layout, "new").unwrap();
    assert_eq!(replacement.snapshot.slot, slot);
    // Simulate a stale state record surviving cleanup while its slot is held by another run.
    atomic::write_private(&path, &snapshot).unwrap();
    assert!(reader.poll(100).is_err());
    assert!(Reader::attach(&layout, "old").is_err());
}

#[test]
fn live_run_capacity_and_publication_failure_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(&dir);
    let mut captures: Vec<_> = (0..32)
        .map(|n| Capture::start(&layout, &format!("run-{n}")).unwrap())
        .collect();
    assert!(Capture::start(&layout, "overflow").is_err());
    let mut released = captures.pop().unwrap();
    released.finish(None).unwrap();
    drop(released);
    let mut next = Capture::start(&layout, "next").unwrap();
    fs::remove_file(&next.path).unwrap();
    fs::create_dir(&next.path).unwrap();
    assert!(next.append(b"must not silently lose output").is_err());
}

#[test]
fn buffered_output_is_published_once_due_and_completion_publishes_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(&dir);
    let mut capture = Capture::start(&layout, "web").unwrap();
    let mut reader = Reader::attach(&layout, "web").unwrap();
    // Deterministic clock: a publication "in the future" is never due, one interval ago is.
    capture.published = Instant::now() + Duration::from_secs(3600);
    capture.append_buffered(b"first ").unwrap();
    capture.append_buffered(b"second").unwrap();
    capture.publish_due().unwrap();
    assert_eq!(reader.poll(100).unwrap(), (Vec::new(), false));
    capture.published = Instant::now() - PUBLISH_INTERVAL;
    capture.publish_due().unwrap();
    assert_eq!(reader.poll(100).unwrap(), (b"first second".to_vec(), false));
    capture.published = Instant::now() + Duration::from_secs(3600);
    capture.append_buffered(b" tail").unwrap();
    capture.finish(None).unwrap();
    assert_eq!(reader.poll(100).unwrap(), (b" tail".to_vec(), true));
    let saved = String::from_utf8(fs::read(&capture.path).unwrap()).unwrap();
    assert!(
        saved.contains(r#""bytes":"Zmlyc3Qgc2Vjb25kIHRhaWw=""#),
        "{saved}"
    );
}

#[test]
fn version_one_snapshots_with_numeric_bytes_still_read() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout(&dir);
    let capture = Capture::start(&layout, "web").unwrap();
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(&capture.path).unwrap()).unwrap();
    legacy["version"] = 1.into();
    legacy["total"] = 2.into();
    legacy["bytes"] = serde_json::json!([104, 105]);
    atomic::write_private(&capture.path, &serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(read(&capture.path).unwrap().bytes, b"hi");
}
