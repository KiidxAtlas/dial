use super::*;

#[test]
fn decision_engine_timeout_and_telemetry_flush_are_bounded() {
    for value in [0, 1, 86400, 86401, u64::MAX] {
        let laya = laya::Config {
            timeout_secs: value,
            ..Default::default()
        };
        assert_eq!(laya.validate().is_ok(), value <= 86400, "laya: {value}");
        let mut config = Config::default();
        config.telemetry.flush_interval_secs = value;
        assert_eq!(
            config.validate().is_ok(),
            (1..=86400).contains(&value),
            "telemetry: {value}"
        );
    }
}
