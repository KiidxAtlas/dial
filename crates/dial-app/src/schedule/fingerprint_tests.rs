use super::*;
use std::cell::Cell;

fn set(patterns: &[&str]) -> globset::GlobSet {
    watch_set(
        &patterns
            .iter()
            .map(|pattern| (*pattern).into())
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

#[test]
fn multiple_watch_sets_share_one_walk_and_isolate_a_failed_set() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.rs"), "a").unwrap();
    fs::write(dir.path().join("src/b.rs"), "b").unwrap();
    fs::write(dir.path().join("notes.txt"), "notes").unwrap();
    fs::write(dir.path().join("ignored.rs"), "ignored").unwrap();
    fs::write(dir.path().join(".ignore"), "ignored.rs\n").unwrap();
    let broad = set(&["*.rs"]);
    let narrow = set(&["src/a.rs"]);
    let notes = set(&["*.txt"]);
    let ignored = set(&["ignored.rs"]);
    let walks = Cell::new(0);
    // A small injected cap exercises exactly the production per-set limit without 20,001 files.
    let mut results =
        fingerprints_with_walk(dir.path(), &[&broad, &narrow, &notes, &ignored], 1, || {
            walks.set(walks.get() + 1);
            ignore::WalkBuilder::new(dir.path())
                .follow_links(false)
                .build()
        })
        .into_iter();
    assert_eq!(walks.get(), 1, "one directory traversal, not one per entry");
    assert!(
        results
            .next()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("more than 1 matching files")
    );
    assert_eq!(
        results.next().unwrap().unwrap(),
        fingerprint(dir.path(), &["src/a.rs".into()]).unwrap()
    );
    assert_eq!(
        results.next().unwrap().unwrap(),
        fingerprint(dir.path(), &["*.txt".into()]).unwrap()
    );
    assert_eq!(
        results.next().unwrap().unwrap(),
        hash_files(vec![]),
        "ignore rules still apply to every set"
    );
}

#[test]
fn traversal_failure_rejects_all_sets_instead_of_returning_partial_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let rust = set(&["*.rs"]);
    let text = set(&["*.txt"]);
    let results = fingerprints(&missing, &[&rust, &text]);
    assert_eq!(results.len(), 2);
    for result in results {
        assert!(
            format!("{:#}", result.unwrap_err())
                .contains("watch coverage incomplete: traversing project files")
        );
    }
}

#[test]
fn empty_batch_does_not_traverse() {
    let results =
        fingerprints_with_walk(Path::new("unused"), &[], 1, || panic!("no sets to check"));
    assert!(results.is_empty());
}

#[test]
fn selection_keeps_clean_entries_cached_but_checks_dirty_pending_and_missing_baselines() {
    let e = Entry {
        id: "watch".into(),
        prompt: "inspect".into(),
        every_secs: None,
        watch: vec!["*.rs".into()],
        allow: vec![],
        approval: "readonly".into(),
        created: 0,
    };
    let mut scheduler = Scheduler::default();
    let errors = EntryErrors::default();
    let mut dirty = HashSet::new();
    assert!(needs_watch_check(&e, &scheduler, &errors, &dirty, false));
    scheduler.ran(&e, 0, Some([1; 32]));
    assert!(!needs_watch_check(&e, &scheduler, &errors, &dirty, false));
    assert!(needs_watch_check(&e, &scheduler, &errors, &dirty, true));
    dirty.insert(e.id.clone());
    assert!(needs_watch_check(&e, &scheduler, &errors, &dirty, false));
    dirty.clear();
    scheduler.due(&e, 1, || Some([2; 32]));
    assert!(
        needs_watch_check(&e, &scheduler, &errors, &dirty, false),
        "debounce must recheck without events"
    );
    scheduler.ran(&e, 2, None);
    assert!(
        needs_watch_check(&e, &scheduler, &errors, &dirty, false),
        "failed after-run baseline must be retried"
    );
}

#[test]
fn content_rewrite_with_restored_mtime_changes_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    fs::write(&path, "before").unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let before = fingerprint(dir.path(), &["*.rs".into()]).unwrap();
    fs::write(&path, "after!").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    assert_ne!(fingerprint(dir.path(), &["*.rs".into()]).unwrap(), before);
}

#[test]
fn matching_sets_share_content_reads_and_byte_budget() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "content").unwrap();
    let broad = set(&["*.rs"]);
    let narrow = set(&["a.rs"]);
    let reads = Cell::new(0);
    let results = fingerprints_with_reader(
        dir.path(),
        &[&broad, &narrow],
        10,
        7,
        8,
        || ignore::WalkBuilder::new(dir.path()).build(),
        |path, metadata| {
            reads.set(reads.get() + 1);
            content_digest(path, metadata)
        },
    );
    assert_eq!(reads.get(), 1);
    for result in results {
        assert_eq!(
            result.unwrap(),
            fingerprint(dir.path(), &["*.rs".into()]).unwrap()
        );
    }
}

#[test]
fn content_coverage_limits_and_read_errors_are_explicit_and_isolated() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "content").unwrap();
    let rust = set(&["*.rs"]);
    let absent = set(&["*.txt"]);
    for (file_bytes, total_bytes, read_error, expected) in [
        (6, 8, false, "exceeds 6 content bytes"),
        (7, 7, false, "shared content byte budget exhausted"),
        (7, 8, true, "injected read failure"),
    ] {
        let mut results = fingerprints_with_reader(
            dir.path(),
            &[&rust, &absent],
            10,
            file_bytes,
            total_bytes,
            || ignore::WalkBuilder::new(dir.path()).build(),
            |path, metadata| {
                if read_error {
                    bail!("injected read failure");
                }
                content_digest(path, metadata)
            },
        )
        .into_iter();
        let error = format!("{:#}", results.next().unwrap().unwrap_err());
        assert!(
            error.contains("watch coverage incomplete") && error.contains(expected),
            "{error}"
        );
        assert_eq!(results.next().unwrap().unwrap(), hash_files(vec![]));
    }
}
