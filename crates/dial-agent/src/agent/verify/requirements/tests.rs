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
            ..Default::default()
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
            ..Default::default()
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

#[test]
fn strict_settlement_is_opt_in_and_rejects_missing_stale_and_unavailable_evidence() {
    let mut config = contract();
    let mut ledger = Ledger::default();
    let visual = Review::default();
    let revision = [1, 0, 0];
    assert!(strict_blocker(&config, &ledger, revision, &visual).is_none());
    config.strict_requirements = true;
    assert!(
        strict_blocker(&config, &ledger, revision, &visual)
            .unwrap()
            .contains("input-contract")
    );
    ledger.observe("run-boundaries", "a", CheckStatus::Passed, revision, true);
    ledger.observe(
        "run-errors",
        "b",
        CheckStatus::EnvironmentUnavailable,
        revision,
        true,
    );
    assert!(strict_blocker(&config, &ledger, revision, &visual).is_some());
    ledger.observe("run-errors", "c", CheckStatus::Passed, revision, true);
    assert!(strict_blocker(&config, &ledger, revision, &visual).is_none());
    assert!(strict_blocker(&config, &ledger, [2, 0, 0], &visual).is_some());
    config.requirements.clear();
    assert!(strict_blocker(&config, &ledger, revision, &visual).is_none());
}

#[test]
fn mapped_deliverables_require_all_contracts_but_unmapped_work_is_unassessed() {
    let mut config = contract();
    config
        .requirements
        .get_mut("input-contract")
        .unwrap()
        .deliverables = vec!["ship".into()];
    config.requirements.insert(
        "another".into(),
        VerificationRequirement {
            checks: vec!["errors".into()],
            deliverables: vec!["ship".into()],
            ..Default::default()
        },
    );
    let mut ledger = Ledger::default();
    let visual = Review::default();
    let working = json!({"deliverables":[
        {"id":"ship","status":"satisfied","evidence_refs":["real-file.rs"]},
        {"id":"explanation","status":"satisfied","evidence_refs":["answer"]}
    ]});
    let revision = [1, 0, 0];
    ledger.observe("run-boundaries", "b", CheckStatus::Passed, revision, true);
    let coverage = snapshot(&config, &ledger, revision, &visual);
    assert_eq!(
        deliverable_acceptance(&coverage, "explanation")["status"],
        "unassessed"
    );
    assert_eq!(
        deliverable_acceptance(&coverage, "ship")["requirements"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        deliverable_blocker(&coverage, &working)
            .unwrap()
            .contains("ship")
    );
    ledger.observe("run-errors", "c", CheckStatus::Passed, revision, true);
    let coverage = snapshot(&config, &ledger, revision, &visual);
    assert!(deliverable_blocker(&coverage, &working).is_none());
    assert_eq!(
        deliverable_acceptance(&coverage, "ship")["status"],
        "observed"
    );
    assert!(
        deliverable_blocker(&snapshot(&config, &ledger, [2, 0, 0], &visual), &working).is_some()
    );
}

#[test]
fn mapped_visual_contract_does_not_accept_an_unrelated_rendered_target() {
    use base64::Engine;
    let mut config = VerificationConfig::default();
    let target = "url:http://localhost:3000/";
    config.requirements.insert(
        "appearance".into(),
        VerificationRequirement {
            visual: true,
            visual_target: Some(target.into()),
            deliverables: vec!["ship".into()],
            ..Default::default()
        },
    );
    let bytes = base64::engine::general_purpose::STANDARD.decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII="
    ).unwrap();
    let images = json!([crate::llm::images::inline_attachment(&bytes).unwrap()]);
    let ledger = Ledger::default();
    let mut visual = Review::default();
    for (locator, expected) in [
        ("url:http://localhost:4000/", "needs_verification"),
        (target, "observed"),
    ] {
        let output =
            crate::runtime::tool_output::ToolOutput::ok("capture".into()).with_captures(vec![
                crate::runtime::execution::NativeCapture::new(
                    &bytes,
                    "image/png",
                    Some(locator.into()),
                ),
            ]);
        visual.capture_output(&output, [0; 3], &images);
        visual.delivered();
        assert_eq!(
            deliverable_acceptance(&snapshot(&config, &ledger, [0; 3], &visual), "ship")["status"],
            expected
        );
    }
}
