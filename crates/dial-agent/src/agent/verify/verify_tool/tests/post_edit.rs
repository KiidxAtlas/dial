use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_review_disabled as with_agent;
use crate::agent::verify::verify_tool::{VerifyBatch, VerifyStep};

const OVERLAY: &str = "approval='auto'\n[compaction]\nenabled=false\n[harness]\nedit_mode='replace'\n[edit]\nstreaming_abort=false\n[verification]\nautomatic_checks=false\n";
const COMMAND: &str = "grep -q after candidate.txt";

#[test]
fn eval_only_batches_deduplicate_native_receipts_without_provider_identity() {
    let mut batch = VerifyBatch::default();
    batch.begin(&[ToolCall {
        id: String::new(),
        name: "eval".into(),
        arguments: json!({}),
        malformed_arguments: false,
    }]);
    let mut first = crate::runtime::execution::Execution {
        invocation: Some(crate::runtime::execution::Invocation::fresh()),
        ..Default::default()
    };
    let mut duplicate = first.clone();
    assert!(batch.settle_once(&mut first));
    assert!(!batch.settle_once(&mut duplicate));
    batch.end();
    assert!(
        !batch.settle_once(&mut first),
        "batch retirement never revives a settled receipt"
    );
    assert!(
        batch.settle_once(&mut Default::default()),
        "a new direct invocation remains distinct"
    );
}

