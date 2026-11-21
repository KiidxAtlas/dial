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
fn scheduled_runs_never_take_the_yolo_mode() {
    // Scheduled runs are unattended: only the read-only and Full auto policies are valid for them.
    let mut scheduled = entry(Some(3600), &[]);
    assert!(scheduled.validate().is_ok());
    scheduled.approval = "auto".into();
    assert!(scheduled.validate().is_ok());
    scheduled.approval = "yolo".into();
    assert!(scheduled.validate().is_err());
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
    s.ran(&e, 1100, None, None);
    assert_eq!(s.due(&e, 1199, || None), None);
    assert_eq!(s.due(&e, 1200, || None), Some(Reason::Interval));
}

#[test]
fn a_runs_own_edits_get_one_followup_then_settle() {
    let mut s = Scheduler::default();
    let e = entry(None, &["*.rs"]);
    let (a, b, c, d) = ([1u8; 32], [2u8; 32], [3u8; 32], [4u8; 32]);
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
    s.ran(&e, 17, Some(c), Some(d));
    assert_eq!(
        s.due(&e, 18, || Some(d)),
        None,
        "the run's own edit waits to settle before its follow-up"
    );
    assert_eq!(
        s.due(&e, 17 + DEBOUNCE_SECS, || Some(d)),
        Some(Reason::Changed),
        "a change made while the run was active gets exactly one follow-up"
    );
    s.ran(&e, 30, Some(d), Some(d));
    assert_eq!(
        s.due(&e, 60, || Some(d)),
        None,
        "a follow-up that changes nothing settles the chain"
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
    scheduler.ran(&e, 10, Some([1; 32]), Some([1; 32]));
    store.save_state(&scheduler).unwrap();
    let mut restored = store.load_scheduler().unwrap();
    restored.reconcile(std::slice::from_ref(&e));
    assert_eq!(restored.due(&e, 20, || Some([2; 32])), None);
    assert_eq!(
        restored.due(&e, 20 + DEBOUNCE_SECS, || Some([2; 32])),
        Some(Reason::Changed)
    );
    // A definition edited while stopped must not inherit its old baseline.
    let mut edited = e.clone();
    edited.watch = vec!["*.toml".into()];
    let mut restored = store.load_scheduler().unwrap();
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
    let mut restored = store.load_scheduler().unwrap();
    restored.reconcile(std::slice::from_ref(&e));
    assert_eq!(restored.due(&e, 10, || Some([2; 32])), None);
    assert_eq!(
        restored.due(&e, 10 + DEBOUNCE_SECS, || Some([2; 32])),
        Some(Reason::Changed)
    );
    store.save_state(&restored).unwrap();
    assert!(
        matches!(store.load_runtime().unwrap().baselines.get(&e.id), Some(SavedBaseline::Bound { definition, .. }) if definition == &e)
    );
}

#[test]
fn failed_starts_preserve_watch_and_interval_triggers_with_capped_backoff() {
    for every in [None, Some(100)] {
        let e = entry(every, &["*.rs"]);
        let mut scheduler = Scheduler::default();
        scheduler.ran(&e, 1000, Some([1; 32]), Some([1; 32]));
        scheduler.due(&e, 1001, || Some([2; 32]));
        let mut at = 1100;
        assert!(scheduler.due(&e, at, || Some([2; 32])).is_some());
        for attempt in 0..10 {
            scheduler.start_failed(&e, at);
            assert_eq!(
                scheduler.last_run[&e.id], 1000,
                "a failed start is not a run"
            );
            assert_eq!(scheduler.watches[&e.id].baseline, Some([1; 32]));
            let delay = (START_RETRY_SECS << attempt).min(MAX_START_RETRY_SECS);
            assert_eq!(scheduler.due(&e, at + delay - 1, || Some([2; 32])), None);
            at += delay;
            assert!(scheduler.due(&e, at, || Some([2; 32])).is_some());
        }
        scheduler.ran(&e, at, Some([2; 32]), Some([2; 32]));
        assert_eq!(scheduler.due(&e, at + 1, || Some([2; 32])), None);
        scheduler.start_failed(&e, at + 2);
        assert_eq!(
            scheduler.start_retries[&e.id].1, START_RETRY_SECS,
            "completion resets launch backoff"
        );
    }
}

#[test]
fn event_bursts_never_block_the_callback_and_overflow_requests_full_scan() {
    let root = PathBuf::from("/project");
    let (sink, inbox) = EventInbox::new(vec![root.clone()]);
    let (done, completed) = std::sync::mpsc::channel();
    let producer = std::thread::spawn(move || {
        for i in 0..MAX_EVENT_PATHS * 10 {
            sink.push(Ok(
                notify::Event::new(notify::EventKind::Any).add_path(root.join(format!("{i}.rs")))
            ));
        }
        done.send(()).unwrap();
    });
    completed
        .recv_timeout(Duration::from_secs(2))
        .expect("callback must not wait for drain");
    producer.join().unwrap();
    assert!(matches!(inbox.drain(), Changes::Everything));
    assert!(matches!(inbox.drain(), Changes::Paths(paths) if paths.is_empty()));
}

#[test]
fn event_inbox_coalesces_paths_ignores_reads_and_preserves_coverage_signals() {
    let root = PathBuf::from("/project");
    let canonical = PathBuf::from("/canonical/project");
    let (sink, inbox) = EventInbox::new(vec![root.clone(), canonical.clone()]);
    for _ in 0..MAX_EVENT_PATHS * 2 {
        sink.push(Ok(notify::Event::new(notify::EventKind::Access(
            notify::event::AccessKind::Read,
        ))
        .add_path(root.join("a.rs"))));
    }
    for path in [root.join("a.rs"), root.join("a.rs"), canonical.join("b.rs")] {
        sink.push(Ok(notify::Event::new(notify::EventKind::Any).add_path(path)));
    }
    let Changes::Paths(paths) = inbox.drain() else {
        panic!("read access is not overflow");
    };
    assert_eq!(
        paths.into_iter().collect::<HashSet<_>>(),
        HashSet::from([PathBuf::from("a.rs"), PathBuf::from("b.rs")])
    );
    for event in [
        Ok(notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)),
        Err(notify::Error::generic("watch failed")),
        Ok(notify::Event::new(notify::EventKind::Any).add_path(PathBuf::from("/foreign/file"))),
    ] {
        sink.push(event);
        assert!(matches!(inbox.drain(), Changes::Everything));
    }
}

