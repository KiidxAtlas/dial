use super::*;

#[test]
fn owner_fixture() {
    if std::env::var_os("DIAL_BG_TEST_OWNER").is_some() {
        std::process::exit(if owner_main().is_ok() { 0 } else { 1 });
    }
}

#[test]
fn controller_fixture() {
    if let Some(root) = std::env::var_os("DIAL_BG_CONTROLLER_ROOT") {
        let registry = PathBuf::from(std::env::var_os("DIAL_BG_CONTROLLER_JOBS").unwrap());
        let ws = Workspace::new(Path::new(&root))
            .unwrap()
            .without_sandbox()
            .with_shell_backend(crate::config::types::ShellBackend::System);
        let args = json!({"action":"start", "name":"survive", "command":"printf x >> once; printf ready; sleep 0.2; printf done", "lifetime": 3});
        let output = start(&ws, &args, &registry, &AtomicBool::new(false)).unwrap();
        assert!(!output.is_error, "{}", output.text);
        std::process::exit(0);
    }
}

fn fixture() -> (tempfile::TempDir, Workspace, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix("dbg-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let registry = temp.path().join("jobs");
    create_dir(&registry).unwrap();
    let ws = Workspace::new(&root)
        .unwrap()
        .without_sandbox()
        .with_shell_backend(crate::config::types::ShellBackend::System);
    (temp, ws, registry)
}

fn await_finished(directory: &Path) {
    let end = Instant::now() + Duration::from_secs(5);
    while !directory.join("finished.json").exists() {
        assert!(
            Instant::now() < end,
            "owner did not finish in fixture deadline"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let record: Record = read_json(&directory.join("owner.json")).unwrap();
    while !confirmed_dead(record.owner) {
        assert!(
            Instant::now() < end,
            "owner did not exit after publishing its result"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn owner_survives_controller_drop_reconnects_and_never_replays() {
    let (_temp, ws, registry) = fixture();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tools::exec::background::durable::unix::tests::controller_fixture",
            "--nocapture",
        ])
        .env("DIAL_BG_CONTROLLER_ROOT", &ws.root)
        .env("DIAL_BG_CONTROLLER_JOBS", &registry)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "fixture controller failed: {status}");
    let root = ws.root.clone();
    drop(ws);
    let reattached = Workspace::new(&root).unwrap();
    let directory = registry.join("survive");
    await_finished(&directory);
    for _ in 0..2 {
        let output = request(&directory, &reattached, &json!({"action":"output"})).unwrap();
        assert!(output.text.contains("readydone"), "{}", output.text);
    }
    assert_eq!(
        fs::read(root.join("once")).unwrap(),
        b"x",
        "reattachment must not relaunch writes"
    );
    request(&directory, &reattached, &json!({"action":"kill"})).unwrap();
    assert!(
        !directory.exists(),
        "explicit kill forgets a finished record"
    );
}

#[test]
fn explicit_kill_and_deadline_stop_owned_jobs() {
    for (name, lifetime, kill) in [("kill", 3, true), ("deadline", 1, false)] {
        let (_temp, ws, registry) = fixture();
        start(&ws, &json!({"action":"start","name":name,"command":"printf ready; sleep 30; printf unexpected > late", "lifetime":lifetime}), &registry, &AtomicBool::new(false)).unwrap();
        let directory = registry.join(name);
        if kill {
            request(&directory, &ws, &json!({"action":"kill"})).unwrap();
        }
        await_finished(&directory);
        let output = request(&directory, &ws, &json!({"action":"output"})).unwrap();
        assert!(
            output.is_error && output.text.contains("effects may be partial"),
            "{}",
            output.text
        );
        assert!(
            !ws.root.join("late").exists(),
            "terminated command must not finish its later write"
        );
    }
}

#[test]
fn rejects_unsafe_control_paths_and_stale_identities_without_signalling() {
    let (_temp, ws, registry) = fixture();
    for invalid in ["", ".", "..", "../escape", "a/b"] {
        assert!(
            name(&json!({"name":invalid})).is_err(),
            "unsafe name {invalid:?}"
        );
    }
    let directory = registry.join("stale");
    create_dir(&directory).unwrap();
    let mut owner = ProcId::current().unwrap();
    owner.start = owner.start.wrapping_add(1);
    publish(
        &directory.join("owner.json"),
        &Record {
            owner,
            root: ws.root.clone(),
        },
    )
    .unwrap();
    let error = request(&directory, &ws, &json!({"action":"output"})).unwrap_err();
    assert!(error.to_string().contains("NOT restarted"), "{error:#}");
    assert!(
        ProcId::current().unwrap().alive(),
        "stale PID record must never signal its reused PID"
    );
    publish(
        &directory.join("owner.json"),
        &Record {
            owner: ProcId::current().unwrap(),
            root: ws.root.clone(),
        },
    )
    .unwrap();
    assert!(
        request(&directory, &ws, &json!({"action":"kill"})).is_err(),
        "a live owner without a final record must not be forgotten"
    );
    assert!(
        directory.exists(),
        "unacknowledged live identity must retain its quota reservation"
    );
    let link = registry.join("link");
    std::os::unix::fs::symlink(&directory, &link).unwrap();
    assert!(
        request(&link, &ws, &json!({"action":"output"})).is_err(),
        "symlink endpoint must be rejected"
    );
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        request(&directory, &ws, &json!({"action":"output"})).is_err(),
        "public control directory must be rejected"
    );
}

#[test]
fn rejects_unsupported_or_unbounded_launch_before_spawning() {
    let (_temp, ws, registry) = fixture();
    for extension in [
        json!({"pty":true}),
        json!({"stdin":true}),
        json!({"lifetime":0}),
        json!({"lifetime":86401}),
        json!({"ready":{"log":"ok"}}),
    ] {
        let mut args = json!({"action":"start","name":"nope","command":"touch should-not-run"});
        args.as_object_mut()
            .unwrap()
            .extend(extension.as_object().unwrap().clone());
        assert!(
            start(&ws, &args, &registry, &AtomicBool::new(false)).is_err(),
            "must refuse {args}"
        );
    }
    assert!(
        !ws.root.join("should-not-run").exists(),
        "invalid launch cannot execute"
    );
}

#[test]
fn disconnected_output_is_bounded_and_keeps_artifact_evidence() {
    let (_temp, ws, registry) = fixture();
    let store =
        crate::storage::artifact::ArtifactStore::new(&registry.parent().unwrap().join("artifacts"))
            .unwrap();
    let ws = ws.with_artifacts(store);
    start(&ws, &json!({"action":"start","name":"output","command":"head -c 400000 /dev/zero | tr '\\000' x; printf tail-marker", "lifetime":3}), &registry, &AtomicBool::new(false)).unwrap();
    let directory = registry.join("output");
    await_finished(&directory);
    let output = request(&directory, &ws, &json!({"action":"output"})).unwrap();
    assert!(
        output.text.len() < 34000 && output.text.contains("tail-marker"),
        "snapshot must retain bounded newest output: {} bytes",
        output.text.len()
    );
    let integrity = output.execution.output.as_ref().unwrap();
    assert!(
        integrity.artifact.is_some(),
        "completed capture must retain artifact evidence"
    );
    assert_eq!(
        integrity.original_bytes,
        Some(400011),
        "capture counts original bytes across disconnect"
    );
    let manifest = fs::read_to_string(directory.join("owner.json")).unwrap();
    assert!(
        !manifest.contains("head -c"),
        "manifest must not become a command replay specification"
    );
}

#[test]
fn supervisor_death_reports_interruption_without_replaying_writes() {
    let (_temp, ws, registry) = fixture();
    start(&ws, &json!({"action":"start","name":"crash","command":"echo $$ > child-pid; printf x >> once; sleep 30", "lifetime":3}), &registry, &AtomicBool::new(false)).unwrap();
    let directory = registry.join("crash");
    let end = Instant::now() + Duration::from_secs(3);
    while !ws.root.join("once").exists() {
        assert!(Instant::now() < end, "fixture child did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let child_pid: i32 = fs::read_to_string(ws.root.join("child-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let child = ProcId::of(child_pid).unwrap();
    let owner: Record = read_json(&directory.join("owner.json")).unwrap();
    assert!(
        owner.owner.signal(libc::SIGKILL),
        "fixture owner must be signallable by checked identity"
    );
    while !confirmed_dead(owner.owner) {
        assert!(Instant::now() < end, "fixture owner did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Abrupt supervisor death is a documented POSIX cleanup boundary; the fixture owns cleanup.
    child.signal_group(libc::SIGKILL);
    assert!(
        request(&directory, &ws, &json!({"action":"output"}))
            .unwrap_err()
            .to_string()
            .contains("NOT restarted"),
        "inspection must report interruption without replay"
    );
    assert!(
        !directory.join("finished.json").exists(),
        "crashed owner has no final observation"
    );
    assert!(
        directory.exists(),
        "inspection must not automatically prune an interrupted owner"
    );
    let output = request(&directory, &ws, &json!({"action":"kill"})).unwrap();
    assert!(
        output.is_error && output.text.contains("effects may be partial"),
        "{}",
        output.text
    );
    assert!(
        !directory.exists(),
        "explicit forget of a confirmed-dead owner must recover its slot even without a final record"
    );
    assert_eq!(
        fs::read(ws.root.join("once")).unwrap(),
        b"x",
        "recovery must never replay writes"
    );
}

#[test]
fn failed_launches_are_retained_until_explicit_forget_recovers_quota() {
    let (_temp, ws, registry) = fixture();
    let artifacts = registry.parent().unwrap().join("broken-artifacts");
    let store = crate::storage::artifact::ArtifactStore::new(&artifacts).unwrap();
    fs::remove_dir_all(&artifacts).unwrap();
    fs::write(&artifacts, b"not a directory").unwrap();
    let ws = ws.with_artifacts(store);
    for index in 0..MAX_JOBS {
        let name = format!("failed-{index}");
        let args =
            json!({"action":"start", "name":name, "command":"printf forbidden > should-not-run"});
        let output = start(&ws, &args, &registry, &AtomicBool::new(false)).unwrap();
        assert!(
            output.is_error && output.text.contains("launch failed"),
            "{}",
            output.text
        );
        let directory = registry.join(&name);
        await_finished(&directory);
        let output = request(&directory, &ws, &json!({"action":"output"})).unwrap();
        assert!(
            output.is_error,
            "failed initialization cannot report success"
        );
        assert!(
            directory.exists(),
            "inspection must not silently prune a failure"
        );
    }
    let args = json!({"action":"start", "name":"retry", "command":"printf x >> recovered"});
    assert!(
        start(&ws, &args, &registry, &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("at most 8")
    );
    request(&registry.join("failed-0"), &ws, &json!({"action":"kill"})).unwrap();
    assert!(
        !registry.join("failed-0").exists(),
        "explicit forget releases the slot"
    );
    fs::remove_file(&artifacts).unwrap();
    let output = start(&ws, &args, &registry, &AtomicBool::new(false)).unwrap();
    assert!(
        !output.is_error,
        "recovered slot must admit a successful retry: {}",
        output.text
    );
    await_finished(&registry.join("retry"));
    assert_eq!(
        fs::read(ws.root.join("recovered")).unwrap(),
        b"x",
        "explicit retry executes once, without replaying failed commands"
    );
    for directory in job_dirs(&registry).unwrap() {
        request(&directory, &ws, &json!({"action":"kill"})).unwrap();
    }
    assert!(
        !ws.root.join("should-not-run").exists(),
        "failure recovery must never execute or replay the command"
    );
    assert!(
        job_dirs(&registry).unwrap().is_empty(),
        "all failed quota slots can be explicitly recovered"
    );
}

#[test]
fn spawn_failure_and_interrupted_acknowledgement_do_not_orphan_quota() {
    let (_temp, mut ws, registry) = fixture();
    let args =
        json!({"action":"start", "name":"pending", "command":"printf forbidden > should-not-run"});
    assert!(
        start_with_owner(
            &ws,
            &args,
            &registry,
            &AtomicBool::new(false),
            &registry.join("missing-executable")
        )
        .is_err()
    );
    assert!(
        job_dirs(&registry).unwrap().is_empty(),
        "spawn failure must remove its empty reservation"
    );

    // A controller can be interrupted before the helper reads the contract. It must retain
    // identity without claiming launch success or allowing an unknown live owner to be forgotten.
    let executable = registry.parent().unwrap().join("slow-owner");
    fs::write(&executable, "#!/bin/sh\nsleep 1\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    // Exceed the pipe's capacity so cancellation also exercises a backpressured transfer.
    ws.additional_roots = vec![ws.root.clone(); 2000];
    let cancel = AtomicBool::new(false);
    let directory = registry.join("pending");
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let end = Instant::now() + Duration::from_secs(3);
            while !directory.join("owner.json").exists() {
                assert!(
                    Instant::now() < end,
                    "controller did not publish owner identity"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(50));
            cancel.store(true, Ordering::Release);
        });
        let error = start_with_owner(&ws, &args, &registry, &cancel, &executable).unwrap_err();
        assert!(
            error.to_string().contains("acknowledgement interrupted"),
            "{error:#}"
        );
    });
    let record: Record = read_json(&directory.join("owner.json")).unwrap();
    assert!(
        record.owner.alive(),
        "interrupted acknowledgement must retain live owner identity"
    );
    assert!(request(&directory, &ws, &json!({"action":"kill"})).is_err());
    assert!(
        directory.exists(),
        "unacknowledged live owner must not be pruned"
    );
    await_finished(&directory);
    let output = request(&directory, &ws, &json!({"action":"output"})).unwrap();
    assert!(
        output.is_error && output.text.contains("NOT restarted"),
        "{}",
        output.text
    );
    request(&directory, &ws, &json!({"action":"kill"})).unwrap();
    assert!(
        job_dirs(&registry).unwrap().is_empty(),
        "dead failed helper can be explicitly forgotten"
    );
    assert!(
        !ws.root.join("should-not-run").exists(),
        "uncertain acknowledgement never replays launch"
    );
}
