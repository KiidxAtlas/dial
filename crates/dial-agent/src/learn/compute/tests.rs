use super::*;
#[test]
fn background_generation_is_retried_after_foreground_handoff_without_scoring_a_partial_reply() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        user_dir: root.path().join("user"),
        state_dir: root.path().join("state"),
        ..Config::default()
    };
    let _lease = Background::acquire(&config).unwrap();
    let mut backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1".into(),
            model: "fixture".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    );
    let mut calls = 0;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            let request = Request::foreground(&config.user_dir).unwrap();
            let lock =
                super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
            lock.lock_exclusive().unwrap();
            drop(request);
            std::thread::sleep(Duration::from_millis(100));
            FileExt::unlock(&lock).unwrap();
        });
        let reply = background_call(&config, &mut backend, |_, cancel| {
            calls += 1;
            if calls == 1 {
                let start = std::time::Instant::now();
                while !cancel.load(Ordering::SeqCst) {
                    assert!(start.elapsed() < Duration::from_secs(2));
                    std::thread::sleep(Duration::from_millis(5));
                }
                return Err(crate::llm::Interrupted.into());
            }
            Ok("complete retried reply")
        })
        .unwrap();
        assert_eq!(reply, "complete retried reply");
    });
    assert_eq!(calls, 2);
    assert!(can_yield());
}
#[test]
fn requests_are_live_and_crashed_markers_do_not_block() {
    let home = tempfile::tempdir().unwrap();
    let request = Request::foreground(home.path()).unwrap();
    assert!(foreground_pending(home.path()).unwrap());
    drop(request);
    assert!(!foreground_pending(home.path()).unwrap());
    std::fs::write(home.path().join("foreground-compute/stale"), "").unwrap();
    assert!(!foreground_pending(home.path()).unwrap());
}

#[test]
fn stop_interrupts_a_paused_training_wait_and_releases_its_lease() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        user_dir: root.path().join("user"),
        state_dir: root.path().join("state"),
        ..Config::default()
    };
    std::fs::create_dir_all(&config.state_dir).unwrap();
    let lease = Background::acquire(&config).unwrap();
    let _foreground = Request::foreground(&config.user_dir).unwrap();
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
    assert!(!can_yield());
    drop(lease);
    let lock = super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
}
