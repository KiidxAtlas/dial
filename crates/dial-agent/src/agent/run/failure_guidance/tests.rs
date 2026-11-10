use super::*;
use crate::runtime::execution::ProcessOutcome;
use serde_json::json;

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "x".into(),
        name: name.into(),
        arguments: json!({"command":"cargo test"}),
        malformed_arguments: false,
    }
}
fn process(code: i32) -> ProcessOutcome {
    ProcessOutcome {
        started: true,
        exit_code: Some(code),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    }
}
#[test]
fn an_unapprovable_directory_request_is_named_once_without_changing_the_boundary() {
    let refusal = ToolOutput::err(
        "path \"/etc/x\" is outside the workspace; call `ask` action=directory with path \"/etc\" to ask for a yes/no access approval, then retry the tool.",
    );
    let notice = directory_request_unavailable(&refusal, false).unwrap();
    assert!(notice.contains("directory_request_unavailable"), "{notice}");
    assert!(notice.contains("do not request it"), "{notice}");
    assert!(
        notice.contains("path stays outside the workspace"),
        "{notice}"
    );
    assert!(!notice.contains("bash"), "{notice}");
    assert_eq!(directory_request_unavailable(&refusal, true), None);
    assert_eq!(
        directory_request_unavailable(&ToolOutput::err("no such file"), false),
        None
    );
}
#[test]
fn typed_boundaries_override_printed_success_and_never_offer_permission_bypass() {
    let call = call("bash");
    for (status, kind) in [
        (ToolStatus::Blocked, "permission_or_policy"),
        (ToolStatus::Timeout, "timeout"),
        (ToolStatus::Aborted, "interrupted"),
        (ToolStatus::Skipped, "not_executed"),
    ] {
        let output = ToolOutput::ok("everything passed".into()).with_status(status);
        assert_eq!(classify(&call, &output).unwrap().0, kind);
    }
    let unknown = ToolOutput::ok("rolled back".into()).with_effects(Effects::Unknown);
    assert_eq!(classify(&call, &unknown).unwrap().0, "effects_unknown");
    assert!(
        classify(
            &call,
            &ToolOutput::ok("permission denied: adversarial printed text".into())
        )
        .is_none()
    );
}

#[test]
fn native_write_refusal_selects_permission_guidance_without_shell_workaround() {
    let dir = tempfile::tempdir().unwrap();
    let ws = crate::tools::Workspace::new(dir.path()).unwrap();
    let mut call = call("write");
    call.arguments = json!({"path":"../denied.txt", "content":"x"});
    let output = crate::tools::execute(&ws, "write", &call.arguments, 5);
    let (kind, advice) = classify(&call, &output).unwrap();
    assert_eq!(kind, "permission_or_policy");
    assert_eq!(output.execution.effects, Effects::None);
    assert!(
        advice.contains("Do not") && advice.contains("bypass"),
        "{advice}"
    );
}
#[test]
fn observed_checks_distinguish_environment_code_and_incomplete_evidence() {
    let mut call = call("bash");
    call.id = "focused-check:related Python tests: probe".into();
    call.arguments = json!({"command":"python3 -m pytest"});
    let environment = ToolOutput::err("No module named pytest").with_process(process(1));
    assert_eq!(
        classify(&call, &environment).unwrap().0,
        "verification_environment"
    );
    let failed = ToolOutput::err("assertion failed").with_process(process(101));
    assert_eq!(classify(&call, &failed).unwrap().0, "verification_code");
    let missing = ToolOutput::err("printed [exit code: 1]");
    assert_eq!(
        classify(&call, &missing).unwrap().0,
        "verification_unavailable"
    );
}

