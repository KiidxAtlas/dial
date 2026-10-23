use super::*;

#[test]
fn computer_timeout_clamps_to_bounds() {
    assert_eq!(clamp_timeout(None), Duration::from_secs(120));
    assert_eq!(clamp_timeout(Some(0.0)), Duration::from_secs(1));
    assert_eq!(clamp_timeout(Some(9999.0)), Duration::from_secs(300));
}

#[test]
fn computer_policy_tiers() {
    assert_eq!(
        computer_policy(&json!({"action": "capabilities"})),
        Policy::Read
    );
    assert_eq!(computer_policy(&json!({"action": "close"})), Policy::Exec);
    assert_eq!(
        computer_policy(&json!({"action": "run", "code": "1"})),
        Policy::Exec
    );
    assert_eq!(
        computer_policy(&json!({"action": "run", "code": "1", "read_only": true})),
        Policy::Exec
    );
    assert_eq!(computer_policy(&Value::Null), Policy::Exec);
    assert_eq!(
        computer_policy(&json!({"action": "call", "chain": [{"method": "nope", "args": []}]})),
        Policy::Exec
    );
}

#[test]
fn computer_run_requires_exactly_one_source() {
    assert!(resolve_run_source(None, None, None).is_err());
    assert!(resolve_run_source(Some("a".into()), Some("b".into()), None).is_err());
    assert_eq!(
        resolve_run_source(Some("1".into()), None, None).unwrap(),
        "1"
    );
}

#[test]
fn computer_inline_cap_keeps_head_and_tail() {
    let text: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
    let capped = cap_inline(text.clone(), |_| Some("artifact://x".into()));
    assert!(capped.len() < text.len());
    assert!(capped.starts_with("line 0\n"));
    assert!(capped.contains("elided"));
    assert!(capped.contains("line 9999"));
    assert!(capped.ends_with("[raw output: artifact://x]"));
    assert_eq!(cap_inline("short".into(), |_| None), "short");
}
#[test]
fn computer_fork_close_does_not_close_parent() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = Config::default();
    let context = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let parent = Computer {
        session: Arc::new(ComputerSession::new(dir.path().display().to_string())),
    };
    let child = parent.fork_session().expect("computer must be owner-local");
    assert!(!child.call(&context, &json!({"action":"close"})).is_error);
    assert!(!parent.session.closed.load(Ordering::Acquire));
    assert!(
        child
            .call(&context, &json!({"action":"capabilities"}))
            .is_error
    );
    assert!(!parent.call(&context, &json!({"action":"close"})).is_error);
    // No desktop backend or process was created by any of these close-only operations.
}

fn run_ok(displays: Vec<ComputerContent>, value: Option<Value>) -> ComputerRunOk {
    ComputerRunOk {
        displays,
        return_value: value,
        ..Default::default()
    }
}

#[test]
fn computer_direct_run_shows_the_returned_value_without_changing_display_text() {
    let none = |_: &str| None;
    let out = run_tool_output(run_ok(vec![], Some(json!([{"id": "7"}]))), true, none);
    assert!(out.text.contains("\"id\": \"7\""), "{}", out.text);
    let data = out.data.unwrap();
    // The Eval helper re-displays data.text, so the fallback must not leak into it.
    assert_eq!(data["text"], "");
    assert_eq!(data["value"], json!([{"id": "7"}]));

    let shown = ComputerContent::Text {
        text: "printed".into(),
    };
    let out = run_tool_output(run_ok(vec![shown], Some(json!(5))), false, none);
    assert_eq!(out.text, "printed");
    assert_eq!(out.data.unwrap()["text"], "printed");

    let out = run_tool_output(run_ok(vec![], Some(json!("plain"))), false, none);
    assert_eq!(out.text, "plain");
    assert_eq!(run_tool_output(run_ok(vec![], None), false, none).text, "");
}

