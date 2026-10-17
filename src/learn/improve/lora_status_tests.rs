use super::*;
#[test]
fn lora_progress_requires_a_live_lock_and_clears_on_exit() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(project.path(), home.path().join("dial"))
            .unwrap();
    let mut config = Config::load(&layout, None).unwrap();
    config.model = "test-model".into();
    config.use_case = "test".into();
    let store = Store::open(
        &config.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )
    .unwrap();
    store.set_meta("lora_phase", "training").unwrap();
    assert!(lora_phase(&config, &store).is_none());
    // Model a descriptor inherited by a concurrent process: it must not retain the probe's
    // shared lock and prevent the next real worker from taking its exclusive lock.
    let probe =
        crate::learn::auto::lock_file(&config.state_dir.join("lora-progress.lock")).unwrap();
    let inherited = probe.try_clone().unwrap();
    assert!(lora_phase_from_lock(&probe, &store).is_none());
    let next = crate::learn::auto::lock_file(&config.state_dir.join("lora-progress.lock")).unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&next).is_ok());
    fs2::FileExt::unlock(&next).unwrap();
    drop(inherited);
    drop(probe);
    {
        let _phase = LoraPhase::start(&config, &store, "training").unwrap();
        assert_eq!(lora_phase(&config, &store).as_deref(), Some("training"));
    }
    assert!(lora_phase(&config, &store).is_none());
    // Simulate a crash marker without a held worker lock.
    store.set_meta("lora_phase", "evaluating").unwrap();
    assert!(lora_phase(&config, &store).is_none());
    let _phase = LoraPhase::start(&config, &store, "evaluating").unwrap();
    assert_eq!(lora_phase(&config, &store).as_deref(), Some("evaluating"));
}
