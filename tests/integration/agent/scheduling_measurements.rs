//! Local scheduling measurements, not provider/model speedup claims.
//! Run timings with `cargo test --locked --test integration scheduling_measurements -- --ignored --nocapture --test-threads=1`.
use super::*;

const READS: usize = 8;

fn read_turn(batched: bool) -> (Duration, usize) {
    let calls: Vec<_> = (0..READS)
        .map(|i| ("read", json!({"path":format!("input-{i}.txt")})))
        .collect();
    let mut script = if batched {
        vec![Reply::Calls(calls)]
    } else {
        calls
            .into_iter()
            .map(|(name, args)| Reply::Call(name, args))
            .collect()
    };
    script.push(Reply::Text("Done."));
    let server = serve(script);
    let mut e = env(&server.url, "auto");
    e.config.task.max_concurrency = 4;
    for i in 0..READS {
        std::fs::write(
            e.project.path().join(format!("input-{i}.txt")),
            format!("evidence-{i}\n"),
        )
        .unwrap();
    }
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let started = Instant::now();
    let report = agent
        .turn("Read every input file independently.", &mut ui)
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let results: Vec<_> = ui
        .results
        .iter()
        .filter(|(name, _, _)| name == "read")
        .collect();
    assert_eq!(results.len(), READS);
    for i in 0..READS {
        assert!(
            results
                .iter()
                .any(|(_, text, failed)| !failed && text.contains(&format!("evidence-{i}")))
        );
    }
    let requests = server.requests.lock().len();
    assert_eq!(requests, if batched { 2 } else { READS + 1 });
    let latency = agent.efficiency_report()["inference_latency"].clone();
    assert_eq!(latency["requests"], requests as u64);
    assert_eq!(latency["failed_requests"], 0);
    assert!(latency["total_ms"].is_number());
    (elapsed, requests)
}

#[test]
fn serial_and_batched_reads_preserve_all_results_and_reduce_round_trips() {
    read_turn(false);
    read_turn(true);
}

#[test]
#[ignore = "opt-in local wall-clock measurement; no CI speed threshold"]
fn measure_serial_vs_batched_reads() {
    // Alternate ordering to reduce systematic warm-cache/order bias; retain every sample.
    for sample in 0..5 {
        for batched in if sample % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let (elapsed, requests) = read_turn(batched);
            eprintln!(
                "local scripted-provider actual-read turn: sample={sample} batched={batched} files={READS} requests={requests} elapsed_ms={:.3}",
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }
}

#[cfg(unix)]
fn shell(ws: &dial::tools::Workspace, command: &str) {
    let output = dial::tools::exec::shell::bash_tool_streaming(
        ws,
        &json!({"command":command}),
        10,
        &|_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!output.is_error, "{}", output.text);
}

#[cfg(unix)]
fn with_background(f: impl FnOnce(&dial::tools::Workspace, &dyn Fn(Value) -> ToolOutput)) {
    let dir = tempfile::tempdir().unwrap();
    // Explicit fixture-only trust mode: no sandbox-enforcement claim or external commands/data.
    let ws = dial::tools::Workspace::new(dir.path())
        .unwrap()
        .without_sandbox();
    let config = Config::default();
    let tool = dial::tools::exec::background::tools(&config)
        .into_iter()
        .next()
        .unwrap();
    let ctx = dial::tools::ext::ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let call = |args: Value| {
        let out = tool.call(&ctx, &args);
        assert!(!out.is_error, "{}", out.text);
        out
    };
    f(&ws, &call);
    assert!(
        !tool.has_live_work(),
        "fixture must settle its managed jobs"
    );
}

#[cfg(unix)]
#[test]
fn background_job_allows_independent_foreground_work_before_release() {
    with_background(|ws, bg| {
        bg(json!({"action":"start", "name":"gated", "stdin":true,
            "command":"printf 'ready\\n'; read release; test -f independent && printf 'settled\\n'"}));
        let ready = bg(json!({"action":"wait", "name":"gated", "until":"ready", "timeout":5}));
        assert!(ready.text.contains("ready"), "{}", ready.text);
        // Job cannot settle until input is sent. This proves overlap without a speed threshold.
        shell(ws, "printf 'independent work completed\\n' > independent");
        assert_eq!(
            std::fs::read_to_string(ws.root().join("independent")).unwrap(),
            "independent work completed\n"
        );
        bg(json!({"action":"input", "name":"gated", "text":"release\n"}));
        let settled = bg(json!({"action":"wait", "name":"gated", "timeout":5}));
        assert!(settled.text.contains("settled"), "{}", settled.text);
    });
}

#[cfg(unix)]
#[test]
#[ignore = "opt-in simulated local shell latency; no CI speed threshold"]
fn measure_foreground_vs_background_independent_work() {
    for sample in 0..5 {
        for overlap in if sample % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            with_background(|ws, bg| {
                let started = Instant::now();
                let command = "sleep 0.2; printf 'long result\\n' > long-result";
                if overlap {
                    bg(json!({"action":"start", "name":"long", "command":command}));
                } else {
                    shell(ws, command);
                }
                shell(
                    ws,
                    "sleep 0.2; printf 'independent result\\n' > independent-result",
                );
                if overlap {
                    bg(json!({"action":"wait", "name":"long", "timeout":10}));
                }
                let elapsed = started.elapsed();
                assert_eq!(
                    std::fs::read_to_string(ws.root().join("long-result")).unwrap(),
                    "long result\n"
                );
                assert_eq!(
                    std::fs::read_to_string(ws.root().join("independent-result")).unwrap(),
                    "independent result\n"
                );
                eprintln!(
                    "simulated local shell workload (two 200ms waits + writes), actual Dial shell/bg execution: sample={sample} overlap={overlap} elapsed_ms={:.3}",
                    elapsed.as_secs_f64() * 1000.0
                );
            });
        }
    }
}
