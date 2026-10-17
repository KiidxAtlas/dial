use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[test]
fn cancellation_interrupts_background_model_calls_without_retry() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        user_dir: root.path().join("user"),
        state_dir: root.path().join("state"),
        ..Default::default()
    };
    let lease = Background::acquire(&config).unwrap();
    let mut backend = crate::llm::provider::Backend::new(
        crate::llm::provider::ProviderKind::Local {
            model_dir: "unused".into(),
        },
        std::path::PathBuf::new(),
        5,
    );
    let external = Arc::new(AtomicBool::new(false));
    let mut calls = 0;
    std::thread::scope(|scope| {
        let flag = external.clone();
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(15));
            flag.store(true, Ordering::SeqCst);
        });
        let result: Result<()> =
            background_call_controlled(&config, &mut backend, Some(&external), |_, cancel| {
                calls += 1;
                let start = std::time::Instant::now();
                while !cancel.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(2) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                ensure!(cancel.load(Ordering::SeqCst), "cancel was not forwarded");
                Err(crate::llm::Interrupted.into())
            });
        assert!(
            result
                .unwrap_err()
                .downcast_ref::<crate::llm::Interrupted>()
                .is_some()
        );
    });
    assert_eq!(calls, 1);
    drop(lease);
    assert!(!can_yield());
}
