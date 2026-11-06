use super::*;

fn context<'a>(workspace: &'a crate::tools::Workspace, config: &'a Config) -> ToolContext<'a> {
    ToolContext {
        workspace,
        config,
        mutation_authorizer: None,
        confirm: None,
    }
}

fn settled(outcome: CellOutcome) -> Value {
    match outcome {
        CellOutcome::Value(value) => value,
        CellOutcome::Moved(job) => panic!("cell unexpectedly moved to background job {job}"),
    }
}

#[test]
fn control_operations_do_not_start_an_interpreter() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let mut config = Config::default();
    config.eval.node = "/missing-dial-test-node".into();
    config.kernel.python = "/missing-dial-test-python".into();
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    for language in ["js", "py"] {
        for action in ["status", "reset", "cancel"] {
            let output = tool.call(
                &context(&workspace, &config),
                &json!({"language":language,"action":action}),
            );
            assert!(!output.is_error, "{language} {action}: {}", output.text);
        }
    }
    assert_eq!(tool.js.pid.load(Ordering::Acquire), 0);
}

#[cfg(unix)]
#[test]
fn installed_languages_reports_only_resolvable_operator_runtimes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let operator = tempfile::tempdir().unwrap();
    let python = operator.path().join("python-fixture");
    std::fs::write(&python, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = Config::default();
    config.eval.node = "/missing-dial-test-node".into();
    config.kernel.python = "/missing-dial-test-python".into();
    assert!(installed_languages(dir.path(), &config).is_empty());
    config.kernel.python = python.to_str().unwrap().into();
    assert_eq!(installed_languages(dir.path(), &config), ["py"]);
    // A launcher inside the editable workspace is never an operator-installed runtime.
    let planted = dir.path().join("python-fixture");
    std::fs::copy(&python, &planted).unwrap();
    std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
    config.kernel.python = planted.to_str().unwrap().into();
    assert!(installed_languages(dir.path(), &config).is_empty());
}

#[test]
fn unknown_language_is_refused_before_any_runtime_starts() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = Config::default();
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let result = tool.run(
        &context(&workspace, &config),
        &json!({"language":"ruby","code":"1"}),
        &AtomicBool::new(false),
        &no_background,
        &mut |_| unreachable!(),
        &AtomicBool::new(false),
    );
    assert!(result.err().unwrap().contains("unsupported eval language"));
    assert_eq!(tool.js.pid.load(Ordering::Acquire), 0);
}

#[test]
fn unknown_action_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = Config::default();
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let result = tool.run(
        &context(&workspace, &config),
        &json!({"language":"py","action":"restart"}),
        &AtomicBool::new(false),
        &no_background,
        &mut |_| unreachable!(),
        &AtomicBool::new(false),
    );
    assert!(result.err().unwrap().contains("unsupported eval action"));
}

