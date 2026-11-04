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

#[test]
fn contracts_restore_requires_matching_fresh_checks_and_invalidates_downstream() {
    use crate::agent::workers::contracts::{Contract, Ledger};
    let (dir, workspace, config) = fixture();
    let artifacts = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(artifacts.path()).unwrap();
    let output = store.retain("immutable candidate").unwrap();
    let workspace = workspace.with_artifacts(store);
    let receipt = || {
        evaluate_with(
            &workspace,
            &config,
            &["behavior".into()],
            |_| observed(0),
            || false,
        )
    };
    let mut ledger = Ledger::default();
    for (id, dependencies) in [
        ("root", vec![]),
        ("child", vec!["root".into()]),
        ("leaf", vec!["child".into()]),
    ] {
        ledger
            .begin(
                &Contract {
                    id: id.into(),
                    depends_on: dependencies,
                },
                json!({"task":id, "acceptance_checks":["behavior"]}),
            )
            .unwrap();
        ledger.attach(id, &format!("task-{id}")).unwrap();
        ledger
            .settle(id, "done", receipt(), Some(output.clone()))
            .unwrap();
    }
    ledger.refresh(&workspace, &config);
    assert!(ledger.blocker().is_none());
    let snapshot = ledger.snapshot();
    let mut restored = Ledger::restore(&snapshot);
    assert!(restored.blocker().is_some());
    assert!(restored.running().is_empty());
    assert!(
        restored.checks("child").is_err(),
        "verify in dependency order"
    );
    for id in ["root", "child", "leaf"] {
        restored.checks(id).unwrap();
        restored.rechecked(id, receipt()).unwrap();
    }
    restored.refresh(&workspace, &config);
    assert!(restored.blocker().is_none());
    assert_eq!(restored.node("root").unwrap()["output"], output);
    fs::write(dir.path().join("source.txt"), "changed input").unwrap();
    restored.refresh(&workspace, &config);
    for id in ["root", "child", "leaf"] {
        assert_eq!(restored.node(id).unwrap()["state"], "needs_verification");
    }
    // A fresh pass for different inputs does not certify the old retained candidate.
    restored.rechecked("root", receipt()).unwrap();
    assert!(restored.blocker().is_some());
    assert_eq!(
        restored.node("root").unwrap()["state"],
        "needs_verification"
    );
    fs::write(dir.path().join("source.txt"), "candidate").unwrap();
    restored.refresh(&workspace, &config);
    assert!(
        restored.blocker().is_some(),
        "restored bytes do not revive evidence"
    );
    restored.rechecked("root", receipt()).unwrap();
    assert_eq!(
        restored.node("child").unwrap()["state"],
        "needs_verification"
    );
    restored.rechecked("child", receipt()).unwrap();
    restored.rechecked("leaf", receipt()).unwrap();
    assert!(restored.blocker().is_none());
}

#[test]
fn contracts_lost_output_and_unknown_effects_never_count_as_acceptance() {
    use crate::agent::workers::contracts::{Contract, Ledger};
    let (_dir, workspace, config) = fixture();
    let mut ledger = Ledger::default();
    ledger
        .begin(
            &Contract {
                id: "root".into(),
                depends_on: vec![],
            },
            json!({"acceptance_checks":["behavior"]}),
        )
        .unwrap();
    ledger.attach("root", "task-1").unwrap();
    let receipt = evaluate_with(
        &workspace,
        &config,
        &["behavior".into()],
        |_| observed(0),
        || false,
    );
    ledger
        .settle(
            "root",
            "done",
            receipt,
            Some(format!("artifact://{}", "0".repeat(64))),
        )
        .unwrap();
    ledger.refresh(&workspace, &config);
    assert_eq!(ledger.node("root").unwrap()["state"], "needs_verification");
    assert!(ledger.blocker().is_some());
    let node = Contract {
        id: "unknown".into(),
        depends_on: vec![],
    };
    let request = json!({"acceptance_checks":["behavior"]});
    ledger.begin(&node, request.clone()).unwrap();
    ledger.attach("unknown", "task-2").unwrap();
    let mut receipt = Acceptance::unverified("check failed after unknown effects");
    receipt.reject("failed");
    ledger.settle("unknown", "lost", receipt, None).unwrap();
    assert!(ledger.begin(&node, request).is_err());
}

