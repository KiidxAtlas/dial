use super::*;
use crate::config::types::Harness;

#[test]
fn background_request_preserves_training_and_defaults_legacy_to_cycle() {
    let request = Request {
        config: Config::default(),
        project: PathBuf::from("project"),
        state: PathBuf::from("state"),
        user: PathBuf::from("user"),
        force: true,
        operation: Operation::Train,
    };
    let mut value = serde_json::to_value(&request).unwrap();
    let restored: Request = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(restored.operation, Operation::Train);
    assert!(restored.force);
    value.as_object_mut().unwrap().remove("operation");
    let legacy: Request = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.operation, Operation::Cycle);
}

#[test]
fn training_dispatch_uses_lora_gate_and_records_failure_without_cycle_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), "model", "project", &Harness::default()).unwrap();
    let mut config = Config {
        state_dir: dir.path().into(),
        model: "model".into(),
        ..Config::default()
    };
    // No inference is needed: the manual LoRA gate rejects disabled training first.
    config.endpoint =
        Some(serde_json::from_value(json!({"base_url": "http://localhost:1234/v1"})).unwrap());
    config.training.enabled = false;
    store.set_meta("last_cycle_trajectory", "7").unwrap();
    let active = store.active().unwrap().id;
    let error = execute_job(&mut config, &store, true, Operation::Train, &mut |_| {}).unwrap_err();
    assert!(error.to_string().contains("LoRA training is disabled"));
    let status: Value =
        serde_json::from_str(&store.get_meta("auto_status").unwrap().unwrap()).unwrap();
    assert_eq!(status["state"], "failed");
    assert!(
        status["error"]
            .as_str()
            .unwrap()
            .contains("LoRA training is disabled")
    );
    assert_eq!(
        store.get_meta("last_cycle_trajectory").unwrap().as_deref(),
        Some("7")
    );
    assert!(store.get_meta("last_cycle_attempt").unwrap().is_none());
    assert_eq!(store.active().unwrap().id, active);
}

#[test]
fn training_rejects_ineligible_models_and_respects_pause_and_control_lock() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, root.path().join("user")).unwrap();
    let state = layout.state_dir.join("models/abcd");
    let store = Store::open(&state, "model", "project", &Harness::default()).unwrap();
    let mut config = Config {
        state_dir: state.clone(),
        model: "model".into(),
        ..Config::default()
    };
    config.training.enabled = false;
    assert!(
        start_training(&layout, &config, &store)
            .unwrap_err()
            .to_string()
            .contains("disabled")
    );
    config.training.enabled = true;
    config.endpoint = Some(
        serde_json::from_value(json!({
            "base_url": "http://localhost:1234/v1"
        }))
        .unwrap(),
    );
    assert!(
        start_training(&layout, &config, &store)
            .unwrap_err()
            .to_string()
            .contains("local MLX checkpoint")
    );
    config.endpoint = None;
    store.set_meta("auto_enabled", "false").unwrap();
    assert!(
        start_training(&layout, &config, &store)
            .unwrap_err()
            .to_string()
            .contains("paused")
    );
    store.set_meta("auto_enabled", "true").unwrap();
    let control = lock_file(&state.join("control.lock")).unwrap();
    control.lock_exclusive().unwrap();
    assert_eq!(
        start_training(&layout, &config, &store).unwrap(),
        Spawn::Refused(Refusal::Running)
    );
    assert!(Refusal::Running.message(&state).contains("/improve jobs"));
    assert!(Refusal::Launching.message(&state).contains("launch.lock"));
    assert!(!state.join("background-request.json").exists());
    drop(control);
    fs::write(state.join("improvement.stop"), b"stop").unwrap();
    assert!(
        execute_job(
            &mut config,
            &store,
            true,
            Operation::Train,
            &mut |_| panic!("stopped training started")
        )
        .is_err()
    );
    assert!(store.get_meta("last_cycle_attempt").unwrap().is_none());
}
#[test]
fn foreground_sessions_share_compute_but_exclude_background_work() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, root.path().join("user")).unwrap();
    let first = compute_lock(&layout).unwrap();
    let second = compute_lock(&layout).unwrap();
    let probe = lock_file(&layout.user_dir.join("compute.lock")).unwrap();
    assert_eq!(
        probe.try_lock_exclusive().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "background work cannot start while sessions hold compute"
    );
    drop(first);
    assert!(probe.try_lock_exclusive().is_err());
    drop(second);
    // A process forked concurrently by another test can hold an inherited copy of the
    // descriptor until its exec closes it, so wait briefly instead of asserting at once.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while probe.try_lock_exclusive().is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "sessions released compute"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