#[test]
fn idle_cancel_does_not_claim_to_discard_an_absent_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = Config::default();
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let result = tool
        .run(
            &context(&workspace, &config),
            &json!({"action":"cancel"}),
            &AtomicBool::new(false),
            &no_background,
            &mut |_| unreachable!(),
            &AtomicBool::new(false),
        )
        .map(settled)
        .unwrap();
    assert_eq!(
        result,
        json!({"cancelled":false,"namespace_discarded":false})
    );
    assert!(!tool.js.interrupt.load(Ordering::Acquire));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn cancel_reports_and_preserves_idle_namespace_but_discards_active_namespace() {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    // Exercise the real Node driver and mandatory process fence, without filesystem isolation.
    let workspace = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .without_sandbox();
    let config = Config::default();
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let run = |args: Value| {
        tool.run(
            &context(&workspace, &config),
            &args,
            &AtomicBool::new(false),
            &no_background,
            &mut |_| unreachable!(),
            &AtomicBool::new(false),
        )
        .map(settled)
        .unwrap()
    };
    assert!(run(json!({"code":"const kept = 42"}))["error"].is_null());
    let pid = tool.js.pid.load(Ordering::Acquire);
    assert!(pid > 0);
    assert_eq!(
        run(json!({"action":"cancel","language":"js"})),
        json!({"cancelled":false,"namespace_discarded":false})
    );
    assert_eq!(run(json!({"code":"kept"}))["value"], "42");
    assert_eq!(tool.js.pid.load(Ordering::Acquire), pid);

    let started = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let started_ref = &started;
        let tool_ref = &tool;
        let workspace_ref = &workspace;
        let config_ref = &config;
        let running = scope.spawn(move || {
            tool_ref.run(
                &context(workspace_ref, config_ref),
                &json!({"code":"while (true) {}"}),
                &AtomicBool::new(false),
                &no_background,
                &mut |_| unreachable!(),
                started_ref,
            )
        });
        let end = Instant::now() + Duration::from_secs(5);
        while !started.load(Ordering::Acquire) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(started.load(Ordering::Acquire), "cell never started");
        assert_eq!(
            run(json!({"action":"cancel","language":"js"})),
            json!({"cancelled":true,"namespace_discarded":true})
        );
        assert!(running.join().unwrap().is_err());
    });
    assert_eq!(tool.js.pid.load(Ordering::Acquire), 0);
    assert!(tool.js.session.lock().is_none());
    let fresh = run(json!({"code":"typeof kept"}));
    assert_eq!(fresh["value"], "'undefined'");
}

#[cfg(target_os = "macos")]
fn background_tool(config: &Config) -> Arc<dyn Tool> {
    crate::tools::exec::background::tools(config)
        .into_iter()
        .next()
        .expect("the background tool is registered")
}

#[cfg(target_os = "macos")]
fn threshold_config(secs: u64) -> Config {
    let mut config = Config::default();
    config.shell.auto_background_secs = secs;
    config
}

#[cfg(target_os = "macos")]
fn unsandboxed(dir: &tempfile::TempDir) -> crate::tools::Workspace {
    crate::tools::Workspace::new(dir.path())
        .unwrap()
        .without_sandbox()
}

#[cfg(target_os = "macos")]
fn run_cell(
    tool: &EvalTool,
    ctx: &ToolContext<'_>,
    args: Value,
    adopt: &dyn Fn(AdoptedJob) -> Result<String, String>,
) -> Result<CellOutcome, String> {
    tool.run(
        ctx,
        &args,
        &AtomicBool::new(false),
        adopt,
        &mut |_| unreachable!(),
        &AtomicBool::new(false),
    )
}

#[cfg(target_os = "macos")]
fn bg_output(bg: &dyn Tool, ctx: &ToolContext<'_>, name: &str) -> String {
    let out = bg.call(ctx, &json!({"action":"wait","name":name,"timeout":30}));
    assert!(!out.is_error, "{}", out.text);
    out.text
}

#[cfg(target_os = "macos")]
fn moved_job(outcome: CellOutcome) -> String {
    match outcome {
        CellOutcome::Moved(job) => job,
        CellOutcome::Value(value) => panic!("expected a background move, got {value}"),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn slow_cell_moves_at_the_threshold_and_its_display_output_moves_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = unsandboxed(&dir);
    let config = threshold_config(1);
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let bg = background_tool(&config);
    let ctx = context(&workspace, &config);
    let adopt = |job: AdoptedJob| bg.adopt_job(job);
    let outcome = run_cell(
        &tool,
        &ctx,
        json!({"code":"__dial_display__('before'); await new Promise(r => setTimeout(r, 2500)); 'done'"}),
        &adopt,
    )
    .unwrap();
    let job = moved_job(outcome);
    let output = bg_output(bg.as_ref(), &ctx, &job);
    assert!(output.contains("before"), "{output}");
    assert!(output.contains("done"), "{output}");
}

#[cfg(target_os = "macos")]
#[test]
fn background_true_moves_at_once_and_background_false_stays_foreground() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = unsandboxed(&dir);
    let config = threshold_config(1);
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let bg = background_tool(&config);
    let ctx = context(&workspace, &config);
    let adopt = |job: AdoptedJob| bg.adopt_job(job);
    let job = moved_job(
        run_cell(
            &tool,
            &ctx,
            json!({"code":"await new Promise(r => setTimeout(r, 800)); 2","background":true}),
            &adopt,
        )
        .unwrap(),
    );
    assert!(bg_output(bg.as_ref(), &ctx, &job).contains('2'));
    let foreground = run_cell(
        &tool,
        &ctx,
        json!({"code":"await new Promise(r => setTimeout(r, 1500)); 7","background":false}),
        &adopt,
    )
    .map(settled)
    .unwrap();
    assert_eq!(foreground["value"], "7");
}