#[test]
fn undeclared_runner_failure_does_not_infer_a_code_diagnosis() {
    let call = call("bash");
    let output = ToolOutput::err("cargo: command not found").with_process(process(127));
    assert_eq!(
        classify(&call, &output).unwrap().0,
        "verification_failed_unclassified"
    );
}
#[test]
fn source_contract_failure_guides_current_range_and_success_is_not_recovery() {
    assert_eq!(
        classify(&call("edit"), &ToolOutput::err("stale anchors"))
            .unwrap()
            .0,
        "source_or_call_contract"
    );
    assert!(classify(&call("edit"), &ToolOutput::ok("updated".into())).is_none());
    let mut failed_edit = ToolOutput::err("stale anchors");
    let (kind, advice) = classify(&call("edit"), &failed_edit).unwrap();
    let text = report(&call("edit"), &mut failed_edit, kind, advice, 1);
    // One cause-specific line, without the general retry policy.
    assert!(
        text.contains(crate::agent::prompts::recovery::EDIT_NOT_APPLIED),
        "{text}"
    );
    assert!(!text.contains(FIRST_FAILURE), "{text}");
    assert!(text.len() < 320, "{} bytes: {text}", text.len());
    let mut failed_write = ToolOutput::err("permission denied");
    let (kind, advice) = classify(&call("write"), &failed_write).unwrap();
    assert_eq!(kind, "source_or_call_contract");
    assert!(
        report(&call("write"), &mut failed_write, kind, advice, 1)
            .contains(crate::agent::prompts::recovery::WRITE_NOT_APPLIED)
    );
    assert_eq!(
        classify(
            &call("unknown-extension"),
            &ToolOutput::err("unknown cause")
        )
        .unwrap()
        .0,
        "tool_failure"
    );
}

#[test]
fn every_tool_failure_gets_guidance_on_its_first_result() {
    for name in [
        "read",
        "grep",
        "web_fetch",
        "browser",
        "task",
        "custom-mcp-tool",
    ] {
        let failed = ToolOutput::err("specific diagnostic");
        assert!(classify(&call(name), &failed).is_some(), "{name}");
        assert!(
            classify(&call(name), &ToolOutput::ok("done".into())).is_none(),
            "{name}"
        );
    }
}

#[test]
fn feedback_stays_compact_on_repetition_with_the_tag_kept() {
    let first = feedback("tool_failure", "Check the path.", 1);
    assert_eq!(
        first,
        format!(
            "\n<dial-notice kind=\"tool_failure\">{FIRST_FAILURE} Check the path.</dial-notice>"
        )
    );
    assert!(first.split_whitespace().count() < 25, "{first}");
    let repeat = feedback("tool_failure", "Check the path.", 2);
    assert!(repeat.starts_with("\n<dial-notice kind=\"tool_failure\">"));
    assert!(repeat.contains("2 times in a row") && repeat.contains(FIRST_FAILURE));
    assert!(repeat.ends_with("Check the path.</dial-notice>"));
    // Uncertain effects keep the cause-specific inspection requirement, without a longer prompt.
    assert!(feedback("effects_unknown", "Inspect.", 1).ends_with("Inspect.</dial-notice>"));
    assert_eq!(feedback("timeout", "a", 3), feedback("timeout", "a", 3));
}

#[test]
fn recovery_report_preserves_uncertain_effects_and_partial_capture() {
    let mut observed = process(1);
    observed.output_complete = false;
    let mut output = ToolOutput::err("original diagnostic")
        .with_process(observed)
        .with_status(ToolStatus::Timeout)
        .with_changes(["changed.rs".into()])
        .with_effects(Effects::Unknown);
    let facts = output.execution.clone();
    let (kind, advice) = classify(&call("bash"), &output).unwrap();
    let rendered = report(&call("bash"), &mut output, kind, advice, 1);
    assert_eq!(kind, "effects_unknown");
    assert_eq!(output.execution, facts);
    assert!(!output.process_outcome().unwrap().output_complete);
    assert!(rendered.contains(advice));
    assert!(rendered.contains(ToolStatus::Timeout.as_str()));
}