#[test]
fn one_large_event_and_wait_batches_have_bounded_retention() {
    let root = PathBuf::from("/project");
    let (sink, inbox) = EventInbox::new(vec![root.clone()]);
    let mut event = notify::Event::new(notify::EventKind::Any);
    event.paths = (0..MAX_EVENT_PATHS * 2)
        .map(|i| root.join(i.to_string()))
        .collect();
    sink.push(Ok(event));
    inbox.wait(
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    );
    assert!(inbox.early.borrow().len() <= MAX_EVENT_PATHS);
    assert!(matches!(inbox.drain(), Changes::Everything));
}

#[test]
fn structural_events_rescan_file_globs_even_without_descendant_notifications() {
    use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
    let root = PathBuf::from("/workspace");
    let (sink, inbox) = EventInbox::new(vec![root.clone()]);
    let set = watch_set(&["src/**/*.rs".into()]).unwrap();
    assert!(!set.is_match("src/generated"));
    for kind in [
        notify::EventKind::Create(CreateKind::Folder),
        notify::EventKind::Remove(RemoveKind::Folder),
        notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
    ] {
        sink.push(Ok(
            notify::Event::new(kind).add_path(root.join("src/generated"))
        ));
        assert!(matches!(inbox.drain(), Changes::Everything), "{kind:?}");
    }
}