#[cfg(target_os = "macos")]
#[test]
fn a_moved_cell_keeps_the_runtime_busy_until_its_job_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = unsandboxed(&dir);
    let config = threshold_config(1);
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let bg = background_tool(&config);
    let ctx = context(&workspace, &config);
    let adopt = |job: AdoptedJob| bg.adopt_job(job);
    let job = moved_job(
        run_cell(
            &tool,
            &ctx,
            json!({"code":"await new Promise(r => setTimeout(r, 2500)); 1","background":true}),
            &adopt,
        )
        .unwrap(),
    );
    let busy = run_cell(&tool, &ctx, json!({"code":"1"}), &adopt)
        .err()
        .unwrap();
    assert!(
        busy.contains(&format!("runtime busy in background job {job}")),
        "{busy}"
    );
    bg_output(bg.as_ref(), &ctx, &job);
    let fresh = run_cell(&tool, &ctx, json!({"code":"1"}), &adopt)
        .map(settled)
        .unwrap();
    assert_eq!(fresh["value"], "1");
}

#[cfg(target_os = "macos")]
#[test]
fn host_callbacks_from_a_moved_cell_are_refused_with_the_acceptance_message() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = unsandboxed(&dir);
    let config = threshold_config(1);
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let bg = background_tool(&config);
    let ctx = context(&workspace, &config);
    let adopt = |job: AdoptedJob| bg.adopt_job(job);
    let job = moved_job(
        run_cell(
            &tool,
            &ctx,
            json!({"code":"await new Promise(r => setTimeout(r, 1500)); (() => { try { tool.bg({action:'list'}); return 'ok'; } catch (e) { return 'refused: ' + e.message; } })()"}),
            &adopt,
        )
        .unwrap(),
    );
    let output = bg_output(bg.as_ref(), &ctx, &job);
    assert!(
        output.contains(
            "this cell was moved to the background; host tool calls are unavailable there"
        ),
        "{output}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn killing_a_moved_cell_frees_the_runtime_for_the_next_cell() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = unsandboxed(&dir);
    let config = threshold_config(1);
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let bg = background_tool(&config);
    let ctx = context(&workspace, &config);
    let adopt = |job: AdoptedJob| bg.adopt_job(job);
    let job = moved_job(
        run_cell(
            &tool,
            &ctx,
            json!({"code":"await new Promise(r => setTimeout(r, 30000)); 1","background":true}),
            &adopt,
        )
        .unwrap(),
    );
    let killed = bg.call(&ctx, &json!({"action":"kill","name":job}));
    assert!(!killed.is_error, "{}", killed.text);
    // Kill forgets the entry, so the job cannot be waited on. The killed cell's thread releases the
    // runtime shortly after the kill, so retry the next cell until it is no longer busy.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let next = loop {
        match run_cell(&tool, &ctx, json!({"code":"1+1"}), &adopt) {
            Ok(outcome) => break settled(outcome),
            Err(error)
                if error.contains("runtime busy") && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("{error}"),
        }
    };
    assert_eq!(next["value"], "2");
}

