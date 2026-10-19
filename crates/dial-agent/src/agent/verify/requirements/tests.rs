use super::*;
use crate::agent::verify::verification::CheckStatus;
use crate::config::types::{VerificationCommand, VerificationRequirement};

fn contract() -> VerificationConfig {
    let mut config = VerificationConfig::default();
    for (name, command) in [("boundary", "run-boundaries"), ("errors", "run-errors")] {
        config.commands.push(VerificationCommand {
            name: name.into(),
            command: command.into(),
            file_extensions: vec![],
            timeout_secs: 30,
        });
    }
    config.requirements.insert(
        "input-contract".into(),
        VerificationRequirement {
            checks: vec!["boundary".into(), "errors".into()],
            visual: false,
        },
    );
    config
}

#[test]
fn requires_every_declared_check_at_the_current_revision() {
    let config = contract();
    let mut ledger = Ledger::default();
    let visual = Review::default();
    let revision = [1, 0, 0];
    ledger.observe("unrelated", "a", CheckStatus::Passed, revision, true);
    assert_eq!(snapshot(&config, &ledger, revision, &visual)["observed"], 0);
    ledger.observe("run-boundaries", "b", CheckStatus::Passed, revision, true);
    assert_eq!(snapshot(&config, &ledger, revision, &visual)["observed"], 0);
    ledger.observe("run-errors", "c", CheckStatus::Passed, revision, true);
    assert_eq!(
        snapshot(&config, &ledger, revision, &visual)["coverage"],
        "declared_requirements_observed"
    );
    assert_eq!(
        snapshot(&config, &ledger, [2, 0, 0], &visual)["observed"],
        0
    );
    for status in [
        CheckStatus::CodeFailed,
        CheckStatus::EnvironmentUnavailable,
        CheckStatus::PermissionDenied,
        CheckStatus::TimedOut,
        CheckStatus::Cancelled,
        CheckStatus::Unverified,
    ] {
        ledger.observe("run-errors", "d", status, revision, true);
        let evidence = snapshot(&config, &ledger, revision, &visual);
        assert_eq!(evidence["observed"], 0, "{status:?}");
        assert_ne!(evidence["requirements"][0]["checks"][1]["status"], "passed");
    }
}

#[test]
fn visual_requirement_needs_delivered_current_browser_pixels() {
    let mut config = contract();
    config.requirements.clear();
    config.requirements.insert(
        "appearance".into(),
        VerificationRequirement {
            checks: vec![],
            visual: true,
        },
    );
    let ledger = Ledger::default();
    let mut visual = Review::default();
    let revision = [1, 2, 3];
    visual.captured(revision, false);
    visual.delivered();
    assert_eq!(snapshot(&config, &ledger, revision, &visual)["observed"], 0);
    visual.captured(revision, true);
    assert_eq!(snapshot(&config, &ledger, revision, &visual)["observed"], 0);
    visual.delivered();
    assert_eq!(snapshot(&config, &ledger, revision, &visual)["observed"], 1);
    assert_eq!(
        snapshot(&config, &ledger, [2, 2, 3], &visual)["observed"],
        0
    );
}

#[test]
fn absent_or_invalid_contract_never_claims_coverage() {
    let mut config = VerificationConfig::default();
    let ledger = Ledger::default();
    let visual = Review::default();
    assert_eq!(
        snapshot(&config, &ledger, [0; 3], &visual)["coverage"],
        "not_independently_assessed"
    );
    config
        .requirements
        .insert("empty".into(), VerificationRequirement::default());
    assert_eq!(
        snapshot(&config, &ledger, [0; 3], &visual)["coverage"],
        "requirements_uncovered"
    );
    config
        .requirements
        .get_mut("empty")
        .unwrap()
        .checks
        .push("unknown".into());
    assert_eq!(
        snapshot(&config, &ledger, [0; 3], &visual)["requirements"][0]["checks"][0]["status"],
        "invalid_contract"
    );
}