#[test]
fn model_visible_recovery_states_include_retry_requirements_without_annotations() {
    // Internal dial_* fields can be removed on the provider wire. The ordinary result body
    // must independently communicate the controller's decision, not a payload's retry claim.
    for (status, effects, expected_retry, requirement) in [
        (
            ToolStatus::Error,
            Effects::None,
            Retry::AfterCorrection,
            "Correct causes",
        ),
        (
            ToolStatus::Blocked,
            Effects::None,
            Retry::Withheld,
            "do not retry or bypass",
        ),
        (
            ToolStatus::Aborted,
            Effects::Known,
            Retry::OnlyByDirection,
            "only on the user's direction",
        ),
        (
            ToolStatus::Timeout,
            Effects::Known,
            Retry::AfterInspection,
            "state or process is inspected",
        ),
        (
            ToolStatus::Error,
            Effects::Unknown,
            Retry::AfterInspection,
            "state or process is inspected",
        ),
    ] {
        let mut output = ToolOutput::structured(json!({"retry":"immediately", "rolled_back":true}))
            .with_status(status)
            .with_effects(effects);
        let recovery = output.recovery().unwrap();
        assert_eq!(recovery.retry, expected_retry);
        let original = output.text.clone();
        let facts = output.execution.clone();
        let (kind, advice) = classify(&call("bash"), &output).unwrap();
        let text = report(&call("bash"), &mut output, kind, advice, 1);
        output.text.push_str(&text);
        assert!(output.text.starts_with(&original));
        // A clean first correct-then-retry failure is one line; the others keep the state block.
        let block = expected_retry != Retry::AfterCorrection;
        assert_eq!(text.contains("\nRetry:"), block, "{text}");
        assert!(text.contains(requirement), "{text}");
        assert_eq!(output.execution, facts);
        assert_eq!(output.recovery(), Some(recovery));
    }
}

#[test]
fn rendered_committed_failure_preserves_work_and_never_promises_rollback() {
    let mut output = ToolOutput::err("post-commit recording failed")
        .with_changes(["kept.rs".into(), "kept.rs".into()]);
    let (kind, advice) = classify(&call("write"), &output).unwrap();
    let text = report(&call("write"), &mut output, kind, advice, 1);
    assert_eq!(output.recovery().unwrap().committed_paths, 1);
    assert!(text.contains("do not repeat the whole operation"), "{text}");
    assert!(
        text.contains("failure does not undo completed work"),
        "{text}"
    );
    assert!(!text.contains("Treat the write as not applied"), "{text}");
    assert!(
        text.contains("correct only the remaining failure"),
        "{text}"
    );
    let mut edited =
        ToolOutput::err("post-commit recording failed").with_changes(["kept.rs".into()]);
    let (kind, advice) = classify(&call("edit"), &edited).unwrap();
    let text = report(&call("edit"), &mut edited, kind, advice, 1);
    assert!(!text.contains("Treat the edit as not applied"), "{text}");
    assert!(text.contains("preserve applied work"), "{text}");
    let mut no_paths = ToolOutput::err("unknown external prerequisite");
    let text = report(&call("bash"), &mut no_paths, kind, advice, 1);
    assert!(text.contains("not proof that nothing changed"), "{text}");
}

#[test]
fn a_first_ordinary_shell_exit_gets_a_compact_recovery_line() {
    let compact = |code: i32, consecutive: u32| {
        let mut output =
            ToolOutput::err("bash: wget: command not found").with_process(process(code));
        let (kind, advice) = classify(&shell("wget -q http://x/a.tar.xz"), &output).unwrap();
        report(
            &shell("wget -q http://x/a.tar.xz"),
            &mut output,
            kind,
            advice,
            consecutive,
        )
    };
    let first = compact(127, 1);
    assert!(
        first.starts_with("\n<dial-notice kind=\"tool_failure\">"),
        "{first}"
    );
    assert!(first.contains("not proof that nothing changed"), "{first}");
    assert!(
        first.contains("only this operation, not the turn"),
        "{first}"
    );
    assert!(first.len() < 300, "{} bytes: {first}", first.len());
    // A repeat keeps the full report with its streak count.
    let repeat = compact(127, 2);
    assert!(
        repeat.contains("\nRetry:") && repeat.contains("2 times in a row"),
        "{repeat}"
    );
    // Partial capture is not routine.
    let mut partial = process(1);
    partial.output_complete = false;
    let mut output = ToolOutput::err("x").with_process(partial);
    let (kind, advice) = classify(&shell("wget -q http://x/a.tar.xz"), &output).unwrap();
    assert!(
        report(
            &shell("wget -q http://x/a.tar.xz"),
            &mut output,
            kind,
            advice,
            1
        )
        .contains("\nRetained:")
    );
}