#[test]
fn project_jobs_distinguish_locks_from_stale_status_and_stop_pauses() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, root.path().join("user")).unwrap();
    let state = layout.state_dir.join("models/abcd");
    let store = Store::open(&state, "model", "project", &Harness::default()).unwrap();
    let mut config = Config {
        state_dir: state.clone(),
        ..Config::default()
    };
    store
        .set_meta(
            "auto_status",
            r#"{"state":"running","pid":123,"stop_supported":true}"#,
        )
        .unwrap();
    store.set_meta("lora_phase", "paused").unwrap();
    let control = lock_file(&state.join("control.lock")).unwrap();
    control.lock_exclusive().unwrap();
    let compute = compute_lock(&layout).unwrap();
    let status = jobs(&layout).unwrap();
    assert_eq!(status["shared_compute_busy"], true);
    assert_eq!(status["project_jobs"][0]["control_busy"], true);
    assert_eq!(status["project_jobs"][0]["lora_phase"], "paused");
    assert!(request_stop(&layout, "../abcd").is_err());
    assert_eq!(
        request_stop(&layout, "abcd").unwrap()["running_worker_supports_stop"],
        true
    );
    assert!(!enabled(&config, &store).unwrap());
    assert!(check_stop(&config).is_err());
    assert!(
        execute(&mut config, &store, true, &mut |_| panic!(
            "stopped cycle started"
        ))
        .is_err()
    );
    drop(control);
    drop(compute);
    // Concurrent subprocess tests can briefly inherit either lock before exec closes it.
    // As above, bound the wait rather than requiring immediate descriptor release after drop.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let status = jobs(&layout).unwrap();
        if status["shared_compute_busy"] == false
            && status["project_jobs"][0]["control_busy"] == false
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "locks still busy: {status}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let status = jobs(&layout).unwrap();
    assert_eq!(status["project_jobs"][0]["scheduler"]["state"], "running");
    resume(&state).unwrap();
    assert!(enabled(&config, &store).unwrap());
    check_stop(&config).unwrap();
    store
        .set_meta("auto_status", r#"{"state":"running","pid":123}"#)
        .unwrap();
    assert_eq!(
        request_stop(&layout, "abcd").unwrap()["running_worker_supports_stop"],
        false
    );
}

#[test]
fn only_new_live_turns_trigger_and_disable_and_backoff_win() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path(), "model", "project", &Harness::default()).unwrap();
    let mut cfg = Config::default();
    cfg.improve.min_new_turns = 2;
    let active = store.active().unwrap();
    assert!(!due(&cfg, &store).unwrap());
    for domain in ["selfplay:read", "project", "project"] {
        let id = store
            .begin_trajectory(active.id, domain, "test", "hello")
            .unwrap();
        store.finish_trajectory(id, Some("answer"), None).unwrap();
    }
    assert!(due(&cfg, &store).unwrap());
    store.set_meta("auto_enabled", "false").unwrap();
    assert!(!due(&cfg, &store).unwrap());
    store.set_meta("auto_enabled", "true").unwrap();
    store
        .set_meta("last_cycle_attempt", &improve::now().to_string())
        .unwrap();
    assert!(!due(&cfg, &store).unwrap());
    store.set_meta("last_cycle_attempt", "0").unwrap();
    store.set_meta("last_cycle_trajectory", "3").unwrap();
    assert!(!due(&cfg, &store).unwrap());
}

#[test]
fn countdown_and_readiness_report_each_real_gate() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path(), "model", "project", &Harness::default()).unwrap();
    let mut cfg = Config {
        // Keep the user dir inside the tempdir so a real ~/.dial pause file cannot leak into the test.
        user_dir: root.path().join("user"),
        ..Config::default()
    };
    // Cloud spend is allowed here, so only the turn-count and cooldown gates apply below.
    cfg.improve.max_daily_usd = 1.0;
    cfg.improve.min_new_turns = 1;
    assert_eq!(readiness(&cfg, &store).unwrap(), "0/1 new turns recorded");
    store.set_meta("last_cycle_attempt", "1000").unwrap();
    assert_eq!(
        remaining(&store, "last_cycle_attempt", 21600, 1001).unwrap(),
        21599
    );
    assert_eq!(
        remaining(&store, "last_cycle_attempt", 21600, 22600).unwrap(),
        0
    );
    assert_eq!(duration(21599), "5h 59m 59s");
    store
        .set_meta("last_cycle_attempt", &improve::now().to_string())
        .unwrap();
    let status = readiness(&cfg, &store).unwrap();
    assert!(status.contains("cooldown") && status.contains("0/1 new turns"));
    let id = store
        .begin_trajectory(store.active().unwrap().id, "project", "test", "hello")
        .unwrap();
    store.finish_trajectory(id, Some("done"), None).unwrap();
    assert!(!readiness(&cfg, &store).unwrap().contains("new turns"));
    store.set_meta("last_cycle_attempt", "0").unwrap();
    assert!(readiness(&cfg, &store).unwrap().contains("eligible"));
    std::fs::create_dir_all(&cfg.user_dir).unwrap();
    std::fs::write(cfg.user_dir.join("background.pause"), b"paused").unwrap();
    assert!(
        readiness(&cfg, &store)
            .unwrap()
            .contains("paused by /compute")
    );
    std::fs::remove_file(cfg.user_dir.join("background.pause")).unwrap();
    cfg.improve.background = false;
    assert!(
        readiness(&cfg, &store)
            .unwrap()
            .contains("background improvement is off")
    );
    cfg.improve.background = true;
    // A cloud main model (no local model string) with no daily budget must say why it will not learn.
    cfg.model = String::new();
    cfg.improve.max_daily_usd = 0.0;
    assert!(
        readiness(&cfg, &store)
            .unwrap()
            .contains("daily spend limit")
    );
    store.set_meta("auto_enabled", "false").unwrap();
    assert_eq!(
        readiness(&cfg, &store).unwrap(),
        "automatic improvement paused"
    );
}
