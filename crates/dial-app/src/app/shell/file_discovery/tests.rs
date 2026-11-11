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
    let listing = walk_bounded(
        root.path(),
        || false,
        Limits {
            entries: 2,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(listing.paths.len() <= 2);
    assert!(listing.diagnostic.is_some());
}

#[test]
fn an_unchanged_rescan_is_not_handed_out_again() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
    let mut files = Files::new();
    let wait = |files: &mut Files| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(listing) = files.poll() {
                return Some(listing);
            }
            if !files.running || Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    files.request(dir.path());
    assert!(wait(&mut files).is_some(), "the first listing is delivered");
    // The periodic rescan finds the same files: nothing new reaches the composer.
    files.requested = Instant::now() - CACHE_TTL - Duration::from_secs(1);
    files.request(dir.path());
    assert!(wait(&mut files).is_none());
    // A new file is a change again.
    std::fs::write(dir.path().join("b.rs"), "fn b() {}").unwrap();
    files.requested = Instant::now() - CACHE_TTL - Duration::from_secs(1);
    files.request(dir.path());
    assert!(wait(&mut files).is_some_and(|listing| listing.paths.len() == 2));
}

fn publish(files: &mut Files, listing: Listing) -> Option<Listing> {
    let generation = files.shared.generation.load(Ordering::Acquire);
    *files.result.lock().unwrap() = Some((generation, listing));
    files.poll()
}

#[test]
fn changed_partial_listings_do_not_repeat_the_same_warning() {
    let root = tempfile::tempdir().unwrap();
    let mut files = Files::new();
    let first = walk_bounded(
        root.path(),
        || false,
        Limits {
            entries: 0,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(publish(&mut files, first).unwrap().diagnostic.is_some());
    let mut second = walk_bounded(
        root.path(),
        || false,
        Limits {
            entries: 0,
            ..Limits::default()
        },
    )
    .unwrap();
    second.paths.push("new.rs".into());
    let second = publish(&mut files, second).unwrap();
    assert_eq!(second.paths, ["new.rs"]);
    assert!(
        second.diagnostic.is_none(),
        "file changes must not repeat a warning"
    );
}

#[test]
fn entry_limit_warning_names_the_actual_limit() {
    let root = tempfile::tempdir().unwrap();
    let listing = walk_bounded(
        root.path(),
        || false,
        Limits {
            entries: 0,
            ..Limits::default()
        },
    )
    .unwrap();
    let diagnostic = listing.diagnostic.unwrap();
    assert!(diagnostic.contains("0 entries"), "{diagnostic}");
    assert!(!diagnostic.contains("unreadable"), "{diagnostic}");
}

#[test]
fn unreadable_root_warning_is_not_reported_as_a_limit() {
    let root = tempfile::tempdir().unwrap();
    let listing = walk(&root.path().join("missing"), || false).unwrap();
    let diagnostic = listing.diagnostic.unwrap();
    assert!(diagnostic.contains("could not be read"), "{diagnostic}");
    assert!(!diagnostic.contains("limit"), "{diagnostic}");
}

#[test]
fn byte_budget_counts_path_names_and_allows_an_exact_fit() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "content does not count").unwrap();
    let exact = walk_bounded(
        root.path(),
        || false,
        Limits {
            bytes: 4,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(exact.paths, ["a.rs"]);
    assert!(exact.diagnostic.is_none());
    let capped = walk_bounded(
        root.path(),
        || false,
        Limits {
            bytes: 3,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(capped.paths.is_empty());
    assert_eq!(capped.causes, [Cause::Bytes(3)]);
    let message = capped.diagnostic.unwrap();
    assert!(
        message.contains("path names") && message.contains("3-byte"),
        "{message}"
    );
}

#[test]
fn time_budget_is_bounded_and_disclosed() {
    let root = tempfile::tempdir().unwrap();
    let listing = walk_bounded(
        root.path(),
        || false,
        Limits {
            time: Duration::ZERO,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(listing.paths.is_empty());
    assert_eq!(listing.causes, [Cause::Time(Duration::ZERO)]);
    assert!(listing.diagnostic.unwrap().contains("0-second time budget"));
}

#[test]
fn slow_background_walk_can_continue_past_the_old_two_second_budget() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "").unwrap();
    let delayed = std::cell::Cell::new(false);
    let listing = walk(root.path(), || {
        if !delayed.replace(true) {
            std::thread::sleep(Duration::from_millis(2100));
        }
        false
    })
    .unwrap();
    assert_eq!(listing.paths, ["a.rs"]);
    assert!(
        listing.diagnostic.is_none(),
        "background scan should survive the old deadline"
    );
}

#[test]
fn ignored_generated_directories_do_not_consume_the_entry_budget() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".gitignore"), "generated/\n").unwrap();
    std::fs::create_dir(root.path().join("generated")).unwrap();
    for index in 0..10 {
        std::fs::write(root.path().join(format!("generated/{index}.rs")), "").unwrap();
    }
    std::fs::write(root.path().join("a.rs"), "").unwrap();
    let listing = walk_bounded(
        root.path(),
        || false,
        Limits {
            entries: 3,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(listing.paths, ["a.rs"]);
    assert!(listing.diagnostic.is_none());
}

fn listing_with(causes: Vec<Cause>) -> Listing {
    Listing {
        paths: vec!["a.rs".into()],
        diagnostic: diagnostic(&causes),
        causes,
    }
}

#[test]
fn new_causes_are_disclosed_without_repeating_previous_causes() {
    let mut files = Files::new();
    assert!(
        publish(&mut files, listing_with(vec![Cause::Unreadable]))
            .unwrap()
            .diagnostic
            .is_some()
    );
    let next = publish(
        &mut files,
        listing_with(vec![Cause::Unreadable, Cause::Time(WALK_BUDGET)]),
    )
    .unwrap();
    let message = next.diagnostic.unwrap();
    assert!(message.contains("10-second time budget"), "{message}");
    assert!(!message.contains("could not be read"), "{message}");
    // A complete scan followed by recurrence still must not spam the same cause.
    assert!(
        publish(&mut files, listing_with(vec![]))
            .unwrap()
            .diagnostic
            .is_none()
    );
    assert!(
        publish(&mut files, listing_with(vec![Cause::Unreadable]))
            .unwrap()
            .diagnostic
            .is_none()
    );
}

#[test]
fn switching_project_roots_resets_warning_deduplication() {
    let root = tempfile::tempdir().unwrap();
    let mut files = Files::new();
    assert!(
        publish(&mut files, listing_with(vec![Cause::Unreadable]))
            .unwrap()
            .diagnostic
            .is_some()
    );
    files.request(root.path());
    assert!(
        files.warned.is_empty(),
        "new project must have its own warnings"
    );
    files.stop();
    assert!(
        publish(&mut files, listing_with(vec![Cause::Unreadable]))
            .unwrap()
            .diagnostic
            .is_some()
    );
}

#[test]
fn stale_results_do_not_suppress_current_generation_warnings() {
    let mut files = Files::new();
    files.shared.generation.store(1, Ordering::Release);
    *files.result.lock().unwrap() = Some((0, listing_with(vec![Cause::Unreadable])));
    assert!(files.poll().is_none());
    assert!(
        publish(&mut files, listing_with(vec![Cause::Unreadable]))
            .unwrap()
            .diagnostic
            .is_some()
    );
}

#[test]
fn cancellation_during_traversal_discards_all_results() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "").unwrap();
    let calls = std::cell::Cell::new(0);
    assert!(
        walk(root.path(), || {
            calls.set(calls.get() + 1);
            calls.get() > 1
        })
        .is_none()
    );
}
