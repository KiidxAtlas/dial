use super::*;

#[test]
fn concurrent_agents_preserve_disjoint_edits_without_rereads() {
    let dir = tempfile::tempdir().unwrap();
    let a = Workspace::new(dir.path()).unwrap();
    let b = Workspace::new(dir.path()).unwrap();
    let path = a.resolve("shared.txt").unwrap();
    let text = "left=0\nright=0\n";
    fs::write(&path, text).unwrap();
    let tag = a.store().record_all(&path, text);
    b.store().record_all(&path, text);
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let one = scope.spawn(|| {
            start.wait();
            edit(&a, &format!("[shared.txt#{tag}]\nPUT 1.=1:\n+left=1\n"))
        });
        let two = scope.spawn(|| {
            start.wait();
            edit(&b, &format!("[shared.txt#{tag}]\nPUT 2.=2:\n+right=2\n"))
        });
        one.join().unwrap().unwrap();
        two.join().unwrap().unwrap();
    });
    assert_eq!(fs::read_to_string(path).unwrap(), "left=1\nright=2\n");
}

#[test]
fn concurrent_overlap_returns_fresh_context_and_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let a = Workspace::new(dir.path()).unwrap();
    let b = Workspace::new(dir.path()).unwrap();
    let path = a.resolve("shared.txt").unwrap();
    fs::write(&path, "value=0\n").unwrap();
    let tag = a.store().record_all(&path, "value=0\n");
    b.store().record_all(&path, "value=0\n");
    let start = std::sync::Barrier::new(2);
    let (one, two) = std::thread::scope(|scope| {
        let one = scope.spawn(|| {
            start.wait();
            edit(&a, &format!("[shared.txt#{tag}]\nPUT 1.=1:\n+value=1\n"))
        });
        let two = scope.spawn(|| {
            start.wait();
            edit(&b, &format!("[shared.txt#{tag}]\nPUT 1.=1:\n+value=2\n"))
        });
        (one.join().unwrap(), two.join().unwrap())
    });
    assert_ne!(one.is_ok(), two.is_ok());
    let error = one.err().or_else(|| two.err()).unwrap();
    let current = fs::read_to_string(&path).unwrap();
    assert!(
        error.contains(&format!("[shared.txt#{}]", tag_of(&current))),
        "{error}"
    );
    assert!(error.contains(&format!("1:{current}")), "{error}");
    assert!(matches!(current.as_str(), "value=1\n" | "value=2\n"));
}

#[test]
fn all_conflicting_files_are_reported_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let mut patch = String::new();
    for name in ["a.txt", "b.txt", "untouched.txt"] {
        let path = ws.resolve(name).unwrap();
        fs::write(&path, "old\n").unwrap();
        let tag = ws.store().record_all(&path, "old\n");
        patch.push_str(&format!("[{name}#{tag}]\nPUT 1.=1:\n+ours\n"));
        if name != "untouched.txt" {
            fs::write(path, "theirs\n").unwrap();
        }
    }
    let error = edit(&ws, &patch).unwrap_err();
    for name in ["a.txt", "b.txt"] {
        assert!(
            error.contains(&format!("[{name}#{}]", tag_of("theirs\n"))),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(ws.resolve(name).unwrap()).unwrap(),
            "theirs\n"
        );
    }
    assert_eq!(
        fs::read_to_string(ws.resolve("untouched.txt").unwrap()).unwrap(),
        "old\n"
    );
    // Both conflict previews are witnesses; the entire repaired batch needs no extra reads.
    let repaired = patch.replace(&tag_of("old\n"), &tag_of("theirs\n"));
    let repaired = repaired.replace(
        &format!("[untouched.txt#{}]", tag_of("theirs\n")),
        &format!("[untouched.txt#{}]", tag_of("old\n")),
    );
    edit(&ws, &repaired).unwrap();
}

#[test]
fn conflict_preserves_old_witnesses_and_reports_shifted_unaffected_anchors() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("shared.txt").unwrap();
    let before = (1..=100).map(|n| format!("line {n}\n")).collect::<String>();
    fs::write(&path, &before).unwrap();
    let tag = ws.store().record(&path, &before, &[(2, 2), (80, 80)]);
    let current = format!("inserted\n{}", before.replace("line 2\n", "changed\n"));
    fs::write(&path, &current).unwrap();
    let error = edit(
        &ws,
        &format!("[shared.txt#{tag}]\nPUT 2.=2:\n+ours\nPUT 80.=80:\n+other\n"),
    )
    .unwrap_err();
    assert!(error.contains("range 80-80 -> 81-81"), "{error}");
    assert!(
        !error.contains("81:line 80"),
        "preview must stay local: {error}"
    );
    let fresh = tag_of(&current);
    let flags = ws.store().witness_flags(&path, &current);
    assert!(flags[80], "unchanged old witness must survive");
    assert!(
        !flags[49],
        "unseen unrelated lines must not become witnesses"
    );
    edit(
        &ws,
        &format!("[shared.txt#{fresh}]\nPUT 3.=3:\n+ours\nPUT 81.=81:\n+other\n"),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        current
            .replace("changed\n", "ours\n")
            .replace("line 80\n", "other\n")
    );
}

#[test]
fn notices_ignore_unseen_changes_but_detect_later_invalidations() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("shared.txt").unwrap();
    let original = "one\ntwo\nthree\nfour\n";
    fs::write(&path, original).unwrap();
    let tag = ws.store().record(&path, original, &[(2, 3)]);
    fs::write(&path, "head\none\ntwo\nthree\nFOUR\n").unwrap();
    assert!(ws.changed_observations().is_empty());
    fs::write(&path, "head\none\nTWO\nthree\nFOUR\n").unwrap();
    assert_eq!(ws.changed_observations(), vec!["shared.txt"]);
    assert!(ws.changed_observations().is_empty());
    assert_eq!(
        ws.store().snapshot(&path, &tag).unwrap().seen.0,
        vec![(2, 3)]
    );
    fs::write(&path, "one\ntwo\ninserted\nthree\nfour\n").unwrap();
    assert_eq!(ws.changed_observations(), vec!["shared.txt"]);
}

#[test]
fn deleted_or_truncated_targets_invalidate_context_without_panicking() {
    let seen = Seen::from_ranges(&[(2, 3)]);
    assert!(invalidates_seen("one\ntwo\nthree\n", "", &seen));
    assert!(invalidates_seen("one\ntwo\nthree\n", "one\n", &seen));
    assert!(!invalidates_seen(
        "one\ntwo\nthree\n",
        "head\none\ntwo\nthree\n",
        &seen
    ));
}
