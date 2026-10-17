use super::*;

fn config(root: &Path) -> Config {
    Config {
        compute_locks: false,
        user_dir: root.join("user"),
        state_dir: root.join("state"),
        ..Config::default()
    }
}

#[test]
fn disabled_background_ignores_shared_lock_and_requests_but_accounts_usage() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    let lock = super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
    let _foreground = Request::foreground(&config.user_dir).unwrap();
    // Even an owner record belonging to this process must survive the unlocked guard.
    register(
        &config.user_dir,
        "foreground",
        root.path(),
        root.path(),
        "fixture",
    )
    .unwrap();
    let owner = std::fs::read(config.user_dir.join("compute-owner.json")).unwrap();
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (done, rx) = std::sync::mpsc::channel();
        let cancelled = &cancel;
        scope.spawn(move || {
            if rx.recv_timeout(Duration::from_secs(1)).is_err() {
                cancelled.store(true, Ordering::SeqCst);
            }
        });
        let guard = Background::acquire_controlled(&config, Some(&cancel)).unwrap();
        done.send(()).unwrap();
        assert!(!can_yield());
        assert!(background_active());
        assert!(!config.user_dir.join("background-compute").exists());
        LEASED_SINCE.with(|since| {
            *since.borrow_mut() = Some(std::time::Instant::now() - Duration::from_secs(3));
        });
        Background::yield_controlled(&config, Some(&cancel)).unwrap();
        assert_eq!(super::super::resources::used_today(&config.user_dir), 3);
        LEASED_SINCE.with(|since| {
            *since.borrow_mut() = Some(std::time::Instant::now() - Duration::from_secs(2));
        });
        drop(guard);
        assert_eq!(super::super::resources::used_today(&config.user_dir), 5);
    });
    assert!(!background_active());
    assert!(!config.user_dir.join("background-compute").exists());
    assert_eq!(
        std::fs::read(config.user_dir.join("compute-owner.json")).unwrap(),
        owner
    );
    let probe = super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
    assert_eq!(
        probe.try_lock_exclusive().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn disabled_background_creates_no_lock_request_or_owner() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    let guard = Background::acquire(&config).unwrap();
    Background::yield_to_coding(&config).unwrap();
    drop(guard);
    for path in ["compute.lock", "background-compute", "compute-owner.json"] {
        assert!(!config.user_dir.join(path).exists(), "created {path}");
    }
}

#[test]
fn disabled_background_obeys_pause_cancellation_and_stop() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    std::fs::create_dir_all(&config.user_dir).unwrap();
    std::fs::create_dir_all(&config.state_dir).unwrap();
    let pause = config.user_dir.join("background.pause");
    std::fs::write(&pause, "").unwrap();
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::SeqCst);
        });
        assert!(Background::acquire_controlled(&config, Some(&cancel)).is_err());
    });
    assert!(!background_active());
    std::fs::remove_file(&pause).unwrap();
    let guard = Background::acquire(&config).unwrap();
    LEASED_SINCE.with(|since| {
        *since.borrow_mut() = Some(std::time::Instant::now() - Duration::from_secs(4));
    });
    std::fs::write(&pause, "").unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            std::fs::write(config.state_dir.join("improvement.stop"), "").unwrap();
        });
        assert!(
            Background::yield_to_coding(&config)
                .unwrap_err()
                .to_string()
                .contains("stopped")
        );
    });
    assert!(!background_active());
    assert_eq!(super::super::resources::used_today(&config.user_dir), 4);
    drop(guard);
    assert_eq!(super::super::resources::used_today(&config.user_dir), 4);
    assert!(Background::acquire(&config).is_err());
}

#[test]
fn disabled_model_call_ignores_foreground_but_still_stops() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    std::fs::create_dir_all(&config.state_dir).unwrap();
    let _guard = Background::acquire(&config).unwrap();
    let _foreground = Request::foreground(&config.user_dir).unwrap();
    let mut backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Local {
            model_dir: "unused".into(),
        },
        std::path::PathBuf::new(),
        5,
    );
    let mut calls = 0;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            std::fs::write(config.state_dir.join("improvement.stop"), "").unwrap();
        });
        let result: Result<()> = background_call(&config, &mut backend, |_, cancel| {
            calls += 1;
            let start = std::time::Instant::now();
            while !cancel.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(5));
            }
            ensure!(cancel.load(Ordering::SeqCst), "stop was not forwarded");
            Err(crate::llm::Interrupted.into())
        });
        assert!(result.unwrap_err().to_string().contains("stopped"));
    });
    assert_eq!(calls, 1);
}
