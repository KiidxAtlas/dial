use super::*;

fn entry(every: Option<u64>, watch: &[&str]) -> Entry {
    Entry {
        id: "s1".into(),
        prompt: "p".into(),
        every_secs: every,
        watch: watch.iter().map(|w| (*w).into()).collect(),
        allow: vec![],
        approval: "readonly".into(),
        created: 1000,
    }
}

#[test]
fn creation_normalizes_allow_lists_without_broadening_permissions() {
    let create = |allow: Vec<String>, yes| {
        Entry::new(
            Some("s1".into()),
            "inspect".into(),
            Some("1h"),
            vec![],
            allow,
            yes,
        )
        .unwrap()
    };
    let repeated = create(vec!["read".into(), "edit".into()], false);
    let comma = create(vec!["read, edit".into(), "read".into()], false);
    assert_eq!(repeated.allow, comma.allow);
    assert_eq!(comma.approval, "readonly");
    assert_eq!(comma.every_secs, Some(3600));
    assert_eq!(comma.access_description(), "read-only + read,edit");
    assert_eq!(create(vec![], false).access_description(), "read-only");
    assert_eq!(
        create(vec!["edit".into()], true).access_description(),
        "all tools approved"
    );
    for bad in ["", "read,", ",edit", "write,bash --yes"] {
        assert!(
            Entry::new(
                None,
                "inspect".into(),
                Some("1h"),
                vec![],
                vec![bad.into()],
                false
            )
            .is_err()
        );
    }
}

#[test]
fn durations_parse_and_reject_garbage() {
    assert_eq!(parse_duration("30s").unwrap(), 30);
    assert_eq!(parse_duration("5m").unwrap(), 300);
    assert_eq!(parse_duration("2h").unwrap(), 7200);
    assert_eq!(parse_duration("1d").unwrap(), 86_400);
    assert_eq!(parse_duration("45").unwrap(), 45);
    for bad in ["", "m", "5x", "-5m", "1.5h"] {
        assert!(parse_duration(bad).is_err(), "{bad}");
    }
}

#[test]
fn entries_need_a_trigger_and_a_sane_interval() {
    assert!(entry(Some(60), &[]).validate().is_ok());
    assert!(entry(None, &["src/**/*.rs"]).validate().is_ok());
    assert!(entry(None, &[]).validate().is_err());
    assert!(entry(Some(5), &[]).validate().is_err());
    assert!(entry(None, &["["]).validate().is_err());
    assert!(
        Entry {
            approval: "ask".into(),
            ..entry(Some(60), &[])
        }
        .validate()
        .is_err()
    );
}

#[test]
fn intervals_count_from_creation_then_from_the_last_run() {
    let mut s = Scheduler::default();
    let e = entry(Some(100), &[]);
    assert_eq!(s.due(&e, 1099, || None), None);
    assert_eq!(s.due(&e, 1100, || None), Some(Reason::Interval));
    s.ran(&e, 1100, None);
    assert_eq!(s.due(&e, 1199, || None), None);
    assert_eq!(s.due(&e, 1200, || None), Some(Reason::Interval));
}

#[test]
fn file_changes_must_settle_and_a_runs_own_edits_do_not_retrigger() {
    let mut s = Scheduler::default();
    let e = entry(None, &["*.rs"]);
    let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
    assert_eq!(
        s.due(&e, 10, || Some(a)),
        None,
        "first sight is the baseline"
    );
    assert_eq!(s.due(&e, 11, || Some(a)), None);
    assert_eq!(s.due(&e, 12, || Some(b)), None, "change seen, debouncing");
    assert_eq!(
        s.due(&e, 13, || Some(c)),
        None,
        "still changing restarts the wait"
    );
    assert_eq!(s.due(&e, 15, || Some(c)), None);
    assert_eq!(s.due(&e, 16, || Some(c)), Some(Reason::Changed));
    s.ran(&e, 17, Some(c));
    assert_eq!(
        s.due(&e, 30, || Some(c)),
        None,
        "edits made by the run itself are the new baseline"
    );
}

#[test]
fn fingerprints_follow_matching_files_only_and_respect_ignores() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.rs"), "1").unwrap();
    fs::write(dir.path().join("notes.txt"), "1").unwrap();
    let pats = vec!["src/**/*.rs".to_owned()];
    let before = fingerprint(dir.path(), &pats).unwrap();
    fs::write(dir.path().join("notes.txt"), "22").unwrap();
    assert_eq!(
        fingerprint(dir.path(), &pats).unwrap(),
        before,
        "unmatched file"
    );
    fs::write(dir.path().join("src/a.rs"), "22").unwrap();
    assert_ne!(fingerprint(dir.path(), &pats).unwrap(), before);
}

#[test]
fn store_adds_numbers_and_removes() {
    let layout_dir = tempfile::tempdir().unwrap();
    let store = Store {
        dir: layout_dir.path().to_path_buf(),
    };
    let mut e = entry(Some(60), &[]);
    e.id.clear();
    assert_eq!(store.add(e.clone()).unwrap(), "s1");
    assert_eq!(store.add(e.clone()).unwrap(), "s2");
    store.remove("s1").unwrap();
    assert_eq!(store.load().unwrap().len(), 1);
    assert!(store.remove("s1").is_err());
}

