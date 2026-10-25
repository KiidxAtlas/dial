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