#[cfg(target_os = "macos")]
#[test]
fn refused_adoption_keeps_the_cell_in_the_foreground_with_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = unsandboxed(&dir);
    let config = threshold_config(1);
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let ctx = context(&workspace, &config);
    let refuse = |_: AdoptedJob| -> Result<String, String> { Err("test: table full".to_owned()) };
    let value = run_cell(
        &tool,
        &ctx,
        json!({"code":"await new Promise(r => setTimeout(r, 1500)); 5"}),
        &refuse,
    )
    .map(settled)
    .unwrap();
    assert_eq!(value["value"], "5");
    let stdout = value["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains(
            "backgrounding unavailable: test: table full; the cell stayed in the foreground"
        ),
        "{stdout}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_full_background_table_keeps_the_cell_in_the_foreground_with_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = unsandboxed(&dir);
    let config = threshold_config(1);
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let bg = background_tool(&config);
    let ctx = context(&workspace, &config);
    for index in 0..8 {
        let out = bg.call(
            &ctx,
            &json!({"action":"start","name":format!("filler{index}"),"command":"sleep 30"}),
        );
        assert!(!out.is_error, "{}", out.text);
    }
    let adopt = |job: AdoptedJob| bg.adopt_job(job);
    let value = run_cell(
        &tool,
        &ctx,
        json!({"code":"await new Promise(r => setTimeout(r, 1500)); 9"}),
        &adopt,
    )
    .map(settled)
    .unwrap();
    assert_eq!(value["value"], "9");
    let stdout = value["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("backgrounding unavailable: 8 background processes are already running"),
        "{stdout}"
    );
    let killed = bg.call(&ctx, &json!({"action":"kill","all":true}));
    assert!(!killed.is_error, "{}", killed.text);
}

