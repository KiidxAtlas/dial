use super::*;
use crate::{config::types::VerificationCommand, runtime::execution::ProcessOutcome};
use std::fs;

fn fixture() -> (tempfile::TempDir, Workspace, VerificationConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("source.txt"), "candidate").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let config = VerificationConfig {
        commands: vec![VerificationCommand {
            name: "behavior".into(),
            command: "configured-command".into(),
            file_extensions: vec![],
            timeout_secs: 10,
        }],
        inputs: [(
            "behavior".into(),
            VerificationInputs {
                paths: vec!["source.txt".into()],
                environment: vec![],
            },
        )]
        .into(),
        ..Default::default()
    };
    (dir, workspace, config)
}

fn observed(code: i32) -> ToolOutput {
    let output = if code == 0 {
        ToolOutput::ok("1 test passed".into())
    } else {
        ToolOutput::err("assertion failed")
    };
    output.with_process(ProcessOutcome {
        started: true,
        exit_code: Some(code),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    })
}

#[test]
fn only_observed_current_configured_checks_accept_a_candidate() {
    let (_dir, workspace, config) = fixture();
    for (output, expected) in [
        (observed(0), State::Accepted),
        (observed(1), State::Rejected),
        (
            ToolOutput::ok("passed; evidence: artifact://obsolete-report".into()),
            State::NeedsVerification,
        ),
        (
            ToolOutput::ok("running 0 tests\n".into()).with_process(ProcessOutcome {
                started: true,
                exit_code: Some(0),
                output_complete: true,
                signal: None,
                timed_out: false,
                cancelled: false,
            }),
            State::NeedsVerification,
        ),
    ] {
        let receipt = evaluate_with(
            &workspace,
            &config,
            &["behavior".into()],
            |_| output.clone(),
            || false,
        );
        assert_eq!(
            receipt.state, expected,
            "{}; output={}",
            receipt.reason, output.text
        );
    }
    let unrelated = evaluate_with(
        &workspace,
        &config,
        &["other".into()],
        |_| panic!("unknown check must not execute"),
        || false,
    );
    assert_eq!(unrelated.state, State::NeedsVerification);
    let mut missing = config;
    missing.inputs.clear();
    let receipt = evaluate_with(
        &workspace,
        &missing,
        &["behavior".into()],
        |_| panic!("missing input contract must not execute"),
        || false,
    );
    assert_eq!(receipt.state, State::NeedsVerification);
}

#[test]
fn parent_validation_is_input_and_configuration_bound_and_sticky() {
    for change_config in [false, true] {
        let (dir, workspace, mut config) = fixture();
        let mut receipt = evaluate_with(
            &workspace,
            &config,
            &["behavior".into()],
            |_| observed(0),
            || false,
        );
        assert_eq!(receipt.state, State::Accepted);
        if change_config {
            config.commands[0].command = "different-command".into();
        } else {
            fs::write(dir.path().join("source.txt"), "concurrent edit").unwrap();
        }
        receipt.revalidate(&workspace, &config);
        assert_eq!(receipt.state, State::NeedsVerification);
        fs::write(dir.path().join("source.txt"), "candidate").unwrap();
        config.commands[0].command = "configured-command".into();
        receipt.revalidate(&workspace, &config);
        assert_eq!(
            receipt.state,
            State::NeedsVerification,
            "restored bytes cannot revive stale evidence"
        );
    }
}

#[test]
fn a_check_mutating_its_inputs_never_earns_acceptance() {
    let (dir, workspace, config) = fixture();
    let receipt = evaluate_with(
        &workspace,
        &config,
        &["behavior".into()],
        |_| {
            fs::write(dir.path().join("source.txt"), "changed during check").unwrap();
            observed(0)
        },
        || false,
    );
    assert_eq!(receipt.state, State::NeedsVerification);
}

#[test]
fn contracted_promotion_revalidates_parent_dependencies_inside_rollback_boundary() {
    use crate::agent::workers::isolation::Isolated;
    use std::sync::atomic::AtomicBool;
    for changed_dependency in [false, true] {
        let (dir, workspace, mut config) = fixture();
        fs::write(dir.path().join("dependency.txt"), "baseline").unwrap();
        config
            .inputs
            .get_mut("behavior")
            .unwrap()
            .paths
            .push("dependency.txt".into());
        let isolated = Isolated::with_config(
            &workspace,
            &Default::default(),
            crate::config::isolation::Preference::Copy,
        )
        .unwrap();
        fs::write(isolated.root().join("source.txt"), "new candidate").unwrap();
        let mut receipt = evaluate_with(
            isolated.workspace(),
            &config,
            &["behavior".into()],
            |_| observed(0),
            || false,
        );
        assert_eq!(receipt.state, State::Accepted);
        if changed_dependency {
            fs::write(dir.path().join("dependency.txt"), "parent edit").unwrap();
        }
        let merged = isolated.merge_checked(&workspace, &AtomicBool::new(false), |target| {
            receipt.revalidate(target, &config);
            anyhow::ensure!(receipt.state == State::Accepted, "inputs invalidated");
            Ok(())
        });
        assert_eq!(merged.integrated, !changed_dependency);
        assert_eq!(
            fs::read_to_string(dir.path().join("source.txt")).unwrap(),
            if changed_dependency {
                "candidate"
            } else {
                "new candidate"
            }
        );
        if changed_dependency {
            assert_eq!(receipt.state, State::NeedsVerification);
            assert!(merged.applied.is_empty());
            assert_eq!(
                fs::read_to_string(dir.path().join("dependency.txt")).unwrap(),
                "parent edit"
            );
            fs::remove_dir_all(merged.kept.unwrap()).unwrap();
        }
    }
}
