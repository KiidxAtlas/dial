use super::*;

fn parse(text: &str) -> Result<CompactionConfig, toml::de::Error> {
    toml::from_str(text)
}

#[test]
fn promotion_is_off_by_default_with_no_targets() {
    let promotion = CompactionConfig::default().promotion;
    assert!(!promotion.enabled);
    assert!(promotion.targets.is_empty());
    assert_eq!(parse("").unwrap().promotion, promotion);
    CompactionConfig::default().validate().unwrap();
}

#[test]
fn promotion_parses_targets_and_rejects_unknown_keys() {
    let config = parse(
        "[promotion]\nenabled = true\n[promotion.targets]\ndefault = [\"@big\"]\n\"openai/*\" = [\"gpt-long\", \"anthropic/claude-long\"]\n",
    )
    .unwrap();
    assert!(config.promotion.enabled);
    assert_eq!(config.promotion.targets["default"], ["@big"]);
    assert_eq!(config.promotion.targets["openai/*"].len(), 2);
    config.validate().unwrap();
    assert!(parse("[promotion]\nenable = true\n").is_err());
}

#[test]
fn promotion_validation_bounds_keys_chains_and_selectors() {
    let with = |key: &str, chain: Vec<String>| {
        let mut config = CompactionConfig::default();
        config.promotion.targets.insert(key.into(), chain);
        config.validate()
    };
    with("default", vec!["big".into()]).unwrap();
    assert!(with("default", vec![]).is_err(), "an empty list is refused");
    assert!(with(" ", vec!["big".into()]).is_err());
    assert!(with(&"k".repeat(513), vec!["big".into()]).is_err());
    assert!(with("default", vec!["  ".into()]).is_err());
    assert!(with("default", vec!["m".repeat(513)]).is_err());
    with("default", vec!["m".into(); 32]).unwrap();
    assert!(with("default", vec!["m".into(); 33]).is_err());
    let mut config = CompactionConfig::default();
    for index in 0..129 {
        config
            .promotion
            .targets
            .insert(format!("p{index}/*"), vec!["m".into()]);
    }
    assert!(config.validate().is_err());
}

#[test]
fn auto_threshold_uses_the_usable_large_window_and_preserves_small_windows() {
    let identity = CompactionIdentity {
        provider: "p",
        model: "m",
        agent: None,
    };
    for policy in [
        CompactionConfig::default(),
        parse("threshold_percent = 'auto'\nthreshold_tokens = 'auto'\n").unwrap(),
    ] {
        for (window, expected) in [
            (8_192, 5_571),
            (32_768, 20_972),
            (65_536, 41_943),
            (65_537, 49_153),
            (200_000, 170_000),
            (1_000_000, 850_000),
        ] {
            assert_eq!(
                policy.effective_threshold(identity, window).unwrap(),
                expected,
                "window {window}"
            );
            for mid_turn in [false, true] {
                assert!(
                    !policy
                        .should_compact(identity, window, expected, mid_turn)
                        .unwrap()
                );
                assert!(
                    policy
                        .should_compact(identity, window, expected + 1, mid_turn)
                        .unwrap()
                );
            }
        }
    }
}

#[test]
fn explicit_thresholds_do_not_change_with_the_large_window_auto_default() {
    let identity = CompactionIdentity {
        provider: "p",
        model: "m",
        agent: None,
    };
    for (text, expected) in [
        ("threshold_percent = 80", 160_000),
        ("threshold_tokens = 120000", 120_000),
        ("[model_thresholds]\n'p/m' = '80%'", 160_000),
        ("[model_thresholds]\n'p/m' = 'f120000'", 120_000),
    ] {
        let policy = parse(text).unwrap();
        assert_eq!(
            policy.effective_threshold(identity, 200_000).unwrap(),
            expected,
            "{text}"
        );
    }
    let policy = parse("reserve_tokens = 50000").unwrap();
    assert_eq!(
        policy.effective_threshold(identity, 200_000).unwrap(),
        150_000
    );
}

#[test]
fn model_numeric_thresholds_remain_bases_and_agent_overrides_remain_triggers() {
    let identity = CompactionIdentity {
        provider: "p",
        model: "m",
        agent: None,
    };
    for (base, expected) in [
        (65_536, 41_943),
        (100_000, 83_616),
        (200_000, 170_000),
        (1_000_000, 170_000),
    ] {
        let policy = parse(&format!("[model_thresholds]\n'p/m' = {base}")).unwrap();
        assert_eq!(
            policy.effective_threshold(identity, 200_000).unwrap(),
            expected,
            "base {base}"
        );
    }
    for (value, expected) in [("100000", 100_000), ("'80%'", 160_000)] {
        let policy = parse(&format!(
            "[model_thresholds]\n'p/m' = 'f120000'\n[agent_threshold_overrides]\nworker = {value}"
        ))
        .unwrap();
        let worker = CompactionIdentity {
            agent: Some("worker"),
            ..identity
        };
        assert_eq!(
            policy.effective_threshold(worker, 200_000).unwrap(),
            expected
        );
        assert_eq!(
            policy.effective_threshold(identity, 200_000).unwrap(),
            120_000
        );
    }
    let policy =
        parse("model_thresholds_enabled = false\n[model_thresholds]\n'p/m' = 100000").unwrap();
    assert_eq!(
        policy.effective_threshold(identity, 200_000).unwrap(),
        170_000
    );
}