fn shell(command: &str) -> ToolCall {
    ToolCall {
        id: "shell".into(),
        name: "bash".into(),
        arguments: json!({"command": command}),
        malformed_arguments: false,
    }
}

const MISSING_READ: &str = "bash: line 1: read: command not found";

/// A started process that exited 127 with the given diagnostic, optionally altered.
fn missing_with(text: &str, alter: impl FnOnce(&mut ProcessOutcome)) -> ToolOutput {
    let mut observed = process(127);
    alter(&mut observed);
    ToolOutput::err(text).with_process(observed)
}

fn known_read(name: &str) -> Option<Capability> {
    (name == "read").then_some(Capability::Loadable)
}

#[test]
fn a_known_tool_run_as_a_shell_command_gets_tool_load_guidance() {
    let call = shell("read src/lib.rs");
    let advice = shell_tool_recovery(&call, &mut missing_with(MISSING_READ, |_| {}), known_read)
        .expect("a started process missing a known tool name must be coached");
    assert!(advice.contains("tool_not_shell"), "{advice}");
    assert!(
        advice.contains("`read`") && advice.contains("tool_search"),
        "{advice}"
    );
    // The second common spelling of the same controller-observed failure is also accepted.
    let mut reverse = missing_with("command not found: read", |_| {});
    assert!(shell_tool_recovery(&call, &mut reverse, known_read).is_some());
}

/// `bg` is a bash job-control builtin, so running the Dial tool name through bash fails with
/// "no job control" instead of exit 127; that observed failure gets the same guidance.
#[test]
fn a_dial_tool_name_shadowed_by_a_shell_builtin_gets_tool_load_guidance() {
    let known_bg = |name: &str| (name == "bg").then_some(Capability::Loadable);
    let no_job_control = |code| {
        missing_with("bash: line 1: bg: no job control", |p| {
            p.exit_code = Some(code)
        })
    };
    let advice = shell_tool_recovery(&shell("bg"), &mut no_job_control(1), known_bg)
        .expect("bg run through bash must be coached");
    assert!(
        advice.contains("`bg`") && advice.contains("tool_search"),
        "{advice}"
    );
    assert!(shell_tool_recovery(&shell("bg"), &mut no_job_control(0), known_bg).is_none());
    // Only listed builtins: another known tool with the same text is not coached.
    let mut other = missing_with("bash: line 1: read: no job control", |p| {
        p.exit_code = Some(1)
    });
    assert!(shell_tool_recovery(&shell("read"), &mut other, known_read).is_none());
}

/// A missing ordinary program gets one batched-probe hint; Dial tool names, other exit codes
/// and incomplete captures do not.
#[test]
fn a_missing_program_gets_a_single_batched_probe_hint() {
    let python = shell("python -c 1");
    let advice = missing_program_recovery(
        &python,
        &missing_with("bash: line 1: python: command not found", |_| {}),
    )
    .expect("exit 127 for an ordinary program must be coached");
    assert!(
        advice.contains("missing_program") && advice.contains("`python`"),
        "{advice}"
    );
    assert!(advice.contains("command -v"), "{advice}");
    assert!(advice.contains("language `js`"), "{advice}");
    let dash =
        missing_program_recovery(&python, &missing_with("sh: 1: xxd: not found", |_| {})).unwrap();
    assert!(dash.contains("`xxd`") && dash.contains("od -A x"), "{dash}");
    let unknown = missing_program_recovery(
        &python,
        &missing_with("bash: frobnicate: command not found", |_| {}),
    )
    .unwrap();
    assert!(!unknown.contains("Known substitute"), "{unknown}");
    let zsh = missing_with("zsh: command not found: node", |_| {});
    assert!(
        missing_program_recovery(&python, &zsh)
            .unwrap()
            .contains("`node`")
    );
    for output in [
        missing_with("bash: line 1: python: command not found", |p| {
            p.exit_code = Some(1)
        }),
        missing_with("bash: line 1: python: command not found", |p| {
            p.output_complete = false
        }),
        missing_with("some other failure", |_| {}),
    ] {
        assert!(missing_program_recovery(&python, &output).is_none());
    }
    assert!(
        missing_program_recovery(
            &call("read"),
            &missing_with("bash: line 1: python: command not found", |_| {})
        )
        .is_none()
    );
}