#[test]
fn damaged_schedule_history_is_an_error_not_an_empty_history() {
    // An empty history re-runs every interval schedule and re-baselines every watch, so a damaged
    // or unreadable state file must stop the load. Errors name the file but never echo its text,
    // which can contain prompts.
    let secret = "SECRET-PROMPT-TEXT";
    for file in [
        "schedule-state.toml",
        "schedule-watch.toml",
        "schedule-runtime.toml",
    ] {
        for damage in [Some(format!("prompt = \"{secret}\"\n[[ broken")), None] {
            let root = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            let layout = Layout::with_user_dir(root.path(), home.path().join("dial")).unwrap();
            let store = Store::new(&layout).unwrap();
            // A missing history is a fresh start, not an error.
            assert!(store.load_scheduler().is_ok());
            fs::create_dir_all(&store.dir).unwrap();
            let path = store.dir.join(file);
            match &damage {
                Some(text) => fs::write(&path, text).unwrap(),
                // A directory where the file should be is a real I/O failure, not corruption.
                None => fs::create_dir(&path).unwrap(),
            }
            let error = store
                .load_scheduler()
                .err()
                .unwrap_or_else(|| panic!("{file} {damage:?} loaded as an empty history"));
            let message = format!("{error:#}");
            assert!(message.contains(file), "{message}");
            assert!(!message.contains(secret), "{message}");
            if file != "schedule-watch.toml" {
                assert!(store.last_runs().is_err());
            }
        }
    }
}

#[test]
fn failed_run_state_save_keeps_the_change_unsaved_until_a_later_save() {
    let root = tempfile::tempdir().unwrap();
    // A regular file where the state directory belongs makes every save fail.
    let blocked = root.path().join("not-a-dir");
    fs::write(&blocked, "").unwrap();
    let blocked = Store { dir: blocked };
    let store = Store {
        dir: root.path().join("state"),
    };
    let e = entry(Some(100), &["*.rs"]);
    let mut scheduler = Scheduler::default();
    // First sight of a watch sets a baseline that must be persisted before any run can start.
    scheduler.reconcile(std::slice::from_ref(&e));
    assert_eq!(scheduler.due(&e, 10, || Some([1; 32])), None);
    let mut reported = None;
    assert!(checkpoint(&blocked, &mut scheduler, &mut reported).is_err());
    assert!(scheduler.unsaved, "a failed save leaves the change pending");
    assert!(reported.is_some());
    checkpoint(&store, &mut scheduler, &mut reported).unwrap();
    assert!(!scheduler.unsaved && reported.is_none());
    let restored = store.load_scheduler().unwrap();
    assert_eq!(restored.watches[&e.id].baseline, Some([1; 32]));
}

#[test]
fn run_state_is_saved_as_one_file_and_legacy_files_are_left_alone_and_ignored() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().join("dial")).unwrap();
    let store = Store::new(&layout).unwrap();
    let e = entry(None, &["*.rs"]);
    fs::create_dir_all(&store.dir).unwrap();
    fs::write(
        store.state_path(),
        toml::to_string(&HashMap::from([(e.id.clone(), 10u64)])).unwrap(),
    )
    .unwrap();
    fs::write(
        store.baseline_path(),
        toml::to_string(&HashMap::from([(e.id.clone(), [1u8; 32])])).unwrap(),
    )
    .unwrap();
    let mut scheduler = store.load_scheduler().unwrap();
    assert_eq!(scheduler.last_run[&e.id], 10);
    scheduler.reconcile(std::slice::from_ref(&e));
    scheduler.ran(&e, 20, Some([2; 32]), Some([2; 32]));
    store.save_state(&scheduler).unwrap();
    // Legacy files are never deleted, and once the consolidated file exists they are not read.
    assert!(store.state_path().exists() && store.baseline_path().exists());
    fs::write(store.state_path(), "not = [valid").unwrap();
    fs::write(store.baseline_path(), "garbage = [").unwrap();
    assert_eq!(store.last_runs().unwrap()[&e.id], 20);
    let restored = store.load_scheduler().unwrap();
    assert_eq!(restored.last_run[&e.id], 20);
    assert_eq!(restored.watches[&e.id].baseline, Some([2; 32]));
}