#[test]
fn computer_abort_warns_about_partial_input_only_for_actions_that_can_send_input() {
    let parse = |value: Value| parse_action(&value).unwrap();
    for (action, sends) in [
        (json!({"action": "capabilities"}), false),
        (
            json!({"action": "run", "code": "1", "read_only": true}),
            false,
        ),
        (json!({"action": "run", "code": "1"}), true),
        (
            json!({"action": "call", "chain": [{"method": "windows", "args": []}]}),
            false,
        ),
        (
            json!({"action": "call", "chain": [{"method": "click", "args": [1, 2]}]}),
            true,
        ),
    ] {
        assert_eq!(
            action_may_send_input(&parse(action.clone())),
            sends,
            "{action}"
        );
    }
    let detail = "desktop operation cancelled: the computer run was aborted; native mutation not started for this operation";
    let read = abort_message(false, detail);
    assert!(!read.contains("partial"), "{read}");
    assert!(!read.contains("Input already sent"), "{read}");
    assert!(abort_message(true, detail).contains("Input already sent may have taken effect"));
}

#[test]
fn computer_native_cancel_preserves_worker_provenance() {
    use desktop::{CancelReason, DesktopSession};
    let session = DesktopSession::new(None).unwrap();
    for (wire, expected) in [
        ("run_timeout", CancelReason::RunTimeout),
        ("run_aborted", CancelReason::RunAborted),
        ("run_failed", CancelReason::RunFailed),
        ("control_released", CancelReason::ControlReleased),
    ] {
        let token = session.token_for_test();
        rpc::dispatch(&session, "cancel", json!({"reason": wire})).unwrap();
        assert!(
            token
                .check()
                .unwrap_err()
                .message
                .contains(expected.describe())
        );
    }
}

#[test]
fn computer_real_runtime_handles_sequential_and_concurrent_reads() {
    // Real worker and native session, with no input or application/page access.
    let dir = tempfile::tempdir().unwrap();
    let session = ComputerSession::new(dir.path().display().to_string());
    let cwd = dir.path().to_str().unwrap();
    let run = || {
        let action = parse_action(&json!({"action": "run", "read_only": true,
            "code": "await wait(10); return (await desktop.control.state()).active;"}))
        .unwrap();
        let result = session
            .execute(action, cwd, false, None, &AbortSignal::new())
            .unwrap();
        assert_eq!(result["returnValue"], false, "{result}");
    };
    for _ in 0..5 {
        run();
    }
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(run);
        }
    });
    run();
}

#[test]
fn computer_run_arguments_preserve_json_keys_without_changing_prototypes() {
    let dir = tempfile::tempdir().unwrap();
    let session = ComputerSession::new(dir.path().display().to_string());
    let argument = json!({
        "__proto__": {"inherited": true},
        "nested": [{"__proto__": null}],
        "text": "quote \" slash \\ newline\n😀"
    });
    let action = parse_action(&json!({
        "action": "run", "read_only": true,
        "fn": "(_, value) => ({value, own: Object.hasOwn(value, '__proto__'), ordinary: Object.getPrototypeOf(value) === Object.prototype, inherited: value.inherited === true, nestedOwn: Object.hasOwn(value.nested[0], '__proto__')})",
        "args": [argument.clone()]
    })).unwrap();
    let result = session
        .execute(
            action,
            dir.path().to_str().unwrap(),
            false,
            None,
            &AbortSignal::new(),
        )
        .unwrap();
    assert_eq!(
        result["returnValue"],
        json!({
            "value": argument, "own": true, "ordinary": true,
            "inherited": false, "nestedOwn": true
        })
    );
}

#[test]
fn computer_function_runs_use_async_bodies_and_keep_session_usable() {
    let dir = tempfile::tempdir().unwrap();
    let session = ComputerSession::new(dir.path().display().to_string());
    for function in [
        "(_, value) => value + 1",
        "async (_, value) => { await Promise.resolve(); return value + 1; }",
    ] {
        let action = parse_action(&json!({
            "action": "run", "read_only": true, "fn": function, "args": [41]
        }))
        .unwrap();
        let result = session
            .execute(
                action,
                dir.path().to_str().unwrap(),
                false,
                None,
                &AbortSignal::new(),
            )
            .unwrap();
        assert_eq!(result["returnValue"], 42);
    }
}

