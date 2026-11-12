use super::*;
use crate::agent::verify::verification::FocusedCheck;
use crate::config::types::{VerificationCommand, VerificationRequirement};

#[test]
fn plan_discovers_mapping_without_running_commands_or_changing_evidence() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config();
    config.verification.commands.push(VerificationCommand {
        name: "api-boundary".into(),
        command: "touch must-not-run".into(),
        file_extensions: vec!["rs".into()],
        timeout_secs: 10,
    });
    config.verification.requirements.insert(
        "api-contract".into(),
        VerificationRequirement {
            checks: vec!["api-boundary".into()],
            deliverables: vec!["api-change".into()],
            ..Default::default()
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    let revision = agent.step_checks.revision();
    let before = agent.step_checks.evidence.snapshot(revision);
    let result = agent.verify_tool(&json!({"plan":true}), &mut NullUi);
    assert!(!result.is_error, "{}", result.text);
    assert!(
        result.text.contains("deliverables [api-change]"),
        "model-facing plan text must name the requirement deliverable: {}",
        result.text
    );
    let data = result.data.unwrap();
    assert_eq!(data["mode"], "plan");
    assert_eq!(data["ran"], false);
    let check = data["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "api-boundary")
        .unwrap();
    assert_eq!(check["requirements"], json!(["api-contract"]));
    assert_eq!(
        data["requirements"]["requirements"][0]["deliverables"],
        json!(["api-change"])
    );
    assert_eq!(data["requirements"]["observed"], 0);
    assert!(!root.path().join("must-not-run").exists());
    assert_eq!(agent.step_checks.evidence.snapshot(revision), before);
}

#[test]
fn plan_reports_stale_and_absent_contracts_without_promoting_evidence() {
    let root = tempfile::tempdir().unwrap();
    let config = config();
    let mut agent = hermetic_agent(&config, root.path());
    agent
        .step_checks
        .evidence
        .observe(UNIT, "old", CheckStatus::Passed, [9, 9, 9], false);
    let before = agent
        .step_checks
        .evidence
        .snapshot(agent.step_checks.revision());
    let result = agent.verify_tool(&json!({"plan":true}), &mut NullUi);
    assert!(result.text.contains("No operator-declared requirements"));
    let data = result.data.unwrap();
    assert_eq!(
        data["requirements"]["coverage"],
        "not_independently_assessed"
    );
    let unit = data["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "unit")
        .unwrap();
    assert_ne!(unit["recorded_status"], "passed");
    assert_eq!(
        agent
            .step_checks
            .evidence
            .snapshot(agent.step_checks.revision()),
        before
    );
}

#[test]
fn plan_refuses_execution_arguments_and_invalid_types_without_running_them() {
    let root = tempfile::tempdir().unwrap();
    let config = config();
    let mut agent = hermetic_agent(&config, root.path());
    for args in [
        json!({"plan":"true"}),
        json!({"plan":null}),
        json!({"plan":true,"targets":["unit"]}),
        json!({"plan":true,"acceptance":{"name":"bad","command":"touch must-not-run"}}),
        json!({"plan":true,"repro":{"name":"bad","command":"touch must-not-run"}}),
    ] {
        assert!(agent.verify_tool(&args, &mut NullUi).is_error, "{args}");
    }
    assert!(!root.path().join("must-not-run").exists());
    assert_eq!(agent.step_checks.evidence.latest_status(UNIT), None);
    assert!(agent.step_checks.evidence.repros().is_empty());
    // An explicit false keeps the legacy omitted-target behavior.
    let legacy = agent.verify_tool(&json!({}), &mut NullUi);
    let disabled = agent.verify_tool(&json!({"plan":false}), &mut NullUi);
    assert_eq!(legacy.text, disabled.text);
    assert_eq!(legacy.data, disabled.data);
}

#[test]
fn plan_bounds_check_preview_and_discloses_omissions() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    for id in 0..70 {
        config.verification.commands.push(VerificationCommand {
            name: format!("check-{id}"),
            command: format!("test -f fixture-{id}"),
            file_extensions: vec![],
            timeout_secs: 1,
        });
    }
    let mut agent = hermetic_agent(&config, root.path());
    let result = agent.verify_tool(&json!({"plan":true}), &mut NullUi);
    assert!(!result.is_error, "{}", result.text);
    assert!(result.text.contains("54 additional check(s)"));
    let data = result.data.unwrap();
    assert_eq!(data["checks"].as_array().unwrap().len(), 64);
    assert_eq!(data["omitted_checks"], 6);
    assert_eq!(data["check_count"], 70);
}

fn requirement_row(result: &ToolOutput, id: &str) -> Value {
    result.data.as_ref().unwrap()["requirements"]["requirements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap()
        .clone()
}

#[test]
fn plan_preserves_retained_failure_obligations_that_normal_selection_prunes() {
    let root = tempfile::tempdir().unwrap();
    let config = config();
    let mut agent = hermetic_agent(&config, root.path());
    // A Python syntax obligation for a file that no longer exists is obsolete for normal selection.
    let obsolete = FocusedCheck {
        name: "python AST: removed.py".into(),
        command: "test -f removed.py".into(),
        timeout_secs: 5,
    };
    let live = FocusedCheck {
        name: "lint".into(),
        command: LINT.into(),
        timeout_secs: 30,
    };
    agent.step_checks.failed_checks = vec![obsolete.clone(), live.clone()];
    agent.step_checks.failed = true;
    let before = agent.step_checks.failed_checks.clone();
    let result = agent.verify_tool(&json!({"plan":true}), &mut NullUi);
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(agent.step_checks.failed_checks, before);
    assert!(agent.step_checks.failed);
    let data = result.data.unwrap();
    let rows = data["checks"].as_array().unwrap();
    assert!(rows.iter().all(|row| row["command"] != obsolete.command));
    let lint = rows.iter().find(|row| row["command"] == LINT).unwrap();
    assert_eq!(lint["selected"], true);
    // The same retained list is pruned by normal selection, so plan's preservation is observable.
    agent
        .step_checks
        .include_failed_checks(root.path(), &mut Vec::new());
    assert_eq!(agent.step_checks.failed_checks, vec![live]);
}

#[test]
fn plan_discovers_visual_target_and_counts_only_a_capture_for_that_target() {
    use base64::Engine;
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    let target = "url:http://localhost:3000/";
    config.verification.requirements.insert(
        "appearance".into(),
        VerificationRequirement {
            visual: true,
            visual_target: Some(target.into()),
            ..Default::default()
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    let bytes = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=")
        .unwrap();
    let images = json!([crate::llm::images::inline_attachment(&bytes).unwrap()]);
    let before = agent.verify_tool(&json!({"plan":true}), &mut NullUi);
    let row = requirement_row(&before, "appearance");
    assert_eq!(row["visual_target"], target);
    assert_eq!(row["visual"], "missing");
    assert!(
        before.text.contains(&format!("visual missing ({target})")),
        "{}",
        before.text
    );
    for (locator, expected) in [
        ("url:http://localhost:4000/", "missing"),
        (target, "observed"),
    ] {
        let revision = agent.step_checks.revision();
        let output =
            crate::runtime::tool_output::ToolOutput::ok("capture".into()).with_captures(vec![
                crate::runtime::execution::NativeCapture::new(
                    &bytes,
                    "image/png",
                    Some(locator.into()),
                ),
            ]);
        agent
            .step_checks
            .visual
            .capture_output(&output, revision, &images);
        agent.step_checks.visual.delivered();
        let result = agent.verify_tool(&json!({"plan":true}), &mut NullUi);
        assert_eq!(
            requirement_row(&result, "appearance")["visual"],
            expected,
            "capture {locator}"
        );
        assert!(
            result
                .text
                .contains(&format!("visual {expected} ({target})")),
            "{}",
            result.text
        );
    }
}

#[test]
fn plan_text_caps_deliverables_while_structured_requirement_keeps_all() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    let deliverables: Vec<String> = (0..20).map(|id| format!("d-{id:02}")).collect();
    config.verification.requirements.insert(
        "wide".into(),
        VerificationRequirement {
            deliverables: deliverables.clone(),
            ..Default::default()
        },
    );
    let mut agent = hermetic_agent(&config, root.path());
    let result = agent.verify_tool(&json!({"plan":true}), &mut NullUi);
    assert!(!result.is_error, "{}", result.text);
    let line = result
        .text
        .lines()
        .find(|line| line.starts_with("Requirement wide ->"))
        .unwrap();
    // The text view lists the first 16 deliverables only; the 16th is the last one shown.
    assert!(line.contains("deliverables [d-00, "), "{line}");
    assert!(line.contains("d-15]"), "{line}");
    assert!(!line.contains("d-16"), "{line}");
    assert!(
        line.contains("4 more deliverable(s) in structured requirements"),
        "{line}"
    );
    assert_eq!(
        requirement_row(&result, "wide")["deliverables"],
        json!(deliverables)
    );
}
