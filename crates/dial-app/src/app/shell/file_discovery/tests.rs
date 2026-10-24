use super::*;

#[test]
fn discovery_cancels_without_publishing_partial_results() {
    let root = tempfile::tempdir().unwrap();
    assert!(walk(root.path(), || true).is_none());
}

#[test]
fn discovery_requests_do_not_wait_for_traversal_and_latest_root_wins() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    std::fs::write(b.path().join("current.rs"), "").unwrap();
    let mut files = Files::new();
    let start = Instant::now();
    for _ in 0..100 {
        files.request(a.path());
        files.request(b.path());
    }
    assert!(start.elapsed() < Duration::from_secs(1));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(listing) = files.poll() {
            assert_eq!(listing.paths, vec!["current.rs"]);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn stopping_discovery_cancels_the_worker_without_joining_on_the_ui_thread() {
    let mut files = Files::new();
    let root = tempfile::tempdir().unwrap();
    files.request(root.path());
    let start = Instant::now();
    files.stop();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(files.shared.stop.load(Ordering::Acquire));
    assert!(files.poll().is_none());
}

#[test]
fn over_cap_discovery_is_bounded_and_disclosed() {
    let root = tempfile::tempdir().unwrap();
    // Directory entries count too: cap applies even when no files are offered.
    for i in 0..4 {
        std::fs::create_dir(root.path().join(i.to_string())).unwrap();
    }
    let listing = walk_bounded(root.path(), || false, 2).unwrap();
    assert!(listing.paths.len() <= 2);
    assert!(listing.diagnostic.is_some());
}
