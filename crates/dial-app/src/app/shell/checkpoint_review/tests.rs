use super::*;

fn file(path: &str, writers: &[&str], later_turns: &[u64]) -> ReviewFile {
    ReviewFile {
        path: path.into(),
        writers: writers.iter().map(|writer| (*writer).into()).collect(),
        later_turns: later_turns.to_vec(),
    }
}

fn inventory() -> TurnReview {
    TurnReview {
        turn: 7,
        label: "edit".into(),
        files: vec![
            file("a.rs", &["main"], &[]),
            file("b.rs", &["main", "worker"], &[]),
            file("c.rs", &["worker"], &[8]),
        ],
        skipped: vec![],
    }
}

#[test]
fn next_previous_wrap_and_restore_selection_after_serialization() {
    let review = inventory();
    let mut browser = ReviewBrowser::default();
    let files = browser.visible(&review);
    for (next, expected) in [
        (true, "b.rs"),
        (true, "c.rs"),
        (true, "a.rs"),
        (false, "c.rs"),
    ] {
        assert_eq!(browser.adjacent(7, &files, next).as_deref(), Some(expected));
    }
    browser
        .reviewed
        .insert(ReviewBrowser::key(7, "a.rs"), "version-a".into());
    browser
        .reviewed
        .insert(ReviewBrowser::key(7, "b.rs"), "version-b".into());
    let restored: ReviewBrowser =
        serde_json::from_value(serde_json::to_value(&browser).unwrap()).unwrap();
    assert_eq!(restored.selected(7, &files), 2);
    assert_eq!(restored.reviewed.len(), 2);
    assert!(browser.adjacent(7, &[], true).is_none());
}

#[test]
fn writer_and_overlap_filters_combine_without_hiding_multiwriter_badges() {
    let review = inventory();
    for (owner, overlap_only, expected) in [
        (None, false, vec!["a.rs", "b.rs", "c.rs"]),
        (Some("main"), false, vec!["a.rs", "b.rs"]),
        (Some("main"), true, vec!["b.rs"]),
        (Some("worker"), true, vec!["b.rs", "c.rs"]),
        (Some("missing"), false, vec![]),
    ] {
        let browser = ReviewBrowser {
            owner: owner.map(str::to_owned),
            overlap_only,
            ..Default::default()
        };
        assert_eq!(
            browser
                .visible(&review)
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
    let label = file_label(&review.files[1], true);
    assert!(label.contains("[main, worker]"));
    assert!(label.contains("overlap:"));
    assert!(label.contains("reviewed"));
}

#[test]
fn latest_changed_turn_skips_empty_turn_and_preview_identity_rejects_later_edits() {
    let state = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(project.path()).unwrap();
    let checkpoints = Checkpoints::new(state.path().join("checkpoints"), root.clone()).unwrap();
    assert_eq!(latest_changed(&checkpoints), None);
    let path = root.join("file.rs");
    std::fs::write(&path, "before\n").unwrap();
    let first = checkpoints.begin_turn("first").unwrap();
    checkpoints.before_write_as(&path, "main").unwrap();
    std::fs::write(&path, "after\n").unwrap();
    checkpoints.begin_turn("empty").unwrap();
    assert_eq!(latest_changed(&checkpoints), Some(first));
    let (_, token) = checkpoints.review_file(first, "file.rs").unwrap();
    validate_preview(&checkpoints, first, "file.rs", &token).unwrap();
    std::fs::write(&path, "user edit\n").unwrap();
    assert!(validate_preview(&checkpoints, first, "file.rs", &token).is_err());
    assert!(
        checkpoints
            .restore_files(first, &[("file.rs".into(), token)], true)
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "user edit\n");
}
