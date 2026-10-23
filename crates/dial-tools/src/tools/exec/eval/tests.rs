use super::*;

#[test]
fn idle_cancel_does_not_claim_to_discard_an_absent_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let tool = EvalTool::new(config.eval.clone());
    let result = tool
        .run(
            &ctx,
            &json!({"action":"cancel"}),
            &AtomicBool::new(false),
            &mut |_| unreachable!(),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        result,
        json!({"cancelled":false,"namespace_discarded":false})
    );
    assert!(!tool.interrupt.load(Ordering::Acquire));
}

#[cfg(target_os = "macos")]
#[test]
fn cancel_reports_and_preserves_idle_namespace_but_discards_active_namespace() {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    // Exercise the real Node driver and mandatory process fence, without filesystem isolation.
    let workspace = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .without_sandbox();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let tool = EvalTool::new(config.eval.clone());
    let run = |args: Value| {
        tool.run(
            &ctx,
            &args,
            &AtomicBool::new(false),
            &mut |_| unreachable!(),
            &AtomicBool::new(false),
        )
        .unwrap()
    };
    assert!(run(json!({"code":"const kept = 42"}))["error"].is_null());
    let pid = tool.pid.load(Ordering::Acquire);
    assert!(pid > 0);
    assert_eq!(
        run(json!({"action":"cancel"})),
        json!({"cancelled":false,"namespace_discarded":false})
    );
    assert_eq!(run(json!({"code":"kept"}))["value"], "42");
    assert_eq!(tool.pid.load(Ordering::Acquire), pid);

    let started = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let started_ref = &started;
        let tool_ref = &tool;
        let workspace_ref = &workspace;
        let config_ref = &config;
        let running = scope.spawn(move || {
            tool_ref.run(
                &ToolContext {
                    workspace: workspace_ref,
                    config: config_ref,
                    mutation_authorizer: None,
                    confirm: None,
                },
                &json!({"code":"while (true) {}"}),
                &AtomicBool::new(false),
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
            run(json!({"action":"cancel"})),
            json!({"cancelled":true,"namespace_discarded":true})
        );
        assert!(running.join().unwrap().is_err());
    });
    assert_eq!(tool.pid.load(Ordering::Acquire), 0);
    assert!(tool.session.lock().is_none());
    let fresh = run(json!({"code":"typeof kept"}));
    assert_eq!(fresh["value"], "'undefined'");
}