#[test]
fn callback_writes_settle_before_a_later_cell_failure_without_double_counting() {
    use crate::tools::exec::background::AdoptedJob;
    use crate::tools::integrations::ext::{Tool, ToolCallback, ToolContext, ToolEvent};
    use std::sync::{Arc, atomic::AtomicBool};
    struct Cell;
    impl Tool for Cell {
        fn name(&self) -> &str {
            "cell"
        }
        fn schema(&self) -> Value {
            crate::runtime::schema::tool_schema("cell", "test callback cell", json!({}), &[])
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn supports_tool_dispatch(&self) -> bool {
            true
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            unreachable!()
        }
        fn call_with_dispatch(
            &self,
            _: &ToolContext,
            _: &Value,
            _: &(dyn Fn(ToolEvent) + Sync),
            _: &AtomicBool,
            _: &dyn Fn(AdoptedJob) -> std::result::Result<String, String>,
            dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
        ) -> ToolOutput {
            let mut receipts = Vec::new();
            for path in ["first.txt", "second.txt"] {
                let output = dispatch(ToolCallback::Call {
                    name: "write".into(),
                    arguments: json!({"path":path,"content":"committed\n"}),
                });
                assert!(!output.is_error, "{}", output.text);
                let invocation = output
                    .execution
                    .invocation
                    .expect("native callback identity");
                receipts.push(json!({"id":invocation.id,"parent":invocation.parent,
                    "settled":output.execution.mutation_settled}));
            }
            let mut output = ToolOutput::err("cell failed after completed writes");
            output.data = Some(json!({"receipts":receipts}));
            output
        }
    }
    with_agent(OVERLAY, |agent| {
        agent.ext.register(Arc::new(Cell));
        let mut backend = agent.backend.take().unwrap();
        let call = ToolCall {
            id: "outer-cell".into(),
            name: "cell".into(),
            arguments: json!({}),
            malformed_arguments: false,
        };
        let mut output = agent.dispatch(&mut backend, &call, &mut NullUi);
        assert!(output.is_error);
        assert_eq!(
            output.execution.effects,
            tools::surface::outcome::Effects::Known
        );
        assert_eq!(output.execution.changed_paths.len(), 2);
        assert_eq!(
            agent.step_checks.mutations, 2,
            "each callback is accounted once"
        );
        let receipts = &output.data.as_ref().unwrap()["receipts"];
        assert_ne!(receipts[0]["id"], receipts[1]["id"]);
        let parent = output.execution.invocation.unwrap().id;
        for receipt in receipts.as_array().unwrap() {
            assert_eq!(receipt["parent"], parent);
            assert_eq!(receipt["settled"], true);
        }
        agent.settle_tool_mutation(&call, &mut output);
        assert_eq!(agent.step_checks.mutations, 2);
        for path in ["first.txt", "second.txt"] {
            assert_eq!(
                std::fs::read_to_string(agent.workspace.root().join(path)).unwrap(),
                "committed\n"
            );
        }
        agent.backend = Some(backend);
    });
}

#[test]
fn empty_provider_ids_and_payload_fields_cannot_conflate_or_forge_settlement() {
    with_agent(OVERLAY, |agent| {
        let call = ToolCall {
            id: String::new(),
            name: "write".into(),
            arguments: json!({"path":"candidate.txt"}),
            malformed_arguments: false,
        };
        for _ in 0..2 {
            let mut output = ToolOutput::ok("committed".into());
            output
                .execution
                .changed_paths
                .push(agent.workspace.root().join("candidate.txt"));
            output.data = Some(json!({"post_write_verify":{"is_error":false}}));
            agent.settle_tool_mutation(&call, &mut output);
            let revision = agent.step_checks.revision();
            let mut copy = output.clone();
            agent.settle_tool_mutation(&call, &mut copy);
            assert_eq!(
                agent.step_checks.revision(),
                revision,
                "settled receipt survives cloning"
            );
        }
        assert_eq!(
            agent.step_checks.mutations, 2,
            "empty provider IDs name distinct invocations"
        );
    });
}

fn args(tool: &str, verify: Option<Value>, old: &str) -> Value {
    let mut args = if tool == "write" {
        json!({"path":"candidate.txt","content":"after\n"})
    } else {
        json!({"path":"candidate.txt","old_string":old,"new_string":"after"})
    };
    if let Some(verify) = verify {
        args["verify"] = verify;
    }
    args
}

fn seed(agent: &Agent<'_>) {
    let path = agent.workspace.root().join("candidate.txt");
    std::fs::write(&path, "before\n").unwrap();
    let read = tools::execute(
        &agent.workspace,
        "read",
        &json!({"path":"candidate.txt"}),
        20,
    );
    assert!(!read.is_error, "{}", read.text);
}

fn dispatch_mutation(agent: &mut Agent<'_>, tool: &str, args: Value) -> ToolOutput {
    let mut backend = agent.backend.take().unwrap();
    let output = agent.dispatch(
        &mut backend,
        &ToolCall {
            id: format!("post-{tool}"),
            name: tool.into(),
            arguments: args,
            malformed_arguments: false,
        },
        &mut NullUi,
    );
    agent.backend = Some(backend);
    output
}

fn overlay(command: &str) -> String {
    format!(
        "{OVERLAY}\n[[verification.commands]]\nname='candidate'\ncommand='{command}'\ntimeout_secs=5\n"
    )
}

#[test]
fn verified_mutations_check_the_committed_bytes_and_preserve_separate_outcomes() {
    for tool in ["edit", "write"] {
        for (command, status) in [
            (COMMAND, "passed"),
            (
                "grep -q absent candidate.txt || { echo AssertionError: candidate missing; exit 1; }",
                "code_failed",
            ),
        ] {
            with_agent(&overlay(command), |agent| {
                seed(agent);
                let out = dispatch_mutation(
                    agent,
                    tool,
                    args(tool, Some(json!(["candidate"])), "before"),
                );
                assert!(!out.is_error, "the {tool} succeeded: {}", out.text);
                assert!(!out.execution.changed_paths.is_empty());
                assert_eq!(
                    std::fs::read_to_string(agent.workspace.root().join("candidate.txt")).unwrap(),
                    "after\n"
                );
                let verify = post_mutation_verify(&out).expect("attached result");
                assert!(
                    out.data
                        .as_ref()
                        .unwrap()
                        .get(format!("post_{tool}_verify"))
                        .is_some()
                );
                assert_eq!(verify["is_error"], status != "passed");
                assert_eq!(verify["data"]["results"][0]["status"], status);
                let summary = if status == "passed" {
                    "Verify: Passed 1, Failed 0"
                } else {
                    "Verify: Passed 0, Failed 1"
                };
                assert!(out.text.contains(summary), "{}", out.text);
                assert_eq!(agent.step_checks.mutations, 1);
                let revision = agent.step_checks.revision();
                assert_eq!(verify["data"]["revision"], json!(revision));
                assert_eq!(
                    agent.step_checks.evidence.current_status(command, revision),
                    status
                );
            });
        }
    }
}

#[test]
fn omitted_empty_and_refused_mutations_do_not_run_checks() {
    for tool in ["edit", "write"] {
        for (verify, refused) in [
            (None, false),
            (Some(json!([])), false),
            (Some(json!(["candidate"])), true),
        ] {
            with_agent(&overlay(COMMAND), |agent| {
                seed(agent);
                let mut arguments = args(
                    tool,
                    verify.clone(),
                    if refused { "not present" } else { "before" },
                );
                if tool == "write" && refused {
                    arguments["path"] = json!("../outside.txt");
                }
                let out = dispatch_mutation(agent, tool, arguments);
                assert_eq!(out.is_error, refused, "{tool}: {}", out.text);
                assert!(post_mutation_verify(&out).is_none());
                if refused {
                    assert!(out.text.contains("verify not run"), "{}", out.text);
                }
                assert_eq!(agent.step_checks.evidence.latest_status(COMMAND), None);
                assert_eq!(
                    std::fs::read_to_string(agent.workspace.root().join("candidate.txt")).unwrap(),
                    if refused { "before\n" } else { "after\n" }
                );
            });
        }
    }
}

#[test]
fn an_unresolvable_target_is_reported_without_inventing_a_pass() {
    for tool in ["edit", "write"] {
        with_agent(OVERLAY, |agent| {
            seed(agent);
            let out = dispatch_mutation(
                agent,
                tool,
                args(tool, Some(json!(["missing_test.py"])), "before"),
            );
            assert!(!out.is_error, "the {tool} still applied: {}", out.text);
            let verify = post_mutation_verify(&out).unwrap();
            assert_eq!(verify["is_error"], true);
            assert!(
                verify["text"].as_str().unwrap().contains("does not exist"),
                "{verify}"
            );
            assert!(verify["data"].is_null(), "nothing ran: {verify}");
        });
    }
}

#[test]
fn disabled_verification_refuses_the_option_before_mutating() {
    let overlay = OVERLAY.replace(
        "automatic_checks=false",
        "automatic_checks=false\nenabled=false",
    );
    for tool in ["edit", "write"] {
        with_agent(&overlay, |agent| {
            seed(agent);
            let out = dispatch_mutation(
                agent,
                tool,
                args(tool, Some(json!(["candidate"])), "before"),
            );
            assert!(out.is_error, "{}", out.text);
            assert!(out.text.contains("verification.enabled"), "{}", out.text);
            assert_eq!(
                std::fs::read_to_string(agent.workspace.root().join("candidate.txt")).unwrap(),
                "before\n"
            );
            assert!(out.execution.changed_paths.is_empty());
        });
    }
}

#[test]
fn denied_post_mutation_command_leaves_the_change_applied_and_runs_no_process() {
    for tool in ["edit", "write"] {
        with_agent(&overlay("touch unauthorized"), |agent| {
            seed(agent);
            agent.approval = crate::config::types::Approval::Ask;
            agent
                .always
                .push(crate::agent::safety::approval::PermissionRule {
                    effect: crate::agent::safety::approval::Effect::Allow,
                    tool: tool.into(),
                    command_prefix: vec![],
                    paths: vec![],
                });
            let out = dispatch_mutation(
                agent,
                tool,
                args(tool, Some(json!(["candidate"])), "before"),
            );
            assert!(!out.is_error, "{}", out.text);
            assert_eq!(
                post_mutation_verify(&out).unwrap()["data"]["results"][0]["status"],
                "permission_denied"
            );
            assert!(!agent.workspace.root().join("unauthorized").exists());
            assert_eq!(
                std::fs::read_to_string(agent.workspace.root().join("candidate.txt")).unwrap(),
                "after\n"
            );
        });
    }
}

#[test]
fn post_write_verify_can_check_a_new_file_and_rejects_invalid_options() {
    with_agent(&overlay(COMMAND), |agent| {
        let refused = dispatch_mutation(agent, "write", args("write", Some(json!("yes")), ""));
        assert!(refused.is_error, "{}", refused.text);
        assert!(!agent.workspace.root().join("candidate.txt").exists());
        let out = dispatch_mutation(
            agent,
            "write",
            args("write", Some(json!(["candidate"])), ""),
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            post_mutation_verify(&out).unwrap()["data"]["results"][0]["status"],
            "passed"
        );
        assert_eq!(agent.step_checks.mutations, 1);
    });
}

/// A model-written unittest file runs under the harness-chosen runner; its counts and the failure
/// trace reach the result, and the run is behavioral evidence.
#[test]
fn a_written_test_file_runs_with_counts_and_traces() {
    const PASSING: &str = "import unittest\n\nclass T(unittest.TestCase):\n    def test_a(self):\n        self.assertEqual(1, 1)\n\n    def test_b(self):\n        self.assertTrue(True)\n";
    let failing = PASSING.replace("self.assertTrue(True)", "self.assertEqual(2, 3)");
    for (source, summary, trace) in [
        (PASSING.to_owned(), "Verify: Passed 2, Failed 0", None),
        (
            failing,
            "Verify: Passed 1, Failed 1",
            Some("AssertionError"),
        ),
    ] {
        with_agent(OVERLAY, |agent| {
            let out = dispatch_mutation(
                agent,
                "write",
                json!({"path":"test_calc.py","content":source,"verify":["test_calc.py"]}),
            );
            assert!(!out.is_error, "{}", out.text);
            assert!(out.text.contains(summary), "{}", out.text);
            let verify = post_mutation_verify(&out).unwrap();
            let result = &verify["data"]["results"][0];
            assert_eq!(result["name"], "verify: test_calc.py");
            assert!(
                result["command"].as_str().unwrap().contains("-m unittest"),
                "{result}"
            );
            assert_eq!(verify["is_error"], trace.is_some());
            if let Some(trace) = trace {
                assert!(out.text.contains(trace), "{}", out.text);
            }
            assert_eq!(agent.behavioral_checks, vec![trace.is_none()]);
        });
    }
}

/// Entries collected from every mutation run once, at the batch's last one; a failed last
/// mutation runs nothing, and a call outside the batch is its own batch.
#[test]
fn batch_entries_run_once_after_the_last_mutation() {
    let call = |id: &str, name: &str, verify: Option<&[&str]>| ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: verify.map_or(json!({}), |verify| json!({ "verify": verify })),
        malformed_arguments: false,
    };
    let test = call("1", "write", Some(&["test_a.py"]));
    let read = call("2", "read", None);
    let source = call("3", "edit", Some(&["test_a.py", "test_b.py"]));
    let mut batch = VerifyBatch::default();
    batch.begin(&[test.clone(), read, source.clone()]);
    assert!(matches!(
        batch.mutation_finished(&test, true),
        VerifyStep::Deferred
    ));
    match batch.mutation_finished(&source, true) {
        VerifyStep::Run(targets) => assert_eq!(targets, ["test_a.py", "test_b.py"]),
        _ => panic!("the last mutation runs every collected entry"),
    }

    let plain = call("4", "edit", None);
    batch.begin(&[test.clone(), plain.clone()]);
    assert!(matches!(
        batch.mutation_finished(&test, true),
        VerifyStep::Deferred
    ));
    assert!(
        matches!(
            batch.mutation_finished(&plain, false),
            VerifyStep::Skipped(1)
        ),
        "a failed last edit runs nothing"
    );

    batch.begin(&[test.clone(), plain.clone()]);
    assert!(matches!(
        batch.mutation_finished(&test, false),
        VerifyStep::Nothing
    ));
    assert!(matches!(
        batch.mutation_finished(&plain, true),
        VerifyStep::Nothing
    ));

    batch.end();
    match batch.mutation_finished(&test, true) {
        VerifyStep::Run(targets) => assert_eq!(targets, ["test_a.py"]),
        _ => panic!("outside a batch a mutation runs its own entries"),
    }
}
