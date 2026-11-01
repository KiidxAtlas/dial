use super::*;
use crate::tools::surface::outcome;
#[test]
fn an_unsettled_mutation_survives_nested_error_catching() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial");
    let cancel = AtomicBool::new(false);
    let output = run(&cancel, || {
        let _caught = run(&cancel, || {
            std::fs::write(&path, "effect remains").unwrap();
            committed([path.clone()]);
            unsettled();
            ToolOutput::err("recovery could not complete")
        });
        ToolOutput::ok("caller caught the error".into())
    });
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "effect remains");
    assert_eq!(output.execution.changed_paths, [path]);
    assert_eq!(output.execution.effects, Effects::Unknown);
    assert_eq!(
        outcome::result_phase("write", &Value::Null, &output),
        crate::runtime::Phase::Incomplete
    );
}
#[test]
fn catching_a_nested_panic_cannot_publish_known_effects() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested");
    let cancel = AtomicBool::new(false);
    let output = run(&cancel, || {
        let _caught = run(&cancel, || {
            std::fs::write(&path, "effect before panic").unwrap();
            committed([path.clone()]);
            panic!("nested tool panicked");
        });
        ToolOutput::ok("handled the nested error".into())
    });
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "effect before panic"
    );
    assert_eq!(output.execution.changed_paths, [path]);
    assert_eq!(output.execution.effects, Effects::Unknown);
}
#[test]
fn panic_after_effect_preserves_change_evidence_and_uncertainty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("changed");
    let output = run(&AtomicBool::new(false), || {
        std::fs::write(&path, "completed effect").unwrap();
        committed([path.clone()]);
        panic!("failure after effect");
    });
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "completed effect");
    assert_eq!(output.execution.changed_paths, [path]);
    assert_eq!(outcome::effects(&output), Effects::Unknown);
    assert_eq!(
        outcome::result_phase("write", &Value::Null, &output),
        crate::runtime::Phase::Incomplete
    );
}
#[test]
fn callback_uncertainty_survives_a_tool_catching_errors() {
    struct Catching;
    impl crate::tools::integrations::ext::Tool for Catching {
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn name(&self) -> &str {
            "catching"
        }
        fn schema(&self) -> Value {
            Value::Null
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
            _: &dyn Fn(crate::tools::exec::background::AdoptedJob) -> Result<String, String>,
            dispatch: &mut dyn FnMut(crate::tools::integrations::ext::ToolCallback) -> ToolOutput,
        ) -> ToolOutput {
            for _ in 0..2 {
                dispatch(crate::tools::integrations::ext::ToolCallback::Call {
                    name: "write".into(),
                    arguments: Value::Null,
                });
            }
            ToolOutput::structured(serde_json::json!({"ignored_errors":true}))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = crate::config::types::Config::default();
    let context = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let mut count = 0;
    let output = with_dispatch(
        &crate::tools::integrations::ext::Registry::new(),
        &Catching,
        &context,
        &Value::Null,
        &|_| {},
        &AtomicBool::new(false),
        &mut |_| {
            count += 1;
            ToolOutput::err("connection lost after write").with_effects(Effects::Unknown)
        },
    );
    assert_eq!(count, 1);
    assert_eq!(outcome::effects(&output), Effects::Unknown);
    assert!(output.is_error);
}
#[test]
fn a_panicking_read_is_a_known_failure_unless_it_recorded_a_commit() {
    let cancel = AtomicBool::new(false);
    let output = contained(&cancel, Effects::None, || panic!("read worker bug"));
    assert!(output.is_error);
    assert_eq!(outcome::effects(&output), Effects::None);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unexpected");
    let output = contained(&cancel, Effects::None, || {
        std::fs::write(&path, "x").unwrap();
        committed([path.clone()]);
        panic!("a read that wrote");
    });
    assert_eq!(
        outcome::effects(&output),
        Effects::Unknown,
        "a recorded commit keeps the panic uncertain"
    );
}
#[test]
fn skipped_and_uncertain_effects_are_distinct() {
    let cancel = AtomicBool::new(true);
    let output = run(&cancel, || panic!("must not run"));
    assert_eq!(outcome::effects(&output), Effects::None);
    cancel.store(false, Ordering::SeqCst);
    let output = run(&cancel, || panic!("effect may have happened"));
    assert_eq!(outcome::effects(&output), Effects::Unknown);
    assert_eq!(
        outcome::result_phase("write", &Value::Null, &output),
        crate::runtime::Phase::Incomplete
    );
    let output = run(&cancel, || {
        ToolOutput::err("interrupted").with_status(ToolStatus::Aborted)
    });
    assert_eq!(outcome::effects(&output), Effects::Unknown);
}

#[cfg(unix)]
fn shell_registry(config: &crate::config::types::Config) -> Registry {
    let mut registry = Registry::new();
    for tool in crate::tools::exec::background::tools(config) {
        registry.register(tool);
    }
    registry
}

#[cfg(unix)]
fn shell_call(
    workspace: &crate::tools::Workspace,
    config: &crate::config::types::Config,
    registry: &Registry,
    name: &str,
    args: Value,
    cancelled: bool,
) -> ToolOutput {
    native(
        &ToolContext {
            workspace,
            config,
            mutation_authorizer: None,
            confirm: None,
        },
        registry,
        name,
        &args,
        &|_| {},
        &AtomicBool::new(cancelled),
    )
    .expect("native tool")
}

#[cfg(unix)]
#[test]
fn bash_pty_is_a_real_terminal_managed_by_bg() {
    use serde_json::json;
    let (dir, ws) = crate::tools::testkit::workspace();
    // Real local PTY coverage, not a sandbox guarantee.
    let ws = ws.without_sandbox();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();
    let config = crate::config::types::Config::default();
    let registry = shell_registry(&config);
    let call = |name, args| shell_call(&ws, &config, &registry, name, args, false);
    let started = call(
        "bash",
        json!({
            "command":"test -t 0 && test -t 1 && test -t 2 || exit 9; printf 'terminal-ready\\n'; IFS= read -r answer; printf 'reply:%s\\n' \"$answer\"; printf '%s' \"$answer\" > reply.txt",
            "cwd":"subdir", "pty":true
        }),
    );
    assert!(!started.is_error, "{}", started.text);
    assert!(
        started.text.lines().any(|line| line == "terminal-ready"),
        "{}",
        started.text
    );
    assert!(
        started
            .process_outcome()
            .is_some_and(|process| process.started && process.exit_code.is_none())
    );
    assert!(started.execution.output.is_some());
    let job = &started.data.as_ref().unwrap()["job"];
    assert_eq!(job["terminal"], true);
    assert!(job["pid"].as_u64().is_some_and(|pid| pid > 0));
    let name = job["name"].as_str().unwrap();
    let launch = crate::runtime::execution::ShellLaunch {
        stdin: crate::runtime::execution::ShellStdin::Terminal,
        terminal_allocated: true,
        job: Some(name.into()),
    };
    assert_eq!(started.execution.shell_launch.as_ref(), Some(&launch));
    let process = started
        .execution
        .process
        .as_ref()
        .expect("PTY startup is observed");
    assert!(process.started && process.exit_code.is_none() && process.signal.is_none());
    assert!(!process.cancelled && !process.timed_out);
    let ready = call(
        "bg",
        json!({"action":"wait", "name":name, "until":"terminal-ready", "timeout":5}),
    );
    assert!(
        !ready.is_error && ready.text.contains("terminal-ready"),
        "{}",
        ready.text
    );
    let input = call(
        "bg",
        json!({"action":"input", "name":name, "text":"roundtrip\n", "timeout":5}),
    );
    assert!(!input.is_error, "{}", input.text);
    let finished = call("bg", json!({"action":"wait", "name":name, "timeout":5}));
    assert!(
        !finished.is_error && finished.text.contains("exit code 0"),
        "{}",
        finished.text
    );
    assert_eq!(ready.execution.shell_launch.as_ref(), Some(&launch));
    assert_eq!(finished.execution.shell_launch.as_ref(), Some(&launch));
    let screen = call("bg", json!({"action":"output", "name":name}));
    assert!(
        !screen.is_error && screen.text.contains("reply:roundtrip"),
        "{}",
        screen.text
    );
    assert_eq!(screen.execution.shell_launch.as_ref(), Some(&launch));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("subdir/reply.txt")).unwrap(),
        "roundtrip"
    );
}

