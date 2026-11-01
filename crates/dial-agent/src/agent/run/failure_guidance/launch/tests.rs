use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::runtime::execution::{OutputIntegrity, ProcessOutcome, ShellLaunch};
use serde_json::json;

fn call() -> ToolCall {
    ToolCall {
        id: "launch".into(),
        name: "bash".into(),
        malformed_arguments: false,
        arguments: json!({"command":"python3 'interactive app.py'", "cwd":"apps", "timeout":120, "session":"state"}),
    }
}
fn output(text: &str, code: i32) -> ToolOutput {
    let mut out = if code == 0 {
        ToolOutput::ok(text.into())
    } else {
        ToolOutput::err(text)
    }
    .with_process(ProcessOutcome {
        started: true,
        exit_code: Some(code),
        signal: None,
        timed_out: false,
        cancelled: false,
        output_complete: true,
    });
    out.execution.shell_launch = Some(ShellLaunch {
        stdin: ShellStdin::Eof,
        terminal_allocated: false,
        job: None,
    });
    out
}
fn capabilities() -> LaunchCapabilities {
    LaunchCapabilities {
        bash: Capability::Offered,
        bg: Capability::Offered,
        no_pty: false,
        launches_allowed: true,
    }
}
const TERMINAL: &str = "_curses.error: cbreak() returned ERR";
const INPUT: &str = "EOFError: EOF when reading a line";

#[test]
fn launch_diagnostics_generalize_across_commands_without_changing_status() {
    for (text, terminal) in [
        (TERMINAL, true),
        (
            "Error: this program requires an interactive terminal (TTY).",
            true,
        ),
        ("Error: interactive mode requires a TTY", true),
        ("Error: Input is not a terminal (fd=0).", true),
        (INPUT, false),
        ("Error: input is required on stdin; received EOF.", false),
    ] {
        for code in [0, 1] {
            let mut out = output(text, code);
            let before = out.execution.clone();
            let advice = shell_launch_recovery(&call(), &mut out, &capabilities()).unwrap();
            let expected = if terminal {
                json!({"command":call().arguments["command"],"cwd":"apps","pty":true})
            } else {
                json!({"command":call().arguments["command"],"cwd":"apps","action":"start","stdin":true})
            };
            assert!(advice.contains(&expected.to_string()), "{text}: {advice}");
            assert!(advice.contains("Do not replay automatically"));
            assert!(advice.contains("session cannot migrate"));
            assert_eq!(out.execution, before);
            assert_eq!(
                ToolStatus::from_output(&out),
                if code == 0 {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                }
            );
        }
    }
}

#[test]
fn application_bugs_quoted_documentation_and_incomplete_evidence_are_not_diagnoses() {
    for text in [
        "TypeError: unsupported operand type(s)",
        "EOFError: unrelated parser failure",
        "Documentation says: _curses.error: cbreak() returned ERR",
        "\"_curses.error: cbreak() returned ERR\"",
        "Example:\n```text\n_curses.error: cbreak() returned ERR\n```",
        "Error: terminal configuration has an application bug",
        "Error: pretty printer requires a valid format string",
        "Error: nonterminal requires a grammar rule",
        "Error: a terminal is not required",
    ] {
        assert!(
            shell_launch_recovery(&call(), &mut output(text, 1), &capabilities()).is_none(),
            "{text}"
        );
    }
    let mut quote = call();
    quote.arguments["command"] = "cat docs/terminal.txt".into();
    assert!(shell_launch_recovery(&quote, &mut output(TERMINAL, 0), &capabilities()).is_none());
    for field in 0..4 {
        let mut out = output(TERMINAL, 1);
        let mut integrity = OutputIntegrity::default();
        match field {
            0 => out.execution.process.as_mut().unwrap().output_complete = false,
            1 => integrity.capture = Completeness::Partial,
            2 => integrity.retention = Completeness::Partial,
            _ => integrity.presentation = Completeness::Partial,
        }
        out.execution.output = Some(integrity);
        assert!(shell_launch_recovery(&call(), &mut out, &capabilities()).is_none());
    }
    assert!(
        shell_launch_recovery(
            &call(),
            &mut output(&format!("{}\n{TERMINAL}", "x".repeat(33 * 1024)), 1),
            &capabilities()
        )
        .is_none()
    );
}