#[test]
fn one_invalid_schedule_does_not_hide_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store {
        dir: dir.path().to_path_buf(),
    };
    fs::write(
        store.path(),
        "[[entries]]\nid='good'\nprompt='p'\nevery_secs=60\napproval='readonly'\ncreated=1\n\
             [[entries]]\nid='bad'\nprompt='p'\nwatch=['[']\napproval='readonly'\ncreated=1\n\
             [[entries]]\nid='good'\nprompt='dup'\nevery_secs=60\napproval='readonly'\ncreated=1\n\
             [[entries]]\nid='typo'\nprompt='p'\nevery_secs=60\napproval='readonly'\ncreated=1\nbogus=1\n",
    )
    .unwrap();
    assert!(
        store.load().is_err(),
        "strict loading still refuses the file for add/remove"
    );
    let (entries, problems) = store.load_lenient().unwrap();
    assert_eq!(
        entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["good"]
    );
    assert_eq!(problems.len(), 3, "{problems:?}");
}

#[test]
fn watch_errors_are_recorded_per_entry_and_retried() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store {
        dir: dir.path().to_path_buf(),
    };
    let mut errors = EntryErrors::default();
    errors.fail(&store, "a", "watch rejected", &anyhow::anyhow!("boom"));
    assert!(errors.current.contains_key("a") && !errors.current.contains_key("b"));
    assert!(!errors.retry_due("a"));
    assert!(
        fs::read_to_string(store.log_path("a"))
            .unwrap()
            .contains("watch rejected: boom")
    );
    errors.recovered("a");
    assert!(errors.current.is_empty());
}

#[test]
fn filesystem_events_report_relative_changed_paths() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    let Ok(events) = FsEvents::start(dir.path()) else {
        return;
    };
    // Let the backend arm (FSEvents delivers with latency) and discard setup noise.
    std::thread::sleep(Duration::from_millis(300));
    let _ = events.drain();
    fs::write(dir.path().join("src/a.rs"), "1").unwrap();
    let set = watch_set(&["src/**/*.rs".to_owned()]).unwrap();
    let started = Instant::now();
    let stop = std::sync::atomic::AtomicBool::new(false);
    loop {
        events.wait(Instant::now() + Duration::from_millis(500), &stop);
        match events.drain() {
            Changes::Everything => break,
            Changes::Paths(paths) if paths.iter().any(|path| set.is_match(path)) => break,
            Changes::Paths(_) => {}
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no filesystem event for src/a.rs"
        );
    }
}

#[test]
fn persisted_watch_restart_detects_changes_instead_of_rebaselining() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().join("dial")).unwrap();
    let store = Store::new(&layout).unwrap();
    let e = entry(None, &["*.rs"]);
    let mut scheduler = Scheduler::default();
    scheduler.reconcile(std::slice::from_ref(&e));
    scheduler.ran(&e, 10, Some([1; 32]));
    store.save_state(&scheduler);
    let mut restored = store.load_scheduler();
    restored.reconcile(std::slice::from_ref(&e));
    assert_eq!(restored.due(&e, 20, || Some([2; 32])), None);
    assert_eq!(
        restored.due(&e, 20 + DEBOUNCE_SECS, || Some([2; 32])),
        Some(Reason::Changed)
    );
    // A definition edited while stopped must not inherit its old baseline.
    let mut edited = e.clone();
    edited.watch = vec!["*.toml".into()];
    let mut restored = store.load_scheduler();
    restored.reconcile(std::slice::from_ref(&edited));
    assert_eq!(restored.due(&edited, 30, || Some([3; 32])), None);
    assert_eq!(restored.due(&edited, 40, || Some([3; 32])), None);
    // Live edits have the same invalidation semantics as edits between restarts.
    restored.reconcile(std::slice::from_ref(&e));
    assert_eq!(restored.due(&e, 50, || Some([1; 32])), None);
}

#[test]
fn legacy_fingerprint_state_survives_initial_definition_load_and_migrates() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().join("dial")).unwrap();
    let store = Store::new(&layout).unwrap();
    let e = entry(None, &["*.rs"]);
    let legacy = HashMap::from([(e.id.clone(), [1u8; 32])]);
    store
        .write_atomic(store.baseline_path(), &toml::to_string(&legacy).unwrap())
        .unwrap();
    let mut restored = store.load_scheduler();
    restored.reconcile(std::slice::from_ref(&e));
    assert_eq!(restored.due(&e, 10, || Some([2; 32])), None);
    assert_eq!(
        restored.due(&e, 10 + DEBOUNCE_SECS, || Some([2; 32])),
        Some(Reason::Changed)
    );
    store.save_state(&restored);
    assert!(
        matches!(store.load_baselines().get(&e.id), Some(SavedBaseline::Bound { definition, .. }) if definition == &e)
    );
}