#[test]
fn non_matching_shell_failures_never_receive_tool_guidance() {
    let cases: Vec<(&str, ToolCall, ToolOutput)> = vec![
        (
            "success",
            shell("read src/lib.rs"),
            ToolOutput::ok("read src/lib.rs".into()),
        ),
        (
            "other exit code",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.exit_code = Some(1)),
        ),
        (
            "unknown name",
            shell("cargo test"),
            missing_with("bash: line 1: cargo: command not found", |_| {}),
        ),
        (
            "echo quotes the mention",
            shell("echo \"read\""),
            missing_with(MISSING_READ, |_| {}),
        ),
        (
            "bash wrapper",
            shell("bash -c read"),
            missing_with(MISSING_READ, |_| {}),
        ),
        (
            "compound command",
            shell("read src/lib.rs && true"),
            missing_with(MISSING_READ, |_| {}),
        ),
        (
            "pipeline",
            shell("read src/lib.rs | cat"),
            missing_with(MISSING_READ, |_| {}),
        ),
        (
            "unrelated missing command",
            shell("read src/lib.rs"),
            missing_with("bash: line 1: other: command not found", |_| {}),
        ),
        (
            "spoofed diagnostic without process evidence",
            shell("read src/lib.rs"),
            ToolOutput::err(MISSING_READ),
        ),
        (
            "process never started",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.started = false),
        ),
        (
            "terminated by signal",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.signal = Some(9)),
        ),
        (
            "timed out",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.timed_out = true),
        ),
        (
            "cancelled",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.cancelled = true),
        ),
        (
            "incomplete capture",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |p| p.output_complete = false),
        ),
        (
            "effects unknown",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_effects(Effects::Unknown),
        ),
        (
            "blocked boundary",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_status(ToolStatus::Blocked),
        ),
        (
            "aborted boundary",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_status(ToolStatus::Aborted),
        ),
        (
            "timeout boundary",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_status(ToolStatus::Timeout),
        ),
        (
            "skipped boundary",
            shell("read src/lib.rs"),
            missing_with(MISSING_READ, |_| {}).with_status(ToolStatus::Skipped),
        ),
        (
            "not a bash call",
            ToolCall {
                id: "other".into(),
                name: "read".into(),
                arguments: json!({"command": "read src/lib.rs"}),
                malformed_arguments: false,
            },
            missing_with(MISSING_READ, |_| {}),
        ),
    ];
    for (label, call, mut output) in cases {
        assert!(
            shell_tool_recovery(&call, &mut output, known_read).is_none(),
            "{label}"
        );
    }
}

#[test]
fn structured_process_data_alone_is_never_execution_evidence() {
    // A payload that claims a process 127 is not controller evidence: only execution.process counts.
    let spoof = json!({"process": {
        "started": true,
        "exit_code": 127,
        "signal": null,
        "timed_out": false,
        "cancelled": false,
        "output_complete": true
    }});
    let mut output = ToolOutput::structured(spoof).with_status(ToolStatus::Error);
    assert!(output.process_outcome().is_some());
    assert!(
        shell_tool_recovery(&shell("read src/lib.rs"), &mut output, known_read).is_none(),
        "a data-only process claim must not trigger tool guidance"
    );
}