#[test]
fn advice_retains_effect_termination_and_policy_boundaries() {
    for status in [
        ToolStatus::Blocked,
        ToolStatus::Skipped,
        ToolStatus::Aborted,
        ToolStatus::Timeout,
    ] {
        assert!(
            shell_launch_recovery(
                &call(),
                &mut output(TERMINAL, 1).with_status(status),
                &capabilities()
            )
            .is_none()
        );
    }
    for boundary in 0..8 {
        let mut out = output(TERMINAL, 1);
        match boundary {
            0 => out.execution.effects = Effects::Unknown,
            1 => out.execution.process.as_mut().unwrap().cancelled = true,
            2 => out.execution.process.as_mut().unwrap().timed_out = true,
            3 => out.execution.process.as_mut().unwrap().signal = Some(9),
            4 => out.execution.process.as_mut().unwrap().started = false,
            5 => out.execution.shell_launch = None,
            6 => {
                out.execution
                    .shell_launch
                    .as_mut()
                    .unwrap()
                    .terminal_allocated = true
            }
            _ => {
                out = output(INPUT, 1);
                out.execution.shell_launch.as_mut().unwrap().stdin = ShellStdin::Pipe;
            }
        }
        assert!(
            shell_launch_recovery(&call(), &mut out, &capabilities()).is_none(),
            "boundary {boundary}"
        );
    }
}

#[test]
fn capabilities_choose_offered_deferred_or_blocked_advice() {
    for (bg, no_pty, expected, available) in [
        (Capability::Offered, false, "permitted bash launch", true),
        (Capability::Loadable, false, "use tool_load", true),
        (Capability::Prohibited, false, "prohibited", false),
        (Capability::Unavailable, false, "unavailable", false),
        (Capability::Offered, true, "disabled by shell.no_pty", false),
    ] {
        let mut caps = capabilities();
        caps.bg = bg;
        if bg == Capability::Loadable {
            caps.bash = Capability::Unavailable;
        }
        caps.no_pty = no_pty;
        assert_eq!(caps.terminal_available(), available);
        let advice = shell_launch_recovery(&call(), &mut output(TERMINAL, 1), &caps).unwrap();
        assert!(advice.contains(expected), "{advice}");
        if !available {
            assert!(!advice.contains("\"pty\":true"));
        }
    }
    let mut caps = capabilities();
    caps.no_pty = true;
    assert!(
        shell_launch_recovery(&call(), &mut output(INPUT, 1), &caps)
            .unwrap()
            .contains("\"stdin\":true")
    );
    caps = capabilities();
    caps.bash = Capability::Prohibited;
    assert!(
        shell_launch_recovery(&call(), &mut output(TERMINAL, 1), &caps)
            .unwrap()
            .contains("permitted bg launch")
    );
    caps.launches_allowed = false;
    assert!(!caps.terminal_available());
    assert!(
        shell_launch_recovery(&call(), &mut output(TERMINAL, 1), &caps)
            .unwrap()
            .contains("prohibited in the current mode")
    );
}

#[test]
fn existing_running_jobs_and_failed_observations_are_not_relaunch_instructions() {
    let mut out = output(TERMINAL, 1);
    out.execution.process.as_mut().unwrap().exit_code = None;
    out.execution.shell_launch.as_mut().unwrap().job = Some("probe-1".into());
    let advice = shell_launch_recovery(&call(), &mut out, &capabilities()).unwrap();
    assert!(advice.contains("existing job") && advice.contains("bg output/wait"));
    assert!(!advice.contains("\"pty\":true"));
    for action in ["output", "wait"] {
        let call = ToolCall {
            name: "bg".into(),
            arguments: json!({"action":action,"name":"probe-1"}),
            ..call()
        };
        let mut out = output(TERMINAL, 1).with_status(ToolStatus::Ok);
        out.execution.shell_launch.as_mut().unwrap().job = Some("probe-1".into());
        assert!(
            shell_launch_recovery(&call, &mut out, &capabilities())
                .unwrap()
                .contains("process exited unsuccessfully")
        );
        assert_eq!(ToolStatus::from_output(&out), ToolStatus::Ok);
    }
}

