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