#[cfg(unix)]
mod worker {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    #[cfg(test)]
    mod window_lookup_tests;

    #[cfg(test)]
    mod control_release_tests;

    fn window(id: &str, title: &str) -> Value {
        json!({"id": id, "app": "Brave Browser", "title": title, "pid": 1,
            "x": 0, "y": 0, "width": 10, "height": 10, "focused": false})
    }

    /// Runs `code` in the real worker over its line protocol, answering `listWindows` natives
    /// from `list_windows`; returns the run's result message.
    fn run_worker(code: &str, list_windows: impl Fn(usize) -> Value) -> Value {
        run_worker_mode(code, list_windows, true)
    }

    fn run_worker_mode(
        code: &str,
        list_windows: impl Fn(usize) -> Value,
        read_only: bool,
    ) -> Value {
        let mut child = Command::new("node")
            .args(["--input-type=module", "-e", include_str!("js/worker.mjs")])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("node is required for computer worker tests");
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (lines, received) = flume::unbounded();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = lines.send(line);
            }
        });
        let mut send = |message: Value| writeln!(stdin, "{message}").unwrap();
        send(
            json!({"type": "run", "id": "1", "code": code, "timeoutMs": 20000,
            "session": {"cwd": "/", "sessionId": "t", "captureMaxWidth": 100,
                "captureMaxHeight": 100, "display": "active", "readOnly": read_only}}),
        );
        let mut listings = 0;
        let mut control_active = true;
        let result = loop {
            let line = received
                .recv_timeout(Duration::from_secs(30))
                .expect("worker went silent");
            let message: Value = serde_json::from_str(&line).unwrap();
            match message["type"].as_str() {
                Some("result") => break message,
                Some("native") => {
                    let value = match message["method"].as_str() {
                        Some("listWindows") => {
                            listings += 1;
                            let value = list_windows(listings);
                            if value["isAbort"] == true
                                || message["args"]["diagnostics"] != true
                                || value.is_object()
                            {
                                value
                            } else {
                                json!({"windows": value, "truncated": false, "limit": null})
                            }
                        }
                        Some("getWindow") => {
                            let value = list_windows(0);
                            if value.is_object() {
                                value
                            } else {
                                value
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .find(|window| window["id"] == message["args"]["id"])
                                    .cloned()
                                    .expect("fixture must contain the requested window")
                            }
                        }
                        Some("capabilities") => json!({"backend": "fake"}),
                        Some("controlState") => json!({"active": control_active}),
                        Some("releaseControl") => {
                            control_active = false;
                            Value::Null
                        }
                        _ => Value::Null,
                    };
                    if value["isAbort"] == true || value["name"].is_string() {
                        send(
                            json!({"type": "native-result", "id": message["id"], "ok": false, "error": value}),
                        );
                    } else {
                        if matches!(
                            message["method"].as_str(),
                            Some("cancel" | "retire" | "releaseControl")
                        ) {
                            // Cleanup must be acknowledged before the worker publishes a result.
                            assert!(
                                matches!(
                                    received.recv_timeout(Duration::from_millis(30)),
                                    Err(flume::RecvTimeoutError::Timeout)
                                ),
                                "worker advanced before cleanup completed"
                            );
                        }
                        send(
                            json!({"type": "native-result", "id": message["id"], "ok": true, "value": value}),
                        );
                    }
                }
                _ => {}
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        result
    }

    fn brave(_: usize) -> Value {
        json!([window("12088", ""), window("12086", "ClickUpLocal")])
    }

    #[test]
    fn computer_window_and_windows_agree_and_ids_re_resolve() {
        let result = run_worker(
            "const all = await desktop.windows({ app: 'brave' });
             const byTitle = await desktop.windows({ title: 'clickup' });
             const one = await desktop.window({ title: 'clickup' });
             const text = await desktop.window('12086');
             const num = await desktop.window(12086);
             return { all: all.map(w => w.id), byTitle: byTitle.map(w => w.id), ids: [one.id, text.id, num.id] };",
            brave,
        );
        assert_eq!(result["ok"], true, "{result}");
        let value = &result["payload"]["returnValue"];
        assert_eq!(value["all"], json!(["12088", "12086"]));
        assert_eq!(value["byTitle"], json!(["12086"]));
        assert_eq!(value["ids"], json!(["12086", "12086", "12086"]));
    }

    #[test]
    fn computer_window_diagnostics_state_scope_and_distinguish_back_to_back_snapshots() {
        let result = run_worker(
            "const errors = [];
             for (const selector of [{ title: '999' }, { title: 'nothing' }]) {
               try { await desktop.window(selector); } catch (e) { errors.push(e.message); }
             }
             try { await desktop.window({ app: 'Brave' }); } catch (e) { errors.push(e.message); }
             return errors;",
            brave,
        );
        assert_eq!(result["ok"], true, "{result}");
        let errors: Vec<&str> = result["payload"]["returnValue"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| error.as_str().unwrap())
            .collect();
        assert_eq!(errors.len(), 3);
        assert!(
            errors[0].starts_with("no window matches {\"title\":\"999\"}"),
            "{}",
            errors[0]
        );
        assert!(errors[0].contains("listed 2 window(s)"));
        assert!(errors[0].contains("windows() and window(filter) share this source"));
        assert!(errors[0].contains("may be absent"));
        assert!(errors[2].starts_with("multiple windows match"));
        assert!(errors[2].contains("12088 Brave Browser"));
        let seq = |message: &str| -> u32 {
            let tail = message.split("window snapshot #").nth(1).unwrap();
            tail.split(' ').next().unwrap().parse().unwrap()
        };
        assert!(seq(errors[0]) < seq(errors[1]) && seq(errors[1]) < seq(errors[2]));
        let worker = |message: &str| {
            message
                .split("from worker ")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
                .to_owned()
        };
        assert_eq!(worker(errors[0]), worker(errors[2]));
    }

    #[test]
    fn computer_listing_cancellation_never_claims_partial_input() {
        let cancelled = |_: usize| {
            json!({"name": "ToolAbortError", "isAbort": true, "isToolError": false,
                "message": "desktop operation cancelled: run aborted; native mutation not started for this operation"})
        };
        let result = run_worker(
            "try { await desktop.windows(); } catch (e) { return e.message; }",
            cancelled,
        );
        let message = result["payload"]["returnValue"].as_str().unwrap();
        assert!(message.contains("cancelled"), "{message}");
        assert!(!message.contains("partial"), "{message}");
    }

    #[test]
    fn computer_wrong_global_gets_an_actionable_hint() {
        let result = run_worker("return await computer.windows();", brave);
        assert_eq!(result["ok"], false, "{result}");
        let message = result["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("computer is not defined"), "{message}");
        assert!(message.contains("`desktop`"), "{message}");
        // Other ReferenceErrors stay untouched.
        let other = run_worker("return missing_name;", brave);
        assert_eq!(other["error"]["message"], "missing_name is not defined");
    }

    #[test]
    fn computer_direct_run_code_sees_desktop_with_permissions_unchanged() {
        // A read_only run refuses input helpers; the context change must not relax that.
        let result = run_worker(
            "try { await desktop.click(1, 1); return 'clicked'; } catch (e) { return e.message; }",
            brave,
        );
        assert!(
            result["payload"]["returnValue"]
                .as_str()
                .unwrap()
                .contains("read-only run"),
            "{result}"
        );
    }
}