#[test]
fn parent_and_child_share_recovery_without_widening_child_allowlists() {
    with_agent("", |agent| {
        let names = vec!["bash".into(), "bg".into()];
        let parent = agent.launch_recovery_capabilities(&names);
        let child = LaunchCapabilities::child(&names, agent.config, &agent.ext);
        assert_eq!(
            shell_launch_recovery(&call(), &mut output(TERMINAL, 1), &parent),
            shell_launch_recovery(&call(), &mut output(TERMINAL, 1), &child)
        );
        assert_eq!(
            agent
                .launch_recovery_capabilities(&["bash".into(), "tool_load".into()])
                .bg,
            Capability::Loadable
        );
        assert_eq!(
            LaunchCapabilities::child(
                &["bash".into(), "tool_load".into()],
                agent.config,
                &agent.ext
            )
            .bg,
            Capability::Prohibited
        );
        agent.set_plan_mode(true);
        assert!(
            !agent
                .launch_recovery_capabilities(&names)
                .terminal_available()
        );
    });
    for readiness in [
        Readiness::Unsupported {
            reason: "host".into(),
        },
        Readiness::MissingPrerequisite {
            reason: "runtime".into(),
        },
    ] {
        assert_eq!(
            capability(true, true, true, true, &readiness),
            Capability::Unavailable
        );
    }
    assert_eq!(
        capability(
            true,
            true,
            true,
            false,
            &Readiness::ApprovalRequired {
                reason: "ask".into()
            }
        ),
        Capability::Offered
    );
}

#[test]
fn existing_job_control_preserves_owner_and_loads_deferred_capabilities_first() {
    for persist in [false, true] {
        for capability in [
            Capability::Offered,
            Capability::Loadable,
            Capability::Prohibited,
        ] {
            let mut caps = capabilities();
            caps.bg = capability;
            let call = ToolCall {
                name: "bg".into(),
                arguments: json!({"action":"output", "name":"same-name", "persist":persist}),
                ..call()
            };
            let mut out = output("", 0);
            out.execution.process.as_mut().unwrap().exit_code = None;
            out.execution.shell_launch.as_mut().unwrap().job = Some("same-name".into());
            let advice = shell_launch_recovery(&call, &mut out, &caps).unwrap();
            if capability == Capability::Prohibited {
                assert!(!advice.contains("Use permitted bg"));
                assert!(!advice.contains("tool_load"));
            } else {
                let mut identity = json!({"name":"same-name"});
                if persist {
                    identity["persist"] = true.into();
                }
                assert!(advice.contains(&identity.to_string()), "{advice}");
                assert_eq!(
                    advice.contains("tool_load"),
                    capability == Capability::Loadable
                );
                assert_eq!(advice.contains("input/resize"), !persist);
            }
        }
    }
}

