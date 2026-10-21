use super::*;
use types::{VerificationCommand, VerificationRequirement};

fn config() -> Config {
    let mut config = Config::default();
    config.verification.commands.push(VerificationCommand {
        name: "boundary".into(),
        command: "python3 tests.py".into(),
        file_extensions: vec![],
        timeout_secs: 30,
    });
    config.verification.requirements.insert(
        "negative-input".into(),
        VerificationRequirement {
            checks: vec!["boundary".into()],
            visual: false,
        },
    );
    config
}

#[test]
fn requirements_roundtrip_and_legacy_config_remains_empty() {
    let expected = config();
    expected.validate().unwrap();
    let restored: types::VerificationConfig =
        toml::from_str(&toml::to_string(&expected.verification).unwrap()).unwrap();
    assert_eq!(restored.requirements["negative-input"].checks, ["boundary"]);
    let legacy: types::VerificationConfig = toml::from_str("enabled=true").unwrap();
    assert!(legacy.requirements.is_empty());
}

#[test]
fn requirement_contract_rejects_missing_ambiguous_duplicate_or_empty_evidence() {
    let mut missing = config();
    missing.verification.commands.clear();
    assert!(missing.validate().is_err());
    let mut ambiguous = config();
    ambiguous
        .verification
        .commands
        .push(ambiguous.verification.commands[0].clone());
    assert!(ambiguous.validate().is_err());
    let mut duplicate = config();
    duplicate
        .verification
        .requirements
        .get_mut("negative-input")
        .unwrap()
        .checks
        .push("boundary".into());
    assert!(duplicate.validate().is_err());
    let mut empty = config();
    empty
        .verification
        .requirements
        .get_mut("negative-input")
        .unwrap()
        .checks
        .clear();
    assert!(empty.validate().is_err());
    empty
        .verification
        .requirements
        .get_mut("negative-input")
        .unwrap()
        .visual = true;
    empty.validate().unwrap();
}

#[test]
fn quality_controls_roundtrip_and_preserve_legacy_defaults() {
    let legacy: types::VerificationConfig = toml::from_str("enabled=true").unwrap();
    assert!(!legacy.strict_requirements);
    assert!(!legacy.review.enabled);
    assert!(legacy.runners.is_empty());
    let mut config = config();
    config.verification.strict_requirements = true;
    config.verification.review.enabled = true;
    config.verification.review.sensitive_paths = vec!["src/auth".into()];
    config.verification.runners.insert(
        "boundary".into(),
        types::VerificationRunner {
            program: "python3".into(),
            module: Some("pytest".into()),
        },
    );
    config.validate().unwrap();
    let restored: types::VerificationConfig =
        toml::from_str(&toml::to_string(&config.verification).unwrap()).unwrap();
    assert!(restored.strict_requirements);
    assert_eq!(
        restored.runners["boundary"].module.as_deref(),
        Some("pytest")
    );
    assert_eq!(restored.review.sensitive_paths, ["src/auth"]);
}

#[test]
fn quality_controls_reject_invalid_or_ambiguous_contracts() {
    let mut config = config();
    config.verification.runners.insert(
        "missing".into(),
        types::VerificationRunner {
            program: "python3".into(),
            module: None,
        },
    );
    assert!(config.validate().is_err());
    config.verification.runners.clear();
    config.verification.review.max_rounds = 0;
    assert!(config.validate().is_err());
    config.verification.review.max_rounds = 1;
    config.verification.review.sensitive_paths = vec!["../outside".into()];
    assert!(config.validate().is_err());
    config.verification.review.sensitive_paths.clear();
    config.verification.enabled = false;
    config.verification.strict_requirements = true;
    assert!(config.validate().is_err());
}
