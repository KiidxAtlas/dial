use super::*;

#[test]
fn explicit_tier_beats_fast_and_slow_modes() {
    let config = Config {
        fast_mode: true,
        slow_mode: true,
        service_tier: "ultrafast".into(),
        ..Default::default()
    };
    assert_eq!(config.effective_service_tier(), "ultrafast");
}

#[test]
fn fast_mode_beats_slow_mode_and_each_alone_applies() {
    let mut config = Config::default();
    assert_eq!(config.effective_service_tier(), "none");
    config.slow_mode = true;
    assert_eq!(config.effective_service_tier(), "flex");
    config.fast_mode = true;
    assert_eq!(config.effective_service_tier(), "priority");
    config.slow_mode = false;
    assert_eq!(config.effective_service_tier(), "priority");
}