#[cfg(unix)]
#[test]
fn bash_pty_start_reports_failure_without_losing_the_managed_job() {
    use serde_json::json;
    let (_dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.without_sandbox();
    let config = crate::config::types::Config::default();
    let registry = shell_registry(&config);
    let started = shell_call(
        &ws,
        &config,
        &registry,
        "bash",
        json!({"pty":true, "command":"printf 'bash-startup-crash\\n' >&2; exit 7"}),
        false,
    );
    assert!(started.is_error, "{}", started.text);
    assert!(
        started
            .text
            .lines()
            .any(|line| line == "bash-startup-crash"),
        "{}",
        started.text
    );
    assert!(
        !started.text.contains("this is not command completion"),
        "{}",
        started.text
    );
    assert_eq!(started.process_outcome().unwrap().exit_code, Some(7));
    assert!(started.execution.output.is_some());
    let name = started.data.as_ref().unwrap()["job"]["name"]
        .as_str()
        .unwrap();
    let output = shell_call(
        &ws,
        &config,
        &registry,
        "bg",
        json!({"action":"output", "name":name}),
        false,
    );
    assert_eq!(output.process_outcome().unwrap().exit_code, Some(7));
}

#[cfg(unix)]
#[test]
fn bash_omitted_or_false_pty_keeps_foreground_pipes() {
    use serde_json::json;
    let (_dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.without_sandbox();
    let config = crate::config::types::Config::default();
    let registry = shell_registry(&config);
    for pty in [None, Some(false)] {
        let mut args =
            json!({"command":"test ! -t 0 && test ! -t 1 && test ! -t 2 && printf pipe-ready"});
        if let Some(pty) = pty {
            args["pty"] = json!(pty);
        }
        let out = shell_call(&ws, &config, &registry, "bash", args, false);
        assert!(
            !out.is_error && out.text.contains("pipe-ready"),
            "{pty:?}: {}",
            out.text
        );
        assert!(
            out.execution
                .process
                .as_ref()
                .is_some_and(|process| process.started)
        );
        assert!(
            out.data
                .as_ref()
                .is_none_or(|data| data.get("job").is_none())
        );
        assert!(
            registry
                .get("bg")
                .unwrap()
                .jobs_view(None)
                .unwrap()
                .jobs
                .is_empty()
        );
    }
}

#[cfg(unix)]
#[test]
fn bash_pty_refusals_do_not_spawn_or_fall_back_to_pipes() {
    use serde_json::json;
    for case in ["no-bg", "no-pty", "session", "timeout", "cancel", "cwd"] {
        let (dir, ws) = crate::tools::testkit::workspace();
        let ws = ws.without_sandbox();
        let outside = tempfile::tempdir().unwrap();
        let mut config = crate::config::types::Config::default();
        config.shell.no_pty = case == "no-pty";
        let registry = if case == "no-bg" {
            Registry::new()
        } else {
            shell_registry(&config)
        };
        let mut args = json!({"command":"printf spawned > marker", "pty":true});
        match case {
            "session" => args["session"] = json!("stateful"),
            "timeout" => args["timeout"] = json!(5),
            "cwd" => args["cwd"] = json!(outside.path()),
            _ => {}
        }
        let out = shell_call(&ws, &config, &registry, "bash", args, case == "cancel");
        assert!(out.is_error, "{case}: {}", out.text);
        assert!(out.execution.shell_launch.is_none(), "{case}: {}", out.text);
        assert_eq!(
            outcome::effects(&out),
            Effects::None,
            "{case}: {}",
            out.text
        );
        assert!(!dir.path().join("marker").exists(), "{case}");
        assert!(!outside.path().join("marker").exists(), "{case}");
        if let Some(bg) = registry.get("bg") {
            assert!(bg.jobs_view(None).unwrap().jobs.is_empty(), "{case}");
        }
    }
}

#[cfg(unix)]
#[test]
fn low_level_bash_cannot_silently_ignore_pty_without_a_registry() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.without_sandbox();
    let args = serde_json::json!({"command":"printf spawned > marker", "pty":true});
    let out = crate::tools::execute(&ws, "bash", &args, 5);
    assert!(out.is_error, "{}", out.text);
    assert!(out.text.contains("registry"), "{}", out.text);
    assert!(!dir.path().join("marker").exists());
}

#[cfg(unix)]
#[test]
fn bash_pty_preserves_interceptor_and_git_guard_admission() {
    use crate::config::tools::bash_interceptor::{BashInterceptorConfig, InterceptMode};
    use serde_json::json;
    for (command, reason) in [
        ("cat marker; printf spawned > marker", "read"),
        (
            "git reset --hard; printf spawned > marker",
            "git reset --hard",
        ),
    ] {
        let (dir, ws) = crate::tools::testkit::workspace();
        let ws = ws
            .without_sandbox()
            .with_shared_checkout(true)
            .with_bash_interceptor(
                BashInterceptorConfig {
                    enabled: true,
                    mode: InterceptMode::Block,
                    patterns: None,
                },
                vec!["read".into(), "bg".into()],
            );
        let config = crate::config::types::Config::default();
        let registry = shell_registry(&config);
        let out = shell_call(
            &ws,
            &config,
            &registry,
            "bash",
            json!({"command":command, "pty":true}),
            false,
        );
        assert!(out.is_error && out.text.contains(reason), "{}", out.text);
        assert_eq!(outcome::effects(&out), Effects::None);
        assert!(
            registry
                .get("bg")
                .unwrap()
                .jobs_view(None)
                .unwrap()
                .jobs
                .is_empty()
        );
        assert!(!dir.path().join("marker").exists());
    }
}

#[test]
fn bash_pty_dispatch_keeps_stdin_open_and_does_not_forward_bg_only_options() {
    use serde_json::json;
    struct Managed;
    impl crate::tools::integrations::ext::Tool for Managed {
        fn name(&self) -> &str {
            "bg"
        }
        fn schema(&self) -> Value {
            json!({"type":"function", "function":{"name":"bg", "parameters":{"type":"object", "properties":{}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            true
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            panic!("must use streaming dispatch");
        }
        fn call_streaming(
            &self,
            ctx: &ToolContext,
            args: &Value,
            _: &(dyn Fn(ToolEvent) + Sync),
            _: &AtomicBool,
        ) -> ToolOutput {
            assert_eq!(
                args,
                &json!({
                    "action":"start", "command":"echo hello", "cwd":ctx.workspace.root(),
                    "pty":true, "stdin":true
                })
            );
            ToolOutput::structured(json!({"job":{"name":"managed", "pid":42, "terminal":true}}))
        }
    }
    let (_dir, workspace) = crate::tools::testkit::workspace();
    let config = crate::config::types::Config::default();
    let context = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let mut registry = Registry::new();
    registry.try_register(std::sync::Arc::new(Managed)).unwrap();
    let output = native(
        &context,
        &registry,
        "bash",
        &json!({
            "command":"echo hello", "pty":true, "stdin":false,
            "persist":true, "ready":{"log":"never"}, "name":"injected"
        }),
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!output.is_error, "{}", output.text);
    assert_eq!(output.data.as_ref().unwrap()["job"]["name"], "managed");
}
