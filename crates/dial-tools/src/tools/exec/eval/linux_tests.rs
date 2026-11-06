use super::*;

#[test]
fn linux_eval_real_entry_points_preserve_threads_callbacks_and_mandatory_fence() {
    for sandboxed in [false, true] {
        if sandboxed && !super::super::sandbox::require_for_test() {
            continue;
        }
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::tools::Workspace::new(directory.path()).unwrap();
        let workspace = if sandboxed {
            workspace
        } else {
            workspace.without_sandbox()
        };
        let mut config = Config::default();
        config.shell.auto_background_secs = 0;
        config.eval.node =
            std::env::var("DIAL_TEST_NODE").unwrap_or_else(|_| "/usr/local/bin/node".into());
        let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
        let ctx = ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        let mut calls = 0;
        let mut run = |language: &str, code: &str| {
            let result = tool
                .run(
                    &ctx,
                    &json!({"language":language,"code":code,"background":false}),
                    &AtomicBool::new(false),
                    &|_| Err("unexpected background".into()),
                    &mut |_| {
                        calls += 1;
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        ToolOutput::ok("callback-result".into())
                    },
                    &AtomicBool::new(false),
                )
                .unwrap();
            match result {
                CellOutcome::Value(value) => value,
                CellOutcome::Moved(_) => panic!("unexpected handoff"),
            }
        };
        let result = run(
            "py",
            "import threading\nvalues=[]\nt=threading.Thread(target=lambda:values.append(42))\nt.start();t.join()\nvalues[0]",
        );
        assert!(result["error"].is_null(), "{result}");
        assert_eq!(result["value"], "42");
        let result = run("py", "tool.call('fixture', {})");
        assert!(result["error"].is_null(), "{result}");
        for code in [
            "import os\nos.fork()",
            "import os,sys\nos.posix_spawn(sys.executable,[sys.executable,'-c','pass'],os.environ)",
            "import subprocess,sys\nsubprocess.Popen([sys.executable,'-c','pass'],start_new_session=True)",
        ] {
            let result = run("py", code);
            assert!(
                result["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("PermissionError")),
                "sandboxed={sandboxed}: {result}"
            );
        }
        if std::path::Path::new(&config.eval.node).is_file() {
            let result = run(
                "js",
                "var Worker = (await import('node:worker_threads')).Worker; await new Promise((resolve,reject)=>{const w=new Worker('require(\"node:worker_threads\").parentPort.postMessage(42)',{eval:true});w.once('message',resolve);w.once('error',reject)})",
            );
            assert!(result["error"].is_null(), "sandboxed={sandboxed}: {result}");
            assert_eq!(result["value"], "42");
            let result = run(
                "js",
                "var cp = await import('node:child_process'); cp.spawnSync((await import('node:process')).execPath, ['-e',''], {detached:true}).error.code",
            );
            assert!(result["error"].is_null(), "{result}");
            assert!(
                result["value"].as_str().unwrap().contains("EPERM"),
                "{result}"
            );
            let result = run(
                "js",
                "(await import('node:process')).stdin; await tool.call('fixture', {})",
            );
            assert!(result["error"].is_null(), "{result}");
        } else {
            assert!(
                std::env::var_os("DIAL_REQUIRE_EVAL").is_none(),
                "Node22 missing"
            );
            eprintln!("skipping optional Node22 eval entry point");
        }
        drop(run);
        assert_eq!(
            calls,
            if std::path::Path::new(&config.eval.node).is_file() {
                2
            } else {
                1
            }
        );
    }
}

#[test]
fn linux_eval_sandbox_refusal_never_executes_code_even_when_platform_is_supported() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(directory.path())
        .unwrap()
        .with_sandbox(super::super::sandbox::Sandbox::Unavailable(
            "forced unavailable".into(),
        ));
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let tool = EvalTool::new(config.eval.clone(), config.kernel.clone());
    for language in ["py", "js"] {
        let result = tool.run(
            &ctx,
            &json!({"language":language,"code":"throw 'must not run'"}),
            &AtomicBool::new(false),
            &|_| Err("unexpected".into()),
            &mut |_| unreachable!(),
            &AtomicBool::new(false),
        );
        let error = result.err().expect("unavailable sandbox admitted code");
        assert!(
            error.contains("forced unavailable") || error.contains("process-fork containment"),
            "{error}"
        );
    }
}