#[test]
fn edit_made_during_a_run_before_its_snapshot_is_followed_up() {
    // The run starts on `a`; the user edits to `b` while it is still active. The launch-time
    // fingerprint is the acknowledged baseline, so the edit must not be absorbed into it.
    let mut s = Scheduler::default();
    let e = entry(None, &["*.rs"]);
    s.reconcile(std::slice::from_ref(&e));
    assert_eq!(s.due(&e, 10, || Some([1; 32])), None);
    s.ran(&e, 20, Some([1; 32]), Some([2; 32]));
    assert_eq!(s.due(&e, 20, || Some([2; 32])), None);
    assert_eq!(
        s.due(&e, 20 + DEBOUNCE_SECS, || Some([2; 32])),
        Some(Reason::Changed),
        "an edit made while the run was active must not be absorbed silently"
    );
}

#[test]
fn followup_that_edits_again_pauses_durably_until_the_schedule_is_readded() {
    let (a, b, c, d) = ([1u8; 32], [2u8; 32], [3u8; 32], [4u8; 32]);
    let e = entry(Some(50), &["*.rs"]);
    let mut s = Scheduler::default();
    s.reconcile(std::slice::from_ref(&e));
    assert_eq!(s.due(&e, 10, || Some(a)), None);
    s.ran(&e, 20, Some(a), Some(b));
    assert_eq!(s.due(&e, 23, || Some(b)), Some(Reason::Changed));
    assert!(
        s.ran(&e, 23, Some(b), Some(c)),
        "a change during the follow-up pauses the entry"
    );
    // Neither the interval nor a later change may start another run.
    assert_eq!(s.due(&e, 1000, || Some(d)), None);
    assert_eq!(s.due(&e, 1000 + DEBOUNCE_SECS, || Some(d)), None);
}

#[test]
fn pending_followup_and_review_pause_survive_a_restart() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().join("dial")).unwrap();
    let store = Store::new(&layout).unwrap();
    let (a, b, c, d) = ([1u8; 32], [2u8; 32], [3u8; 32], [4u8; 32]);
    let e = entry(Some(50), &["*.rs"]);
    let mut s = Scheduler::default();
    s.reconcile(std::slice::from_ref(&e));
    s.due(&e, 10, || Some(a));
    s.ran(&e, 20, Some(a), Some(b));
    store.save_state(&s).unwrap();
    let mut restored = store.load_scheduler().unwrap();
    restored.reconcile(std::slice::from_ref(&e));
    assert_eq!(
        restored.due(&e, 20 + DEBOUNCE_SECS, || Some(b)),
        Some(Reason::Changed),
        "the pending follow-up survives a restart"
    );
    assert!(restored.ran(&e, 23, Some(b), Some(c)));
    store.save_state(&restored).unwrap();
    let mut again = store.load_scheduler().unwrap();
    again.reconcile(std::slice::from_ref(&e));
    assert_eq!(
        again.due(&e, 1000, || Some(d)),
        None,
        "the pause survives a restart"
    );
    // Re-adding the schedule (a new definition, new creation time) is the acknowledgment.
    let mut readded = e.clone();
    readded.created += 1;
    again.reconcile(std::slice::from_ref(&readded));
    assert!(!again.watches.contains_key(&e.id));
}

#[test]
fn unobservable_run_keeps_the_last_trustworthy_baseline() {
    let (a, b) = ([1u8; 32], [2u8; 32]);
    let e = entry(None, &["*.rs"]);
    let mut s = Scheduler::default();
    s.reconcile(std::slice::from_ref(&e));
    s.due(&e, 10, || Some(a));
    s.ran(&e, 20, Some(a), None);
    assert_eq!(s.watches[&e.id].baseline, Some(a), "no silent re-baseline");
    assert_eq!(
        s.due(&e, 30, || Some(a)),
        None,
        "unchanged content ends the check"
    );
    s.ran(&e, 40, Some(a), None);
    assert_eq!(s.due(&e, 50, || Some(b)), None);
    assert_eq!(
        s.due(&e, 50 + DEBOUNCE_SECS, || Some(b)),
        Some(Reason::Changed),
        "a change after an unobservable run is still seen"
    );
    // A fresh scheduler whose first run cannot be observed keeps the launch content as baseline.
    let mut fresh = Scheduler::default();
    fresh.reconcile(std::slice::from_ref(&e));
    fresh.ran(&e, 20, Some(a), None);
    assert_eq!(fresh.watches[&e.id].baseline, Some(a));
}

