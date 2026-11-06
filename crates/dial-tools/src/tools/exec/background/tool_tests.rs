use super::*;
use crate::tools::testkit::workspace;
use std::sync::mpsc;

fn owner() -> Background {
    Background {
        table: Mutex::new(Table::default()),
        interrupt: AtomicBool::new(false),
        pty_allowed: true,
    }
}

fn run(tool: &Background, ws: &Workspace, args: Value) -> ToolOutput {
    tool.call(
        &ToolContext {
            workspace: ws,
            config: &Config::default(),
            mutation_authorizer: None,
            confirm: None,
        },
        &args,
    )
}

#[test]
fn structured_success_and_failure_survive_wait_and_output_without_process_facts() {
    let (_dir, ws) = workspace();
    for failed in [false, true] {
        let tool = owner();
        let mut expected = if failed {
            ToolOutput::err("typed failure")
        } else {
            ToolOutput::ok("typed success".into())
        };
        expected.data = Some(json!({"nested": [1, {"value": true}]}));
        expected = expected.with_effects(crate::tools::surface::outcome::Effects::None);
        let result = expected.clone();
        tool.adopt_job(AdoptedJob::Tool(ToolJob {
            name: Some("typed".into()),
            command: "test tool".into(),
            work: Box::new(move |_| result),
        }))
        .unwrap();
        for action in ["wait", "output"] {
            let result = run(
                &tool,
                &ws,
                json!({"action": action, "name": "typed", "timeout": 5}),
            );
            assert_eq!(result.is_error, failed, "{}", result.text);
            assert_eq!(result.data, expected.data);
            assert_eq!(
                serde_json::to_value(&result.execution).unwrap(),
                serde_json::to_value(&expected.execution).unwrap()
            );
            assert!(result.text.contains(&expected.text));
            assert!(result.process_outcome().is_none());
        }
        assert!(tool.drain_notifications().iter().any(|event| {
            event.kind == "complete"
                && event
                    .text
                    .contains(if failed { "failed" } else { "completed" })
        }));
    }
}

#[test]
fn cancellation_keeps_uncompleted_tools_owned_for_named_and_all_kills() {
    let (_dir, ws) = workspace();
    for all in [false, true] {
        let tool = owner();
        let (release, released) = mpsc::channel();
        let (requested, observed) = mpsc::channel();
        tool.adopt_job(AdoptedJob::Tool(ToolJob {
            name: Some("slow".into()),
            command: "delayed".into(),
            work: Box::new(move |cancel| {
                while !cancel.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                requested.send(()).unwrap();
                released.recv().unwrap();
                ToolOutput::err("cancelled after actual completion")
            }),
        }))
        .unwrap();
        let args = if all {
            json!({"action":"kill", "all":true})
        } else {
            json!({"action":"kill", "name":"slow"})
        };
        let output = run(&tool, &ws, args);
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            output.text.contains("cancellation requested"),
            "{}",
            output.text
        );
        assert!(tool.has_live_work());
        assert_eq!(tool.awaited_work().len(), 1);
        assert!(tool.session_work().is_empty());
        assert!(
            !tool
                .drain_notifications()
                .iter()
                .any(|event| event.kind == "complete")
        );
        assert!(
            tool.adopt_job(AdoptedJob::Tool(ToolJob {
                name: Some("slow".into()),
                command: "duplicate".into(),
                work: Box::new(|_| panic!("must not launch"))
            }))
            .is_err()
        );
        release.send(()).unwrap();
        let result = run(
            &tool,
            &ws,
            json!({"action":"wait", "name":"slow", "timeout":5}),
        );
        assert!(result.is_error, "{}", result.text);
        assert!(!tool.has_live_work());
    }
}

#[test]
fn call_launch_validation_rejects_shell_fields_and_direct_execution() {
    let valid =
        json!({"action":"start", "name":"job", "call":{"name":"read", "arguments":{"path":"x"}}});
    let parsed = tool_call_request(&valid).unwrap().unwrap();
    assert_eq!(parsed.name.as_deref(), Some("job"));
    assert_eq!(parsed.tool, "read");
    assert_eq!(parsed.arguments, json!({"path":"x"}));
    for field in [
        "command", "cwd", "persist", "lifetime", "pty", "stdin", "rows", "cols", "ready",
        "timeout", "until", "tail", "all", "text",
    ] {
        let mut args = valid.clone();
        args[field] = json!(false);
        assert!(tool_call_request(&args).is_err(), "{field}");
    }
    for action in [
        "wait",
        "output",
        "list",
        "kill",
        "input",
        "resize",
        "close_stdin",
    ] {
        let mut args = valid.clone();
        args["action"] = json!(action);
        assert!(tool_call_request(&args).is_err(), "{action}");
    }
    for call in [
        Value::Null,
        json!({"name":"read"}),
        json!({"name":"read","arguments":[]}),
        json!({"name":"read","arguments":{},"extra":true}),
    ] {
        let mut args = valid.clone();
        args["call"] = call;
        assert!(tool_call_request(&args).is_err());
    }
    let (_dir, ws) = workspace();
    let tool = owner();
    let output = run(&tool, &ws, valid);
    assert!(output.is_error && output.text.contains("controller admission"));
    assert!(!tool.has_live_work());
}