#[test]
fn running_contract_rechecks_upstream_authority_before_promotion_and_retains_stale_candidate() {
    use crate::agent::workers::{
        contracts::{Contract, Ledger},
        subagents::{self, Outcome, Status},
    };
    struct Approved;
    impl Approver for Approved {
        fn authorize(
            &mut self,
            _: &crate::llm::ToolCall,
            _: &str,
            _: bool,
        ) -> Result<(), crate::agent::safety::approval::Denial> {
            Ok(())
        }
    }
    // Upstream is deliberately not in the child's declared inputs: the ledger binding itself
    // must guard promotion, rather than accidentally relying on overlapping check inputs.
    for changed in [false, true] {
        let (dir, workspace, mut verification) = fixture();
        fs::write(dir.path().join("upstream.txt"), "upstream").unwrap();
        verification.commands[0].command = "test -f source.txt && printf '1 test passed\\n'".into();
        verification.commands.push(VerificationCommand {
            name: "upstream".into(),
            command: "unused".into(),
            file_extensions: vec![],
            timeout_secs: 5,
        });
        verification.inputs.insert(
            "upstream".into(),
            VerificationInputs {
                paths: vec!["upstream.txt".into()],
                environment: vec![],
            },
        );
        let artifacts = tempfile::tempdir().unwrap();
        let store = crate::storage::artifact::ArtifactStore::new(artifacts.path()).unwrap();
        let output = store.retain("upstream result").unwrap();
        let workspace = workspace.with_artifacts(store);
        let mut ledger = Ledger::default();
        ledger
            .begin(
                &Contract {
                    id: "upstream".into(),
                    depends_on: vec![],
                },
                json!({"acceptance_checks":["upstream"]}),
            )
            .unwrap();
        let receipt = evaluate_with(
            &workspace,
            &verification,
            &["upstream".into()],
            |_| observed(0),
            || false,
        );
        ledger
            .settle("upstream", "done", receipt, Some(output))
            .unwrap();
        ledger
            .begin(
                &Contract {
                    id: "child".into(),
                    depends_on: vec!["upstream".into()],
                },
                json!({"acceptance_checks":["behavior"]}),
            )
            .unwrap();
        ledger.attach("child", "task-child").unwrap();
        let mut config = crate::config::types::Config {
            verification,
            ..Default::default()
        };
        config.isolation.backend = crate::config::isolation::Preference::Copy;
        let mut request = subagents::parse(&json!({"agent":"worker","task":"edit","solution_space":"source only","acceptance_checks":["behavior"]}), &config).unwrap();
        request.jobs[0].promotion = Some(ledger.promotion("child"));
        let registry = crate::tools::integrations::ext::Registry::new();
        let harness = crate::config::Harness::default();
        let interrupt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let outcome = subagents::run_job(
            Env {
                config: &config,
                workspace: &workspace,
                ext: &registry,
                layout: None,
                harness: &harness,
                interrupt: &interrupt,
                usage: None,
                context_cap: None,
                memory: None,
                plan_mode: false,
                agents_messages: &[],
            },
            &request.jobs[0],
            &mut Approved,
            |env, _| {
                fs::write(env.workspace.root().join("source.txt"), "new candidate").unwrap();
                if changed {
                    fs::write(dir.path().join("upstream.txt"), "new upstream").unwrap();
                    // Controller revalidation happens while the child is still running.
                    ledger.refresh(&workspace, &config.verification);
                }
                let mut outcome = Outcome::incomplete(1, "unused");
                outcome.status = Status::done("worker says accepted".into());
                outcome
            },
        );
        let merge = outcome.isolation.unwrap();
        assert_eq!(merge.integrated, !changed, "{merge:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join("source.txt")).unwrap(),
            if changed {
                "candidate"
            } else {
                "new candidate"
            }
        );
        if changed {
            assert!(merge.applied.is_empty());
            assert_eq!(
                ledger.node("upstream").unwrap()["state"],
                "needs_verification"
            );
            let kept = merge.kept.expect("stale candidate retained");
            assert_eq!(
                fs::read_to_string(kept.join("source.txt")).unwrap(),
                "new candidate"
            );
            fs::remove_dir_all(kept).unwrap();
        }
    }
}