#[test]
fn only_real_watch_state_changes_mark_the_run_state_unsaved() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().join("dial")).unwrap();
    let store = Store::new(&layout).unwrap();
    let e = entry(None, &["*.rs"]);
    let (a, b) = ([1u8; 32], [2u8; 32]);
    let mut s = Scheduler::default();
    s.reconcile(std::slice::from_ref(&e));
    let mut reported = None;
    // First sight of the baseline is a real change.
    assert_eq!(s.due(&e, 10, || Some(a)), None);
    assert!(s.unsaved);
    checkpoint(&store, &mut s, &mut reported).unwrap();
    assert!(!s.unsaved);
    // Unchanged content and an unchanged reload must not dirty the run state.
    assert_eq!(s.due(&e, 11, || Some(a)), None);
    s.reconcile(std::slice::from_ref(&e));
    assert!(
        !s.unsaved,
        "an unchanged check must not dirty the run state"
    );
    // A new change becomes pending: that is persisted state.
    assert_eq!(s.due(&e, 12, || Some(b)), None);
    assert!(s.unsaved);
    checkpoint(&store, &mut s, &mut reported).unwrap();
    // The pending change survives a reload and fires once it settles.
    let mut restored = store.load_scheduler().unwrap();
    restored.reconcile(std::slice::from_ref(&e));
    assert!(!restored.unsaved);
    assert_eq!(
        restored.due(&e, 12 + DEBOUNCE_SECS, || Some(b)),
        Some(Reason::Changed)
    );
    assert!(
        !restored.unsaved,
        "a settled trigger changes no persisted state by itself"
    );
}

#[test]
fn unobservable_follow_up_pauses_and_keeps_the_trustworthy_baseline() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().join("dial")).unwrap();
    let store = Store::new(&layout).unwrap();
    let (a, b) = ([1u8; 32], [2u8; 32]);
    let e = entry(Some(50), &["*.rs"]);
    let mut s = Scheduler::default();
    s.reconcile(std::slice::from_ref(&e));
    assert_eq!(s.due(&e, 10, || Some(a)), None);
    // The run's result is unobservable: the next check must compare against the trusted baseline.
    assert!(!s.ran(&e, 20, Some(a), None));
    assert_eq!(s.due(&e, 30, || Some(b)), None);
    assert_eq!(
        s.due(&e, 30 + DEBOUNCE_SECS, || Some(b)),
        Some(Reason::Changed)
    );
    // Its follow-up is also unobservable: it pauses instead of retrying forever.
    assert!(
        s.ran(&e, 33, Some(b), None),
        "an unobservable follow-up pauses"
    );
    assert_eq!(s.watches[&e.id].baseline, Some(a), "no silent re-baseline");
    checkpoint(&store, &mut s, &mut None).unwrap();
    // Neither the interval nor a restart may bypass the pause.
    assert_eq!(s.due(&e, 1000, || Some(b)), None);
    let mut restored = store.load_scheduler().unwrap();
    restored.reconcile(std::slice::from_ref(&e));
    assert_eq!(restored.due(&e, 2000, || Some(b)), None);
    assert_eq!(restored.due(&e, 2000 + DEBOUNCE_SECS, || Some(a)), None);
}

#[test]
fn publication_sync_failure_is_not_acknowledged_or_rolled_back() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store {
        dir: temp.path().to_path_buf(),
    };
    for path in [store.path(), store.state_path()] {
        fs::write(&path, "old").unwrap();
        let error = store
            .write_atomic_with_sync(path.clone(), "new", |dir| {
                assert_eq!(dir, store.dir);
                assert_eq!(fs::read_to_string(&path).unwrap(), "new");
                anyhow::bail!("injected directory sync failure");
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("durability uncertain"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }
}