#[test]
fn structured_launch_candidates_are_schema_valid_and_do_not_execute() {
    with_agent("", |agent| {
        let catalog = agent.tool_catalog().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let sentinel = temp.path().join("must-not-exist");
        let mut call = call();
        call.arguments["command"] = format!("touch '{}'", sentinel.display()).into();
        for (hint, state, offered_bash, expected_tool, expected_availability) in [
            (TERMINAL, Capability::Offered, true, "bash", "offered"),
            (TERMINAL, Capability::Loadable, false, "bg", "loadable"),
            (INPUT, Capability::Offered, true, "bg", "offered"),
            (INPUT, Capability::Loadable, true, "bg", "loadable"),
            (TERMINAL, Capability::Prohibited, true, "bg", "prohibited"),
            (INPUT, Capability::Unavailable, true, "bg", "unavailable"),
        ] {
            let mut caps = capabilities();
            caps.bg = state;
            if !offered_bash {
                caps.bash = Capability::Unavailable;
            }
            let mut out = output(hint, 1);
            let facts = out.execution.clone();
            let text = out.text.clone();
            let rendered = shell_launch_recovery(&call, &mut out, &caps).unwrap();
            let record = &out.data.as_ref().unwrap()["recovery_advisory"][0];
            assert_eq!(record["source_call_id"], call.id);
            assert_eq!(record["advisory_only"], true);
            let candidate = &record["candidates"][0];
            assert_eq!(candidate["tool"], expected_tool);
            assert_eq!(candidate["availability"], expected_availability);
            if state.usable() {
                let args = candidate.get("arguments").unwrap();
                catalog.validate(expected_tool, args).unwrap();
                assert_eq!(args["command"], call.arguments["command"]);
                assert_eq!(args["cwd"], call.arguments["cwd"]);
                assert!(rendered.contains(&args.to_string()));
                assert!(args.get("session").is_none() && args.get("timeout").is_none());
            } else {
                assert!(candidate.get("arguments").is_none());
            }
            assert_eq!(
                rendered.contains("tool_load"),
                state == Capability::Loadable
            );
            let (kind, advice) = classify(&call, &out).unwrap();
            report(&call, &mut out, kind, advice, 1);
            assert_eq!(
                out.data.as_ref().unwrap()["recovery_advisory"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(out.execution, facts);
            assert_eq!(out.text, text);
            assert!(
                !sentinel.exists(),
                "advice must never dispatch a proposed command"
            );
        }
    });
}

#[test]
fn shell_tool_coaching_distinguishes_parent_loader_from_fixed_worker_surface() {
    with_agent("", |agent| {
        let names = vec!["bash".into(), "tool_load".into()];
        let call = ToolCall {
            arguments: json!({"command":"bg"}),
            ..call()
        };
        let failure = || output("bash: line 1: bg: no job control", 1);
        let parent = shell_tool_recovery(&call, &mut failure(), |name| {
            agent.recovery_capability(name, &names)
        })
        .unwrap();
        let worker = shell_tool_recovery(&call, &mut failure(), |name| {
            LaunchCapabilities::child_tool(name, &names, agent.config, &agent.ext)
        })
        .unwrap();
        assert!(parent.contains("tool_load"));
        assert!(worker.contains("prohibited") && !worker.contains("tool_load"));
        assert_eq!(agent.recovery_capability("not-a-dial-tool", &names), None);
        assert_eq!(
            LaunchCapabilities::child_tool("not-a-dial-tool", &names, agent.config, &agent.ext),
            None
        );
        let without_loader = agent.recovery_capability("bg", &["bash".into()]);
        assert_eq!(without_loader, Some(Capability::Unavailable));
        let offered = vec!["bash".into(), "bg".into()];
        assert_eq!(
            shell_tool_recovery(&call, &mut failure(), |name| agent
                .recovery_capability(name, &offered)),
            shell_tool_recovery(
                &call,
                &mut failure(),
                |name| LaunchCapabilities::child_tool(name, &offered, agent.config, &agent.ext)
            )
        );
    });
}

#[test]
fn invalid_or_unbounded_launch_arguments_are_not_reconstructed() {
    for args in [
        json!({"command":"run app", "cwd":7}),
        json!({"command":"x".repeat(9000)}),
    ] {
        let call = ToolCall {
            arguments: args,
            ..call()
        };
        let mut out = output(TERMINAL, 1);
        let rendered = shell_launch_recovery(&call, &mut out, &capabilities()).unwrap();
        assert!(!rendered.contains("permitted bash launch"));
        assert!(
            out.data.as_ref().unwrap()["recovery_advisory"][0]["candidates"][0]
                .get("arguments")
                .is_none()
        );
    }
}
