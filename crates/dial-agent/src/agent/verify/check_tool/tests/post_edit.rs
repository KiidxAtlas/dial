use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_review_disabled as with_agent;

const OVERLAY: &str = "approval='auto'\n[compaction]\nenabled=false\n[harness]\nedit_mode='replace'\n[edit]\nstreaming_abort=false\n[verification]\nautomatic_checks=false\n";
const COMMAND: &str = "grep -q after candidate.txt";

fn args(tool: &str, check: Option<bool>, old: &str) -> Value {
    let mut args = if tool == "write" {
        json!({"path":"candidate.txt","content":"after\n"})
    } else {
        json!({"path":"candidate.txt","old_string":old,"new_string":"after"})
    };
    if let Some(check) = check {
        args["check"] = json!(check);
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
fn opted_mutations_check_the_committed_bytes_and_preserve_separate_outcomes() {
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
                let out = dispatch_mutation(agent, tool, args(tool, Some(true), "before"));
                assert!(!out.is_error, "the {tool} succeeded: {}", out.text);
                assert!(!out.execution.changed_paths.is_empty());
                assert_eq!(
                    std::fs::read_to_string(agent.workspace.root().join("candidate.txt")).unwrap(),
                    "after\n"
                );
                let check = post_mutation_check(&out).expect("attached result");
                assert!(
                    out.data
                        .as_ref()
                        .unwrap()
                        .get(format!("post_{tool}_check"))
                        .is_some()
                );
                assert_eq!(check["is_error"], status != "passed");
                assert_eq!(check["data"]["results"][0]["status"], status);
                assert_eq!(agent.step_checks.mutations, 1);
                let revision = agent.step_checks.revision();
                assert_eq!(check["data"]["revision"], json!(revision));
                assert_eq!(
                    agent.step_checks.evidence.current_status(command, revision),
                    status
                );
            });
        }
    }
}

#[test]
fn omitted_false_and_refused_mutations_do_not_run_checks() {
    for tool in ["edit", "write"] {
        for (check, refused) in [(None, false), (Some(false), false), (Some(true), true)] {
            with_agent(&overlay(COMMAND), |agent| {
                seed(agent);
                let mut arguments =
                    args(tool, check, if refused { "not present" } else { "before" });
                if tool == "write" && refused {
                    arguments["path"] = json!("../outside.txt");
                }
                let out = dispatch_mutation(agent, tool, arguments);
                assert_eq!(out.is_error, refused, "{tool}: {}", out.text);
                assert!(post_mutation_check(&out).is_none());
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
fn no_applicable_checks_is_reported_without_inventing_a_pass() {
    for tool in ["edit", "write"] {
        with_agent(OVERLAY, |agent| {
            seed(agent);
            let out = dispatch_mutation(agent, tool, args(tool, Some(true), "before"));
            assert!(!out.is_error, "{}", out.text);
            let check = post_mutation_check(&out).unwrap();
            assert!(
                check["text"]
                    .as_str()
                    .unwrap()
                    .contains("No focused check applies")
            );
            assert_eq!(check["data"]["results"], json!([]));
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
            let out = dispatch_mutation(agent, tool, args(tool, Some(true), "before"));
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
            let out = dispatch_mutation(agent, tool, args(tool, Some(true), "before"));
            assert!(!out.is_error, "{}", out.text);
            assert_eq!(
                post_mutation_check(&out).unwrap()["data"]["results"][0]["status"],
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
fn post_write_checks_can_verify_a_new_file_and_reject_invalid_options() {
    with_agent(&overlay(COMMAND), |agent| {
        let mut arguments = args("write", Some(true), "");
        arguments["check"] = json!("yes");
        let refused = dispatch_mutation(agent, "write", arguments);
        assert!(refused.is_error, "{}", refused.text);
        assert!(!agent.workspace.root().join("candidate.txt").exists());
        let out = dispatch_mutation(agent, "write", args("write", Some(true), ""));
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            post_mutation_check(&out).unwrap()["data"]["results"][0]["status"],
            "passed"
        );
        assert_eq!(agent.step_checks.mutations, 1);
    });
}