#[cfg(target_os = "macos")]
#[test]
fn native_callback_receipts_and_image_references_survive_eval_without_trusting_payload_claims() {
    use crate::runtime::execution::NativeCapture;
    let dir = tempfile::tempdir().unwrap();
    let workspace = unsandboxed(&dir);
    let config = threshold_config(0);
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let ctx = context(&workspace, &config);
    let capture = NativeCapture::new(b"pixels", "image/png", Some("window:7".into()));
    let reference = json!({"sha256":capture.sha256,"inline":"cGl4ZWxz"});
    for native in [false, true] {
        let out = tool.call_with_dispatch(
            &ctx,
            &json!({"code":"tool.capture({}); 'done'", "background":false}),
            &|_| {},
            &AtomicBool::new(false),
            &no_background,
            &mut |callback| {
                assert!(matches!(callback, ToolCallback::Call { name, .. } if name == "capture"));
                let output = ToolOutput::structured(json!({"captures":[capture],
                    "images":[{"reference":reference}]}));
                if native {
                    output.with_captures(vec![capture.clone()])
                } else {
                    output
                }
            },
        );
        assert!(!out.is_error, "{}", out.text);
        if native {
            assert_eq!(out.execution.captures, vec![capture.clone()]);
            assert_eq!(out.data.unwrap()["dial_images"], json!([reference]));
        } else {
            assert!(out.execution.captures.is_empty());
            assert!(out.data.unwrap().get("dial_images").is_none());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn eval_cannot_relabel_pixels_from_before_a_later_callback_as_current() {
    use crate::runtime::execution::NativeCapture;
    let dir = tempfile::tempdir().unwrap();
    let workspace = unsandboxed(&dir);
    let config = threshold_config(0);
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let ctx = context(&workspace, &config);
    let capture = NativeCapture::new(b"pixels", "image/png", Some("window:7".into()));
    let reference = json!({"sha256":capture.sha256,"inline":"cGl4ZWxz"});
    for (code, current) in [
        ("var old = tool.capture({}); tool.mutate({}); old", false),
        ("tool.mutate({}); tool.capture({})", true),
        ("tool.capture({}); tool.mutate({}); tool.capture({})", true),
        ("tool.capture({}); tool.schemas", true),
    ] {
        let out = tool.call_with_dispatch(
            &ctx,
            &json!({"code":code, "background":false}),
            &|_| {},
            &AtomicBool::new(false),
            &no_background,
            &mut |callback| match callback {
                ToolCallback::Call { name, .. } if name == "capture" => {
                    ToolOutput::structured(json!({"images":[{"reference":reference}]}))
                        .with_captures(vec![capture.clone()])
                }
                ToolCallback::Call { name, .. } if name == "mutate" => {
                    ToolOutput::ok("changed".into())
                }
                ToolCallback::Schemas => ToolOutput::structured(json!([])),
                other => panic!("unexpected callback: {other:?}"),
            },
        );
        assert!(!out.is_error, "{}", out.text);
        if current {
            assert_eq!(out.execution.captures, vec![capture.clone()], "{code}");
            assert_eq!(
                out.data.unwrap()["dial_images"],
                json!([reference]),
                "{code}"
            );
        } else {
            assert!(out.execution.captures.is_empty(), "{code}");
            assert!(out.data.unwrap().get("dial_images").is_none(), "{code}");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_linux_python_eval_keeps_state_callbacks_and_threads_with_mandatory_fence() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .without_sandbox();
    let config = Config::default();
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    let mut callbacks = 0;
    for (code, expected) in [
        (
            "kept = 40\nimport threading\nvalues = []\nt = threading.Thread(target=lambda: values.append(2))\nt.start(); t.join()\nkept + values[0]",
            "42",
        ),
        (
            "answer = tool.call('read', {'path':'fixture'})\nkept + answer['data']['value']",
            "42",
        ),
        (
            "import os, errno\ntry:\n    child = os.fork()\nexcept OSError as error:\n    assert error.errno == errno.EPERM\nelse:\n    if child == 0: os._exit(91)\n    os.waitpid(child, 0)\n    raise AssertionError('process escaped')\nkept",
            "40",
        ),
    ] {
        let value = tool.run(
            &context(&workspace, &config),
            &json!({"language":"py", "code":code, "background":false}),
            &AtomicBool::new(false),
            &no_background,
            &mut |callback| {
                assert!(matches!(callback, ToolCallback::Call { name, arguments } if name == "read" && arguments["path"] == "fixture"));
                callbacks += 1;
                ToolOutput::structured(json!({"value":2}))
            },
            &AtomicBool::new(false),
        ).map(settled).unwrap();
        assert!(value["error"].is_null(), "{value}");
        assert_eq!(value["value"], expected, "{code}");
    }
    assert_eq!(callbacks, 1);
    assert!(
        !tool
            .call(
                &context(&workspace, &config),
                &json!({"language":"py", "action":"reset"})
            )
            .is_error
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn generated_artifact_cell_validates_saved_values_and_preserves_write_failures() {
    for (outcome, source_text, expected_total, callback) in [
        (
            "saved",
            "status,value\npaid,2\nunpaid,90\npaid,8\n",
            10,
            "tool.write(arguments)",
        ),
        (
            "saved",
            "status,value\npaid,6\nunpaid,90\npaid,11\n",
            17,
            "tool.write(arguments)",
        ),
        (
            "saved",
            "status,value\npaid,2\nunpaid,90\npaid,8\n",
            10,
            "await tool.write(arguments)",
        ),
        (
            "saved",
            "status,value\npaid,2\nunpaid,90\npaid,8\n",
            10,
            "await tool.call('write', arguments)",
        ),
        (
            "denied",
            "status,value\npaid,2\nunpaid,90\npaid,8\n",
            10,
            "tool.write(arguments)",
        ),
        (
            "denied",
            "status,value\npaid,2\nunpaid,90\npaid,8\n",
            10,
            "await tool.write(arguments)",
        ),
        (
            "corrupted",
            "status,value\npaid,2\nunpaid,90\npaid,8\n",
            10,
            "tool.write(arguments)",
        ),
        (
            "corrupted",
            "status,value\npaid,2\nunpaid,90\npaid,8\n",
            10,
            "await tool.write(arguments)",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = crate::tools::Workspace::new(dir.path())
            .unwrap()
            .without_sandbox();
        let config = Config::default();
        let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
        let path = dir.path().join("summary.json");
        let source = dir.path().join("values.csv");
        std::fs::write(&source, source_text).unwrap();
        let code = format!(
            "from pathlib import Path\nimport csv, io, json\nsource = {}\noriginal = Path(source).read_bytes()\nrows = list(csv.DictReader(io.StringIO(original.decode('utf-8'))))\npaid = [row for row in rows if row['status'] == 'paid']\ncomputed = {{'count': len(paid), 'total': sum(int(row['value']) for row in paid)}}\npath = {}\ncontent = json.dumps(computed)\narguments = {{'path': path, 'content': content}}\nreceipt = {callback}\nassert isinstance(receipt, dict) and receipt['is_error'] is False\nassert json.loads(json.dumps(receipt)) == receipt\nassert (await receipt) is receipt\nassert (await receipt) is receipt\nsaved = json.loads(Path(path).read_text())\nassert saved == computed, 'saved artifact differs from computed values'\nassert set(saved) == {{'count', 'total'}} and isinstance(saved['count'], int) and isinstance(saved['total'], int)\nassert Path(source).read_bytes() == original, 'source changed'\nprint('validated paid totals')",
            serde_json::to_string(&source).unwrap(),
            serde_json::to_string(&path).unwrap()
        );
        let mut writes = 0;
        let result = tool
            .run(
                &context(&workspace, &config),
                &json!({"language":"py", "code":code, "background":false}),
                &AtomicBool::new(false),
                &no_background,
                &mut |callback| {
                    let ToolCallback::Call { name, arguments } = callback else {
                        panic!("unexpected callback: {callback:?}");
                    };
                    assert_eq!(name, "write");
                    writes += 1;
                    if outcome == "denied" {
                        return ToolOutput::err("fixture: write approval denied");
                    }
                    let output = crate::tools::execute(&workspace, &name, &arguments, 30);
                    assert!(!output.is_error, "{}", output.text);
                    if outcome == "corrupted" {
                        std::fs::write(&path, r#"{"count": 0, "total": 0}"#).unwrap();
                    }
                    output
                },
                &AtomicBool::new(false),
            )
            .map(settled)
            .unwrap();
        assert_eq!(writes, 1, "{outcome}: must not replay the write");
        assert_eq!(std::fs::read(&source).unwrap(), source_text.as_bytes());
        let stdout = result["stdout"].as_str().unwrap_or_default();
        match outcome {
            "saved" => {
                assert!(result["error"].is_null(), "{result}");
                assert!(stdout.contains("validated paid totals"), "{result}");
                assert_eq!(
                    serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
                    json!({"count": 2, "total": expected_total})
                );
            }
            "denied" => {
                assert!(
                    result["error"]
                        .as_str()
                        .unwrap()
                        .contains("approval denied"),
                    "{result}"
                );
                assert!(!path.exists(), "denied callback wrote the artifact");
                assert!(!stdout.contains("validated"), "{result}");
            }
            "corrupted" => {
                assert!(
                    result["error"]
                        .as_str()
                        .unwrap()
                        .contains("saved artifact differs"),
                    "{result}"
                );
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    r#"{"count": 0, "total": 0}"#
                );
                assert!(!stdout.contains("validated"), "{result}");
            }
            other => panic!("unknown fixture outcome: {other}"),
        }
    }
}