#[test]
fn large_results_are_bounded_with_full_structured_artifact_and_panics_complete() {
    let (dir, ws) = workspace();
    let store =
        crate::storage::artifact::ArtifactStore::new(&dir.path().join("artifacts")).unwrap();
    let ws = ws.with_artifacts(store.clone());
    let tool = owner();
    let payload = "x".repeat(MAX_OUTPUT * 3);
    let expected = payload.clone();
    tool.adopt_job(AdoptedJob::Tool(ToolJob {
        name: Some("large".into()),
        command: "large tool".into(),
        work: Box::new(move |_| {
            let mut out = ToolOutput::ok(payload.clone());
            out.data = Some(json!({"large": payload}));
            out
        }),
    }))
    .unwrap();
    let output = run(
        &tool,
        &ws,
        json!({"action":"wait","name":"large","timeout":5}),
    );
    assert!(output.text.len() < MAX_OUTPUT + 1024);
    let uri = output.data.as_ref().unwrap()["background_result"]["artifact"]
        .as_str()
        .unwrap();
    let full = store.read_text(uri, 1024 * 1024).unwrap().unwrap();
    let full: Value = serde_json::from_str(&full).unwrap();
    assert_eq!(full["text"], expected);
    assert_eq!(full["data"]["large"], expected);
    tool.adopt_job(AdoptedJob::Tool(ToolJob {
        name: Some("panic".into()),
        command: "panic".into(),
        work: Box::new(|_| panic!("test panic")),
    }))
    .unwrap();
    let result = run(
        &tool,
        &ws,
        json!({"action":"wait","name":"panic","timeout":5}),
    );
    assert!(result.is_error && result.text.contains("panicked"));
    assert!(!tool.has_live_work());
}

#[test]
fn artifact_caps_and_unavailable_storage_are_explicit() {
    for capped in [false, true] {
        let (dir, ws) = workspace();
        let ws = if capped {
            let store = crate::storage::artifact::ArtifactStore::new(&dir.path().join("artifacts"))
                .unwrap()
                .with_limits(&crate::storage::artifact::ArtifactConfig {
                    max_bytes: 1024,
                    head_bytes: 512,
                });
            ws.with_artifacts(store)
        } else {
            ws
        };
        let tool = owner();
        tool.adopt_job(AdoptedJob::Tool(ToolJob {
            name: Some("bounded".into()),
            command: "bounded".into(),
            work: Box::new(|_| {
                let mut result = ToolOutput::ok("result".into());
                result.data = Some(json!({"large": "x".repeat(MAX_OUTPUT * 2)}));
                result
            }),
        }))
        .unwrap();
        let output = run(
            &tool,
            &ws,
            json!({"action":"wait","name":"bounded","timeout":5}),
        );
        assert!(
            output.is_error,
            "lost structured data must not be reported as delivered"
        );
        assert_eq!(
            output.data.as_ref().unwrap()["background_result"]["artifact_complete"],
            false
        );
        assert!(
            output.text.contains(if capped {
                "partial output:"
            } else {
                "artifact storage unavailable"
            }),
            "{}",
            output.text
        );
        assert!(output.process_outcome().is_none());
        assert!(
            tool.jobs_view(None)
                .jobs
                .iter()
                .all(|job| job.process_outcome.is_none())
        );
    }
}

#[test]
fn oversized_results_fail_retention_instead_of_reporting_success() {
    let (_dir, ws) = workspace();
    let tool = owner();
    tool.adopt_job(AdoptedJob::Tool(ToolJob {
        name: Some("oversized".into()),
        command: "oversized".into(),
        work: Box::new(|_| ToolOutput::ok("x".repeat(MAX_TOOL_RESULT_BYTES))),
    }))
    .unwrap();
    let result = run(
        &tool,
        &ws,
        json!({"action":"wait", "name":"oversized", "timeout":5}),
    );
    assert!(
        result.is_error && result.text.contains("exceeds retention limit"),
        "{}",
        result.text
    );
    assert!(result.text.contains("full result unavailable"));
    assert!(result.text.len() < MAX_OUTPUT);
    assert!(!tool.has_live_work());
}

#[test]
fn tool_jobs_share_capacity_and_finished_names_can_be_reused() {
    let (_dir, ws) = workspace();
    let tool = owner();
    let release = Arc::new(AtomicBool::new(false));
    for index in 0..table::MAX_PROCESSES {
        let release = release.clone();
        tool.adopt_job(AdoptedJob::Tool(ToolJob {
            name: Some(format!("job{index}")),
            command: "capacity".into(),
            work: Box::new(move |cancel| {
                while !release.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                ToolOutput::ok("done".into())
            }),
        }))
        .unwrap();
    }
    assert!(!tool.accepts_adoption());
    assert!(
        tool.adopt_job(AdoptedJob::Tool(ToolJob {
            name: None,
            command: "over capacity".into(),
            work: Box::new(|_| panic!("must not launch"))
        }))
        .is_err()
    );
    release.store(true, Ordering::Release);
    for index in 0..table::MAX_PROCESSES {
        assert!(
            !run(
                &tool,
                &ws,
                json!({"action":"wait", "name": format!("job{index}"), "timeout":5})
            )
            .is_error
        );
    }
    assert!(tool.accepts_adoption());
    tool.adopt_job(AdoptedJob::Tool(ToolJob {
        name: Some("job0".into()),
        command: "replacement".into(),
        work: Box::new(|_| ToolOutput::err("replacement result")),
    }))
    .unwrap();
    let result = run(
        &tool,
        &ws,
        json!({"action":"wait", "name":"job0", "timeout":5}),
    );
    assert!(result.is_error && result.text.contains("replacement result"));
}
