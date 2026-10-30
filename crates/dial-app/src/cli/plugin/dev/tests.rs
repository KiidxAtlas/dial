use super::*;

#[test]
fn source_fingerprint_tracks_content_not_cache_or_creation_order() {
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    for (root, entries) in [
        (one.path(), ["b.py", "a.py"]),
        (two.path(), ["a.py", "b.py"]),
    ] {
        for name in entries {
            fs::write(root.join(name), name).unwrap();
        }
    }
    let original = fingerprint(one.path()).unwrap();
    assert_eq!(original, fingerprint(two.path()).unwrap());
    fs::create_dir(one.path().join("__pycache__")).unwrap();
    fs::write(one.path().join("__pycache__/ignored.pyc"), "cache").unwrap();
    assert_eq!(original, fingerprint(one.path()).unwrap());
    fs::write(one.path().join("a.py"), "same").unwrap();
    assert_ne!(original, fingerprint(one.path()).unwrap());
}

#[test]
fn source_watcher_enforces_file_entry_and_depth_budgets() {
    let large = tempfile::tempdir().unwrap();
    fs::File::create(large.path().join("large.py"))
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    assert!(
        fingerprint(large.path())
            .unwrap_err()
            .to_string()
            .contains("8 MiB")
    );
    let crowded = tempfile::tempdir().unwrap();
    for n in 0..=MAX_FILES {
        fs::write(crowded.path().join(n.to_string()), []).unwrap();
    }
    assert!(
        fingerprint(crowded.path())
            .unwrap_err()
            .to_string()
            .contains("entries")
    );
    let deep = tempfile::tempdir().unwrap();
    let mut path = deep.path().to_owned();
    for _ in 0..33 {
        path.push("sub");
        fs::create_dir(&path).unwrap();
    }
    assert!(
        fingerprint(deep.path())
            .unwrap_err()
            .to_string()
            .contains("directory levels")
    );
}

#[cfg(unix)]
#[test]
fn source_watcher_does_not_follow_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "first").unwrap();
    let before = fingerprint(root.path()).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
    assert_eq!(before, fingerprint(root.path()).unwrap());
    fs::write(outside.path().join("secret"), "changed").unwrap();
    assert_eq!(before, fingerprint(root.path()).unwrap());
}

#[test]
fn development_callbacks_never_grant_agent_or_effect_authority() {
    let host = Host::default();
    for method in [
        "tools.call",
        "tools.list",
        "settings.update",
        "providers.add",
        "context.get",
    ] {
        assert!(
            display_callback(&host, "demo", method, &json!({}))
                .unwrap_err()
                .to_string()
                .contains("no effectful approval")
        );
    }
    assert!(
        display_callback(
            &host,
            "demo",
            "views.update",
            &json!({"id":"other", "revision":1, "elements":[]})
        )
        .is_err()
    );
    assert!(display_callback(&host, "demo", "views.remove", &json!({"id":"other"})).is_err());
    assert!(
        display_callback(
            &host,
            "demo",
            "progress",
            &json!({"text":"x".repeat(64 << 10)})
        )
        .is_err()
    );
    assert!(
        display_callback(
            &host,
            "demo",
            "progress",
            &json!({"callback":{"params":{"text":"working\u{001b}[31m"}}})
        )
        .is_ok()
    );
    assert!(host.take_effects().is_empty());
}

#[test]
fn isolated_development_worker_stops_before_reload_and_reports_bad_source() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    authoring::create(&source, Some("demo"), Path::new("python3")).unwrap();
    let manifest_path = source.join("dial-plugin.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["python"]["runtime"] = json!("worker");
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let store = PluginStore::new(root.path().join("isolated/plugins"));
    let first = load(&source, &store, None).unwrap();
    assert_eq!(first.host.persistent_names(), vec!["demo"]);
    poll(&first, &AtomicBool::new(false)).unwrap();
    let old_integrity = store.get("demo").unwrap().integrity;
    drop(first);
    manifest["version"] = json!("0.2.0");
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let second = load(&source, &store, Some("demo")).unwrap();
    assert_ne!(old_integrity, store.get("demo").unwrap().integrity);
    poll(&second, &AtomicBool::new(false)).unwrap();
    drop(second);
    fs::write(
        source.join("extension.py"),
        "raise RuntimeError('dev initialization failed')\n",
    )
    .unwrap();
    assert!(load(&source, &store, Some("demo")).is_err());
    // Failed initialization must not leave a lease blocking operator cleanup.
    store.remove("demo").unwrap();
    assert!(store.list().unwrap().is_empty());
}
