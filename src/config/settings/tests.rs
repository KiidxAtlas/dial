use super::*;
#[test]
fn invalid_publication_keeps_snapshot_and_success_notifies_owned_subscribers() {
    let mut settings = Settings::new(Config::default()).unwrap();
    let listener = settings.subscribe();
    let before = settings.snapshot().revision;
    let mut invalid = Config::default();
    invalid.harness.temperature = f64::NAN;
    assert!(settings.publish(invalid).is_err());
    assert_eq!(settings.snapshot().revision, before);
    assert!(listener.revisions.try_recv().is_err());
    let mut changed = Config::default();
    changed.harness.max_steps = 12;
    assert!(settings.publish(changed).unwrap());
    assert_eq!(listener.revisions.recv().unwrap(), before + 1);
    assert_eq!(
        Setting::<u32>::new("harness.max_steps")
            .get(settings.snapshot())
            .unwrap(),
        12
    );
}
#[test]
fn typed_handles_refuse_unknown_keys_and_wrong_types() {
    let settings = Settings::new(Config::default()).unwrap();
    assert!(
        Setting::<bool>::new("not_a_setting")
            .get(settings.snapshot())
            .is_err()
    );
    assert!(
        Setting::<bool>::new("harness.max_steps")
            .get(settings.snapshot())
            .is_err()
    );
    assert!(reference()["settings"].as_array().unwrap().len() > 100);
}