#[test]
fn structured_reports_preserve_producer_values_collisions_and_controller_facts() {
    for original in [
        json!(null),
        json!("producer text"),
        json!([1, {"nested": true}]),
        json!({"result": 7}),
        json!({"recovery_advisory": "producer collision", "result": 7}),
        json!({"recovery_advisory": null}),
        json!({"recovery_advisory": [{"producer": true}]}),
    ] {
        let mut output = ToolOutput::structured(original.clone())
            .with_status(ToolStatus::Blocked)
            .with_effects(Effects::Known);
        let text = output.text.clone();
        let execution = output.execution.clone();
        let recovery = output.recovery();
        let call = call("read");
        let (kind, advice) = classify(&call, &output).unwrap();
        let rendered = report(&call, &mut output, kind, advice, 1);
        let data = output.data.as_ref().unwrap();
        if let Some(object) = original.as_object() {
            for (key, value) in object {
                if key == "recovery_advisory" {
                    let preserved = &data[key][0];
                    assert_eq!(preserved, value.as_array().map_or(value, |array| &array[0]));
                } else {
                    assert_eq!(&data[key], value);
                }
            }
        } else {
            assert_eq!(data["producer_data"], original);
        }
        let records = data["recovery_advisory"].as_array().unwrap();
        let record = records.last().unwrap();
        assert_eq!(record["category"], "permission_or_policy");
        assert_eq!(record["source_call_id"], call.id);
        assert_eq!(record["advisory_only"], true);
        assert_eq!(record["evidence_ref"], "execution.recovery");
        assert_eq!(record["required_observation"], advice);
        assert!(record["candidates"].as_array().unwrap().is_empty());
        assert!(rendered.contains(advice));
        assert_eq!(output.text, text);
        assert_eq!(output.execution, execution);
        assert_eq!(output.recovery(), recovery);
    }
}

#[test]
fn shell_recovery_follows_capability_state_without_inventing_arguments() {
    for (state, expected, load) in [
        (Capability::Offered, "Call it directly", false),
        (Capability::Loadable, "use tool_search", true),
        (Capability::Prohibited, "prohibited", false),
        (Capability::Unavailable, "unavailable", false),
    ] {
        let mut output = missing_with(MISSING_READ, |_| {});
        let facts = output.execution.clone();
        let text = output.text.clone();
        let call = shell("read src/lib.rs");
        let rendered =
            shell_tool_recovery(&call, &mut output, |name| (name == "read").then_some(state))
                .unwrap();
        assert!(rendered.contains(expected), "{rendered}");
        assert_eq!(rendered.contains("tool_search"), load, "{rendered}");
        let record = &output.data.as_ref().unwrap()["recovery_advisory"][0];
        assert_eq!(record["category"], "tool_not_shell");
        let candidate = &record["candidates"][0];
        assert_eq!(candidate["tool"], "read");
        assert_eq!(
            candidate["availability"],
            serde_json::to_value(state).unwrap()
        );
        assert!(candidate.get("arguments").is_none());
        let previous = record.clone();
        let (kind, advice) = classify(&call, &output).unwrap();
        report(&call, &mut output, kind, advice, 1);
        assert_eq!(
            output.data.as_ref().unwrap()["recovery_advisory"][0],
            previous
        );
        assert_eq!(output.execution, facts);
        assert_eq!(output.text, text);
    }
}

#[test]
fn failed_focused_check_with_no_diagnostic_is_not_code_or_unavailable() {
    let mut call = call("bash");
    call.id = "focused-check:related Python tests: probe".into();
    call.arguments = json!({"command":"python3 -m pytest"});
    let output = ToolOutput::err("unclassified diagnostic").with_process(process(1));
    assert_eq!(
        classify(&call, &output).unwrap().0,
        "verification_failed_unclassified"
    );
}

#[test]
fn a_retry_barrier_refusal_is_a_correctable_repeat_not_a_permission_boundary() {
    let call = shell("python3 -m venv.venv");
    let refusal = |text: &str| {
        ToolOutput::err(text)
            .with_status(ToolStatus::Blocked)
            .with_effects(Effects::None)
    };
    let barrier = refusal(&format!(
        "{} this unchanged `bash` call already failed and was not executed again.",
        crate::agent::run::engine::RETRY_BARRIER_LEAD
    ));
    assert!(is_retry_barrier(&barrier));
    let (kind, advice) = classify(&call, &barrier).unwrap();
    assert_eq!(kind, "retry_barrier");
    assert!(advice.contains("not a permission boundary"), "{advice}");
    let mut output = barrier;
    let text = report(&call, &mut output, kind, advice, 2);
    assert!(!text.contains("do not retry or bypass"), "{text}");
    assert!(!text.contains("missing permission"), "{text}");
    assert!(text.contains("Change the arguments"), "{text}");
    // A real policy refusal keeps its boundary guidance.
    let denied = refusal("denied by policy");
    assert!(!is_retry_barrier(&denied));
    assert_eq!(classify(&call, &denied).unwrap().0, "permission_or_policy");
}

