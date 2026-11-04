use super::*;

fn with_max_concurrency(value: usize) -> TaskConfig {
    TaskConfig {
        max_concurrency: value,
        ..TaskConfig::default()
    }
}

#[test]
fn max_concurrency_validation_accepts_only_the_documented_range() {
    // (configured value, accepted by validate)
    let cases = [(0, false), (1, true), (32, true), (64, true), (65, false)];
    for (value, accepted) in cases {
        let config = with_max_concurrency(value);
        assert_eq!(
            config.validate().is_ok(),
            accepted,
            "max_concurrency={value}"
        );
    }
}

#[test]
fn parallel_read_and_dependency_wave_limits_follow_the_worker_budget() {
    // (max_concurrency, parallel read window, dependency wave ceiling)
    let cases = [
        (1, 1, 1),
        (32, 32, 32),
        (64, 64, 32),
        // Defensive clamps for values validation would reject; the windows stay in 1..=64.
        (0, 1, 1),
        (65, 64, 32),
        (usize::MAX, 64, 32),
    ];
    for (value, read, wave) in cases {
        let config = with_max_concurrency(value);
        assert_eq!(
            config.parallel_read_limit(),
            read,
            "read window for {value}"
        );
        assert_eq!(
            config.dependency_wave_limit(),
            wave,
            "wave ceiling for {value}"
        );
    }
}

#[test]
fn default_config_keeps_its_existing_windows() {
    let config = TaskConfig::default();
    assert_eq!(config.max_concurrency, 32);
    assert_eq!(config.parallel_read_limit(), 32);
    assert_eq!(config.dependency_wave_limit(), DEPENDENCY_WAVE_CEILING);
}
