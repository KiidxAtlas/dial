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

fn with_inputs(paths: &[&str], environment: &[&str]) -> Config {
    let mut config = config();
    config.verification.inputs.insert(
        "boundary".into(),
        types::VerificationInputs {
            paths: paths.iter().map(|p| p.to_string()).collect(),
            environment: environment.iter().map(|e| e.to_string()).collect(),
        },
    );
    config
}

#[test]
fn check_inputs_roundtrip_and_legacy_config_remains_empty() {
    let legacy: types::VerificationConfig = toml::from_str("enabled=true").unwrap();
    assert!(legacy.inputs.is_empty());
    let config = with_inputs(&["src", "tests.py"], &["PYTHONPATH", "_X1"]);
    config.validate().unwrap();
    let restored: types::VerificationConfig =
        toml::from_str(&toml::to_string(&config.verification).unwrap()).unwrap();
    assert_eq!(restored.inputs, config.verification.inputs);
    assert!(
        toml::from_str::<types::VerificationConfig>("[inputs.a]\nglobs=[]\n").is_err(),
        "unknown input fields are rejected"
    );
}

#[test]
fn check_inputs_reject_unknown_ambiguous_or_unsafe_declarations() {
    let mut unknown = with_inputs(&["src"], &[]);
    let value = unknown.verification.inputs.remove("boundary").unwrap();
    unknown.verification.inputs.insert("missing".into(), value);
    assert!(unknown.validate().is_err());

    let mut same_name = with_inputs(&["src"], &[]);
    let mut other = same_name.verification.commands[0].clone();
    other.command = "python3 other.py".into();
    same_name.verification.commands.push(other);
    assert!(same_name.validate().is_err());

    let mut same_command = with_inputs(&["src"], &[]);
    let mut other = same_command.verification.commands[0].clone();
    other.name = "second".into();
    same_command.verification.commands.push(other);
    assert!(same_command.validate().is_err());

    let long = "a".repeat(1025);
    let many: Vec<String> = (0..65).map(|i| format!("p{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let mut bad_paths = vec![
        vec![],
        vec![""],
        vec!["."],
        vec!["a/./b"],
        vec!["../x"],
        vec!["/abs"],
        vec!["a//b"],
        vec!["a/"],
        vec!["a\\b"],
        vec!["*.rs"],
        vec!["a?"],
        vec!["[a]"],
        vec!["a\nb"],
        vec!["a", "a"],
        vec![long.as_str()],
    ];
    bad_paths.push(many);
    for paths in bad_paths {
        assert!(with_inputs(&paths, &[]).validate().is_err(), "{paths:?}");
    }
    let many_env: Vec<String> = (0..65).map(|i| format!("E{i}")).collect();
    let many_env: Vec<&str> = many_env.iter().map(String::as_str).collect();
    for env in [
        vec![""],
        vec!["1A"],
        vec!["A-B"],
        vec!["A=B"],
        vec!["A B"],
        vec!["A", "A"],
        many_env,
    ] {
        assert!(with_inputs(&["src"], &env).validate().is_err(), "{env:?}");
    }
}