#[test]
fn command_shape_hints_name_filter_exits_async_jobs_and_absent_listeners() {
    let exit = |code| ToolOutput::err("").with_process(process(code));
    let hint = |command: &str, output: &ToolOutput| command_shape_hint(&shell(command), output);
    let filtered = hint("cargo test 2>&1 | grep -E '^test result'", &exit(1)).unwrap();
    assert!(
        filtered.contains("filter_exit") && filtered.contains("final `grep`"),
        "{filtered}"
    );
    assert!(
        hint("make && cargo test | /usr/bin/rg FAILED", &exit(1))
            .unwrap()
            .contains("`rg`")
    );
    for (command, code) in [
        ("cargo test | grep ok", 2),
        ("grep -rn x src", 1),
        ("cargo test | tail -5", 1),
        ("cargo test | grep ok; false", 1),
    ] {
        assert_eq!(hint(command, &exit(code)), None, "{command}");
    }
    let started = ToolOutput::ok("Listening".into()).with_process(process(0));
    let job = hint("PORT=3519 node server.mjs &", &started).unwrap();
    assert!(job.contains("async_job") && job.contains("`bg`"), "{job}");
    assert!(hint("node a.js & node b.js & wait", &started).is_none());
    for command in [
        "make && make test",
        "cargo test 2>&1",
        "cmd &> log",
        "cmd >& log",
    ] {
        assert!(hint(command, &started).is_none(), "{command}");
    }
    let browse = ToolCall {
        name: "browser".into(),
        ..shell("")
    };
    let refused = ToolOutput::err(
        "Browser network failure: browser guarded HTTP request: io: Connection refused (os error 61)",
    );
    let advice = command_shape_hint(&browse, &refused).unwrap();
    assert!(
        advice.contains("nothing_listening") && advice.contains("`bg`"),
        "{advice}"
    );
    assert!(command_shape_hint(&browse, &ToolOutput::err("page crashed")).is_none());
}

#[test]
fn eval_spawn_denial_points_to_bash_without_matching_other_failures() {
    let eval = ToolCall {
        name: "eval".into(),
        ..shell("")
    };
    // Observed macOS cell failure; a raised cell reports unknown effects.
    let python = ToolOutput::err(
        "Traceback (most recent call last):\n  File \"<dial-kernel>\", line 2, in <module>\n  File \"/usr/lib/python3.12/subprocess.py\", line 1885, in _execute_child\n    self.pid = _fork_exec(\nPermissionError: [Errno 1] Operation not permitted\n",
    )
    .with_effects(Effects::Unknown);
    let advice = command_shape_hint(&eval, &python).unwrap();
    assert!(
        advice.contains("eval_no_subprocess") && advice.contains("`bash`"),
        "{advice}"
    );
    let node = ToolOutput::err("Error: spawnSync git EPERM\n    at Object.spawnSync");
    assert!(command_shape_hint(&eval, &node).is_some());
    for text in [
        "PermissionError: [Errno 13] Permission denied: '/etc/shadow'",
        "Traceback: _fork_exec failed with ValueError",
        "Error: EACCES: permission denied, open 'x'",
    ] {
        assert!(
            command_shape_hint(&eval, &ToolOutput::err(text)).is_none(),
            "{text}"
        );
    }
    let mut succeeded = ToolOutput::ok("PermissionError _fork_exec in printed log".into());
    succeeded.is_error = false;
    assert!(command_shape_hint(&eval, &succeeded).is_none());
    // The same text from another tool keeps that tool's own rules.
    assert!(command_shape_hint(&shell("python3 x.py"), &python).is_none());
}
