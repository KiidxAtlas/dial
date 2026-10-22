//! The agent loop end to end against a scripted OpenAI-compatible server (no real model).
use dial::{
    agent::{Agent, Decision, TurnOutcome, Ui, make_backend},
    config::paths::Layout,
    config::types::{Config, VerificationCommand},
    runtime::tool_output::ToolOutput,
    storage::session::{Session, SessionMeta},
    storage::store::Store,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn system_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
fn first_user(request: &Value) -> &Value {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap()
}

enum Reply {
    Http(u16, &'static str),
    Text(&'static str),
    Say(String),
    MeasuredSay(String, u64),
    Call(&'static str, Value),
    IdentifiedCall(&'static str, Value, String),
    NarratedCall(&'static str, &'static str, Value),
    RawCall(&'static str, String),
    DroppedCall,
    Calls(Vec<(&'static str, Value)>),
    Cut,
    Think { reasoning: String, pause: Duration },
}

#[test]
fn parent_schedules_nohook_reads_concurrently() {
    struct ConcurrentRead(AtomicUsize);
    impl dial::tools::ext::Tool for ConcurrentRead {
        fn name(&self) -> &str {
            "harness_probe"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":"harness_probe","description":"fixture","parameters":{"type":"object","properties":{"index":{"type":"integer"}},"required":["index"]}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn concurrency(&self, _: &Value) -> dial::tools::ext::Concurrency {
            dial::tools::ext::Concurrency::Shared
        }
        fn call(&self, _: &dial::tools::ext::ToolContext, args: &Value) -> ToolOutput {
            self.0.fetch_add(1, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.0.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if self.0.load(Ordering::SeqCst) < 2 {
                ToolOutput::err("reads did not overlap")
            } else {
                ToolOutput::ok(format!("overlapped read {}", args["index"]))
            }
        }
    }
    let server = serve(vec![
        Reply::Call("tool_load", json!({"names":["harness_probe"]})),
        Reply::Calls(vec![
            ("harness_probe", json!({"index":0})),
            ("harness_probe", json!({"index":1})),
        ]),
        Reply::Text("Done."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.task.max_concurrency = 2;
    let probe = Arc::new(ConcurrentRead(AtomicUsize::new(0)));
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.register_tool(probe.clone()).unwrap();
    let mut ui = Recorder::default();
    a.turn("Inspect both values", &mut ui).unwrap();
    assert_eq!(probe.0.load(Ordering::SeqCst), 2);
    assert_eq!(server.requests.lock().len(), 3);
    for index in 0..2 {
        assert!(ui.results.iter().any(|(name, text, failed)| {
            name == "harness_probe" && !failed && text == &format!("overlapped read {index}")
        }));
    }
}

#[cfg(unix)]
#[test]
fn batched_read_hook_rewrites_are_validated_and_authorized_without_a_model_retry() {
    for (replacement, denied, expected) in [
        (json!({"path":"rewritten.txt"}), false, "rewritten evidence"),
        (
            json!({"path":"rewritten.txt", "offset":"invalid"}),
            false,
            "invalid tool call:",
        ),
        (json!({"path":"rewritten.txt"}), true, "denied"),
    ] {
        let server = serve(vec![
            Reply::Calls(vec![
                ("read", json!({"path":"first.txt"})),
                ("read", json!({"path":"second.txt"})),
            ]),
            Reply::Text("Done."),
        ]);
        let mut e = env(&server.url, "auto");
        e.config.task.max_concurrency = 2;
        std::fs::write(
            e.project.path().join("rewritten.txt"),
            "rewritten evidence\n",
        )
        .unwrap();
        e.config.hooks.push(dial::config::types::HookConfig {
            event: "pre_tool_use".into(),
            matcher: Some("read".into()),
            command: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "cat >/dev/null; printf x >> pre-count; printf '%s' '{}'",
                    json!({"input":replacement})
                ),
            ],
            timeout_secs: 5,
            fail_closed: None,
        });
        e.config.hooks.push(dial::config::types::HookConfig {
            event: "post_tool_use".into(),
            matcher: Some("read".into()),
            command: vec![
                "sh".into(),
                "-c".into(),
                "cat >/dev/null; printf x >> post-count".into(),
            ],
            timeout_secs: 5,
            fail_closed: None,
        });
        let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        if denied {
            a.set_permission_rules(vec![dial::agent::approval::PermissionRule {
                effect: dial::agent::approval::Effect::Deny,
                tool: "read".into(),
                command_prefix: vec![],
                paths: vec!["rewritten.txt".into()],
            }]);
        }
        let mut ui = Recorder::default();
        a.turn("Inspect the two files", &mut ui).unwrap();
        assert_eq!(server.requests.lock().len(), 2, "no rewrite retry round");
        let results: Vec<_> = ui
            .results
            .iter()
            .filter(|(name, _, _)| name == "read")
            .collect();
        assert_eq!(results.len(), 2);
        for (_, text, failed) in results {
            assert!(text.contains(expected), "{text}");
            assert_eq!(*failed, expected != "rewritten evidence", "{text}");
            if *failed {
                assert!(
                    !text.contains("rewritten evidence"),
                    "refused arguments executed"
                );
            }
        }
        assert_eq!(
            std::fs::read_to_string(e.project.path().join("pre-count")).unwrap(),
            "xx"
        );
        if denied {
            assert!(!e.project.path().join("post-count").exists());
        } else {
            assert_eq!(
                std::fs::read_to_string(e.project.path().join("post-count")).unwrap(),
                "xx"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn read_batch_lookahead_does_not_execute_a_later_withheld_read() {
    let read = || Reply::Call("read", json!({"path":"repeated.txt"}));
    let server = serve(vec![
        read(),
        read(),
        Reply::Calls(vec![
            ("read", json!({"path":"fresh.txt"})),
            ("read", json!({"path":"repeated.txt"})),
        ]),
        Reply::Text("Done."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.task.max_concurrency = 2;
    for path in ["repeated.txt", "fresh.txt"] {
        std::fs::write(e.project.path().join(path), format!("{path} evidence\n")).unwrap();
    }
    // Post-only hooks preserve parallel admission and count actual executions, not displayed results.
    e.config.hooks.push(dial::config::types::HookConfig {
        event: "post_tool_use".into(),
        matcher: Some("read".into()),
        command: vec![
            "sh".into(),
            "-c".into(),
            "cat >/dev/null; printf x >> read-count".into(),
        ],
        timeout_secs: 5,
        fail_closed: None,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("Inspect the files", &mut ui).unwrap();
    assert_eq!(server.requests.lock().len(), 4);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("read-count")).unwrap(),
        "xxx"
    );
    assert!(ui.results.iter().any(|(_, text, failed)| *failed
        && text.contains("Recovery checkpoint:")
        && text.contains("This call was not executed")));
}

#[cfg(unix)]
#[test]
fn coherent_runtime_stop_hooks_keep_blockers_and_input_waits_truthful() {
    let always_block = dial::config::types::HookConfig { event:"stop".into(), matcher:None, command:vec!["sh".into(),"-c".into(),r#"cat >/dev/null; printf '{"decision":"block","reason":"Missing acceptance condition."}'"#.into()], timeout_secs:5, fail_closed:None };
    let server = serve(vec![
        Reply::Text("Report complete."),
        Reply::Text("Report complete."),
        Reply::Text("Report complete."),
        Reply::Text("Report complete."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    e.config.hooks.push(always_block.clone());
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Produce a report", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    assert!(report.text.contains("Missing acceptance condition"));
    assert!(report.text.contains("Candidate work summary"));
    assert!(report.text.contains("Report complete."));
    assert_eq!(server.requests.lock().len(), 4);
    assert_eq!(agent.metrics().turns_completed, 0);
    assert!(agent.session().runtime().snapshots().iter().any(
        |node| node.kind == "completion" && node.result.phase == dial::runtime::Phase::Blocked
    ));

    let server = serve(vec![Reply::Call(
        "finish",
        json!({"outcome":"needs_input","summary":"Choose a destination","reason":"Which directory?"}),
    )]);
    let mut e = env(&server.url, "auto");
    e.config.hooks.push(always_block);
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Create the file", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::NeedsInput);
    assert_eq!(
        server.requests.lock().len(),
        1,
        "A stop hook cannot resume a turn waiting for the user"
    );
}

#[cfg(unix)]
#[test]
fn cancellation_during_stop_hook_keeps_candidate_summary_without_resuming() {
    struct CancellingHookUi {
        cancel: Arc<AtomicBool>,
        started: std::path::PathBuf,
    }
    impl Ui for CancellingHookUi {
        fn heartbeat(&mut self) {
            if self.started.exists() {
                self.cancel.store(true, Ordering::SeqCst);
                dial::platform::wake::signal();
            }
        }
    }
    let server = serve(vec![Reply::Text("Report complete.")]);
    let mut e = env(&server.url, "auto");
    e.config.hooks.push(dial::config::types::HookConfig {
        event: "stop".into(),
        matcher: None,
        command: vec![
            "sh".into(),
            "-c".into(),
            "cat >/dev/null; touch hook-started; sleep 5".into(),
        ],
        timeout_secs: 10,
        fail_closed: None,
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let mut agent = agent_with(&e, None, cancel.clone());
    let mut ui = CancellingHookUi {
        cancel,
        started: e.project.path().join("hook-started"),
    };
    let report = agent.turn("Produce a report", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(report.interrupted);
    assert!(report.text.contains("Candidate work summary"));
    assert!(report.text.contains("Report complete."));
    assert_eq!(server.requests.lock().len(), 1);
    assert_eq!(agent.metrics().turns_completed, 0);
}

#[test]
fn consistent_ux_promised_actions_stop_incomplete_after_bounded_recovery() {
    let server = serve(vec![
        Reply::Text("I will run the tests next."),
        Reply::Text("Let me inspect the parser."),
        Reply::Text("I will fix the parser."),
        Reply::Text("I will check the result."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("Fix the parser", &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert_eq!(a.metrics().turns_completed, 0);
    assert_eq!(server.requests.lock().len(), 4);
}

#[test]
fn consistent_ux_quoted_and_optional_actions_finish_without_an_extra_request() {
    for answer in [
        "Example string: \"I'll check that\".",
        "All tests passed. Next, I'll leave the optional cleanup for later.",
    ] {
        let server = serve(vec![Reply::Text(answer)]);
        let e = env(&server.url, "auto");
        let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        assert_eq!(
            a.turn("Explain the result", &mut Recorder::default())
                .unwrap()
                .outcome,
            TurnOutcome::Completed
        );
        assert_eq!(server.requests.lock().len(), 1);
    }
}

#[test]
fn consistent_ux_unlimited_children_bound_empty_promised_and_malformed_replies() {
    for kind in ["empty", "promise", "malformed"] {
        let side = serve_fn(
            Arc::new(move |_| match kind {
                "empty" => Reply::Text(""),
                "promise" => Reply::Text("I will inspect the parser next."),
                _ => Reply::DroppedCall,
            }),
            None,
            None,
        );
        let main = serve(vec![
            Reply::Call(
                "task",
                json!({"task":"Inspect parser","solution_space":"read-only source analysis","max_steps":0}),
            ),
            Reply::Text("The child did not finish its investigation."),
        ]);
        let mut e = env(&main.url, "auto");
        with_subagent_role(&mut e, &side.url);
        run_turn(&e, &mut Recorder::default());
        let result = task_result(&main);
        assert!(result.contains("incomplete"), "{kind}: {result}");
        assert_eq!(
            side.requests.lock().len(),
            if kind == "malformed" { 8 } else { 4 },
            "{kind}"
        );
    }
}

#[test]
fn consistent_ux_background_worker_findings_are_delivered_without_status_polling() {
    let side = serve(vec![Reply::Text(
        "The parser drops the last line at EOF. Fix parse_tail in parser.rs.",
    )]);
    let main = serve(vec![Reply::Text("The worker identified the EOF defect.")]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let started = a
        .start_subagents(
            &json!({"task":"Inspect parser","solution_space":"read-only source analysis"}),
            &mut ui,
        )
        .unwrap();
    assert_eq!(started[0]["status"], "running");
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.live_task_control().any_running() {
        assert!(Instant::now() < deadline);
        a.pump_subagents(&mut ui);
        thread::sleep(Duration::from_millis(10));
    }
    a.turn("Use the worker findings", &mut ui).unwrap();
    let request = main.requests.lock()[0].clone();
    let notices: Vec<_> = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.contains("[Worker result from Dial"))
        })
        .collect();
    assert_eq!(notices.len(), 1);
    let notice = notices[0]["content"].as_str().unwrap();
    assert!(notice.contains("Fix parse_tail in parser.rs"));
    assert!(!notice.contains("portable_rules") && !notice.contains("solution_space"));
}

#[test]
fn worker_message_reaches_parent_next_step() {
    let side = serve(vec![
        Reply::Call(
            "message",
            json!({"to":"parent","text":"parse_tail drops the last line"}),
        ),
        Reply::Text("Investigation complete."),
    ]);
    let main = serve(vec![Reply::Text("Noted the worker message.")]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.start_subagents(
        &json!({"task":"Inspect parser","solution_space":"read-only source analysis"}),
        &mut ui,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.live_task_control().any_running() {
        assert!(Instant::now() < deadline);
        a.pump_subagents(&mut ui);
        thread::sleep(Duration::from_millis(10));
    }
    a.turn("Use the worker message", &mut ui).unwrap();
    let request = main.requests.lock()[0].clone();
    let text = request["messages"].to_string();
    assert!(text.contains("[Message from"), "{text}");
    assert!(text.contains("parse_tail drops the last line"), "{text}");
}

#[test]
fn repeated_silence_can_retry_by_explicit_user_direction() {
    #[derive(Default)]
    struct RecoveryUi {
        asked: usize,
        phases: Vec<dial::agent::RequestPhase>,
    }
    impl Ui for RecoveryUi {
        fn interactive_input(&self) -> bool {
            true
        }
        fn request_phase(&mut self, phase: dial::agent::RequestPhase) {
            self.phases.push(phase);
        }
        fn ask_user(&mut self, _: &str, _: &str, choices: &[String]) -> Option<String> {
            self.asked += 1;
            Some(choices[0].clone())
        }
    }
    let server = serve(vec![
        Reply::Think {
            reasoning: "thinking".into(),
            pause: Duration::from_secs(2),
        },
        Reply::Think {
            reasoning: "thinking".into(),
            pause: Duration::from_secs(2),
        },
        Reply::Text("Resumed after explicit user direction."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.request_recovery.silence_secs = 1;
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut a = agent_with(&e, None, interrupt.clone());
    let mut ui = RecoveryUi::default();
    let report = a
        .turn("Finish the task without losing its context", &mut ui)
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.text, "Resumed after explicit user direction.");
    assert_eq!(ui.asked, 1);
    use dial::agent::RequestPhase;
    assert_eq!(
        ui.phases
            .iter()
            .filter(|phase| **phase == RequestPhase::Cancelling)
            .count(),
        2
    );
    assert_eq!(
        ui.phases
            .iter()
            .filter(|phase| **phase == RequestPhase::Waiting)
            .count(),
        3
    );
    assert!(ui.phases.contains(&RequestPhase::Generating));
    assert_eq!(server.requests.lock().len(), 3);
    assert!(!interrupt.load(Ordering::SeqCst));
    assert!(!e.project.path().join("gate-leak.txt").exists());
}

#[test]
fn managed_restart_never_targets_an_external_endpoint() {
    let server = serve(vec![]);
    let e = env(&server.url, "auto");
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    assert!(agent.restart_managed_model().is_err());
    assert!(server.requests.lock().is_empty());
}

#[test]
fn silent_request_retries_only_inference_and_preserves_completed_work() {
    let server = serve(vec![
        Reply::Call(
            "write",
            json!({"path":"kept.txt","content":"completed work"}),
        ),
        Reply::Think {
            reasoning: "thinking".into(),
            pause: Duration::from_secs(2),
        },
        Reply::Text("Recovered with completed work preserved."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.request_recovery.silence_secs = 1;
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut a = agent_with(&e, None, interrupt.clone());
    let mut ui = Recorder::default();
    let report = a
        .turn(
            "Write kept.txt, then finish without replaying completed work",
            &mut ui,
        )
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("kept.txt")).unwrap(),
        "completed work"
    );
    assert!(
        !e.project.path().join("gate-leak.txt").exists(),
        "late tool from discarded request must never execute"
    );
    assert!(
        !interrupt.load(Ordering::SeqCst),
        "request recovery is not a whole-turn user interrupt"
    );
    assert_eq!(
        ui.results
            .iter()
            .filter(|(name, _, _)| name == "write")
            .count(),
        1
    );
    assert!(
        ui.notices
            .iter()
            .any(|notice| notice.contains("same context"))
    );
    assert_eq!(report.text, "Recovered with completed work preserved.");
    let requests = server.requests.lock();
    assert_eq!(
        requests.len(),
        3,
        "write, silent attempt, one retry; the final diff review rides on the first request after the write"
    );
    assert_eq!(
        requests[1]["messages"], requests[2]["messages"],
        "retry must reuse the same request context"
    );
}

#[test]
fn repeated_silence_reports_incomplete_without_erasing_the_task_or_files() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"Preserve the unfinished objective"}]}),
        ),
        Reply::Call(
            "write",
            json!({"path":"kept.txt","content":"completed work"}),
        ),
        Reply::Think {
            reasoning: "thinking".into(),
            pause: Duration::from_secs(2),
        },
        Reply::Think {
            reasoning: "thinking".into(),
            pause: Duration::from_secs(2),
        },
    ]);
    let mut e = env(&server.url, "auto");
    e.config.request_recovery.silence_secs = 1;
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut a = agent_with(&e, None, interrupt.clone());
    let report = a
        .turn(
            "Preserve existing work and report unfinished recovery honestly",
            &mut Recorder::default(),
        )
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert!(a.todo().has_open());
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("kept.txt")).unwrap(),
        "completed work"
    );
    assert!(!e.project.path().join("gate-leak.txt").exists());
    assert!(!interrupt.load(Ordering::SeqCst));
    assert_eq!(
        server.requests.lock().len(),
        4,
        "only one automatic retry is allowed"
    );
}

/// Request-local contract/task guidance is deliberately outside the reusable history prefix.
fn conversation_messages(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            !(message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("[Current executable tool contract]")))
        })
        .cloned()
        .collect()
}

/// Serves the scripted replies in order (SSE when the client streams) and records every request.
struct Scripted {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

fn serve(script: Vec<Reply>) -> Scripted {
    serve_with(script, None, None)
}

fn serve_authenticated(script: Vec<Reply>, token: Option<&'static str>) -> Scripted {
    serve_with(script, token, None)
}

/// `lm_context`: behave like LM Studio, reporting the model as loaded with this context window.
fn serve_with(
    script: Vec<Reply>,
    token: Option<&'static str>,
    lm_context: Option<u32>,
) -> Scripted {
    let script = Mutex::new(script.into_iter());
    serve_fn(
        Arc::new(move |_| {
            script
                .lock()
                .next()
                .unwrap_or(Reply::Text("(script exhausted)"))
        }),
        token,
        lm_context,
    )
}

/// Picks each chat reply from the request itself, so clients that arrive concurrently (parallel
/// subagents) get the right answer in any order. Every connection is served on its own thread.
type Choose = Arc<dyn Fn(&Value) -> Reply + Send + Sync>;

/// Parallel tests each hold sockets, databases and child pipes; macOS defaults to 256 descriptors,
/// which the suite exceeds. Raise this process's soft limit (never above the hard limit), once.
fn raise_fd_limit() {
    #[cfg(unix)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let mut lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: plain getrlimit/setrlimit calls on a local, fully initialised struct.
            unsafe {
                if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
                    let target = lim.rlim_max.min(10_240);
                    if lim.rlim_cur < target {
                        lim.rlim_cur = target;
                        libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
                    }
                }
            }
        });
    }
}

fn serve_fn(choose: Choose, token: Option<&'static str>, lm_context: Option<u32>) -> Scripted {
    raise_fd_limit();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (choose, seen) = (choose.clone(), seen.clone());
            thread::spawn(move || answer(stream, &choose, &seen, token, lm_context));
        }
    });
    Scripted { url, requests }
}

fn answer(
    mut stream: TcpStream,
    choose: &Choose,
    seen: &Mutex<Vec<Value>>,
    token: Option<&str>,
    lm_context: Option<u32>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0usize;
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap_or(0);
    let mut authorization = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap();
        }
        if line.to_ascii_lowercase().starts_with("authorization:") {
            authorization = line
                .split_once(':')
                .map(|(_, value)| value.trim().to_owned());
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    // Metadata probes are not chat requests: they must never consume a scripted reply.
    if request_line.starts_with("GET ") {
        let (code, payload) = match lm_context {
            Some(context) if request_line.contains("/api/v0/models/") => (
                200,
                json!({
                "id":"scripted","object":"model","type":"llm","state":"loaded",
                "max_context_length":1_048_576,"loaded_context_length":context})
                .to_string(),
            ),
            _ => (404, "{\"error\":\"not found\"}".to_owned()),
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        return;
    }
    if token.is_some_and(|token| authorization.as_deref() != Some(&format!("Bearer {token}"))) {
        write!(
            stream,
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        return;
    }
    let request: Value = serde_json::from_slice(&body).unwrap();
    let streaming = request["stream"] == true;
    seen.lock().push(request.clone());
    if std::env::var("DIAL_TRACE").is_ok() {
        let msgs = request["messages"].as_array().cloned().unwrap_or_default();
        for m in msgs.iter().skip(msgs.len().saturating_sub(4)) {
            let c = m["content"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| m.to_string());
            eprintln!(
                "TRACE req#{} {}: {}",
                seen.lock().len() - 1,
                m["role"],
                c.chars().take(160).collect::<String>().replace('\n', " | ")
            );
        }
    }
    let say = |text: &str| (json!({"role":"assistant","content":text}), "stop");
    let mut measured_prompt = None;
    let (delta, finish) = match choose(&request) {
        Reply::Http(code, message) => {
            let payload = json!({"error":{"message":message}}).to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            return;
        }
        Reply::Think { reasoning, pause } => {
            assert!(streaming);
            let first = format!(
                "data: {}\n\n",
                json!({"choices":[{"index":0,"delta":{"reasoning_content":reasoning},"finish_reason":null}]})
            );
            // A fully formed write appears later in the cancelled response; it must never run.
            let tail = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"late","type":"function","function":{"name":"write","arguments":json!({"path":"gate-leak.txt","content":"must not run"}).to_string()}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{first}",
                first.len() + tail.len()
            );
            let _ = stream.flush();
            thread::sleep(pause);
            let _ = write!(stream, "{tail}");
            return;
        }
        Reply::Text(t) => say(t),
        Reply::Say(t) => say(&t),
        Reply::MeasuredSay(text, tokens) => {
            measured_prompt = Some(tokens);
            say(&text)
        }
        Reply::Cut => (json!({"role":"assistant","content":""}), "length"),
        Reply::Call(name, args) => (
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function",
            "function":{"name":name,"arguments":args.to_string()}}]}),
            "tool_calls",
        ),
        Reply::IdentifiedCall(name, args, id) => (
            json!({"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function",
            "function":{"name":name,"arguments":args.to_string()}}]}),
            "tool_calls",
        ),
        Reply::NarratedCall(text, name, args) => (
            json!({"role":"assistant","content":text,"tool_calls":[{"index":0,"id":"call_1","type":"function",
            "function":{"name":name,"arguments":args.to_string()}}]}),
            "tool_calls",
        ),
        Reply::DroppedCall => (json!({"role":"assistant","content":null}), "tool_calls"),
        Reply::RawCall(name, raw) => (
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function",
            "function":{"name":name,"arguments":raw}}]}),
            "tool_calls",
        ),
        Reply::Calls(calls) => (
            json!({"role":"assistant","tool_calls":calls.into_iter().enumerate().map(|(index,(name,args))|
            json!({"index":index,"id":format!("call_{index}"),"type":"function","function":{"name":name,"arguments":args.to_string()}})).collect::<Vec<_>>()}),
            "tool_calls",
        ),
    };
    // Report what a real provider would: the request's own size, at the estimator's rate of about four bytes per token.
    let prompt_tokens = measured_prompt.unwrap_or(body.len() as u64 / 4);
    let usage = json!({"prompt_tokens":prompt_tokens,"completion_tokens":5,"total_tokens":prompt_tokens + 5});
    let (kind, payload) = if streaming {
        let chunk = |c: Value| format!("data: {c}\n\n");
        (
            "text/event-stream",
            format!(
                "{}{}{}data: [DONE]\n\n",
                chunk(json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]})),
                chunk(json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})),
                chunk(json!({"choices":[],"usage":usage}))
            ),
        )
    } else {
        (
            "application/json",
            json!({"choices":[{"index":0,"message":delta,"finish_reason":finish}],"usage":usage})
                .to_string(),
        )
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
}

struct Env {
    _home: tempfile::TempDir,
    project: tempfile::TempDir,
    layout: Layout,
    config: Config,
}

/// Tests that run real `bash` need `sandbox-exec`, which macOS cannot nest. Skip (with a note) only
/// when this process is itself sandboxed; everywhere else the test must really run.
fn skip_without_sandbox() -> bool {
    let nested = dial::tools::sandbox::already_sandboxed();
    if nested {
        eprintln!("skipping: already inside a sandbox, which macOS cannot nest");
    }
    nested
}

fn env(url: &str, approval: &str) -> Env {
    raise_fd_limit();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let overlay: toml::Table = toml::from_str(&format!(
        "model = \"scripted\"\napproval = \"{approval}\"\nintegrations = []\n[edit]\nmode = \"hashline\"\n[endpoint]\nbase_url = \"{url}\"\n[sandbox]\nmode = \"strict\"\n")).unwrap();
    let mut config = Config::load(&layout, Some(overlay)).unwrap();
    // Most scripted providers test one agent task, not the separate architecture maintenance call.
    config.codebase.synthesize_architecture = false;
    config.codebase.architecture_model.clear(); // Scripted architecture tests use their configured HTTP provider.
    // Scripted fixtures deliberately call named tools without discovery. Economy selection
    // gets explicit coverage below; unrelated approval/worker tests keep their original toolbox.
    config.assistance.compact_tools = false;
    Env {
        _home: home,
        project,
        layout,
        config,
    }
}

#[test]
fn capability_search_enables_real_history_retrieval_in_the_next_round() {
    let server = serve(vec![
        Reply::Call("tool_search", json!({"query":"session_search"})),
        Reply::Call("session_search", json!({"query":"DISCOVERY-KEY","limit":3})),
        Reply::Text("Retrieved the current session."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.assistance.compact_tools = true;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a
        .turn("Retrieve DISCOVERY-KEY from session history", &mut ui)
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(ui.calls, ["tool_search", "session_search"]);
    let (_, text, error) = ui
        .results
        .iter()
        .find(|(name, _, _)| name == "session_search")
        .unwrap();
    assert!(!error, "{text}");
    assert!(
        text.contains("DISCOVERY-KEY"),
        "retrieval must return actual persisted history"
    );
    assert!(
        server.requests.lock()[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|schema| schema["function"]["name"] == "session_search")
    );
}

#[test]
fn discovery_does_not_authorize_an_absent_tool_in_the_same_batch() {
    let server = serve(vec![
        Reply::Calls(vec![
            ("tool_search", json!({"query":"session_search"})),
            ("session_search", json!({"query":"DISCOVERY-KEY","limit":3})),
        ]),
        Reply::Call("session_search", json!({"query":"DISCOVERY-KEY","limit":3})),
        Reply::Text("Retrieved with the next-round contract."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.assistance.compact_tools = true;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("Retrieve DISCOVERY-KEY from session history", &mut ui)
        .unwrap();
    let results: Vec<_> = ui
        .results
        .iter()
        .filter(|(name, _, _)| name == "session_search")
        .collect();
    assert_eq!(results.len(), 2);
    assert!(
        results[0].2,
        "the old request contract still forbids the first call"
    );
    assert!(
        !results[1].2,
        "a fresh request may execute the enabled tool"
    );
    assert!(results[1].1.contains("DISCOVERY-KEY"));
}

#[test]
fn cloud_tool_economy_preserves_core_and_requires_loading_specialists() {
    let server = serve(vec![
        Reply::Call("tool_load", json!({"names":["web_fetch"]})),
        Reply::Text("loaded"),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.integrations = vec!["web_fetch".into()];
    e.config.sandbox.allow_network = false;
    e.config.assistance.compact_tools = true;
    e.config.assistance.initial_tool_limit = 13;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Inspect a file", &mut Recorder::default()).unwrap();
    let requests = server.requests.lock();
    let names = |index: usize| {
        requests[index]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect::<Vec<_>>()
    };
    let initial = names(0);
    // Core tools are always offered, including `task`, `skill`, `message`, `check` and the memory tools, so the initial set is the core, not `initial_tool_limit`.
    assert!(initial.len() <= 20, "{initial:?}");
    for core in [
        "read",
        "edit",
        "write",
        "grep",
        "find",
        "glob",
        "bash",
        "todo",
        "finish",
        "check",
        "tool_load",
        "tool_search",
        "artifact_read",
        "working_state",
    ] {
        assert!(initial.contains(&core), "{core}");
    }
    assert!(!initial.contains(&"web_fetch"));
    assert!(names(1).contains(&"web_fetch"));
    let report = a.efficiency_report();
    assert_eq!(
        report["usage"]["by_model"]["openai-compatible/scripted"]["main"]["requests"],
        2
    );
    assert!(report["usage"]["main"]["input"].as_u64().unwrap() > 0);
    assert_eq!(report["projection_bytes"]["requests"], 2);
    assert_eq!(report["projection_bytes"]["lightweight_requests"], 2);
    assert_eq!(report["completed"], true);
    assert!(
        report["projection_bytes"]["tool_schema_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn lightweight_contract_escalates_after_a_tool_error_and_resets_next_turn() {
    let server = serve(vec![
        Reply::Call("read", json!({"path":"missing"})),
        Reply::Text("blocked"),
        Reply::Text("next"),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Read one file", &mut Recorder::default()).unwrap();
    let first = a.efficiency_report();
    assert_eq!(first["projection_bytes"]["lightweight_requests"], 1);
    a.turn("Answer briefly", &mut Recorder::default()).unwrap();
    assert_eq!(a.efficiency_report()["usage"]["main"]["requests"], 1);
    assert_eq!(
        a.efficiency_report()["projection_bytes"]["lightweight_requests"],
        1
    );
}

#[test]
fn shared_economy_reduces_projected_schema_and_guidance_without_changing_the_answer() {
    let mut measured = Vec::new();
    for enabled in [false, true] {
        let server = serve(vec![Reply::Text("ok")]);
        let mut e = env(&server.url, "auto");
        e.config.efficiency.enabled = enabled;
        e.config.assistance.compact_tools = true;
        let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let answer = a
            .turn("Reply with exactly ok", &mut Recorder::default())
            .unwrap();
        assert_eq!(answer.text, "ok");
        let metrics = a.efficiency_report();
        let bytes = &metrics["projection_bytes"];
        measured.push((
            bytes["tool_schema_bytes"].as_u64().unwrap(),
            bytes["request_tail_bytes"].as_u64().unwrap(),
        ));
        assert_eq!(metrics["usage"]["main"]["requests"], 1);
        assert_eq!(server.requests.lock().len(), 1);
    }
    assert!(measured[1].0 < measured[0].0 && measured[1].1 <= measured[0].1);
    eprintln!(
        "fixture projected schema/tail bytes: legacy {:?}, efficient {:?}; not billed tokens",
        measured[0], measured[1]
    );
}

#[derive(Default)]
struct Recorder {
    text: String,
    calls: Vec<String>,
    approvals: Vec<String>,
    summaries: Vec<String>,
    notices: Vec<String>,
    roles: Vec<String>,
    images: Vec<Value>,
    results: Vec<(String, String, bool)>,
    deny: bool,
}

impl Ui for Recorder {
    // Bookkeeping the TUI hides still counts as observed output for these tests.
    fn diagnostic(&mut self, message: &str) {
        self.notices.push(message.to_owned());
    }
    fn text_delta(&mut self, text: &str) {
        self.text.push_str(text);
    }
    fn tool_call(&mut self, name: &str, _: &str) {
        self.calls.push(name.into());
    }
    fn tool_result(&mut self, name: &str, output: &ToolOutput) {
        self.results
            .push((name.into(), output.text.clone(), output.is_error));
    }
    fn notice(&mut self, message: &str) {
        self.notices.push(message.into());
    }
    fn image(&mut self, reference: &Value) {
        self.images.push(reference.clone());
    }
    fn role(&mut self, role: &str, _: &str) {
        self.roles.push(role.into());
    }
    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        self.approvals.push(tool.into());
        self.summaries.push(summary.into());
        if self.deny {
            Decision::Deny
        } else {
            Decision::Allow
        }
    }
}

fn agent<'a>(
    e: &'a Env,
    _backend: &'a mut dial::llm::provider::Backend,
    store: Option<&'a Store>,
) -> Agent<'a> {
    agent_with(e, store, Arc::new(AtomicBool::new(false)))
}

fn agent_with<'a>(e: &'a Env, store: Option<&'a Store>, interrupt: Arc<AtomicBool>) -> Agent<'a> {
    let mut session = Session::create(
        &e.layout,
        SessionMeta {
            model: "scripted".into(),
        },
    )
    .unwrap();
    if e.config
        .harness
        .tools
        .iter()
        .any(|tool| tool.as_str() == "task")
    {
        // Delegation tests opt in to `task`; the lazy tool gate otherwise withholds it.
        session
            .set_control_state(json!({"enabled_tools": ["task"]}))
            .unwrap();
    }
    let backend = make_backend(&e.config).unwrap();
    Agent::interactive(
        &e.config,
        backend,
        e.layout.clone(),
        session,
        None,
        store,
        interrupt,
    )
    .unwrap()
}

fn open_todo() -> Reply {
    Reply::Call(
        "todo",
        json!({"op":"set","tasks":[{"content":"create and verify marker","status":"in_progress"}]}),
    )
}

#[test]
fn architecture_uses_its_selected_model_and_restores_coding() {
    let main = serve(vec![
        Reply::Text("First answer."),
        Reply::Text("Second answer."),
    ]);
    let side=serve(vec![Reply::Say(json!({"sections":[
        {"id":"purpose","kind":"purpose","title":"Application","summary":"Routes requests.","sources":["main.rs"]},
        {"id":"router","kind":"component","title":"Router","summary":"run handles routing.","sources":["main.rs"]}
    ],"unknowns":[]}).to_string())]);
    let mut e = env(&main.url, "auto");
    e.config.codebase.synthesize_architecture = true;
    e.config.codebase.ambient_context = true;
    e.config.model_roles.insert(
        "small".into(),
        dial::llm::model_config::ModelSelection {
            provider: "summary".into(),
            model: "architecture-model".into(),
            base_url: Some(side.url.clone()),
            context_window: Some(8192),
            ..Default::default()
        },
    );
    e.config.validate().unwrap();
    std::fs::write(
        e.project.path().join("main.rs"),
        "fn main() { run(); }\nfn run() {}\n",
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    assert_eq!(
        a.turn("Explain routing", &mut ui).unwrap().text,
        "First answer."
    );
    assert_eq!(
        a.turn("Explain again", &mut ui).unwrap().text,
        "Second answer."
    );
    assert_eq!(main.requests.lock().len(), 2);
    let requests = side.requests.lock();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "architecture-model");
    assert!(
        requests[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Summarize useful architecture")
    );
    assert!(
        ui.notices
            .iter()
            .any(|notice| notice.contains("Saved 2 architecture sections"))
    );
}

#[test]
fn architecture_is_built_after_the_task_and_reused_without_extra_generations() {
    let server = serve_fn(
        Arc::new(|request| {
            if request["messages"][0]["content"]
                .as_str()
                .unwrap_or("")
                .starts_with("Summarize useful architecture")
            {
                Reply::Say(json!({"sections":[
                {"id":"purpose","kind":"purpose","title":"Application","summary":"Routes requests.","sources":["main.rs"]},
                {"id":"router","kind":"component","title":"Router","summary":"run handles routing.","sources":["main.rs"]}
            ],"unknowns":["Persistence is not shown."]}).to_string())
            } else {
                Reply::Text("Explained.")
            }
        }),
        None,
        None,
    );
    let mut e = env(&server.url, "auto");
    e.config.codebase.synthesize_architecture = true;
    e.config.codebase.ambient_context = true;
    std::fs::write(
        e.project.path().join("main.rs"),
        "fn main() { run(); }\nfn run() {}\n",
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("Explain routing", &mut ui).unwrap();
    assert_eq!(server.requests.lock().len(), 2);
    assert!(
        !server.requests.lock()[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Synthesize")
    );
    drop(a);
    let mut restarted = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    restarted.turn("Explain routing again", &mut ui).unwrap();
    let requests = server.requests.lock();
    assert_eq!(
        requests.len(),
        3,
        "unchanged sources must not trigger another synthesis"
    );
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("run handles routing")
    );
    assert!(
        ui.notices
            .iter()
            .any(|notice| notice.contains("Saved 2 architecture sections"))
    );
}

#[test]
fn stale_assumptions_block_blind_edit_retries_until_current_source_is_read() {
    let server = serve(vec![
        Reply::Call(
            "write",
            json!({"path":"main.rs","content":"fn main() { println!(\"changed\"); }\n"}),
        ),
        Reply::Call(
            "write",
            json!({"path":"main.rs","content":"fn main() { println!(\"changed\"); }\n"}),
        ),
        Reply::Call("read", json!({"path":"main.rs"})),
        Reply::Call(
            "write",
            json!({"path":"main.rs","content":"fn main() { println!(\"changed\"); }\n"}),
        ),
        Reply::Text("Updated after checking current evidence."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.enabled = false;
    std::fs::write(e.project.path().join("main.rs"), "fn main() {}\n").unwrap();
    let cache = Arc::new(dial::tools::codebase::Codebase::open(&e.layout).unwrap());
    let ws = dial::tools::Workspace::new(e.project.path()).unwrap();
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let tool = dial::tools::codebase::tool(cache.clone(), "old".into());
    let saved=tool.call(&dial::tools::ext::ToolContext { workspace: &ws, config: &e.config, mutation_authorizer: None, confirm: None },&json!({"action":"remember","topic":"entry ownership","summary":"Main owns dispatch","sources":["main.rs"]}));
    assert!(!saved.is_error, "{}", saved.text);
    cache
        .invalidate_finding("entry ownership", "Ownership has not been established")
        .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Change main output", &mut Recorder::default())
        .unwrap();
    let requests = server.requests.lock();
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Before editing main.rs")
    );
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("Read current main.rs before retrying")
    );
    assert!(
        requests[3]["messages"].to_string().contains("fn main() {}"),
        "blocked retries must not have changed the file before read"
    );
    assert!(
        std::fs::read_to_string(e.project.path().join("main.rs"))
            .unwrap()
            .contains("changed")
    );
}

#[test]
fn malformed_architecture_does_not_fail_the_task_or_repeat_automatically() {
    let server = serve(vec![
        Reply::Text("Done."),
        Reply::Text("not architecture JSON"),
        Reply::Text("Done again."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.codebase.synthesize_architecture = true;
    e.config.codebase.ambient_context = true;
    std::fs::write(e.project.path().join("main.rs"), "fn main() {}\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    assert_eq!(
        a.turn("Explain", &mut ui).unwrap().outcome,
        dial::agent::TurnOutcome::Completed
    );
    assert!(
        ui.notices
            .iter()
            .any(|n| n.contains("Architecture update deferred"))
    );
    a.turn("Explain again", &mut ui).unwrap();
    // A malformed synthesis is deferred, not recorded as attempted, so the next completed turn
    // retries it once (see `defer_architecture_attempt_on_failure`); the task itself never fails.
    assert_eq!(server.requests.lock().len(), 4);
}

#[test]
fn queued_work_skips_architecture_maintenance_without_consuming_model_calls() {
    struct QueuedUi;
    impl Ui for QueuedUi {
        fn maintenance_ready(&mut self) -> bool {
            false
        }
    }
    let server = serve(vec![Reply::Text("Done.")]);
    let mut e = env(&server.url, "auto");
    e.config.codebase.synthesize_architecture = true;
    e.config.codebase.ambient_context = true;
    std::fs::write(e.project.path().join("main.rs"), "fn main() {}\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Explain", &mut QueuedUi).unwrap();
    assert_eq!(server.requests.lock().len(), 1);
    let cache = dial::tools::codebase::Codebase::open(&e.layout).unwrap();
    assert!(cache.architecture_synthesis(8000, false).unwrap().is_some());
}

fn close_todo() -> Reply {
    Reply::Call("todo", json!({"op":"done"}))
}

#[test]
fn report_and_todo_done_in_one_reply_closes_without_dummy_reads() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"Deliver recommendations"}]}),
        ),
        Reply::NarratedCall(
            "Here are the findings: runner mismatch explains the failed verification; preserve code and rerun with the project environment.",
            "todo",
            json!({"op":"done"}),
        ),
        Reply::Text("Report delivered."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("Give recommendations", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert!(!a.todo().has_open());
    assert_eq!(server.requests.lock().len(), 3);
}

#[test]
fn synthesis_step_closes_after_text_without_a_dummy_tool_call() {
    let synthesis = "Architecture: preserve the control-plane boundary. Tests: cover failure recovery. Documentation: explain verification limits.";
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"Provide synthesis"}]}),
        ),
        Reply::Text(synthesis),
    ]);
    let e = env(&server.url, "auto");
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Provide complete synthesis", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.text, synthesis);
    assert!(!agent.todo().has_open());
    assert_eq!(
        server.requests.lock().len(),
        2,
        "No dummy read, todo completion round or duplicate report"
    );
}

#[test]
fn repeated_todo_rejections_stop_despite_reworded_claims() {
    let mut replies = vec![Reply::Call(
        "todo",
        json!({"op":"set","tasks":[{"content":"Inspect the source","investigation":true}]}),
    )];
    for i in 0..4 {
        replies.push(Reply::Call("todo", json!({"op":"done", "finding":format!("It is complete, explanation {i}"),"evidence":[""]})));
    }
    let server = serve(replies);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Inspect the source", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 6);
    assert!(
        requests.last().unwrap()["messages"]
            .to_string()
            .contains("Escalation: 4 todo updates have been rejected")
    );
}

#[test]
fn todo_recovery_supplies_a_completion_template_and_clears_after_correction() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"Inspect source","investigation":true}]}),
        ),
        Reply::Call("read", json!({"path":"source.txt"})),
        Reply::Call("todo", json!({"op":"done","finding":42})),
        Reply::Call(
            "todo",
            json!({"op":"done","finding":"source.txt contains evidence","evidence":["source.txt:1"]}),
        ),
        Reply::Text("Investigation complete."),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("source.txt"), "evidence\n").unwrap();
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Inspect source", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert!(!agent.todo().has_open());
    let requests = server.requests.lock();
    let reminder = requests[3]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(reminder.contains("[Todo recovery from Dial]"), "{reminder}");
    assert!(reminder.contains("{\"op\":\"done\"}"), "{reminder}");
    assert!(
        reminder.contains("no finding or evidence is required"),
        "{reminder}"
    );
    assert!(
        !requests[4]["messages"].as_array().unwrap().iter().any(|m| {
            m["dial_transient"] == true
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("[Todo recovery from Dial]"))
        })
    );
}

#[test]
fn todo_recovery_bounds_alternating_operations_despite_successful_reads_and_views() {
    let mut replies = vec![Reply::Call(
        "todo",
        json!({"op":"set","tasks":[{"content":"Inspect source","investigation":true}]}),
    )];
    for attempt in 0..4 {
        replies.push(Reply::Call(
            "read",
            json!({"path":"source.txt","offset":attempt + 1,"limit":1}),
        ));
        replies.push(Reply::Call("todo", json!({"op":"view"})));
        replies.push(if attempt % 2 == 0 {
            Reply::Call("todo", json!({"op":"done","finding":format!("Found it {attempt}"),"evidence":[""]}))
        } else {
            Reply::Call("todo", json!({"op":"set","tasks":[{"content":format!("Found it {attempt}"),"investigation":true,"status":"invalid"}]}))
        });
    }
    replies.push(Reply::Text("false success"));
    let server = serve(replies);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    e.config.reliability.enabled = false;
    e.config.harness.max_steps = 0;
    std::fs::write(
        e.project.path().join("source.txt"),
        "one\ntwo\nthree\nfour\n",
    )
    .unwrap();
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Inspect source", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 14);
    assert!(
        requests.last().unwrap()["messages"]
            .to_string()
            .contains("Escalation: 4 todo updates have been rejected")
    );
    assert_eq!(agent.todo().tasks().len(), 1);
    assert_eq!(agent.todo().tasks()[0].content, "Inspect source");
}

#[test]
fn completion_nudge_is_sent_once_and_never_saved_to_history() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"Inspect source","investigation":true}]}),
        ),
        Reply::Text("Source inspection is still open."),
        Reply::Call("read", json!({"path":"source.txt"})),
        Reply::Call(
            "todo",
            json!({"op":"done","finding":"source.txt contains evidence","evidence":["source.txt:1"]}),
        ),
        Reply::Text("Investigation complete."),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("source.txt"), "evidence\n").unwrap();
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Inspect source", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = server.requests.lock();
    let nudge = "You said you would continue with a tool call or action but stopped";
    let sent_with_nudge = requests
        .iter()
        .filter(|r| r["messages"].to_string().contains(nudge))
        .count();
    assert_eq!(sent_with_nudge, 1, "the nudge should be sent exactly once");
    let last = requests.last().unwrap();
    assert!(
        !last["messages"].to_string().contains(nudge),
        "the nudge must not be replayed from history"
    );
}

#[test]
fn identical_rejected_todo_updates_stop_even_when_the_plan_changes_between_them() {
    let mut replies = vec![Reply::Call(
        "todo",
        json!({"op":"set","tasks":[{"content":"Inspect source","investigation":true}]}),
    )];
    for attempt in 0..4 {
        replies.push(Reply::Call("todo", json!({"op":"done","index":0})));
        replies.push(Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":format!("Inspect source {attempt}"),"investigation":true}]}),
        ));
    }
    replies.push(Reply::Text("should not be reached"));
    let server = serve(replies);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    e.config.reliability.enabled = false;
    e.config.harness.max_steps = 0;
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Inspect source", &mut Recorder::default())
        .unwrap();
    // A successful plan change resets the rejection history, so nothing escalates or stops.
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    assert_eq!(report.text, "should not be reached");
    assert!(
        server.requests.lock().last().unwrap()["messages"]
            .to_string()
            .contains("Escalation:")
    );
}

#[test]
fn todo_recovery_schema_errors_guide_done_without_appending_more_tasks() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"Inspect source","investigation":true}]}),
        ),
        Reply::Call("read", json!({"path":"source.txt"})),
        Reply::Call(
            "todo",
            json!({"op":"done","finding":"Found evidence","evidence":"source.txt:1"}),
        ),
        Reply::Call(
            "todo",
            json!({"op":"done","finding":"Found evidence","evidence":["source.txt:1"]}),
        ),
        Reply::Text("Complete."),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("source.txt"), "evidence\n").unwrap();
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Inspect source", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = server.requests.lock();
    let feedback = requests[3]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| {
            m["role"] == "tool"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("invalid tool call"))
        })
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(!feedback.contains("\"op\":\"append\""), "{feedback}");
    assert!(
        feedback.contains("\"op\":\"done\"")
            && feedback.contains("no finding or evidence is required"),
        "{feedback}"
    );
}

#[test]
fn repeated_invalid_plans_stop_despite_reworded_tasks_and_interleaved_reads() {
    let mut replies = Vec::new();
    for i in 0..4 {
        if i > 0 {
            replies.push(Reply::Call("read", json!({"path":"src/lib.rs"})));
        }
        replies.push(Reply::Call(
            "todo",
            json!({
                "op":"set", "tasks":[{
                    "content":format!("Already inspected the source, attempt {i}"),
                    "investigation":true, "status":"invalid"
                }]
            }),
        ));
    }
    let server = serve(replies);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    e.config.reliability.enabled = false;
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Inspect the source", &mut Recorder::default())
        .unwrap();
    // Repeated rejections escalate in the reminder and the turn continues.
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 8);
    assert!(
        requests.last().unwrap()["messages"]
            .to_string()
            .contains("Escalation: 4 todo updates have been rejected"),
    );
    assert!(!agent.todo().has_open());
}

#[test]
fn final_report_reconciles_optional_progress_without_extra_requests() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"Architecture report"},{"content":"Test report"}]}),
        ),
        Reply::Text(
            "Architecture: UI and inference boundaries stay separate. Tests: exercise failure recovery and completion review.",
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Write both reports", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    // A14-01: a completed turn never marks work that was not started as done.
    assert!(agent.todo().has_open());
    assert_eq!(server.requests.lock().len(), 2);
}

#[test]
fn prose_does_not_replace_investigation_tool_evidence() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"Inspect source","investigation":true}]}),
        ),
        Reply::Text(
            "The architecture looks correct based on what I recall; I have not read the source.",
        ),
        Reply::Call(
            "todo",
            json!({"op":"done","finding":"Looks correct","evidence":["src/lib.rs:1"]}),
        ),
        Reply::Call(
            "todo",
            json!({"op":"block","index":1,"reason":"Current source has not been inspected"}),
        ),
        Reply::Text("Blocked: source not inspected."),
    ]);
    let e = env(&server.url, "auto");
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Inspect source", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    assert!(agent.todo().has_open());
    assert!(
        !server.requests.lock()[3]["messages"]
            .to_string()
            .contains("Step not closed")
    );
}

#[test]
fn optional_findings_redirect_duplicate_reads_and_survive_resume() {
    // Exercises the task evidence packet, which is opt-in.
    let conclusion =
        json!({"op":"done", "finding":"The current value is five", "evidence":["marker.txt:1"]});
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[
                {"content":"What is the current value?","investigation":true},
                {"content":"Change to the requested value"}
            ]}),
        ),
        Reply::Call("todo", json!({"op":"view"})),
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Call("todo", conclusion.clone()),
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Call("todo", json!({"op":"view"})),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Found the value","reason":"What value should replace five?"}),
        ),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.efficiency.task_packet = true;
    std::fs::write(e.project.path().join("marker.txt"), "five\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    assert_eq!(
        a.turn("Investigate and update the value", &mut ui)
            .unwrap()
            .outcome,
        TurnOutcome::Blocked
    );
    let requests = server.requests.lock();
    assert!(
        !requests[2]["messages"]
            .to_string()
            .contains("no tool has run")
    );
    assert!(
        !requests[4]["messages"]
            .to_string()
            .contains("Investigation done needs")
    );
    let duplicate = requests[5]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(duplicate.starts_with("[Unchanged read]"), "{duplicate}");
    assert!(duplicate.contains("still shown above"));
    assert!(
        !requests[6]["messages"]
            .to_string()
            .contains("Investigation done needs")
    );
    assert!(
        !ui.notices
            .iter()
            .any(|n| n.contains("unchanged observation")),
        "read pointers must not emit a verbose recovery notice"
    );
    let path = a.session().path().unwrap().to_owned();
    drop(a);
    let resumed = Session::open(&path).unwrap();
    let a = Agent::interactive(
        &e.config,
        make_backend(&e.config).unwrap(),
        e.layout.clone(),
        resumed,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(
        a.todo().tasks()[0].finding.as_deref(),
        Some("The current value is five")
    );
    assert_eq!(a.todo().tasks()[0].evidence, ["marker.txt:1"]);
    assert!(a.todo().has_open());
}

#[test]
fn interactive_recovery_asks_for_direction_and_continues_with_corrected_evidence() {
    #[derive(Default)]
    struct RecoveryUi {
        asked: usize,
    }
    impl Ui for RecoveryUi {
        fn ask_user(
            &mut self,
            title: &str,
            _question: &str,
            _options: &[String],
        ) -> Option<String> {
            assert_eq!(title, "Recovery needs direction");
            self.asked += 1;
            Some("Read marker.txt instead of guessing the missing path".into())
        }
    }
    let mut script = (0..5)
        .map(|_| Reply::Call("read", json!({"path":"missing.txt"})))
        .collect::<Vec<_>>();
    script.push(Reply::Call("read", json!({"path":"marker.txt"})));
    script.push(Reply::Text("Recovered with current evidence."));
    let server = serve(script);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    std::fs::write(e.project.path().join("marker.txt"), "real evidence").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = RecoveryUi::default();
    let report = a.turn("Inspect the project", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    // Repeated failures escalate in place; the user is no longer interrupted to redirect.
    assert_eq!(ui.asked, 0);
    assert!(
        server.requests.lock().last().unwrap()["messages"]
            .to_string()
            .contains("real evidence")
    );
}

#[test]
fn repeated_reads_across_failed_checks_stop_and_keep_compact_evidence() {
    let mut script = vec![
        open_todo(),
        Reply::IdentifiedCall("read", json!({"path":"marker.txt"}), "original".into()),
    ];
    for attempt in 0..5 {
        script.push(Reply::IdentifiedCall(
            "bash",
            json!({"command":format!("false # check {attempt}")}),
            format!("check-{attempt}"),
        ));
        script.push(Reply::IdentifiedCall(
            "read",
            json!({"path":format!("other-{attempt}.txt")}),
            format!("other-{attempt}"),
        ));
        script.push(Reply::IdentifiedCall(
            "read",
            json!({"path":"marker.txt"}),
            format!("repeat-{attempt}"),
        ));
    }
    script.push(Reply::Text("Must never reach this request"));
    let server = serve(script);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    let original = format!("ORIGINAL_EVIDENCE\n{}", "evidence ".repeat(400));
    std::fs::write(e.project.path().join("marker.txt"), &original).unwrap();
    for attempt in 0..5 {
        std::fs::write(
            e.project.path().join(format!("other-{attempt}.txt")),
            "other evidence",
        )
        .unwrap();
    }
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a
        .turn("Inspect the current code and fix the build", &mut ui)
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    let requests = server.requests.lock();
    assert!(
        requests[5]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["function"]["name"] == "read")
    );
    assert!(
        requests[5]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["function"]["name"] == "read")
    );
    let messages = requests.last().unwrap()["messages"].as_array().unwrap();
    let duplicates: Vec<_> = messages
        .iter()
        .filter(|m| {
            m["content"]
                .as_str()
                .is_some_and(|s| s.starts_with("[Unchanged read]"))
        })
        .collect();
    assert_eq!(duplicates.len(), 1);
    for message in duplicates {
        assert!(
            !message["content"]
                .as_str()
                .unwrap()
                .contains("ORIGINAL_EVIDENCE")
        );
        assert!(
            message["content"]
                .as_str()
                .unwrap()
                .contains("tool call original")
        );
        assert!(
            !message["content"].as_str().unwrap().contains("artifact://"),
            "the visible original is sufficient; do not add a redundant artifact link"
        );
    }
    let original_message = a
        .session()
        .messages()
        .into_iter()
        .find(|m| m["tool_call_id"] == "original")
        .unwrap();
    assert!(
        original_message["content"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_EVIDENCE")
    );
    assert!(
        original_message["dial_evidence"]["artifact"]
            .as_str()
            .unwrap()
            .starts_with("artifact://")
    );
    assert!(
        requests[5]["messages"]
            .to_string()
            .contains("ORIGINAL_EVIDENCE"),
        "duplicate must not supersede the original observation"
    );
}

#[test]
fn repeated_successful_shell_inspections_withhold_bash_and_stop_without_step_limit() {
    if skip_without_sandbox() {
        return;
    }
    let command = "cd . && grep '^version' pyproject.toml && grep '__version__' version.py";
    let mut script = vec![open_todo()];
    script.extend((0..8).map(|n| {
        Reply::IdentifiedCall(
            "bash",
            json!({"command":command, "timeout":30+n}),
            format!("version-{n}"),
        )
    }));
    script.push(Reply::Text("Must never reach this request"));
    let server = serve(script);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    e.config.harness.max_steps = 0;
    std::fs::write(
        e.project.path().join("pyproject.toml"),
        "version = \"0.3.31\"\n",
    )
    .unwrap();
    std::fs::write(
        e.project.path().join("version.py"),
        "__version__ = \"0.3.31\"\n",
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("Verify the package version", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    assert_eq!(
        ui.results
            .iter()
            .filter(|(name, _, failed)| name == "bash" && !failed)
            .count(),
        4,
        "report: {}; results: {:?}",
        report.text,
        ui.results
    );
    let requests = server.requests.lock();
    assert!(requests.len() >= 9);
    assert!(
        tool_names(&requests[3]).iter().any(|name| name == "bash"),
        "different commands remain available"
    );
    let messages = a.session().messages();
    let original = messages
        .iter()
        .find(|m| m["tool_call_id"] == "version-0")
        .unwrap();
    assert!(original["content"].as_str().unwrap().contains("0.3.31"));
    let duplicate = messages
        .iter()
        .find(|m| m["tool_call_id"] == "version-1")
        .unwrap();
    assert!(
        duplicate["content"]
            .as_str()
            .unwrap()
            .contains("unchanged from tool call version-0")
    );
    assert!(!duplicate["content"].as_str().unwrap().contains("0.3.31"));
    let ignored = messages
        .iter()
        .find(|m| m["tool_call_id"] == "version-2")
        .unwrap();
    assert!(
        ignored["content"]
            .as_str()
            .unwrap()
            .contains("This call was not executed")
    );
}

#[test]
fn loop_checkpoint_withholds_reads_rejects_ignored_feedback_and_restores_after_progress() {
    let read = |id: &str| Reply::IdentifiedCall("read", json!({"path":"marker.txt"}), id.into());
    let server = serve(vec![
        open_todo(),
        read("original"),
        read("duplicate"),
        read("ignored"),
        Reply::IdentifiedCall(
            "grep",
            json!({"path":"marker.txt","pattern":"original"}),
            "different-check".into(),
        ),
        Reply::Call("write", json!({"path":"marker.txt","content":"repaired\n"})),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Inspected the value","reason":"Which value should replace original?"}),
        ),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("marker.txt"), "original\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("Investigate marker.txt", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    let requests = server.requests.lock();
    let offered = |index: usize, name: &str| {
        requests[index]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["function"]["name"] == name)
    };
    assert!(
        offered(3, "read") && offered(4, "read"),
        "only repeated arguments are withheld"
    );
    assert!(offered(4, "grep") && offered(4, "finish"));
    assert!(
        offered(5, "read"),
        "a narrower read must remain available even before a mutation"
    );
    assert!(offered(6, "read"), "an actual repair restores read");
    assert!(
        requests[3]["messages"]
            .to_string()
            .contains("Recovery checkpoint from Dial")
    );
    let rejected = a
        .session()
        .messages()
        .into_iter()
        .find(|m| m["tool_call_id"] == "ignored")
        .unwrap();
    assert!(
        rejected["content"]
            .as_str()
            .unwrap()
            .contains("This call was not executed")
    );
}

#[test]
fn user_steering_can_override_a_recovery_checkpoint() {
    #[derive(Default)]
    struct SteeringUi {
        queued: bool,
    }
    impl Ui for SteeringUi {
        fn notice(&mut self, text: &str) {
            if text.contains("unchanged observation") {
                self.queued = true;
            }
        }
        // The recovery checkpoint is bookkeeping the TUI hides; the hook still observes it.
        fn diagnostic(&mut self, text: &str) {
            self.notice(text);
        }
        fn take_steering(&mut self) -> Vec<String> {
            if std::mem::take(&mut self.queued) {
                vec!["Read another.txt instead".into()]
            } else {
                vec![]
            }
        }
    }
    let server = serve(vec![
        open_todo(),
        // A duplicate read is a quiet pointer, not a recovery diagnostic. Use a repeated
        // search to establish the actual recovery checkpoint this test steers out of.
        Reply::IdentifiedCall(
            "grep",
            json!({"path":"marker.txt", "pattern":"original"}),
            "original".into(),
        ),
        Reply::IdentifiedCall(
            "grep",
            json!({"path":"marker.txt", "pattern":"original"}),
            "duplicate".into(),
        ),
        Reply::IdentifiedCall("read", json!({"path":"another.txt"}), "steered".into()),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Read the requested file","reason":"Which value should change?"}),
        ),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("marker.txt"), "original\n").unwrap();
    std::fs::write(e.project.path().join("another.txt"), "STEERED_SOURCE\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    assert_eq!(
        a.turn("Inspect marker.txt", &mut SteeringUi::default())
            .unwrap()
            .outcome,
        TurnOutcome::Blocked
    );
    let requests = server.requests.lock();
    assert!(
        requests[3]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["function"]["name"] == "read")
    );
    assert!(
        requests[3]["messages"]
            .to_string()
            .contains("Read another.txt instead")
    );
    assert!(
        requests[4]["messages"]
            .to_string()
            .contains("STEERED_SOURCE")
    );
}

#[test]
fn recovery_consumes_prefetched_reads_without_relabeling_or_shifting_results() {
    let server = serve(vec![
        open_todo(),
        Reply::Calls(vec![
            ("read", json!({"path":"marker.txt"})),
            ("read", json!({"path":"marker.txt"})),
            ("read", json!({"path":"marker.txt"})),
            ("read", json!({"path":"another.txt"})),
        ]),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Inspected both files","reason":"Which value should change?"}),
        ),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("marker.txt"), "ORIGINAL_SOURCE\n").unwrap();
    std::fs::write(e.project.path().join("another.txt"), "ANOTHER_SOURCE\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    assert_eq!(
        a.turn("Inspect both files", &mut Recorder::default())
            .unwrap()
            .outcome,
        TurnOutcome::Blocked
    );
    let messages = a.session().messages();
    let third = messages
        .iter()
        .find(|m| m["tool_call_id"] == "call_2")
        .unwrap();
    let third_content = third["content"].as_str().unwrap();
    assert!(
        third_content.starts_with("[Unchanged read]"),
        "{third_content}"
    );
    assert!(third_content.contains("tool call call_0"));
    assert!(!third_content.contains("ORIGINAL_SOURCE"));
    assert!(!third_content.contains("This call was not executed"));
    let fourth = messages
        .iter()
        .find(|m| m["tool_call_id"] == "call_3")
        .unwrap();
    assert!(
        fourth["content"]
            .as_str()
            .unwrap()
            .contains("ANOTHER_SOURCE")
    );
    assert!(
        !fourth["content"]
            .as_str()
            .unwrap()
            .contains("This call was not executed")
    );
}

#[test]
fn repeated_edit_failures_allow_corrected_arguments_without_a_dummy_mutation() {
    let edit = |id: &str, old: &str| {
        Reply::IdentifiedCall(
            "edit",
            json!({"path":"marker.txt","old_string":old,"new_string":"fixed\n"}),
            id.into(),
        )
    };
    let server = serve(vec![
        open_todo(),
        Reply::IdentifiedCall("read", json!({"path":"marker.txt"}), "original".into()),
        edit("bad-1", "wrong"),
        edit("bad-2", "wrong"),
        edit("bad-3", "wrong"),
        edit("withheld", "original\n"),
        Reply::IdentifiedCall("read", json!({"path":"marker.txt"}), "fresh-check".into()),
        Reply::Call("write", json!({"path":"marker.txt","content":"repaired\n"})),
        edit("corrected", "repaired\n"),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Updated the value","reason":"Which build command should verify this change?"}),
        ),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.edit_mode_override = Some(dial::config::types::EditMode::Replace);
    std::fs::write(e.project.path().join("marker.txt"), "original\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    assert_eq!(
        a.turn("Update marker.txt", &mut Recorder::default())
            .unwrap()
            .outcome,
        TurnOutcome::Blocked
    );
    let corrected = a
        .session()
        .messages()
        .into_iter()
        .find(|m| m["tool_call_id"] == "withheld")
        .unwrap();
    assert!(
        !corrected["content"]
            .as_str()
            .unwrap()
            .contains("This call was not executed")
    );
    assert!(
        a.session().messages().iter().any(|message| {
            message["tool_call_id"] == "fresh-check"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("fixed"))
        }),
        "the corrected edit must change the file before any later write"
    );
    let requests = server.requests.lock();
    let offered = |index: usize, name: &str| {
        requests[index]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["function"]["name"] == name)
    };
    assert!(offered(5, "edit") && offered(6, "edit"));
    assert!(
        offered(7, "edit"),
        "corrected edit arguments remain available without an unrelated repair"
    );
    assert!(offered(8, "edit"), "a confirmed repair restores edit");
    assert!(
        offered(9, "read"),
        "successful mutation restores verification reads"
    );
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "fixed\n"
    );
}

#[test]
fn context_includes_tool_evidence_before_the_next_request_without_changing_usage() {
    struct ContextUi {
        requests: Arc<Mutex<Vec<Value>>>,
        pending_read: bool,
        after_read: Option<(u64, bool, usize)>,
        usage: Vec<(Option<u64>, Option<u64>)>,
    }
    impl Ui for ContextUi {
        fn tool_result(&mut self, name: &str, _: &ToolOutput) {
            self.pending_read = name == "read";
        }
        fn context_usage(&mut self, tokens: u64, estimated: bool) {
            if self.pending_read {
                self.after_read = Some((tokens, estimated, self.requests.lock().len()));
                self.pending_read = false;
            }
        }
        fn usage(&mut self, prompt: Option<u64>, output: Option<u64>, _: f64) {
            self.usage.push((prompt, output));
        }
    }
    let server = serve(vec![
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Text("Inspected."),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(
        e.project.path().join("marker.txt"),
        "evidence\n".repeat(1000),
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = ContextUi {
        requests: server.requests.clone(),
        pending_read: false,
        after_read: None,
        usage: Vec::new(),
    };
    a.turn("Read marker.txt", &mut ui).unwrap();
    let (tokens, estimated, requests) = ui.after_read.expect("tool results must update the meter");
    assert!(tokens > 2000 && estimated);
    assert_eq!(requests, 1, "update before starting the next request");
    assert_eq!(ui.usage.len(), 2);
    assert!(
        ui.usage[1].0 > ui.usage[0].0,
        "the evidence read grows the reported prompt: {:?}",
        ui.usage
    );
    assert!(ui.usage.iter().all(|(_, output)| *output == Some(5)));
}

#[test]
fn unfinished_todos_continue_into_real_edits_and_completion() {
    let server = serve(vec![
        open_todo(),
        Reply::Text("Marker creation is still pending."),
        Reply::Call("write", json!({"path":"marker.txt","content":"done"})),
        close_todo(),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Created marker."}),
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("create marker", &mut Recorder::default()).unwrap();
    assert_eq!(report.text, "Created marker.");
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "done"
    );
    assert!(
        server.requests.lock()[2]["messages"]
            .to_string()
            .contains("You said you would continue with a tool call or action but stopped")
    );
    assert_eq!(
        server.requests.lock().len(),
        5,
        "finish completes without a harness review round"
    );
    let status = a.session().control_state();
    assert_eq!(
        status["reliability"]["edit_loop"]["diff_surfaced_mutations"],
        1
    );
    assert_eq!(
        status["reliability"]["verification"],
        "no automatic focused check available; behavior unverified"
    );
    assert_eq!(a.metrics().turns_completed, 1);
}

#[test]
fn code_history_finds_a_diff_an_earlier_turn_made() {
    let server = serve(vec![
        Reply::Call(
            "write",
            json!({"path":"marker.txt","content":"retry_backoff = 3\n"}),
        ),
        Reply::Text("Created marker."),
        Reply::Call("tool_load", json!({"names":["code_history"]})),
        Reply::Call(
            "code_history",
            json!({"action":"search","query":"retry_backoff"}),
        ),
        Reply::Text("Found it."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("add the backoff marker", &mut Recorder::default())
        .unwrap();
    let mut ui = Recorder::default();
    a.turn("when did the backoff marker appear?", &mut ui)
        .unwrap();
    let (_, text, error) = ui
        .results
        .iter()
        .find(|(name, _, _)| name == "code_history")
        .expect("code_history ran");
    assert!(!error, "{text}");
    assert!(
        text.contains("marker.txt (created +1 -0)") && text.contains("add the backoff marker"),
        "{text}"
    );
}

#[test]
fn edits_complete_without_a_harness_review_round() {
    let server = serve(vec![
        Reply::Call("write", json!({"path":"marker.txt","content":"first"})),
        Reply::Call("write", json!({"path":"marker.txt","content":"second"})),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Updated marker."}),
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("create and revise marker", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "second"
    );
    let requests = server.requests.lock();
    assert_eq!(
        requests.len(),
        3,
        "finish is accepted without a review round"
    );
    assert!(!requests[2]["messages"].to_string().contains("+second"));
    drop(requests);
    let status = a.session().control_state();
    assert_eq!(status["reliability"]["edit_loop"]["mutations"], 2);
    assert_eq!(
        status["reliability"]["edit_loop"]["diff_surfaced_mutations"],
        2
    );
}

#[test]
fn simple_edit_completes_on_the_first_final_response() {
    let server = serve(vec![
        Reply::Call("write", json!({"path":"marker.txt","content":"created"})),
        Reply::Text("Reviewed the marker; no automatic behavioral check was available."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn(
            "Create marker.txt and review the change",
            &mut Recorder::default(),
        )
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "created"
    );
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 2, "no review round after the final answer");
    drop(requests);
    assert_eq!(
        a.session()
            .messages()
            .iter()
            .filter(|message| message["role"] == "assistant" && message["content"] == report.text)
            .count(),
        1
    );
}

#[test]
fn completed_plan_delivers_review_without_a_redundant_finish_round() {
    let server = serve(vec![
        open_todo(),
        Reply::Call("write", json!({"path":"marker.txt","content":"done"})),
        close_todo(),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Created and reviewed marker."}),
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn(
            "Debug the failing marker.txt with a plan",
            &mut Recorder::default(),
        )
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 4);
    assert!(!a.todo().has_open());
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "done"
    );
}

#[test]
fn text_only_completion_surfaces_uncheckpointed_shell_mutations() {
    if skip_without_sandbox() {
        return;
    }
    let server = serve(vec![
        Reply::Call("bash", json!({"command":"printf shell > untracked.txt"})),
        Reply::Text("done"),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("create a file through shell", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.text, "done");
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("untracked.txt")).unwrap(),
        "shell"
    );
    assert_eq!(server.requests.lock().len(), 2, "no git review round");
    let status = a.session().control_state();
    assert_eq!(
        status["reliability"]["edit_loop"]["uncheckpointed_shell_commands"],
        1
    );
    assert_eq!(
        status["reliability"]["edit_loop"]["phase"],
        "uncheckpointed_operations_disclosed_not_verified"
    );
}

#[test]
fn safe_git_review_batch_completes_without_another_effects_review_round() {
    if skip_without_sandbox() {
        return;
    }
    let git = "git -c core.fsmonitor=false -c core.pager=cat --no-pager";
    let command = format!(
        "{git} status --short --untracked-files=all; {git} diff --no-color --no-ext-diff --no-textconv; {git} diff --no-color --no-ext-diff --no-textconv --staged"
    );
    let server = serve(vec![
        Reply::Call("bash", json!({"command":command})),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Reviewed the working tree."}),
        ),
    ]);
    let e = env(&server.url, "auto");
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(e.project.path())
            .status()
            .unwrap()
            .success()
    );
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("Inspect the working tree", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        server.requests.lock().len(),
        2,
        "the review must not cause a new finish round"
    );
    assert_eq!(
        a.session().control_state()["reliability"]["edit_loop"]["uncheckpointed_shell_commands"],
        0
    );
}

#[test]
fn explicit_completion_records_its_actual_request_for_training() {
    let server = serve(vec![Reply::Call(
        "finish",
        json!({"outcome":"completed","summary":"Evidence checked."}),
    )]);
    let e = env(&server.url, "auto");
    let store = Store::open(&e.layout.state_dir, "scripted", "t", &e.config.harness).unwrap();
    let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("inspect evidence", &mut Recorder::default())
        .unwrap();
    let payload = store
        .trajectory_messages(report.trajectory.unwrap())
        .unwrap()
        .unwrap();
    let messages = payload["examples"][0]["messages"].as_array().unwrap();
    assert_eq!(
        json!(&messages[..messages.len() - 1]),
        server.requests.lock()[0]["messages"]
    );
    assert_eq!(
        messages.last().unwrap()["tool_calls"][0]["function"]["name"],
        "finish"
    );
}

#[test]
fn steering_after_finish_preserves_newly_loaded_tools() {
    struct Steer {
        ready: bool,
        sent: bool,
    }
    impl Ui for Steer {
        fn tool_result(&mut self, name: &str, output: &ToolOutput) {
            if name == "finish" && !output.is_error {
                self.ready = true;
            }
        }
        fn take_steering(&mut self) -> Vec<String> {
            if self.ready && !self.sent {
                self.sent = true;
                vec!["Also read the evidence skill.".into()]
            } else {
                vec![]
            }
        }
    }
    let server = serve(vec![
        Reply::Calls(vec![
            ("tool_load", json!({"names":["skill"]})),
            (
                "finish",
                json!({"outcome":"completed","summary":"First task done."}),
            ),
        ]),
        Reply::Call("skill", json!({"name":"evidence"})),
        Reply::Text("Skills checked."),
    ]);
    let e = env(&server.url, "auto");
    let skill_dir = e.layout.skills_dirs()[0].join("evidence");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: evidence\ndescription: test evidence\n---\n\nRead the actual evidence.",
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn(
            "finish first task",
            &mut Steer {
                ready: false,
                sent: false,
            },
        )
        .unwrap();
    assert_eq!(report.text, "Skills checked.");
    assert_eq!(a.metrics().turns_completed, 1);
    let requests = server.requests.lock();
    assert!(
        requests[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "skill")
    );
    assert!(requests[2]["messages"].as_array().unwrap().iter().any(|m| {
        m["role"] == "tool"
            && m["content"]
                .as_str()
                .unwrap_or("")
                .contains("Read the actual evidence.")
    }));
}

#[test]
fn skill_toggles_refresh_the_catalog_between_turns() {
    use dial::storage::skills::{Scope, management};
    let server = serve(vec![
        Reply::Text("First."),
        Reply::Text("Second."),
        Reply::Text("Third."),
    ]);
    let e = env(&server.url, "auto");
    let dir = e.layout.skills_dirs()[0].join("review");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: review\ndescription: UNIQUE_SKILL_CATALOG_MARKER\n---\nReview.",
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("hello", &mut dial::agent::NullUi).unwrap();
    management::set_enabled(&e.layout, "review", false, Scope::Project).unwrap();
    a.turn("hello again", &mut dial::agent::NullUi).unwrap();
    management::set_enabled(&e.layout, "review", true, Scope::Project).unwrap();
    a.turn("hello once more", &mut dial::agent::NullUi).unwrap();
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 3);
    for (request, enabled) in requests.iter().zip([true, false, true]) {
        assert_eq!(
            system_text(request).contains("UNIQUE_SKILL_CATALOG_MARKER"),
            enabled
        );
    }
}

#[test]
fn text_only_acknowledgements_do_not_repeat_completion_reminders() {
    let server = serve(vec![
        open_todo(),
        Reply::Text("Marker work is pending."),
        Reply::Text("Still pending."),
        Reply::Text("Still pending."),
        Reply::Text("Still pending."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("create marker", &mut Recorder::default()).unwrap();
    assert!(
        report.text.contains("Stopped with incomplete work"),
        "{}",
        report.text
    );
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert_eq!(server.requests.lock().len(), 5);
    assert_eq!(a.metrics().turns_completed, 0);
    assert_eq!(
        a.todo().tasks()[0].status,
        dial::agent::todo::Status::InProgress
    );
}

#[test]
fn explicit_blockers_preserve_open_todos() {
    let server = serve(vec![
        open_todo(),
        Reply::Call(
            "finish",
            json!({
                "outcome":"blocked",
                "summary":"Need the destination.",
                "reason":"Which directory should contain marker.txt?"
            }),
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("create marker", &mut Recorder::default()).unwrap();
    assert!(report.text.contains("Which directory"));
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    assert_eq!(server.requests.lock().len(), 2);
    assert_eq!(a.metrics().turns_completed, 0);
    assert_eq!(
        a.todo().tasks()[0].status,
        dial::agent::todo::Status::InProgress
    );
}

#[test]
fn finish_needs_input_waits_and_continues_with_the_user_answer() {
    #[derive(Default)]
    struct AnswerUi {
        title: String,
        question: String,
        options: Vec<String>,
    }
    impl Ui for AnswerUi {
        fn ask_user(&mut self, title: &str, question: &str, options: &[String]) -> Option<String> {
            self.title = title.to_owned();
            self.question = question.to_owned();
            self.options = options.to_vec();
            Some("a custom destination".to_owned())
        }
    }

    let server = serve(vec![
        Reply::Call(
            "finish",
            json!({
                "outcome":"needs_input",
                "summary":"Need the destination.",
                "reason":"Where should marker.txt go?",
                "options":["Project root", "Temporary directory"]
            }),
        ),
        Reply::Text("Custom destination accepted."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = AnswerUi::default();
    let report = a.turn("create marker", &mut ui).unwrap();

    assert_eq!(report.outcome, TurnOutcome::Completed);
    let tool_results = server.requests.lock()[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        ui.title, "Need the destination.",
        "tool results: {tool_results:?}"
    );
    assert_eq!(ui.question, "Where should marker.txt go?");
    assert_eq!(ui.options, vec!["Project root", "Temporary directory"]);
    assert_eq!(server.requests.lock().len(), 2);
    assert!(
        server.requests.lock()[1]["messages"]
            .to_string()
            .contains("User response: a custom destination")
    );
}

#[test]
fn plain_clarification_preserves_todos_without_continuation_or_false_completion() {
    let server = serve(vec![
        open_todo(),
        Reply::Text("Could you clarify which directory should contain marker.txt?"),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("create marker", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::NeedsInput);
    assert!(report.text.contains("Could you clarify"));
    assert_eq!(server.requests.lock().len(), 2);
    assert_eq!(a.metrics().turns_completed, 0);
    assert_eq!(
        a.todo().tasks()[0].status,
        dial::agent::todo::Status::InProgress
    );
    assert!(!ui.notices.iter().any(|n| n.contains("Continuing: todo")));
}

#[test]
fn explicit_finish_closes_optional_progress_in_the_same_batch() {
    let server = serve(vec![
        open_todo(),
        Reply::Call("write", json!({"path":"marker.txt","content":"done"})),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Marker created."}),
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("Create marker", &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "done"
    );
    assert!(!a.todo().has_open());
    assert_eq!(server.requests.lock().len(), 3);
}

#[test]
fn stopping_with_only_blocked_todos_ends_the_turn_blocked_without_reminders() {
    let server = serve(vec![
        open_todo(),
        Reply::Call(
            "todo",
            json!({"op":"block","reason":"no write access to the target"}),
        ),
        Reply::Text("Blocked on write access."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("create marker", &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    assert_eq!(report.text, "Blocked on write access.");
    assert_eq!(
        server.requests.lock().len(),
        3,
        "blocked work earns no continuation reminder"
    );
    assert_eq!(a.metrics().turns_completed, 0);
}

#[test]
fn unfinished_todos_survive_session_resume_and_finish_with_verification() {
    let server = serve(vec![
        open_todo(),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Need a destination","reason":"Which directory?"}),
        ),
        Reply::Call("write", json!({"path":"marker.txt","content":"done"})),
        close_todo(),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Created marker."}),
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    assert_eq!(
        a.turn("create marker", &mut Recorder::default())
            .unwrap()
            .outcome,
        TurnOutcome::Blocked
    );
    let path = a.session().path().unwrap().to_owned();
    drop(a);
    let resumed = Session::open(&path).unwrap();
    let mut a = Agent::interactive(
        &e.config,
        make_backend(&e.config).unwrap(),
        e.layout.clone(),
        resumed,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert_eq!(a.todo().tasks().len(), 1);
    let report = a
        .turn("Use this project directory", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        a.todo().tasks()[0].status,
        dial::agent::todo::Status::Completed
    );
    assert_eq!(a.session().control_state()["turn_outcome"], "completed");
}

#[test]
fn failed_call_in_a_batch_prevents_finish_completed() {
    let server = serve(vec![
        Reply::Calls(vec![
            ("read", json!({"path":"missing.txt"})),
            ("finish", json!({"outcome":"completed","summary":"Done"})),
        ]),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Source is missing","reason":"Provide missing.txt."}),
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    assert_eq!(
        a.turn("inspect missing.txt", &mut Recorder::default())
            .unwrap()
            .outcome,
        TurnOutcome::Blocked
    );
    assert!(
        server.requests.lock()[1]["messages"]
            .to_string()
            .contains("cannot follow a failed call")
    );
    assert_eq!(a.metrics().turns_completed, 0);
}

#[test]
fn finish_requires_a_reason_and_must_be_last_in_its_batch() {
    let server = serve(vec![
        Reply::Call(
            "finish",
            json!({"outcome":"needs_input","summary":"Need details"}),
        ),
        Reply::Calls(vec![
            ("finish", json!({"outcome":"completed","summary":"Done"})),
            ("todo", json!({"op":"view"})),
        ]),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Need a destination","reason":"Which directory?"}),
        ),
    ]);
    let e = env(&server.url, "ask");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    assert_eq!(
        a.turn("create marker", &mut ui).unwrap().outcome,
        TurnOutcome::Blocked
    );
    let requests = server.requests.lock();
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("requires reason")
    );
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("must be the last call")
    );
    assert!(
        ui.approvals.is_empty(),
        "finish and todo only change session state"
    );
}

#[test]
fn finish_completed_waits_for_live_task_results() {
    let main = serve(vec![
        Reply::Call("finish", json!({"outcome":"completed","summary":"Done"})),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Waiting on investigation","reason":"The live investigation is still running."}),
        ),
    ]);
    let side = serve_fn(
        Arc::new(|_| {
            thread::sleep(Duration::from_millis(400));
            Reply::Text("investigation result")
        }),
        None,
        None,
    );
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.start_subagents(
        &json!({"task":"inspect the project","solution_space":"s"}),
        &mut ui,
    )
    .unwrap();
    assert!(
        !a.release_idle_models(),
        "a live child must retain the foreground compute reservation"
    );
    let report = a.turn("finish investigation", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    assert!(
        main.requests.lock()[1]["messages"]
            .to_string()
            .contains("live tasks are running")
    );
    assert_eq!(a.metrics().turns_completed, 0);
}

#[test]
fn cancellation_during_a_completion_reminder_stops_continuation() {
    struct Cancelling {
        interrupt: Arc<AtomicBool>,
    }
    impl Ui for Cancelling {
        fn notice(&mut self, text: &str) {
            if text.contains("unfinished work") {
                self.interrupt.store(true, Ordering::SeqCst);
            }
        }
    }
    let server = serve(vec![open_todo(), Reply::Text("Marker work is pending.")]);
    let e = env(&server.url, "auto");
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut a = agent_with(&e, None, interrupt.clone());
    let report = a
        .turn("create marker", &mut Cancelling { interrupt })
        .unwrap();
    assert!(report.interrupted);
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(server.requests.lock().len(), 2);
}

#[test]
fn oversized_tool_recovery_does_not_abort_the_session() {
    let server = serve(vec![
        Reply::Text("{\"name\": \"read\", \"arguments\": {\"path\": \"a.txt\"}"),
        Reply::Call("read", json!({"path":"a.txt"})),
        Reply::Text("It says hello."),
        Reply::Text("Still available."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.context_window = 200000;
    std::fs::write(e.project.path().join("a.txt"), "hello\n").unwrap();
    std::fs::create_dir_all(&e.layout.state_dir).unwrap();
    let store = Store::open(&e.layout.state_dir, "scripted", "t", &e.config.harness).unwrap();
    let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let prompt = format!(
        "What does a.txt say? Archive data follows: {}",
        "x".repeat(250 * 1024)
    );
    let report = a.turn(&prompt, &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.text, "It says hello.");
    assert_eq!(
        ui.calls,
        ["read"],
        "a malformed call must not execute, or replay the repaired call"
    );
    assert!(store.tool_recoveries().unwrap().is_empty());
    assert!(
        server.requests.lock()[1]["messages"]
            .to_string()
            .contains(&prompt)
    );
    let next = a.turn("Are you still available?", &mut ui).unwrap();
    assert_eq!(next.outcome, TurnOutcome::Completed);
    assert_eq!(next.text, "Still available.");
}

#[test]
fn repaired_tool_call_becomes_a_verified_training_recovery() {
    let server = serve(vec![
        // Malformed: no tool-call channel, JSON-looking text.
        Reply::Text("{\"name\": \"read\", \"arguments\": {\"path\": \"a.txt\"}"),
        Reply::Call("read", json!({"path": "a.txt"})),
        Reply::Text("It says hello."),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("a.txt"), "hello\n").unwrap();
    std::fs::create_dir_all(&e.layout.state_dir).unwrap();
    let store = Store::open(&e.layout.state_dir, "scripted", "t", &e.config.harness).unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, Some(&store));
    let mut ui = Recorder::default();
    let report = a.turn("what does a.txt say?", &mut ui).unwrap();
    assert_eq!(report.text, "It says hello.");
    assert_eq!(ui.calls, ["read"], "the plain-text call must not execute");
    let recoveries = store.tool_recoveries().unwrap();
    assert_eq!(recoveries.len(), 1, "one verified repair");
    let payload = &recoveries[0].1;
    assert_eq!(payload["failure_kinds"], json!(["text_call"]));
    assert_eq!(payload["rejected"][0]["role"], "assistant");
    assert_eq!(
        payload["rejected"][0]["content"],
        "{\"name\": \"read\", \"arguments\": {\"path\": \"a.txt\"}"
    );
    let root = std::fs::canonicalize(e.project.path()).unwrap();
    assert_eq!(payload["project_root"].as_str(), root.to_str());
    // Training context is the actual pre-failure request, including contract guidance,
    // but never the subsequent correction message.
    assert_eq!(payload["context"], server.requests.lock()[0]["messages"]);
    assert_eq!(
        payload["corrected"]["tool_calls"][0]["function"]["name"],
        "read"
    );
    // The model was told what went wrong before its second attempt.
    let second = &server.requests.lock()[1];
    assert!(
        second["messages"]
            .to_string()
            .contains("tool call written as plain text")
    );
}

#[test]
fn repeated_unusable_tool_calls_stop_even_without_a_step_limit() {
    for max_steps in [0, 2] {
        for mode in 0..3 {
            let replies = (0..10)
                .map(|n| {
                    if mode == 0 || (mode == 2 && n % 2 == 0) {
                        Reply::DroppedCall
                    } else {
                        Reply::Text("{\"name\": \"read\", \"arguments\": {\"path\": \"a.txt\"}")
                    }
                })
                .collect();
            let server = serve(replies);
            let mut e = env(&server.url, "auto");
            e.config.harness.max_steps = max_steps;
            let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
            let mut ui = Recorder::default();
            let report = a.turn("Read a.txt", &mut ui).unwrap();
            assert_eq!(report.outcome, TurnOutcome::Incomplete);
            assert!(
                report.text.contains("unusable tool calls"),
                "{}",
                report.text
            );
            assert_eq!(server.requests.lock().len(), 8);
            assert!(ui.calls.is_empty(), "invalid calls must never execute");
        }
    }
}

#[test]
fn valid_calls_reset_unusable_tool_recovery() {
    let mut replies = Vec::new();
    for n in 0..2 {
        for _ in 0..7 {
            replies.push(Reply::DroppedCall);
        }
        replies.push(Reply::Call("read", json!({"path":format!("f{n}.txt")})));
    }
    replies.push(Reply::Text("done"));
    let server = serve(replies);
    let mut e = env(&server.url, "auto");
    e.config.harness.max_steps = 0;
    for n in 0..2 {
        std::fs::write(e.project.path().join(format!("f{n}.txt")), "hello").unwrap();
    }
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("Read the numbered files", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(report.text, "done");
    assert_eq!(ui.calls, ["read", "read"]);
    assert_eq!(server.requests.lock().len(), 17);
}

#[test]
fn matching_verified_repairs_become_advisory_context_in_the_user_turn() {
    let server = serve(vec![Reply::Text("Current file inspection is pending.")]);
    let mut e = env(&server.url, "ask");
    e.config.assistance.repair_hints = true;
    std::fs::create_dir_all(&e.layout.state_dir).unwrap();
    let store = Store::open(&e.layout.state_dir, "scripted", "t", &e.config.harness).unwrap();
    let version = store.active().unwrap().id;
    let trajectory = store
        .begin_trajectory(version, "coding", "prior-repair", "prompt")
        .unwrap();
    let project_root = std::fs::canonicalize(e.project.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    store.tool_recovery(trajectory, version, &json!({
        "project_root": project_root,
        "context": [
            {"role":"system","content":"work safely"},
            {"role":"user","content":"Fix the parser for emoji filenames"}
        ],
        "tools": [{"type":"function","function":{"name":"edit","parameters":{"type":"object"}}}],
        "corrected": {"role":"assistant","content":null,"tool_calls":[{
            "id":"repair-1","type":"function",
            "function":{"name":"edit","arguments":"{\"path\":\"private/secret.txt\",\"input\":\"SECRET_VALUE\"}"}
        }]},
        "rejected": [{"role":"assistant","content":"invalid shape"}],
        "failure_kinds": ["schema"]
    })).unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, Some(&store));
    let report = a
        .turn(
            "Please fix the parser and emoji filenames",
            &mut Recorder::default(),
        )
        .unwrap();
    assert_eq!(report.text, "Current file inspection is pending.");
    let requests = server.requests.lock();
    let first_request = &requests[0]["messages"];
    assert!(
        !first_request[0]["content"]
            .as_str()
            .unwrap()
            .contains("verified repair patterns"),
        "the system prompt stays fixed"
    );
    let user = first_request
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(user.contains("Relevant verified repair patterns"));
    assert!(user.contains("edit") && user.contains("path") && user.contains("schema"));
    assert!(!user.contains("private/secret.txt") && !user.contains("SECRET_VALUE"));
}

#[test]
fn denied_and_readonly_mutations_never_touch_files() {
    for (mode, deny) in [("ask", true), ("readonly", false)] {
        let server = serve(vec![
            Reply::Call("write", json!({"path": "out.txt", "content": "x"})),
            Reply::Text("I could not write it."),
        ]);
        let e = env(&server.url, mode);
        let mut backend = make_backend(&e.config).unwrap();
        let mut a = agent(&e, &mut backend, None);
        let mut ui = Recorder {
            deny,
            ..Recorder::default()
        };
        let report = a.turn("write out.txt", &mut ui).unwrap();
        assert_eq!(report.text, "I could not write it.");
        assert!(
            !e.project.path().join("out.txt").exists(),
            "{mode}: file must not exist"
        );
        if mode == "ask" {
            assert_eq!(ui.approvals, ["write"]);
        } else {
            assert!(ui.approvals.is_empty());
        }
        let followup = &server.requests.lock()[1];
        assert!(
            followup["messages"].to_string().contains("denied"),
            "{mode}: model must be told"
        );
    }
}

#[test]
fn read_is_anchored_and_an_approved_write_is_undone_by_rollback() {
    let server = serve(vec![
        Reply::Call("read", json!({"path": "f.txt"})),
        Reply::Call("write", json!({"path": "f.txt", "content": "changed\n"})),
        Reply::Text("done"),
    ]);
    let e = env(&server.url, "ask");
    std::fs::write(e.project.path().join("f.txt"), "one\ntwo\n").unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("rewrite f.txt", &mut ui).unwrap();
    let read_result = server.requests.lock()[1]["messages"].to_string();
    assert!(
        read_result.contains("f.txt#") && read_result.contains("1:one"),
        "anchored read output: {read_result}"
    );
    assert_eq!(
        ui.approvals,
        ["write"],
        "read needs no approval; the write does"
    );
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("f.txt")).unwrap(),
        "changed\n"
    );
    let report = a.checkpoints().unwrap().rollback(1).unwrap();
    assert_eq!(report.restored.len(), 1);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("f.txt")).unwrap(),
        "one\ntwo\n"
    );
}

#[test]
fn compaction_keeps_recent_turns_and_replaces_older_ones_with_a_summary() {
    // Summarizer windows are answered by content: a large fixed prompt on a 32k window can split the
    // summary into several calls, and each must receive the same summary.
    let chat = Mutex::new(std::collections::VecDeque::from([
        Reply::Say(format!(
            "first answer {}",
            (0..150).map(|i| format!("detail{i} ")).collect::<String>()
        )),
        Reply::Text("second answer"),
        Reply::Text("third answer"),
    ]));
    let server = serve_fn(
        Arc::new(move |request: &Value| {
            let summarizing = request["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .and_then(|message| message["content"].as_str())
                .is_some_and(|text| text.starts_with("<conversation>"));
            if summarizing {
                return Reply::Text("SUMMARY: the user asked three things");
            }
            chat.lock()
                .pop_front()
                .unwrap_or(Reply::Text("(script exhausted)"))
        }),
        None,
        None,
    );
    let mut e = env(&server.url, "auto");
    // The first answer alone exceeds this budget, while the newest two turns fit.
    e.config.compaction.keep_recent_tokens = 100;
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    for q in ["one?", "two?", "three?"] {
        a.turn(q, &mut ui).unwrap();
    }
    a.compact(&mut ui).unwrap();
    let usage = a.usage().0;
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Agent].requests,
        3
    );
    assert!(usage.by_purpose[&dial::llm::usage::Purpose::Compaction].requests > 0);
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Compaction].requests,
        usage.workers.requests
    );
    let messages = a.session().messages();
    assert!(
        messages[0]["content"].as_str().unwrap().contains("SUMMARY"),
        "summary leads the history"
    );
    let rendered = messages
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        rendered.contains("two?") && rendered.contains("three?"),
        "recent turns stay verbatim: {rendered}"
    );
    let carried = messages
        .iter()
        .find(|m| m["dial_compaction_critical"] == true)
        .unwrap();
    assert_eq!(
        carried["dial_critical_state"]["runtime_user_request"]["original_goal"],
        "one?"
    );
    assert_eq!(
        carried["dial_critical_state"]["runtime_user_request"]["current_instruction"],
        "three?"
    );
    assert!(
        !rendered.contains("first answer"),
        "old assistant conversation is folded into the summary"
    );
    assert!(
        !messages
            .iter()
            .skip(1)
            .any(|m| m["role"] == "user" && m["content"] == "one?"),
        "the oldest standalone turn is removed from active context"
    );
}

#[test]
fn calibrated_compaction_reports_a_reduction_and_continues_below_the_trigger() {
    use dial::config::compaction::{CompactionMethod, CompactionOptions};

    let mut recovered_tokens = None;
    for (auto_continue, suppress, at_trigger) in [
        (true, false, false),
        (true, false, false),
        (false, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let normal = AtomicUsize::new(0);
        let server = serve_fn(
            Arc::new(move |request: &Value| {
                // Exercise the real tool-free summary route, rejecting the reported bad shape.
                if !request.get("tools").is_some_and(Value::is_array) {
                    return Reply::Http(400, "tools: Input should be a valid array");
                }
                if request["tools"].as_array().unwrap().is_empty() {
                    return Reply::Text(
                        "SUMMARY: preserve the task and continue the unfinished work.",
                    );
                }
                let text = if normal.fetch_add(1, Ordering::SeqCst) == 0 {
                    (0..1500)
                        .map(|i| format!("Inspected item {i}: recorded its current state.\n"))
                        .collect()
                } else {
                    "Recent answer.".into()
                };
                // Provider tokenization is deliberately much larger than bytes/4. Comparing
                // the raw prepared estimate to the calibrated replacement used to show growth.
                Reply::MeasuredSay(text, 70_000)
            }),
            None,
            None,
        );
        let mut e = env(&server.url, "auto");
        e.config.context_window = 200_000;
        e.config.compaction.enabled = false;
        e.config.compaction.mid_turn_enabled = false;
        e.config.compaction.keep_recent_tokens = 100;
        e.config.compaction.auto_continue = auto_continue;
        if let Some(tokens) = recovered_tokens {
            e.config.compaction.threshold_tokens = Some(if at_trigger {
                tokens - 128
            } else {
                tokens + 128
            });
        }
        let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let mut ui = Recorder::default();
        a.turn("Inspect the project", &mut ui).unwrap();
        a.turn("Continue the work", &mut ui).unwrap();
        let outcome = a
            .compact_with_options(
                &mut ui,
                CompactionOptions {
                    method: Some(CompactionMethod::Soft),
                    suppress_continuation: suppress,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(outcome.tokens_before > outcome.tokens_after, "{outcome:?}");
        assert_eq!(
            outcome.continue_allowed,
            auto_continue && !suppress && !at_trigger,
            "safe foreground compaction must not pause merely for missing the recovery band: {outcome:?}"
        );
        recovered_tokens.get_or_insert(outcome.tokens_after);
    }
}

#[test]
fn a_single_long_turn_can_be_compacted_instead_of_failing_with_nothing_to_compact() {
    // One user request followed by many large tool results has no earlier user message to cut at.
    // Each page is about 10 KiB; six of them plus the prompt fit the 32k window without filling it.
    let mut script: Vec<Reply> = (0..6)
        .map(|i| {
            Reply::Call(
                "read",
                json!({"path": format!("big.txt:{}-{}", i * 200 + 1, (i + 1) * 200)}),
            )
        })
        .collect();
    script.push(Reply::Text("all read"));
    // Summarizer input is window-budgeted and may use several chunks. This fixture proves the
    // manual cut's result rather than pinning a particular chunk count or role output reserve.
    script.extend((0..64).map(|_| Reply::Text("SUMMARY: read the whole big file")));
    let server = serve(script);
    let mut e = env(&server.url, "auto");
    // The default 20k keep-recent budget exceeds this 16k window, so the one turn would never be cut.
    e.config.compaction.keep_recent_tokens = 1000;
    // This fixture exercises the explicit manual boundary, independent of automatic trigger timing.
    e.config.compaction.enabled = false;
    e.config.compaction.mid_turn_enabled = false;
    let body: String = (0..1200)
        .map(|n| format!("line {n:04} {}\n", "x".repeat(40)))
        .collect();
    std::fs::write(e.project.path().join("big.txt"), body).unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("read the whole file", &mut ui).unwrap();
    let before = a.session().messages().len();
    a.compact(&mut ui)
        .expect("one long turn must be compactable");
    let after = a.session().messages();
    assert!(
        after[0]["content"].as_str().unwrap().contains("SUMMARY"),
        "summary leads the history: {after:?}"
    );
    assert!(
        after.len() < before,
        "history shrank: {before} -> {}",
        after.len()
    );
    assert!(
        after.iter().skip(1).all(|m| m["role"] != "tool") || after[1]["role"] != "tool",
        "no orphaned tool result at the cut"
    );
    assert_eq!(a.metrics().compactions, 1);
}

#[test]
fn manual_compaction_with_nothing_to_cut_is_a_successful_no_op() {
    let server = serve(vec![Reply::Text("hello back")]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("hi", &mut ui).unwrap();
    let before = a.session().messages();
    let outcome = a
        .compact_with_options(
            &mut ui,
            dial::config::compaction::CompactionOptions::default(),
        )
        .expect("manual compaction with nothing to cut must succeed");
    assert_eq!(
        outcome.kind,
        dial::agent::compaction::CompactionOutcomeKind::Unchanged
    );
    assert_eq!(a.session().messages(), before, "history is untouched");
    assert_eq!(a.metrics().compactions, 0);
    assert!(
        !a.usage()
            .0
            .by_purpose
            .contains_key(&dial::llm::usage::Purpose::Compaction)
    );
    assert!(
        ui.notices
            .iter()
            .any(|n| n.contains("nothing to compact yet")),
        "user is told why nothing happened: {:?}",
        ui.notices
    );
}

#[test]
fn model_selected_successful_shell_command_is_not_a_training_label() {
    let server = serve(vec![
        Reply::Call("bash", json!({"command":"echo cargo test; true"})),
        Reply::Text("Everything passed."),
    ]);
    let e = env(&server.url, "auto");
    let store = Store::open(&e.layout.state_dir, "scripted", "t", &e.config.harness).unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, Some(&store));
    let report = a
        .turn("Check the project.", &mut Recorder::default())
        .unwrap();
    assert!(store.verified_successes(100.0, 10).unwrap().is_empty());
    assert!(
        store
            .trajectory_messages(report.trajectory.unwrap())
            .unwrap()
            .is_some()
    );
    assert_eq!(store.live_score(a.version().id).unwrap(), (0, None));
}

#[test]
fn budget_exhaustion_never_passes_a_file_only_verifier() {
    let server = serve(vec![Reply::Call(
        "write",
        json!({"path":"out.txt","content":"correct"}),
    )]);
    let mut e = env(&server.url, "auto");
    e.config.harness.max_steps = 1;
    let version = dial::config::types::Version {
        id: 1,
        parent: None,
        kind: "initial".into(),
        harness: e.config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let result = dial::agent::check_case(
        &mut dial::agent::Runtime::new(),
        &e.config,
        &version,
        "Write out.txt and explain what you changed.",
        &Default::default(),
        &dial::learn::verify::Check::FileEquals {
            path: "out.txt".into(),
            content: "correct".into(),
        },
    )
    .unwrap();
    assert!(
        !result,
        "finishing the workspace mutation alone cannot excuse an unfinished run"
    );
}

#[test]
fn saved_provider_credentials_authorize_real_http_requests() {
    let server = serve_authenticated(vec![Reply::Text("authorized")], Some("fixture-token"));
    let e = env(&server.url, "auto");
    dial::llm::catalog::save_credential(&e.layout.user_dir, &server.url, "fixture-token").unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    assert_eq!(
        a.turn("hello", &mut Recorder::default()).unwrap().text,
        "authorized"
    );
}

#[test]
fn delegated_task_uses_its_role_and_returns_to_the_main_model() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"task":"Inspect this project.","solution_space":"s"}),
        ),
        Reply::Text("main finished"),
    ]);
    let side = serve(vec![Reply::Text("independent findings")]);
    let mut e = env(&main.url, "auto");
    e.config.model_roles.insert(
        "small".into(),
        dial::llm::model_config::ModelSelection {
            provider: "reviewer".into(),
            model: "side-model".into(),
            base_url: Some(side.url.clone()),
            ..Default::default()
        },
    );
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    assert_eq!(
        a.turn("Delegate an investigation.", &mut Recorder::default())
            .unwrap()
            .text,
        "main finished"
    );
    assert_eq!(side.requests.lock()[0]["model"], "side-model");
    let main_requests = main.requests.lock();
    assert_eq!(main_requests[1]["model"], "scripted");
    assert!(
        main_requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool" && m["content"] == "independent findings")
    );
}

#[test]
fn task_role_models_reach_sync_and_background_requests_without_changing_the_parent() {
    for action in ["run", "start"] {
        for scenario in ["task", "specialists", "helper-defaults"] {
            let mut replies = vec![Reply::Call(
                "task",
                json!({"action":action,"tasks":[
                    {"agent":"worker","task":"implement","solution_space":"s"},
                    {"agent":"scout","task":"inspect","solution_space":"s"},
                    {"agent":"reviewer","task":"review","solution_space":"s"},
                    {"agent":"planner","task":"plan","solution_space":"s"},
                    {"agent":"test-runner","task":"test","solution_space":"s"}
                ]}),
            )];
            if action == "start" {
                replies.push(Reply::Call("task", json!({"action":"wait"})));
            }
            replies.push(Reply::Text("parent finished"));
            let main = serve(replies);
            let side = serve_fn(Arc::new(|_| Reply::Text("child finished")), None, None);
            let mut e = env(&main.url, "auto");
            let roles: &[&str] = match scenario {
                "task" => &["task"],
                "specialists" => &["task", "small", "tester", "planner"],
                _ => &["coach", "small"],
            };
            for role in roles {
                e.config.model_roles.insert(
                    (*role).into(),
                    dial::llm::model_config::ModelSelection {
                        provider: "reviewer".into(),
                        model: format!("{role}-model"),
                        base_url: Some(side.url.clone()),
                        ..Default::default()
                    },
                );
            }
            let mut ui = Recorder::default();
            assert_eq!(
                run_turn(&e, &mut ui).text,
                "parent finished",
                "{action}, scenario={scenario}"
            );
            let mut models: Vec<_> = side
                .requests
                .lock()
                .iter()
                .map(|r| r["model"].as_str().unwrap().to_owned())
                .collect();
            models.sort();
            let mut expected = match scenario {
                "specialists" => vec![
                    "task-model",
                    "small-model",
                    "tester-model",
                    "planner-model",
                    "tester-model",
                ],
                "helper-defaults" => vec![
                    "coach-model",
                    "small-model",
                    "coach-model",
                    "coach-model",
                    "coach-model",
                ],
                _ => vec!["task-model"; 5],
            };
            expected.sort();
            assert_eq!(
                models, expected,
                "{action}, scenario={scenario}: {:?}",
                ui.results
            );
            assert!(
                main.requests
                    .lock()
                    .iter()
                    .all(|r| r["model"] == "scripted")
            );
        }
    }
}

#[test]
fn ordinary_turns_offer_task_and_skill_and_state_the_delegation_rules() {
    let server = serve(vec![Reply::Text("done")]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Summarize the readme.", &mut Recorder::default())
        .unwrap();
    let requests = server.requests.lock();
    let names = tool_names(&requests[0]);
    assert!(names.iter().any(|n| n == "task"), "{names:?}");
    assert!(names.iter().any(|n| n == "skill"), "{names:?}");
    assert!(
        requests[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "system" && m["content"].to_string().contains("# Delegation")),
        "system prompt lacks the delegation section"
    );
}

#[test]
fn task_items_receive_shared_context_goal_and_their_skill_body() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({
                "context": "SHARED_CONTEXT_MARKER: the parser lives in src/parse.rs",
                "tasks": [{
                    "task": "Inspect the parser.",
                    "solution_space": "read-only survey",
                    "skills": ["evidence"]
                }]
            }),
        ),
        Reply::Text("main finished"),
    ]);
    let side = serve(vec![Reply::Text("independent findings")]);
    let mut e = env(&main.url, "auto");
    e.config.model_roles.insert(
        "small".into(),
        dial::llm::model_config::ModelSelection {
            provider: "reviewer".into(),
            model: "side-model".into(),
            base_url: Some(side.url.clone()),
            ..Default::default()
        },
    );
    let skill_dir = e.layout.skills_dirs()[0].join("evidence");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: evidence\ndescription: test evidence\n---\n\nRead the actual evidence.",
    )
    .unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    assert_eq!(
        a.turn("Delegate an investigation.", &mut Recorder::default())
            .unwrap()
            .text,
        "main finished"
    );
    let side_requests = side.requests.lock();
    let child = side_requests[0].to_string();
    assert!(child.contains("SHARED_CONTEXT_MARKER"), "{child}");
    assert!(child.contains("Delegate an investigation."), "{child}");
    assert!(child.contains("Inspect the parser."), "{child}");
    assert!(child.contains("read-only survey"), "{child}");
    assert!(child.contains("Read the actual evidence."), "{child}");
    let main_requests = main.requests.lock();
    let result = main_requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()["content"]
        .to_string();
    assert!(result.contains("side-model"), "{result}");
    assert!(result.contains("small"), "{result}");
}

fn has_message_starting(request: &Value, prefix: &str) -> bool {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"].as_str().is_some_and(|c| c.starts_with(prefix)))
}

#[test]
fn prewalk_switches_to_small_after_the_first_write_and_keeps_it_for_later_turns() {
    let main = serve(vec![
        open_todo(),
        Reply::Call("write", json!({"path":"marker.txt","content":"done\n"})),
    ]);
    let side = serve(vec![
        Reply::Call("todo", json!({"op":"done"})),
        Reply::Text("small finished"),
        Reply::Text("small again"),
    ]);
    let mut e = env(&main.url, "auto");
    e.config.model_roles.insert(
        "small".into(),
        dial::llm::model_config::ModelSelection {
            provider: "reviewer".into(),
            model: "side-model".into(),
            base_url: Some(side.url.clone()),
            ..Default::default()
        },
    );
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let armed = a.prewalk_startup("@small").unwrap();
    assert!(armed[0].starts_with("Prewalk: armed for reviewer/side-model"));

    let mut ui = Recorder::default();
    assert_eq!(
        a.turn("Create the marker.", &mut ui).unwrap().text,
        "small finished"
    );
    {
        let main_requests = main.requests.lock();
        assert_eq!(
            main_requests.len(),
            2,
            "main plans and writes; the third request goes to small"
        );
        assert!(has_message_starting(
            &main_requests[0],
            "STOP: In NEXT reply"
        ));
    }
    {
        let side_requests = side.requests.lock();
        assert_eq!(
            side_requests.len(),
            2,
            "small closes the todo, then answers"
        );
        assert_eq!(side_requests[0]["model"], "side-model");
        assert!(has_message_starting(
            &side_requests[0],
            "Before task complete, verify:"
        ));
    }
    assert!(
        ui.notices
            .iter()
            .any(|n| n == "Prewalk: switched to reviewer/side-model after first write call.")
    );

    assert_eq!(
        a.turn("Continue.", &mut Recorder::default()).unwrap().text,
        "small again"
    );
    assert_eq!(main.requests.lock().len(), 2);
    assert_eq!(side.requests.lock().len(), 3);
}

#[test]
fn automatic_compaction_uses_the_selected_role_during_a_live_turn() {
    let main = serve(vec![
        Reply::Text("first"),
        Reply::Say(
            (0..4000)
                .map(|i| format!("{i:04} -- result {i:04}\n"))
                .collect(),
        ),
        Reply::Text("third"),
    ]);
    // The summarizer may split a large transcript into several windows; each gets one reply.
    let side = serve(vec![
        Reply::Text("summary of earlier project work"),
        Reply::Text("summary of earlier project work"),
        Reply::Text("summary of earlier project work"),
        Reply::Text("summary of earlier project work"),
    ]);
    let mut e = env(&main.url, "auto");
    // Turn two's large reply leaves the estimate above the 65% threshold (~21.3k of a pinned 32k window).
    e.config.context_window = 32768;
    e.config.compaction.threshold_percent = Some(65.0);
    e.config.model_roles.insert(
        "small".into(),
        dial::llm::model_config::ModelSelection {
            provider: "summary".into(),
            model: "summary-model".into(),
            base_url: Some(side.url.clone()),
            ..Default::default()
        },
    );
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("one?", &mut ui).unwrap();
    a.turn("two?", &mut ui).unwrap();
    assert_eq!(a.turn("three?", &mut ui).unwrap().text, "third");
    assert_eq!(side.requests.lock()[0]["model"], "summary-model");
    let usage = a.usage().0;
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Agent].requests,
        3
    );
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Compaction].requests,
        side.requests.lock().len() as u64
    );
    assert!(
        a.session().messages()[0]["content"]
            .as_str()
            .unwrap()
            .contains("summary of earlier")
    );
    assert!(
        ui.notices
            .iter()
            .any(|notice| notice.contains("Context compacted") && notice.contains("messages"))
    );
}

#[test]
fn lm_studio_loaded_context_overrides_the_configured_window_and_drives_compaction() {
    // LM Studio loaded this model with 16384 tokens although Dial is configured for 32768.
    let server = serve_with(
        vec![
            Reply::Text("first"),
            Reply::Say(
                (0..600)
                    .map(|i| format!("{i:04} -- result {i:04}\n"))
                    .collect(),
            ),
            Reply::Text("third"),
        ],
        None,
        Some(16384),
    );
    let mut e = env(&server.url, "auto");
    e.config.harness.tools = vec!["read".into()];
    e.config.compaction.threshold_percent = Some(35.0);
    // The default 20k keep-recent budget exceeds this 16k window, so nothing would be older than the tail.
    e.config.compaction.keep_recent_tokens = 1000;
    // Handoff folds the transcript into several windows; every window gets the same summary.
    let side = serve_fn(
        Arc::new(|_| Reply::Text("SUMMARY of the first two turns")),
        None,
        None,
    );
    e.config.model_roles.insert(
        "small".into(),
        dial::llm::model_config::ModelSelection {
            provider: "summary".into(),
            model: "summary-model".into(),
            base_url: Some(side.url.clone()),
            ..Default::default()
        },
    );
    assert_eq!(e.config.context_window, 32768);
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("one?", &mut ui).unwrap();
    a.turn("two?", &mut ui).unwrap();
    assert_eq!(
        a.context_window(),
        16384,
        "the loaded size, not the configured one, is the budget"
    );
    let third = a.turn("three?", &mut ui).unwrap().text;
    // A compaction that reduced the request no longer pauses the turn.
    assert!(!third.contains("paused by compaction policy"), "{third}");
    assert!(
        a.session().messages()[0]["content"]
            .as_str()
            .unwrap()
            .contains("SUMMARY"),
        "history must have been compacted {:?}",
        ui.notices
    );
}

#[test]
fn local_context_auto_follows_loaded_capacity_and_manual_caps_still_apply() {
    use dial::config::types::ContextWindow;
    for (mode, expected) in [
        (ContextWindow::Auto, 65536),
        (ContextWindow::Tokens(8192), 8192),
    ] {
        let server = serve_with(vec![Reply::Text("first")], None, Some(65536));
        let mut e = env(&server.url, "auto");
        e.config.harness.tools = vec!["read".into()];
        e.config.local_context_window = mode;
        // A stale catalog or unknown-model fallback must not pin Auto to 32768 after probing.
        e.config.context_window = 32768;
        let mut backend = make_backend(&e.config).unwrap();
        let mut a = agent(&e, &mut backend, None);
        a.turn("one?", &mut Recorder::default()).unwrap();
        assert_eq!(a.context_window(), expected);
    }
}

fn summary_requests(server: &Scripted) -> usize {
    server
        .requests
        .lock()
        .iter()
        .filter(|r| {
            r["messages"][0]["content"]
                .as_str()
                .is_some_and(|c| c.starts_with("Summarize user-AI"))
        })
        .count()
}

#[test]
fn compaction_that_frees_too_little_pauses_for_an_explicit_resume() {
    // LM Studio loaded 16384 tokens. Turn two is large (~4k tokens), so the request after it is over the
    // 35% threshold (~5.7k of a 16k window, band ~4.6k). One summary pass over turn one frees little: the fixed prompt (~4.8k) keeps the
    // next request above 80% of the threshold, so the turn pauses instead of continuing unattended.
    let server = serve_with(
        vec![
            Reply::Say(
                (0..40)
                    .map(|i| format!("line {i:02} of the first answer\n"))
                    .collect(),
            ),
            Reply::Say(
                (0..1000)
                    .map(|i| format!("{i:04} -- result {i:04}\n"))
                    .collect(),
            ),
            Reply::Text("third"),
        ],
        None,
        Some(16384),
    );
    let mut e = env(&server.url, "auto");
    e.config.harness.tools = vec!["read".into()];
    e.config.compaction.threshold_percent = Some(35.0);
    e.config.compaction.keep_recent_tokens = 1000;
    let side = serve(vec![Reply::Text("SUMMARY ONE"), Reply::Text("SUMMARY TWO")]);
    e.config.model_roles.insert(
        "small".into(),
        dial::llm::model_config::ModelSelection {
            provider: "summary".into(),
            model: "summary-model".into(),
            base_url: Some(side.url.clone()),
            ..Default::default()
        },
    );
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("one?", &mut ui).unwrap();
    a.turn("two?", &mut ui).unwrap();
    let third = a.turn("three?", &mut ui).unwrap().text;
    // A compaction that reduced the request no longer pauses the turn.
    assert!(!third.contains("paused by compaction policy"), "{third}");
    assert_eq!(
        a.metrics().compactions,
        1,
        "one pass; no unattended retry loop"
    );
    // The admitted pass is preserved; the pause must not silently run another summary cycle.
    let head = a
        .session()
        .messages()
        .iter()
        .take(3)
        .map(|m| m.to_string())
        .collect::<Vec<_>>();
    assert!(
        head.iter().any(|m| m.contains("SUMMARY ONE")),
        "{head:?} {:?}",
        ui.notices
    );
    assert_eq!(a.session().control_state()["current_instruction"], "three?");
}

#[test]
fn compaction_that_cannot_help_says_why_instead_of_summarising_again_and_again() {
    // Give the fixture enough immutable context to exceed half the served window independently of
    // the default prompt's wording. No history summary can repair the output-reserve deficit.
    let server = serve_with(
        vec![
            Reply::Text("first"),
            Reply::Text("second"),
            Reply::Text("third"),
        ],
        None,
        Some(8192),
    );
    let mut e = env(&server.url, "auto");
    e.config.harness.tools = vec!["read".into()];
    e.config.harness.max_tokens = 4096;
    e.config
        .harness
        .system_prompt
        .push_str(&"fixed immutable context ".repeat(600));
    let side = serve(vec![Reply::Text("SUMMARY")]);
    e.config.model_roles.insert(
        "small".into(),
        dial::llm::model_config::ModelSelection {
            provider: "summary".into(),
            model: "summary-model".into(),
            base_url: Some(side.url.clone()),
            ..Default::default()
        },
    );
    // The immutable context must exceed the 50% threshold before any summary is attempted.
    e.config.compaction.threshold_percent = Some(50.0);
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    for instruction in ["one?", "two?", "three?"] {
        let error = a
            .turn(instruction, &mut ui)
            .err()
            .expect("immutable context cannot leave the requested response reserve");
        assert!(
            error.to_string().contains("reserved for a useful response"),
            "{error:#}"
        );
    }
    assert!(
        server.requests.lock().is_empty(),
        "must refuse before inference"
    );
    assert_eq!(a.session().control_state()["current_instruction"], "three?");
    assert_eq!(
        summary_requests(&side),
        0,
        "a fixed prompt that cannot fit is refused before any summary"
    );
    let notice = ui
        .notices
        .iter()
        .find(|n| n.contains("could not make enough room"))
        .unwrap_or_else(|| panic!("{:?}", ui.notices));
    assert!(
        notice.contains("immutable system/tool/transient context")
            && notice.contains("history compaction cannot fix this"),
        "{notice}"
    );
}
#[test]
fn servers_without_a_native_context_api_keep_the_configured_window() {
    let server = serve(vec![Reply::Text("hi")]);
    let e = env(&server.url, "auto");
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    a.turn("hello", &mut Recorder::default()).unwrap();
    assert_eq!(a.context_window(), 32768);
    // A 404 on the metadata probe must not have been mistaken for a chat reply.
    assert_eq!(server.requests.lock().len(), 1);
}

#[test]
fn automatic_output_ceiling_fits_the_request_window() {
    let mut script = vec![
        Reply::Say((0..150).map(|i| format!("detail{i} ")).collect()),
        Reply::Text("done"),
        Reply::Text("third answer"),
    ];
    script.extend((0..64).map(|_| Reply::Text("SUMMARY: retained the original goal")));
    let server = serve(script);
    let mut e = env(&server.url, "auto");
    e.config.harness.max_tokens = 0;
    e.config.context_window = 32768;
    e.config.compaction.keep_recent_tokens = 100;
    e.config.compaction.enabled = false;
    e.config.compaction.mid_turn_enabled = false;
    let backend =
        make_backend(&e.config)
            .unwrap()
            .with_metadata(dial::llm::catalog::ModelMetadata {
                max_output_tokens: Some(128000),
                ..Default::default()
            });
    assert_eq!(backend.output_budget(0), 128000);
    let session = Session::create(
        &e.layout,
        SessionMeta {
            model: "scripted".into(),
        },
    )
    .unwrap();
    let mut a = Agent::interactive(
        &e.config,
        backend,
        e.layout.clone(),
        session,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    a.turn("hello", &mut Recorder::default()).unwrap();
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let output = request
        .get("max_completion_tokens")
        .or_else(|| request.get("max_tokens"))
        .and_then(Value::as_u64)
        .expect("finite wire output limit");
    let prompt = dial::llm::wire::estimate_parts(
        request["messages"].as_array().unwrap(),
        request["tools"].as_array().unwrap(),
    );
    assert!(
        prompt + output <= u64::from(e.config.context_window),
        "prompt={prompt}, output={output}"
    );
    assert!(output >= u64::from(e.config.compaction.reserve(e.config.context_window)));
    assert_eq!(
        e.config.harness.max_tokens, 0,
        "operator configuration remains unchanged"
    );
    drop(requests);
    a.turn("second", &mut Recorder::default()).unwrap();
    a.turn("third", &mut Recorder::default()).unwrap();
    a.compact(&mut Recorder::default())
        .expect("automatic output also leaves room for explicit compaction");
    assert!(a.session().messages().iter().any(|message| {
        message["content"]
            .as_str()
            .is_some_and(|text| text.contains("SUMMARY"))
    }));
}

#[test]
fn small_window_completes_first_turn_with_on_demand_tools() {
    let server = serve(vec![Reply::Text("done")]);
    let mut e = env(&server.url, "auto");
    e.config.assistance.compact_tools = true;
    e.config.context_window = 8192;
    // Unlike the delegation fixture helper, a fresh session has no explicitly pinned task schema.
    let session = Session::create(
        &e.layout,
        SessionMeta {
            model: "scripted".into(),
        },
    )
    .unwrap();
    let mut a = Agent::interactive(
        &e.config,
        make_backend(&e.config).unwrap(),
        e.layout.clone(),
        session,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut ui = Recorder::default();
    let report = a.turn("hello", &mut ui).unwrap();
    assert_eq!(report.text, "done");
    // A fresh on-demand catalog leaves response headroom without removing required primitives.
    let names = tool_names(&server.requests.lock()[0]);
    for tool in ["read", "edit", "finish", "tool_load"] {
        assert!(names.iter().any(|n| n == tool), "{tool} missing: {names:?}");
    }
}

#[test]
fn small_window_refuses_to_spend_response_reserve_or_drop_a_pinned_tool() {
    let server = serve(vec![Reply::Text("must not be requested")]);
    let mut e = env(&server.url, "auto");
    e.config.assistance.compact_tools = true;
    e.config.context_window = 8192;
    e.config
        .harness
        .system_prompt
        .push_str(&"immutable project requirement ".repeat(100));
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None); // Explicitly pins task for delegation.
    let error = a
        .turn("hello", &mut Recorder::default())
        .err()
        .expect("response reserve must be preserved");
    assert!(
        error.to_string().contains("reserved for a useful response"),
        "{error:#}"
    );
    assert!(server.requests.lock().is_empty());
    assert_eq!(
        a.session().control_state()["enabled_tools"],
        json!(["task"])
    );
    assert_eq!(a.session().control_state()["current_instruction"], "hello");
}

#[test]
fn window_too_small_for_the_full_set_offers_a_lean_tool_set() {
    let server = serve(vec![Reply::Text("baseline"), Reply::Text("done")]);
    let mut e = env(&server.url, "auto");
    e.config.assistance.compact_tools = true;
    // `agent_with` seeds enabled_tools=["task"] whenever the harness selects task (delegation
    // tests). A fresh session starts with nothing on demand, which is the lean set under test.
    e.config
        .harness
        .tools
        .retain(|tool| tool.as_str() != "task");
    // Sandbox instructions and path lengths differ by platform. Measure the full request
    // instead of assuming that a fixed window overflows on every supported host.
    let full_tokens = {
        let mut backend = make_backend(&e.config).unwrap();
        let mut a = agent(&e, &mut backend, None);
        assert_eq!(
            a.turn("hello", &mut Recorder::default()).unwrap().text,
            "baseline"
        );
        let requests = server.requests.lock();
        let request = &requests[0];
        assert!(tool_names(request).iter().any(|name| name == "find"));
        dial::llm::wire::estimate_parts(
            request["messages"].as_array().unwrap(),
            request["tools"].as_array().unwrap(),
        )
    };
    e.config.context_window = u32::try_from(full_tokens.checked_sub(256).unwrap()).unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    let report = a.turn("hello", &mut ui).unwrap();
    assert_eq!(report.text, "done");
    assert!(
        ui.notices
            .iter()
            .any(|notice| notice.contains("needs response headroom")),
        "{:?}",
        ui.notices
    );
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 2);
    let names = tool_names(&requests[1]);
    for tool in ["read", "write", "edit", "bash", "finish", "tool_search"] {
        assert!(names.iter().any(|n| n == tool), "{tool} missing: {names:?}");
    }
    for tool in ["task", "find", "skill"] {
        assert!(
            !names.iter().any(|n| n == tool),
            "{tool} offered: {names:?}"
        );
    }
}

#[test]
fn too_small_window_for_even_the_lean_set_fails_actionably() {
    let server = serve(vec![Reply::Text("unreached")]);
    let mut e = env(&server.url, "auto");
    e.config.assistance.compact_tools = true;
    e.config.context_window = 1024;
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    let error = match a.turn("hello", &mut ui) {
        Err(error) => format!("{error:#}"),
        Ok(report) => panic!("expected an overflow error, got {:?}", report.text),
    };
    assert!(
        error.contains("1024-token context window")
            && error.contains("even with the lean tool set")
            && error.contains("larger-window model"),
        "{error}"
    );
    assert!(server.requests.lock().is_empty(), "nothing may be sent");
}

#[test]
fn roomy_window_keeps_the_full_tool_set_without_notice() {
    let server = serve(vec![Reply::Text("done")]);
    let mut e = env(&server.url, "auto");
    e.config.assistance.compact_tools = true;
    e.config.context_window = 32768;
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("hello", &mut ui).unwrap();
    assert!(
        !ui.notices.iter().any(|n| n.contains("lean tool set")),
        "{:?}",
        ui.notices
    );
    let names = tool_names(&server.requests.lock()[0]);
    for tool in ["task", "find", "skill"] {
        assert!(names.iter().any(|n| n == tool), "{tool} missing: {names:?}");
    }
}

// ------------------------------------------------------------------------------------ subagents

fn with_subagent_role(e: &mut Env, url: &str) {
    // Specialist assignments win; implementation workers use the task role.
    for role in ["main", "task", "small", "tester", "planner"] {
        e.config.model_roles.insert(
            role.into(),
            dial::llm::model_config::ModelSelection {
                provider: "reviewer".into(),
                model: "side-model".into(),
                base_url: Some(url.into()),
                ..Default::default()
            },
        );
    }
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

/// The assignment a subagent was started with: the text after `Assignment:` in its first user
/// message, which is the brief (parent background and shared context precede it).
fn prompt_of(request: &Value) -> String {
    let brief = first_user(request)["content"].as_str().unwrap_or_default();
    brief
        .rsplit_once("Assignment:\n")
        .map_or(brief, |(_, task)| task)
        .to_owned()
}

fn has_tool_result(request: &Value) -> bool {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool")
}

#[test]
fn child_repeated_successful_shell_inspections_stop_before_exhausting_steps() {
    if skip_without_sandbox() {
        return;
    }
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"task":"Verify the package version","solution_space":"s","agent":"worker","max_steps":12}),
        ),
        Reply::Text("child stopped"),
    ]);
    let side = serve(
        (0..8)
            .map(|n| Reply::IdentifiedCall(
                "bash",
                json!({"command":"cd . && grep '^version' pyproject.toml && grep '__version__' version.py", "timeout":30+n}),
                format!("version-{n}"),
            ))
            .collect(),
    );
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    std::fs::write(
        e.project.path().join("pyproject.toml"),
        "version = \"0.3.31\"\n",
    )
    .unwrap();
    std::fs::write(
        e.project.path().join("version.py"),
        "__version__ = \"0.3.31\"\n",
    )
    .unwrap();
    let mut ui = Recorder::default();
    run_turn(&e, &mut ui);
    let result = task_result(&main);
    assert!(
        result.contains("script exhausted"),
        "{result}; results: {:?}",
        ui.results
    );
    let requests = side.requests.lock();
    assert_eq!(requests.len(), 9);
    let results: Vec<_> = requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert!(results[0]["content"].as_str().unwrap().contains("0.3.31"));
    assert!(
        results[1]["content"]
            .as_str()
            .unwrap()
            .contains("unchanged from tool call version-0")
    );
    assert!(!results[1]["content"].as_str().unwrap().contains("0.3.31"));
}

#[test]
fn parent_and_child_continue_after_reaped_timeouts() {
    if skip_without_sandbox() {
        return;
    }
    let calls = || {
        Reply::Calls(vec![
            (
                "bash",
                json!({"command":"printf before > effect.txt; sleep 5", "timeout":1}),
            ),
            (
                "write",
                json!({"path":"must-not-run.txt", "content":"unsafe continuation"}),
            ),
        ])
    };
    let parent = serve(vec![calls(), Reply::Text("Recovered after timeout.")]);
    let e = env(&parent.url, "auto");
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = agent.turn("Perform the operations", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    assert_eq!(report.text, "Recovered after timeout.");
    assert!(
        ui.results
            .iter()
            .any(|(name, text, failed)| name == "bash" && *failed && text.contains("timed out"))
    );
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("effect.txt")).unwrap(),
        "before"
    );
    assert!(e.project.path().join("must-not-run.txt").exists());
    assert_eq!(parent.requests.lock().len(), 2);
    assert!(
        agent
            .session()
            .messages()
            .iter()
            .all(|m| m["dial_effects"] != "unknown")
    );

    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"task":"Perform the operations", "solution_space":"s", "agent":"worker", "max_steps":8}),
        ),
        Reply::Text("The worker recovered after timeout."),
    ]);
    let side = serve(vec![calls(), Reply::Text("Recovered after timeout.")]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    run_turn(&e, &mut Recorder::default());
    assert!(
        task_result(&main).contains("Recovered after timeout."),
        "{}",
        task_result(&main)
    );
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("effect.txt")).unwrap(),
        "before"
    );
    assert!(e.project.path().join("must-not-run.txt").exists());
    assert_eq!(side.requests.lock().len(), 2);
}

#[test]
fn child_repeated_reads_reuse_evidence_and_stop_before_exhausting_steps() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"task":"Inspect marker.txt and report","solution_space":"s","agent":"scout","max_steps":12}),
        ),
        Reply::Text("child stopped"),
    ]);
    let side = serve(
        (0..8)
            .map(|n| {
                let args = if n % 2 == 0 {
                    json!({"path":"marker.txt:1-2"})
                } else {
                    json!({"path":"marker.txt", "offset":1, "limit":2})
                };
                Reply::IdentifiedCall("read", args, format!("read-{n}"))
            })
            .collect(),
    );
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    std::fs::write(
        e.project.path().join("marker.txt"),
        "ORIGINAL_EVIDENCE\ntwo\n",
    )
    .unwrap();
    run_turn(&e, &mut Recorder::default());
    let result = task_result(&main);
    // Repeats are answered with a pointer and escalate; the child keeps running until it answers.
    assert!(result.contains("script exhausted"), "{result}");
    let requests = side.requests.lock();
    assert_eq!(requests.len(), 9);
    let messages = requests[2]["messages"].as_array().unwrap();
    let results: Vec<_> = messages.iter().filter(|m| m["role"] == "tool").collect();
    assert!(
        results[0]["content"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_EVIDENCE")
    );
    let duplicate = results[1]["content"].as_str().unwrap();
    assert!(duplicate.starts_with("[Unchanged read]"), "{duplicate}");
    assert!(duplicate.contains("tool call read-0"));
    assert!(duplicate.contains("still shown above"));
    assert!(!duplicate.contains("ORIGINAL_EVIDENCE"));
}

#[test]
fn delegation_efficiency_observes_exact_inspection_overlap_after_report_delivery() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"task":"Inspect marker.txt and report", "solution_space":"s", "agent":"scout", "max_steps":4}),
        ),
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Text("checked"),
    ]);
    let side = serve(vec![
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Text("marker.txt contains evidence"),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    std::fs::write(e.project.path().join("marker.txt"), "evidence\n").unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    a.turn("delegate the inspection", &mut Recorder::default())
        .unwrap();
    let report = a.efficiency_report();
    assert_eq!(report["projection_bytes"]["delivered_worker_results"], 1);
    assert_eq!(
        report["projection_bytes"]["parent_inspections_matching_delivered_workers"],
        1
    );
    assert_eq!(side.requests.lock().len(), 2);
}

#[test]
fn child_rereads_after_an_edit_are_fresh_evidence() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"task":"Update marker.txt","solution_space":"s","agent":"worker"}),
        ),
        Reply::Text("done"),
    ]);
    let side = serve(vec![
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Call(
            "write",
            json!({"path":"marker.txt", "content":"new evidence"}),
        ),
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Text("changed and checked"),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    std::fs::write(e.project.path().join("marker.txt"), "old evidence").unwrap();
    run_turn(&e, &mut Recorder::default());
    let result = task_result(&main);
    assert!(result.contains("changed and checked"), "{result}");
    let requests = side.requests.lock();
    let messages = requests[4]["messages"].as_array().unwrap();
    let result = messages.iter().rev().find(|m| m["role"] == "tool").unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(result.contains("new evidence"));
    assert!(!result.contains("This observation is unchanged"));
}

/// What the parent's second request carried back from its `task` call.
fn task_result(main: &Scripted) -> String {
    let requests = main.requests.lock();
    let messages = requests[1]["messages"].as_array().unwrap();
    messages
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap()
        .to_owned()
}

fn run_turn(e: &Env, ui: &mut Recorder) -> dial::agent::TurnReport {
    let mut backend = make_backend(&e.config).unwrap();
    agent(e, &mut backend, None).turn("delegate", ui).unwrap()
}

#[test]
fn each_agent_type_gets_its_own_prompt_and_tool_allowlist() {
    let cases: [(&str, &[&str]); 5] = [
        ("scout", &["read", "glob", "grep", "bash", "skill"]),
        ("reviewer", &["read", "glob", "grep", "skill"]),
        ("planner", &["read", "glob", "grep", "skill"]),
        (
            "worker",
            &["read", "glob", "grep", "bash", "edit", "write", "skill"],
        ),
        // A config preset is a custom scout.
        ("security", &["read", "glob", "grep", "bash", "skill"]),
    ];
    for (agent_type, tools) in cases {
        let main = serve(vec![
            Reply::Call(
                "task",
                json!({"agent": agent_type, "task": "look", "solution_space": "s"}),
            ),
            Reply::Text("ok"),
        ]);
        let side = serve(vec![Reply::Text("child done")]);
        let mut e = env(&main.url, "auto");
        e.config
            .subagents
            .insert("security".into(), "Hunt for injection bugs.".into());
        with_subagent_role(&mut e, &side.url);
        run_turn(&e, &mut Recorder::default());
        let request = side.requests.lock()[0].clone();
        let names = tool_names(&request);
        // The child's offered list is sorted, so compare membership rather than order.
        for tool in tools {
            assert!(
                names.iter().any(|name| name == tool),
                "{agent_type} lacks {tool}: {names:?}"
            );
        }
        assert!(
            names.iter().any(|name| name == "artifact_read"),
            "{agent_type}: {names:?}"
        );
        // Read-only agents may receive enabled read-only extensions (codebase/memory recall,
        // archive inspection), never writers.
        if agent_type != "worker" {
            for extra in names.iter().filter(|name| !tools.contains(&name.as_str())) {
                assert!(
                    matches!(
                        extra.as_str(),
                        "codebase_memory"
                            | "find"
                            | "recall"
                            | "memory_history"
                            | "artifact_read"
                            | "archive"
                            | "ask"
                    ),
                    "{agent_type} got non-read-only tool {extra}"
                );
            }
        }
        let system = system_text(&request);
        assert!(
            system.contains(&format!("You are a {agent_type} subagent")),
            "{agent_type}: {system}"
        );
        assert_eq!(
            system.contains("Hunt for injection bugs."),
            agent_type == "security"
        );
        // The model is told about every type, the preset and the batch form.
        let offered = main.requests.lock()[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["function"]["name"] == "task")
            .unwrap()
            .clone();
        let description = offered["function"]["description"].as_str().unwrap();
        for name in ["scout", "reviewer", "planner", "worker", "security"] {
            assert!(description.contains(name), "{name}");
        }
        assert!(offered["function"]["parameters"]["properties"]["tasks"].is_object());
    }
}

#[test]
fn an_invalid_task_call_runs_nothing_and_tells_the_model_why() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"tasks": [{"task": "fine", "solution_space": "s"}, {"agent": "nope", "task": "x", "solution_space": "s"}]}),
        ),
        Reply::Text("ok"),
    ]);
    let side = serve(vec![Reply::Text("never asked")]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    run_turn(&e, &mut Recorder::default());
    assert!(
        side.requests.lock().is_empty(),
        "a batch with one bad task must not start any"
    );
    assert!(
        task_result(&main).contains("tasks[1]: unknown agent \"nope\""),
        "{}",
        task_result(&main)
    );
}

#[test]
fn a_batch_runs_read_only_agents_concurrently_and_reports_in_input_order() {
    let arrived = Arc::new(AtomicUsize::new(0));
    let stalled = Arc::new(AtomicBool::new(false));
    let side = {
        let (arrived, stalled) = (arrived.clone(), stalled.clone());
        serve_fn(
            Arc::new(move |request| {
                let prompt = prompt_of(request);
                arrived.fetch_add(1, Ordering::SeqCst);
                // Hold every request until all three children are in flight: only concurrent children get past.
                let deadline = Instant::now() + Duration::from_secs(10);
                while arrived.load(Ordering::SeqCst) < 3 {
                    if Instant::now() > deadline {
                        stalled.store(true, Ordering::SeqCst);
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                // Finish in reverse order so the report order cannot be an accident of timing.
                thread::sleep(Duration::from_millis(match prompt.as_str() {
                    "first" => 300,
                    "second" => 150,
                    _ => 0,
                }));
                Reply::Say(format!("answer to {prompt}"))
            }),
            None,
            None,
        )
    };
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"tasks": [
            {"task": "first", "name": "alpha", "solution_space": "s"}, {"agent": "reviewer", "task": "second", "solution_space": "s"}, {"agent": "planner", "task": "third", "solution_space": "s"}]}),
        ),
        Reply::Text("main finished"),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    // A localhost endpoint is one shared server and takes one request at a time unless the operator raises it.
    e.config
        .task
        .provider_concurrency
        .insert("reviewer".into(), 3);
    let mut ui = Recorder::default();
    assert_eq!(run_turn(&e, &mut ui).text, "main finished");
    assert!(
        !stalled.load(Ordering::SeqCst),
        "the three children were never in flight together"
    );
    let result = task_result(&main);
    let at = |needle: &str| {
        result
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing from:\n{result}"))
    };
    assert!(
        at("## 1. alpha (scout, role small, model reviewer/side-model) - ok")
            < at("answer to first")
    );
    assert!(
        at("answer to first")
            < at("## 2. reviewer#2 (reviewer, role tester, model reviewer/side-model) - ok")
    );
    assert!(
        at("## 2. reviewer#2 (reviewer, role tester, model reviewer/side-model) - ok")
            < at("answer to second")
    );
    assert!(
        at("answer to second")
            < at("## 3. planner#3 (planner, role planner, model reviewer/side-model) - ok")
    );
    assert!(
        at("## 3. planner#3 (planner, role planner, model reviewer/side-model) - ok")
            < at("answer to third")
    );
    assert!(result.starts_with("3 subagent task(s): 3 ok, 0 failed."));
    // The user sees who ran and how each ended, and the UI switches to the subagent role and back.
    assert!(
        ui.notices
            .iter()
            .any(|n| n.starts_with("subagent alpha (1/3)")),
        "{:?}",
        ui.notices
    );
    assert!(
        ui.notices
            .iter()
            .any(|n| n.starts_with("subagent planner#3 finished")),
        "{:?}",
        ui.notices
    );
    assert_eq!(ui.roles, ["small", "tester", "planner", "main"]);
    // Each child ran as its own conversation: a system prompt and one brief. The parent's goal is
    // passed only as labelled background; the assignment itself is the child's own task.
    let prompts: Vec<String> = side.requests.lock().iter().map(prompt_of).collect();
    assert_eq!(prompts.len(), 3);
    assert!(
        prompts.iter().all(|p| !p.contains("delegate")),
        "{prompts:?}"
    );
    assert!(side.requests.lock().iter().all(|r| {
        r["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] != "system")
            .count()
            == 1
    }));
}

#[test]
fn one_failing_task_does_not_sink_the_batch() {
    let side = serve_fn(
        Arc::new(|request| match prompt_of(request).as_str() {
            "stuck" => Reply::Call("read", json!({"path": "x.txt"})),
            _ => Reply::Text("fine"),
        }),
        None,
        None,
    );
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"tasks": [{"task": "good", "solution_space": "s"}, {"task": "stuck", "solution_space": "s", "max_steps": 1}]}),
        ),
        Reply::Text("ok"),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    run_turn(&e, &mut Recorder::default());
    let result = task_result(&main);
    assert!(
        result.starts_with("2 subagent task(s): 1 ok, 1 failed."),
        "{result}"
    );
    assert!(
        result.contains("subagent incomplete: used all 1 steps without a final answer"),
        "{result}"
    );
}

#[test]
fn workers_in_a_batch_run_alone_after_the_read_only_tasks_and_need_approval() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let side = {
        let log = log.clone();
        serve_fn(
            Arc::new(move |request| {
                let prompt = prompt_of(request);
                log.lock().push(format!("start {prompt}"));
                thread::sleep(Duration::from_millis(120));
                log.lock().push(format!("end {prompt}"));
                Reply::Say(format!("done {prompt}"))
            }),
            None,
            None,
        )
    };
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"tasks": [
            {"agent": "worker", "task": "edit", "solution_space": "s"}, {"task": "scan-a", "solution_space": "s"}, {"task": "scan-b", "solution_space": "s"}]}),
        ),
        Reply::Text("ok"),
    ]);
    let mut e = env(&main.url, "ask");
    with_subagent_role(&mut e, &side.url);
    let mut ui = Recorder::default();
    run_turn(&e, &mut ui);
    assert_eq!(
        ui.approvals,
        ["task"],
        "launching a batch that contains a worker is itself approved"
    );
    let log = log.lock();
    let pos = |entry: &str| {
        log.iter()
            .position(|l| l == entry)
            .unwrap_or_else(|| panic!("{entry} missing from {log:?}"))
    };
    assert!(
        pos("start edit") > pos("end scan-a") && pos("start edit") > pos("end scan-b"),
        "{log:?}"
    );
    assert_eq!(
        pos("end edit"),
        pos("start edit") + 1,
        "nothing may overlap a worker: {log:?}"
    );
    let result = task_result(&main);
    assert!(
        result.find("## 1. worker#1").unwrap() < result.find("## 2. scout#2").unwrap(),
        "{result}"
    );
}

#[test]
fn safe_inspection_from_concurrent_subagents_needs_no_prompt() {
    let side = serve_fn(
        Arc::new(|request| {
            if has_tool_result(request) {
                Reply::Text("ok")
            } else {
                Reply::Call("bash", json!({"command": "echo hi"}))
            }
        }),
        None,
        None,
    );
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"tasks": [{"task": "a", "solution_space": "s", "name": "one"}, {"task": "b", "solution_space": "s", "name": "two"}]}),
        ),
        Reply::Text("ok"),
    ]);
    let mut e = env(&main.url, "ask");
    with_subagent_role(&mut e, &side.url);
    let mut ui = Recorder {
        deny: true,
        ..Recorder::default()
    };
    run_turn(&e, &mut ui);
    assert!(
        ui.approvals.is_empty(),
        "read-only shell inspection needs no approval"
    );
    let followups: Vec<Value> = side
        .requests
        .lock()
        .iter()
        .filter(|r| has_tool_result(r))
        .cloned()
        .collect();
    assert_eq!(followups.len(), 2);
    assert!(
        followups.iter().all(
            |r| r["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool"
                    && m["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("hi") && !text.contains("denied")))
        )
    );
}

#[test]
fn a_worker_edits_through_the_parents_approval_and_checkpoints() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"agent": "worker", "task": "rewrite f.txt", "solution_space": "s"}),
        ),
        Reply::Text("main finished"),
    ]);
    let side = serve(vec![
        Reply::Call("read", json!({"path": "f.txt"})),
        Reply::Call("write", json!({"path": "f.txt", "content": "changed\n"})),
        Reply::Text("changed f.txt"),
    ]);
    let mut e = env(&main.url, "ask");
    with_subagent_role(&mut e, &side.url);
    std::fs::write(e.project.path().join("f.txt"), "one\ntwo\n").unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("delegate", &mut ui).unwrap();
    assert_eq!(ui.approvals, ["task", "write"]);
    assert_eq!(ui.summaries[1], "[worker] f.txt");
    assert_eq!(task_result(&main), "changed f.txt");
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("f.txt")).unwrap(),
        "changed\n"
    );
    a.checkpoints().unwrap().rollback(1).unwrap();
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("f.txt")).unwrap(),
        "one\ntwo\n",
        "the worker's write is undoable like the parent's"
    );
}

#[test]
fn refusing_the_task_keeps_a_worker_from_starting() {
    for (mode, deny) in [("ask", true), ("readonly", false)] {
        let main = serve(vec![
            Reply::Call(
                "task",
                json!({"agent": "worker", "task": "write out.txt", "solution_space": "s"}),
            ),
            Reply::Text("ok"),
        ]);
        let side = serve(vec![
            Reply::Call("write", json!({"path": "out.txt", "content": "x"})),
            Reply::Text("done"),
        ]);
        let mut e = env(&main.url, mode);
        with_subagent_role(&mut e, &side.url);
        let mut ui = Recorder {
            deny,
            ..Recorder::default()
        };
        run_turn(&e, &mut ui);
        assert!(
            side.requests.lock().is_empty(),
            "{mode}: no worker may start"
        );
        assert!(!e.project.path().join("out.txt").exists());
        assert!(task_result(&main).contains("denied"), "{mode}");
    }
}

#[test]
fn read_only_agents_need_no_approval_even_in_readonly_mode() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"agent": "reviewer", "task": "review", "solution_space": "s"}),
        ),
        Reply::Text("ok"),
    ]);
    let side = serve(vec![Reply::Text("looks fine")]);
    let mut e = env(&main.url, "readonly");
    with_subagent_role(&mut e, &side.url);
    let mut ui = Recorder::default();
    run_turn(&e, &mut ui);
    assert!(ui.approvals.is_empty());
    assert_eq!(task_result(&main), "looks fine");
}

#[test]
fn subagents_without_explicit_spawn_permissions_cannot_delegate() {
    let main = serve(vec![
        Reply::Call("task", json!({"task": "outer", "solution_space": "s"})),
        Reply::Text("ok"),
    ]);
    let side = serve(vec![
        Reply::Call("task", json!({"task": "inner", "solution_space": "s"})),
        Reply::Text("gave up"),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    run_turn(&e, &mut Recorder::default());
    let requests = side.requests.lock();
    assert_eq!(
        requests.len(),
        2,
        "the nested task must not have started a third conversation"
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("unknown or unavailable tool: task")
    );
    drop(requests);
    assert_eq!(task_result(&main), "gave up");
}

#[test]
fn structured_output_is_validated_and_retried_once_with_the_error() {
    let schema = json!({"type": "object", "required": ["count"], "properties": {"count": {"type": "integer"}}, "additionalProperties": false});
    // Recovers: prose first, then a fenced JSON value that satisfies the schema.
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"task": "count", "solution_space": "s", "output_schema": schema}),
        ),
        Reply::Text("ok"),
    ]);
    let side = serve(vec![
        Reply::Text("The count is 3"),
        Reply::Text("```json\n{\"count\": 3}\n```"),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    run_turn(&e, &mut Recorder::default());
    assert_eq!(task_result(&main), "{\"count\":3}");
    let requests = side.requests.lock();
    assert!(system_text(&requests[0]).contains("JSON Schema"));
    let correction = requests[1]["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        correction.contains("rejected") && correction.contains("not valid JSON"),
        "{correction}"
    );
    drop(requests);

    // Gives up: the second answer is still wrong, so there is exactly one retry and then a failure.
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"task": "count", "solution_space": "s", "output_schema": schema}),
        ),
        Reply::Text("ok"),
    ]);
    let side = serve(vec![
        Reply::Text("{\"count\": \"x\"}"),
        Reply::Text("{\"count\": \"y\"}"),
    ]);
    let mut e = env(&main.url, "auto");
    e.config.task.max_schema_retries = 1;
    with_subagent_role(&mut e, &side.url);
    run_turn(&e, &mut Recorder::default());
    let result = task_result(&main);
    assert!(
        result.contains("does not match output_schema")
            && result.contains("$.count: expected integer, got string"),
        "{result}"
    );
    assert_eq!(side.requests.lock().len(), 2);
}

#[test]
fn an_interrupt_stops_running_subagents_and_the_turn() {
    let flag = Arc::new(AtomicBool::new(false));
    let side = {
        let flag = flag.clone();
        serve_fn(
            Arc::new(move |_| {
                flag.store(true, Ordering::SeqCst);
                Reply::Call("read", json!({"path": "x.txt"}))
            }),
            None,
            None,
        )
    };
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"tasks": [{"task": "a", "solution_space": "s"}, {"task": "b", "solution_space": "s"}, {"task": "c", "solution_space": "s"}]}),
        ),
        Reply::Text("must not be asked"),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    let mut a = agent_with(&e, None, flag);
    let report = a.turn("delegate", &mut Recorder::default()).unwrap();
    assert!(report.interrupted);
    assert_eq!(
        main.requests.lock().len(),
        1,
        "the parent must not ask the model again"
    );
    assert!(
        side.requests.lock().iter().all(|r| !has_tool_result(r)),
        "no child may continue after the interrupt"
    );
}

#[test]
fn hermetic_agents_neither_offer_nor_run_task() {
    let server = serve(vec![
        Reply::Call("task", json!({"task": "x", "solution_space": "s"})),
        Reply::Text("done"),
    ]);
    let e = env(&server.url, "auto");
    let version = dial::config::types::Version {
        id: 1,
        parent: None,
        kind: "initial".into(),
        harness: e.config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let passed = dial::agent::check_case(
        &mut dial::agent::Runtime::new(),
        &e.config,
        &version,
        "say done",
        &Default::default(),
        &dial::learn::verify::Check::AnswerExact {
            expected: "done".into(),
        },
    )
    .unwrap();
    assert!(passed);
    let requests = server.requests.lock();
    let offered = tool_names(&requests[0]);
    assert!(
        offered.iter().all(
            |n| ["read", "write", "edit", "glob", "grep", "bash", "todo"].contains(&n.as_str())
        ),
        "{offered:?}"
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("unknown or unavailable tool: task")
    );
}

#[test]
fn a_subagent_preset_cannot_reuse_an_agent_type_name() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let overlay = |name: &str| -> toml::Table {
        toml::from_str(&format!(
        "model = \"m\"\n[endpoint]\nbase_url = \"http://127.0.0.1:1/v1\"\n[subagents]\n{name} = \"Extra guidance.\"\n")).unwrap()
    };
    let error = Config::load(&layout, Some(overlay("worker")))
        .expect_err("a preset named like an agent type must be refused");
    assert!(
        format!("{error:#}").contains("collides with a built-in agent type"),
        "{error:#}"
    );
    assert!(Config::load(&layout, Some(overlay("security"))).is_ok());
}

#[test]
fn disabling_the_reasoning_guard_preserves_larger_output_recovery() {
    // A reasoning model can spend its whole budget thinking and return nothing with finish_reason=length.
    let server = serve(vec![Reply::Cut, Reply::Text("finished")]);
    let mut e = env(&server.url, "ask");
    e.config.reasoning_guard.enabled = false;
    // An explicit budget below the context window leaves room to double on retry.
    e.config.harness.max_tokens = 8192;
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    let report = a.turn("do the thing", &mut ui).unwrap();
    assert_eq!(
        report.text, "finished",
        "the cut reply must not end the turn"
    );
    let requests = server.requests.lock();
    let (first, second) = (
        requests[0]["max_tokens"].as_u64().unwrap(),
        requests[1]["max_tokens"].as_u64().unwrap(),
    );
    assert_eq!(second, first * 2, "the retry asks for a bigger budget");
    assert!(
        ui.notices.iter().any(|n| n.contains("output limit")),
        "{:?}",
        ui.notices
    );
}

#[test]
fn reasoning_guard_counts_successful_output_limit_recovery() {
    let server = serve(vec![Reply::Cut, Reply::Text("finished")]);
    let e = env(&server.url, "ask");
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    let report = a.turn("do the thing", &mut ui).unwrap();
    assert_eq!(report.text, "finished");
    assert_eq!(a.metrics().reasoning_interceptions, 1);
    assert_eq!(a.metrics().truncated_recoveries, 1);
    assert_eq!(server.requests.lock().len(), 2);
}

#[test]
fn default_investigation_uses_configured_output_budget_without_retry() {
    let server = serve(vec![Reply::Text("Located the relevant code.")]);
    let mut e = env(&server.url, "ask");
    e.config.harness.max_tokens = 8192;
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    let report = a.turn("Locate the relevant code", &mut ui).unwrap();
    assert_eq!(report.text, "Located the relevant code.");
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["max_tokens"], e.config.harness.max_tokens);
    assert!(!ui.notices.iter().any(|n| n.contains("output limit")));
}

#[test]
fn the_same_failing_call_repeated_three_times_is_called_out_to_the_model() {
    let missing = || Reply::Call("read", json!({"path": "missing.txt"}));
    let server = serve(vec![
        missing(),
        missing(),
        missing(),
        Reply::Text("giving up"),
    ]);
    let e = env(&server.url, "ask");
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let report = a
        .turn("read missing.txt", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.text, "giving up");
    let requests = server.requests.lock();
    assert!(
        !requests[2]["messages"]
            .to_string()
            .contains("failed 3 times"),
        "not before the third failure"
    );
    assert!(
        requests[3]["messages"]
            .to_string()
            .contains("failed 3 times with the same arguments"),
        "{}",
        requests[3]["messages"]
    );
}

#[test]
fn malformed_read_gets_immediate_guidance_and_corrected_read_continues() {
    let server = serve(vec![
        Reply::Call("read", json!({"path":"f.txt:1- five"})),
        Reply::Call("read", json!({"path":"f.txt:1-2"})),
        Reply::Text("Recovered and read the requested lines."),
    ]);
    let e = env(&server.url, "ask");
    std::fs::write(e.project.path().join("f.txt"), "one\ntwo\n").unwrap();
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let report = a
        .turn(
            "Read the first two lines of f.txt",
            &mut Recorder::default(),
        )
        .unwrap();
    assert_eq!(report.text, "Recovered and read the requested lines.");
    let requests = server.requests.lock();
    let feedback = requests[1]["messages"].to_string();
    assert!(feedback.contains("Invalid line selector"), "{feedback}");
    assert!(feedback.contains("Read recovery"), "{feedback}");
    assert!(feedback.contains("offset/limit"), "{feedback}");
    let evidence = requests[2]["messages"].to_string();
    assert!(evidence.contains("f.txt#"), "{evidence}");
}

#[test]
fn reasoning_gate_recovers_into_real_work_without_raising_the_token_budget() {
    let server = serve(vec![
        Reply::Think {
            reasoning: "r".repeat(256),
            pause: Duration::ZERO,
        },
        Reply::Call("read", json!({"path":"f.txt"})),
        Reply::Text("Evidence received."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.reasoning_guard.max_reasoning_bytes = 256;
    e.config.reasoning_guard.require_plan = false;
    std::fs::write(e.project.path().join("f.txt"), "evidence").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut a = agent_with(&e, None, cancel.clone());
    let mut ui = Recorder::default();
    let report = a.turn("inspect f.txt", &mut ui).unwrap();
    assert_eq!(
        (report.text.as_str(), report.outcome),
        ("Evidence received.", TurnOutcome::Completed)
    );
    assert_eq!(ui.calls, ["read"]);
    assert!(!e.project.path().join("gate-leak.txt").exists());
    assert!(
        !cancel.load(Ordering::SeqCst),
        "request cancellation must not set the user interrupt"
    );
    assert_eq!(a.metrics().reasoning_interceptions, 1);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["max_tokens"], requests[1]["max_tokens"]);
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Take the next small, useful tool action now")
    );
    assert_eq!(
        requests[0]["messages"][0], requests[1]["messages"][0],
        "keep the system prompt cache stable"
    );
}

#[test]
fn stream_repetition_recovers_without_todo_or_replaying_failed_content() {
    for thinking in [false, true] {
        let repeated = "REPLAY_LOOP_MARKER: I must inspect the same evidence again. ".repeat(60);
        let bad = if thinking {
            Reply::Think {
                reasoning: repeated,
                pause: Duration::ZERO,
            }
        } else {
            Reply::Say(repeated)
        };
        let server = serve(vec![
            bad,
            Reply::Call("read", json!({"path":"f.txt"})),
            Reply::Text("Recovered."),
        ]);
        let e = env(&server.url, "auto");
        assert!(!e.config.reasoning_guard.require_plan);
        std::fs::write(e.project.path().join("f.txt"), "evidence").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut a = agent_with(&e, None, cancel.clone());
        let mut ui = Recorder::default();
        let report = a.turn("Inspect f.txt", &mut ui).unwrap();
        assert_eq!(
            (report.text.as_str(), report.outcome),
            ("Recovered.", TurnOutcome::Completed)
        );
        assert_eq!(ui.calls, ["read"]);
        assert!(!cancel.load(Ordering::SeqCst));
        assert!(!e.project.path().join("gate-leak.txt").exists());
        let requests = server.requests.lock();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["max_tokens"], requests[1]["max_tokens"]);
        assert!(
            !requests[1]["messages"]
                .to_string()
                .contains("REPLAY_LOOP_MARKER")
        );
        assert!(!requests[1]["messages"].to_string().contains("call todo"));
    }
}

#[test]
fn repeated_stream_hits_stop_even_with_unlimited_steps() {
    let bad = || Reply::Say("REPLAY_LOOP_MARKER: I keep repeating the same answer. ".repeat(60));
    let server = serve(vec![bad(), bad()]);
    let mut e = env(&server.url, "auto");
    e.config.harness.max_steps = 0;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("Explain the evidence", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert_eq!(server.requests.lock().len(), 2);
    assert_eq!(a.metrics().reasoning_interceptions, 2);
}

#[test]
fn reasoning_gate_requires_a_todo_plan_when_opted_in() {
    let server = serve(vec![
        Reply::Think {
            reasoning: "r".repeat(256),
            pause: Duration::ZERO,
        },
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"Read f.txt","investigation":true}]}),
        ),
        Reply::Call("read", json!({"path":"f.txt"})),
        Reply::Call(
            "todo",
            json!({"op":"done","finding":"f.txt holds evidence","evidence":["f.txt:1"]}),
        ),
        Reply::Text("Evidence received."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.reasoning_guard.max_reasoning_bytes = 256;
    assert!(!e.config.reasoning_guard.require_plan);
    e.config.reasoning_guard.require_plan = true;
    std::fs::write(e.project.path().join("f.txt"), "evidence").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("inspect f.txt", &mut ui).unwrap();
    assert_eq!(report.text, "Evidence received.");
    assert_eq!(ui.calls, ["todo", "read", "todo"]);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 5);
    let retry = requests[1]["messages"].to_string();
    assert!(retry.contains("call todo with 1-3 concrete"));
    let offered: Vec<&str> = requests[1]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert_eq!(
        offered,
        ["finish", "todo"],
        "only todo and finish are offered until a plan exists"
    );
}

#[test]
fn reasoning_gate_bounds_silent_reasoning_and_stops_incomplete_after_one_retry() {
    let think = || Reply::Think {
        reasoning: "thinking".into(),
        pause: Duration::from_millis(1800),
    };
    let server = serve(vec![think(), think(), Reply::Text("must not request")]);
    let mut e = env(&server.url, "auto");
    e.config.reasoning_guard.max_pre_action_secs = 1;
    let cancel = Arc::new(AtomicBool::new(false));
    let mut a = agent_with(&e, None, cancel.clone());
    let mut ui = Recorder::default();
    let started = Instant::now();
    let report = a.turn("act", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert!(!report.interrupted && !cancel.load(Ordering::SeqCst));
    assert_eq!(server.requests.lock().len(), 2);
    assert!(ui.calls.is_empty() && !e.project.path().join("gate-leak.txt").exists());
    assert!(
        ui.notices
            .iter()
            .any(|n| n.contains("one recovery attempt"))
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "a silent stream must be cancellable"
    );
}

#[test]
fn reasoning_only_length_stops_share_the_one_recovery_budget_with_stream_interceptions() {
    let server = serve(vec![
        Reply::Cut,
        Reply::Think {
            reasoning: "r".repeat(256),
            pause: Duration::ZERO,
        },
        Reply::Text("must not request"),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.reasoning_guard.max_reasoning_bytes = 256;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("act", &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["max_tokens"], requests[1]["max_tokens"]);
}

#[test]
fn user_cancellation_during_reasoning_does_not_trigger_a_gate_retry() {
    let server = serve(vec![Reply::Think {
        reasoning: "thinking".into(),
        pause: Duration::from_millis(1800),
    }]);
    let e = env(&server.url, "auto");
    let cancel = Arc::new(AtomicBool::new(false));
    struct CancelUi(Arc<AtomicBool>);
    impl Ui for CancelUi {
        fn reasoning_delta(&mut self, _text: &str) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let mut a = agent_with(&e, None, cancel.clone());
    let report = a.turn("act", &mut CancelUi(cancel)).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert_eq!(server.requests.lock().len(), 1);
    assert_eq!(a.metrics().reasoning_interceptions, 0);
}

#[test]
fn child_reasoning_uses_the_same_gate_and_bounded_recovery() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"agent":"worker","task":"inspect","solution_space":"s"}),
        ),
        Reply::Text("received"),
    ]);
    let think = || Reply::Think {
        reasoning: "r".repeat(256),
        pause: Duration::ZERO,
    };
    let side = serve(vec![think(), think(), Reply::Text("must not request")]);
    let mut e = env(&main.url, "auto");
    e.config.reasoning_guard.max_reasoning_bytes = 256;
    with_subagent_role(&mut e, &side.url);
    run_turn(&e, &mut Recorder::default());
    assert_eq!(side.requests.lock().len(), 2);
    assert!(task_result(&main).contains("one recovery attempt"));
    assert!(!e.project.path().join("gate-leak.txt").exists());
}

#[test]
fn reasoning_guard_settings_load_defaults_and_validate_limits() {
    let e = env("http://127.0.0.1:1/v1", "auto");
    assert!(e.config.reasoning_guard.enabled);
    // 0 means unlimited: no pre-action deadline by default, and 0 remains valid.
    assert_eq!(e.config.reasoning_guard.max_pre_action_secs, 0);
    let overlay = toml::from_str(
        "[reasoning_guard]\nenabled = false\nmax_pre_action_secs = 120\nmax_reasoning_bytes = 4096",
    )
    .unwrap();
    let mut config = Config::load(&e.layout, Some(overlay)).unwrap();
    assert!(!config.reasoning_guard.enabled);
    assert_eq!(config.reasoning_guard.max_reasoning_bytes, 4096);
    config.reasoning_guard.max_pre_action_secs = 0;
    config.validate().unwrap();
    config.reasoning_guard.max_pre_action_secs = 86401;
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_pre_action_secs")
    );
    config.reasoning_guard.max_pre_action_secs = 1;
    config.reasoning_guard.max_reasoning_bytes = 255;
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_reasoning_bytes")
    );
}

#[test]
fn codebase_understanding_is_saved_across_new_sessions_and_invalidated_by_edits() {
    let server = serve(vec![
        Reply::Call("read", json!({"path":"src/router.rs"})),
        Reply::Call(
            "codebase_memory",
            json!({"action":"remember","topic":"request routing","summary":"Router forwards requests into storage.","sources":["src/router.rs"]}),
        ),
        Reply::Text("understood"),
        Reply::Text("reused"),
        Reply::Text("refreshed"),
    ]);
    let mut e = env(&server.url, "ask");
    e.config.codebase.ambient_context = true;
    std::fs::create_dir(e.project.path().join("src")).unwrap();
    std::fs::write(
        e.project.path().join("src/router.rs"),
        "//! Routes requests into storage.\npub fn route_request() {}\n",
    )
    .unwrap();
    let mut first = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let first_id = first.session().id().to_owned();
    let mut ui = Recorder::default();
    assert_eq!(
        first
            .turn("understand request routing", &mut ui)
            .unwrap()
            .text,
        "understood"
    );
    assert!(
        ui.approvals.is_empty(),
        "saving harness knowledge must not ask to mutate project files"
    );
    drop(first);
    let mut second = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    assert_ne!(first_id, second.session().id());
    assert_eq!(
        second
            .turn("change request routing", &mut Recorder::default())
            .unwrap()
            .text,
        "reused"
    );
    std::fs::write(
        e.project.path().join("src/router.rs"),
        "pub fn new_routing_path() {}\n",
    )
    .unwrap();
    assert_eq!(
        second
            .turn("change request routing again", &mut Recorder::default())
            .unwrap()
            .text,
        "refreshed"
    );
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 5);
    assert!(
        requests[0]["messages"]
            .to_string()
            .contains("route_request"),
        "automatic index should be available before any model tool call"
    );
    let reused = requests[3]["messages"].to_string();
    assert!(
        reused.contains("Router forwards requests into storage.") && reused.contains(&first_id),
        "{reused}"
    );
    let persistent = conversation_messages(&requests[4]);
    let newest = persistent
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap()
        .to_string();
    assert!(
        newest.contains("needs refresh") && newest.contains("new_routing_path"),
        "{newest}"
    );
    assert!(
        !newest.contains("Router forwards requests into storage.")
            && !newest.contains("route_request")
    );
    for pair in requests[..3].windows(2) {
        assert_eq!(
            pair[0]["messages"][0], pair[1]["messages"][0],
            "saved context must not rewrite the system prompt mid-turn"
        );
    }
}

#[test]
fn independent_child_follows_the_ambient_codebase_context_setting() {
    for ambient in [true, false] {
        let main = serve(vec![
            Reply::Call(
                "task",
                json!({"task":"inspect request router","solution_space":"s"}),
            ),
            Reply::Text("received"),
        ]);
        let side = serve(vec![Reply::Text("located")]);
        let mut e = env(&main.url, "auto");
        e.config.codebase.ambient_context = ambient;
        std::fs::write(
            e.project.path().join("router.rs"),
            "pub fn request_router() {}\n",
        )
        .unwrap();
        with_subagent_role(&mut e, &side.url);
        run_turn(&e, &mut Recorder::default());
        let child = side.requests.lock()[0].clone();
        assert_eq!(prompt_of(&child), "inspect request router");
        let messages = child["messages"].to_string();
        assert_eq!(
            messages.contains("request_router")
                && messages.contains("Saved codebase understanding"),
            ambient,
            "ambient_context = {ambient}: {messages}"
        );
    }
}

#[test]
fn disabling_codebase_context_preserves_session_only_behavior() {
    let server = serve(vec![Reply::Text("done")]);
    let mut e = env(&server.url, "auto");
    e.config.codebase.enabled = false;
    std::fs::write(
        e.project.path().join("router.rs"),
        "pub fn request_router() {}\n",
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    assert!(
        !a.available_tool_names()
            .iter()
            .any(|name| name == "codebase_memory")
    );
    a.turn("inspect router", &mut Recorder::default()).unwrap();
    assert!(
        !server.requests.lock()[0]["messages"]
            .to_string()
            .contains("Saved codebase understanding")
    );
    assert!(!e.layout.state_dir.join("codebase.sqlite3").exists());
}

#[test]
fn malformed_edit_feedback_reaches_the_next_parent_and_child_request() {
    let bad = || {
        Reply::Call(
            "edit",
            json!({"input":"[marker.txt#1234]\nPUT 1.=:\n+fixed\n"}),
        )
    };
    let assert_feedback = |request: &Value| {
        let messages = request["messages"].as_array().unwrap();
        let feedback = messages
            .iter()
            .find(|m| {
                m["role"] == "tool"
                    && m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("range end:"))
            })
            .expect("the failed tool result must be in the next request");
        let text = feedback["content"].as_str().unwrap();
        assert!(text.contains("PUT 1.=1:"), "{text}");
        assert!(
            text.contains("fix the patch target, not the file"),
            "{text}"
        );
    };

    let parent = serve(vec![
        bad(),
        Reply::Text("Need to correct the patch target."),
    ]);
    let e = env(&parent.url, "auto");
    std::fs::write(e.project.path().join("marker.txt"), "original\n").unwrap();
    run_turn(&e, &mut Recorder::default());
    assert_feedback(&parent.requests.lock()[1]);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "original\n"
    );

    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"agent":"worker","task":"fix marker.txt","solution_space":"s"}),
        ),
        Reply::Text("Received the failure diagnosis."),
    ]);
    let side = serve(vec![
        bad(),
        Reply::Text("Need to correct the patch target."),
    ]);
    let mut e = env(&main.url, "auto");
    std::fs::write(e.project.path().join("marker.txt"), "original\n").unwrap();
    with_subagent_role(&mut e, &side.url);
    run_turn(&e, &mut Recorder::default());
    assert_feedback(&side.requests.lock()[1]);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "original\n"
    );
}

#[test]
fn rereads_do_not_allow_identical_failed_edits_to_loop_forever() {
    let bad = || {
        Reply::Call(
            "edit",
            json!({"input":"[marker.txt#1234]\nPUT 1.=1:\n+x\nunprefixed\n"}),
        )
    };
    let reread = || Reply::Call("read", json!({"path":"marker.txt"}));
    let server = serve(vec![
        bad(),
        reread(),
        bad(),
        reread(),
        bad(),
        reread(),
        bad(),
        Reply::Text("false success"),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    e.config.reliability.enabled = false; // Exercise the existing identical-call guard independently of the earlier contract-round gate.
    std::fs::write(e.project.path().join("marker.txt"), "original\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a
        .turn("fix marker", &mut ui)
        .expect("a stuck turn is reported, not raised as an error");
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    assert_eq!(server.requests.lock().len(), 8);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "original\n"
    );
    let messages = a.session().messages();
    assert_eq!(
        messages.iter().filter(|m| m["role"] == "tool").count(),
        7,
        "every tool result is persisted"
    );
}

#[test]
fn automatic_focused_checks_keep_pumping_the_ui() {
    if skip_without_sandbox() {
        return;
    }
    struct ResponsiveUi {
        heartbeats: usize,
    }
    impl Ui for ResponsiveUi {
        fn heartbeat(&mut self) {
            self.heartbeats += 1;
        }
    }
    let server = serve(vec![
        Reply::Call("write", json!({"path":"marker.txt","content":"ok"})),
        Reply::Text("done"),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "slow-check".into(),
        command: "sleep 0.4".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = ResponsiveUi { heartbeats: 0 };
    a.turn("create marker", &mut ui).unwrap();
    assert!(
        ui.heartbeats >= 3,
        "automatic verification must pump input while the check runs; got {} heartbeats",
        ui.heartbeats
    );
    assert_eq!(
        server.requests.lock().len(),
        2,
        "successful checks must not start a repair request; the final diff review rides on the request after the write"
    );
}

#[test]
fn cancellation_during_automatic_checks_stops_without_a_repair_turn() {
    if skip_without_sandbox() {
        return;
    }
    struct CancellingUi {
        cancel: Arc<AtomicBool>,
        checking: bool,
    }
    impl Ui for CancellingUi {
        fn tool_call(&mut self, _: &str, summary: &str) {
            self.checking |= summary.starts_with("Verify ");
        }
        fn heartbeat(&mut self) {
            if self.checking {
                self.cancel.store(true, Ordering::SeqCst);
                dial::platform::wake::signal();
            }
        }
    }
    let server = serve(vec![
        Reply::Call("write", json!({"path":"marker.txt","content":"ok"})),
        Reply::Text("done"),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "slow-check".into(),
        command: "sleep 5".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let mut a = agent_with(&e, None, cancel.clone());
    let mut ui = CancellingUi {
        cancel,
        checking: false,
    };
    let start = std::time::Instant::now();
    let report = a.turn("create marker", &mut ui).unwrap();
    assert!(report.interrupted);
    assert_eq!(report.outcome, TurnOutcome::Interrupted);
    assert!(report.text.contains("Candidate work summary"));
    assert!(report.text.ends_with("done"));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "cancelled checks must end promptly"
    );
    assert_eq!(
        server.requests.lock().len(),
        2,
        "cancellation must not start a repair request"
    );
}

#[test]
fn tool_groups_preserve_order_and_mutations_are_barriers() {
    let server = serve(vec![
        Reply::Calls(vec![
            ("read", json!({"path":"a.txt"})),
            ("read", json!({"path":"b.txt"})),
            ("write", json!({"path":"a.txt","content":"after"})),
            ("read", json!({"path":"a.txt"})),
        ]),
        Reply::Text("done"),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("a.txt"), "before").unwrap();
    std::fs::write(e.project.path().join("b.txt"), "second").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("read, update, reread", &mut Recorder::default())
        .unwrap();
    let requests = server.requests.lock();
    let results: Vec<&Value> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(
        results
            .iter()
            .map(|m| m["tool_call_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["call_0", "call_1", "call_2", "call_3"]
    );
    let first_result = results[0]["content"].as_str().unwrap();
    assert!(first_result.contains("before") && !first_result.contains("Superseded observation"));
    let persisted = a.session().messages();
    let first_uri =
        persisted.iter().find(|m| m["role"] == "tool").unwrap()["dial_evidence"]["artifact"]
            .as_str()
            .unwrap();
    let artifacts =
        dial::storage::artifact::ArtifactStore::new(&e.layout.state_dir.join("artifacts")).unwrap();
    let first_evidence = artifacts.retrieve(first_uri, 1, 100).unwrap();
    assert!(
        first_evidence.contains("before") && !first_evidence.contains("after"),
        "first read happened before mutation: {first_evidence}"
    );
    let persisted = a.session().messages();
    let stored_results: Vec<&Value> = persisted.iter().filter(|m| m["role"] == "tool").collect();
    let latest_uri = stored_results[3]["dial_evidence"]["artifact"]
        .as_str()
        .unwrap();
    let latest_evidence = artifacts.retrieve(latest_uri, 1, 100).unwrap();
    assert!(
        latest_evidence.contains("after") && !latest_evidence.contains("before"),
        "reread happened after mutation: {latest_evidence}"
    );
    assert_eq!(
        results[2]["content"], stored_results[2]["content"],
        "mutation evidence is never superseded"
    );
    assert!(results[1]["content"].as_str().unwrap().contains("second"));
    assert!(results[3]["content"].as_str().unwrap().contains("after"));
    assert_eq!(
        requests[0]["messages"][0], requests[1]["messages"][0],
        "system prefix must stay byte-identical"
    );
}

#[test]
fn interrupt_repairs_all_unanswered_calls_and_resume_keeps_them() {
    struct Stop(Arc<AtomicBool>);
    impl Ui for Stop {
        fn tool_result(&mut self, _: &str, _: &ToolOutput) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let server = serve(vec![Reply::Calls(vec![
        ("read", json!({"path":"a.txt"})),
        ("write", json!({"path":"never.txt","content":"no"})),
        ("read", json!({"path":"a.txt"})),
    ])]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("a.txt"), "hello").unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let mut a = agent_with(&e, None, flag.clone());
    assert!(a.turn("work", &mut Stop(flag)).unwrap().interrupted);
    assert!(!e.project.path().join("never.txt").exists());
    let reopened = Session::read(a.session().path().unwrap()).unwrap();
    let messages = reopened.messages();
    let results: Vec<&Value> = messages.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(
        results
            .iter()
            .map(|m| m["tool_call_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["call_0", "call_1", "call_2"]
    );
    assert!(
        results[1]["content"]
            .as_str()
            .unwrap()
            .contains("interrupted")
    );
}

#[test]
fn crashed_session_is_repaired_on_open() {
    let server = serve(vec![]);
    let e = env(&server.url, "auto");
    let mut session = Session::create(
        &e.layout,
        SessionMeta {
            model: "scripted".into(),
        },
    )
    .unwrap();
    session
        .append(json!({"role":"assistant","tool_calls":[
            {"id":"a","type":"function","function":{"name":"read","arguments":"{}"}},
            {"id":"b","type":"function","function":{"name":"read","arguments":"{}"}}
        ]}))
        .unwrap();
    session
        .append(json!({"role":"tool","tool_call_id":"a","content":"done"}))
        .unwrap();
    let path = session.path().unwrap().to_owned();
    drop(session);
    let mut resumed = Session::open(&path).unwrap();
    assert_eq!(resumed.messages().last().unwrap()["tool_call_id"], "b");
    assert_eq!(resumed.repair_incomplete_tools().unwrap(), 0);
}

#[test]
fn steering_during_reasoning_cancels_generation_without_interrupting_the_turn() {
    #[derive(Default)]
    struct SteeringUi {
        ready: bool,
        sent: bool,
        cancelled_old: bool,
    }
    impl Ui for SteeringUi {
        fn reasoning_delta(&mut self, _: &str) {
            self.ready = true;
        }
        fn notice(&mut self, text: &str) {
            self.cancelled_old |= text.contains("Stopped the old generation");
        }
        fn take_steering(&mut self) -> Vec<String> {
            if self.ready && !self.sent {
                self.sent = true;
                vec!["Stop the old task. Answer what is two plus two.".into()]
            } else {
                vec![]
            }
        }
    }
    let server = serve(vec![
        Reply::Think {
            reasoning: "I will keep working on the old task.".into(),
            pause: Duration::from_millis(500),
        },
        Reply::Text("Four."),
    ]);
    let e = env(&server.url, "auto");
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut a = agent_with(&e, None, interrupt.clone());
    let mut ui = SteeringUi::default();
    let report = a.turn("Create a file", &mut ui).unwrap();
    assert!(ui.cancelled_old);
    assert!(!interrupt.load(Ordering::SeqCst));
    assert!(!report.interrupted);
    assert_eq!(report.text, "Four.");
    assert!(!e.project.path().join("gate-leak.txt").exists());
    assert_eq!(server.requests.lock().len(), 2);
}

#[test]
fn steering_arriving_with_a_response_prevents_stale_tool_execution() {
    #[derive(Default)]
    struct SteeringUi {
        ready: bool,
        sent: bool,
    }
    impl Ui for SteeringUi {
        fn usage(&mut self, _: Option<u64>, _: Option<u64>, _: f64) {
            self.ready = true;
        }
        fn take_steering(&mut self) -> Vec<String> {
            if self.ready && !self.sent {
                self.sent = true;
                vec!["Stop editing. Answer my new question: what is two plus two?".into()]
            } else {
                vec![]
            }
        }
    }
    let server = serve(vec![
        Reply::Call(
            "write",
            json!({"path":"stale.txt","content":"must not run"}),
        ),
        Reply::Text("Four."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("Create stale.txt", &mut SteeringUi::default())
        .unwrap();
    assert!(
        !e.project.path().join("stale.txt").exists(),
        "the old response must be discarded before approval or execution"
    );
    assert_eq!(report.text, "Four.");
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("what is two plus two?")
    );
    assert!(!requests[1]["messages"].to_string().contains("must not run"));
}

#[test]
fn steering_between_tools_skips_unstarted_calls_and_keeps_completed_results() {
    #[derive(Default)]
    struct SteeringUi {
        ready: bool,
        sent: bool,
    }
    impl Ui for SteeringUi {
        fn tool_result(&mut self, name: &str, out: &ToolOutput) {
            if name == "write" && !out.is_error {
                self.ready = true;
            }
        }
        fn take_steering(&mut self) -> Vec<String> {
            if self.ready && !self.sent {
                self.sent = true;
                vec!["Stop writing files and ask me what to do next.".into()]
            } else {
                vec![]
            }
        }
    }
    let server = serve(vec![
        Reply::Calls(vec![
            ("write", json!({"path":"first.txt","content":"completed"})),
            (
                "write",
                json!({"path":"stale.txt","content":"must not run"}),
            ),
        ]),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Stopped after the first file","reason":"What should I do next?"}),
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("Create first.txt and stale.txt", &mut SteeringUi::default())
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("first.txt")).unwrap(),
        "completed"
    );
    assert!(!e.project.path().join("stale.txt").exists());
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    let requests = server.requests.lock();
    let results: Vec<_> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(
        results.len(),
        2,
        "every old call must have an honest completed or skipped result"
    );
    assert!(
        results[1]["content"]
            .as_str()
            .unwrap()
            .contains("not executed")
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("latest user instruction")
    );
}

#[test]
fn child_repeated_narration_gets_loop_feedback_with_different_calls() {
    const NARRATION: &str = "I can see the current version. Let me verify the current state and check the UI header integration.";
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"agent":"scout", "task":"Inspect three sources", "solution_space":"s"}),
        ),
        Reply::Text("done"),
    ]);
    let mut script = (0..3)
        .map(|n| Reply::NarratedCall(NARRATION, "read", json!({"path":format!("source-{n}.txt")})))
        .collect::<Vec<_>>();
    script.push(Reply::Text("The sources are inspected."));
    let side = serve(script);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    for n in 0..3 {
        std::fs::write(
            e.project.path().join(format!("source-{n}.txt")),
            format!("evidence {n}"),
        )
        .unwrap();
    }
    run_turn(&e, &mut Recorder::default());
    assert!(task_result(&main).contains("The sources are inspected."));
    assert!(
        side.requests.lock()[3]["messages"]
            .to_string()
            .contains("[Loop check from Dial]")
    );
}

#[test]
fn repeated_narration_gets_loop_feedback_even_with_different_tool_calls() {
    const NARRATION: &str = "I can see the current version. Let me verify the current state and check the UI header integration.";
    let mut script = (0..3)
        .map(|n| Reply::NarratedCall(NARRATION, "read", json!({"path":format!("source-{n}.txt")})))
        .collect::<Vec<_>>();
    script.push(Reply::Text("The sources are inspected."));
    let server = serve(script);
    let e = env(&server.url, "auto");
    for n in 0..3 {
        std::fs::write(
            e.project.path().join(format!("source-{n}.txt")),
            format!("evidence {n}"),
        )
        .unwrap();
    }
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Inspect three sources", &mut Recorder::default())
        .unwrap();
    assert!(
        server.requests.lock()[3]["messages"]
            .to_string()
            .contains("[Loop check from Dial]")
    );
}

#[test]
fn steering_after_final_text_extends_the_turn() {
    struct Steer {
        ready: bool,
        sent: bool,
    }
    impl Ui for Steer {
        fn text_delta(&mut self, _: &str) {
            self.ready = true;
        }
        fn take_steering(&mut self) -> Vec<String> {
            if self.ready && !self.sent {
                self.sent = true;
                vec!["Also explain the risks.".into()]
            } else {
                vec![]
            }
        }
    }
    let server = serve(vec![Reply::Text("first"), Reply::Text("the risks")]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    assert_eq!(
        a.turn(
            "explain",
            &mut Steer {
                ready: false,
                sent: false
            }
        )
        .unwrap()
        .text,
        "the risks"
    );
    let requests = server.requests.lock();
    assert!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "user" && m["content"] == "Also explain the risks.")
    );
}

#[test]
fn plan_mode_refuses_mutations_and_approved_exit_continues() {
    let server = serve(vec![
        Reply::Call("write", json!({"path":"never.txt","content":"no"})),
        Reply::Call("exit_plan", json!({"plan":"Create approved.txt."})),
        Reply::Call("write", json!({"path":"approved.txt","content":"yes"})),
        Reply::Text("done"),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.set_plan_mode(true);
    let mut ui = Recorder::default();
    a.turn("plan", &mut ui).unwrap();
    assert!(!a.plan_mode());
    assert!(!e.project.path().join("never.txt").exists());
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("approved.txt")).unwrap(),
        "yes"
    );
    assert_eq!(ui.approvals, ["exit_plan"]);
    let requests = server.requests.lock();
    assert!(
        !requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "write")
    );
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("Approved plan")
    );
}

#[test]
fn rejected_plan_stays_read_only_and_returns_reason() {
    struct Reject;
    impl Ui for Reject {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            Decision::Deny
        }
        fn approval_reason(&mut self) -> Option<String> {
            Some("Do not alter the database.".into())
        }
    }
    let server = serve(vec![
        Reply::Call("exit_plan", json!({"plan":"Change database"})),
        Reply::Text("revised"),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.set_plan_mode(true);
    a.turn("plan", &mut Reject).unwrap();
    assert!(a.plan_mode());
    assert!(
        server.requests.lock()[1]["messages"]
            .to_string()
            .contains("Do not alter the database.")
    );
}

#[test]
fn shell_inspection_skips_prompt_but_destructive_command_asks() {
    let server = serve(vec![
        Reply::Call("bash", json!({"command":"pwd"})),
        Reply::Call("bash", json!({"command":"rm -rf x"})),
        Reply::Text("done"),
    ]);
    let e = env(&server.url, "ask");
    std::fs::write(e.project.path().join("x"), "keep").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder {
        deny: true,
        ..Recorder::default()
    };
    a.turn("inspect", &mut ui).unwrap();
    assert_eq!(ui.approvals, ["bash"]);
    assert!(ui.summaries[0].starts_with("rm -rf x"));
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("x")).unwrap(),
        "keep"
    );
}

#[test]
fn always_allow_shell_permission_is_not_a_blanket_bash_grant() {
    if skip_without_sandbox() {
        return;
    }
    struct AlwaysOnce {
        prompts: Vec<String>,
    }
    impl Ui for AlwaysOnce {
        fn approve(&mut self, _: &str, summary: &str) -> Decision {
            self.prompts.push(summary.into());
            if self.prompts.len() == 1 {
                Decision::AllowAlways
            } else {
                Decision::Deny
            }
        }
    }
    let server = serve(vec![
        Reply::Call("bash", json!({"command":"touch one"})),
        Reply::Call("bash", json!({"command":"touch one"})),
        Reply::Call("bash", json!({"command":"rm -rf one"})),
        Reply::Text("done"),
    ]);
    let e = env(&server.url, "ask");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = AlwaysOnce { prompts: vec![] };
    a.turn("create a marker", &mut ui).unwrap();
    assert_eq!(ui.prompts.len(), 2, "{:?}", ui.prompts);
    assert!(ui.prompts[1].starts_with("rm -rf one"));
    assert!(e.project.path().join("one").is_file());
}

#[test]
fn image_read_attaches_validated_bytes_without_persisting_base64() {
    let server = serve(vec![
        Reply::Call("read", json!({"path":"tiny.png"})),
        Reply::Text("a pixel"),
    ]);
    let e = env(&server.url, "auto");
    let png = base64::Engine::decode(&base64::engine::general_purpose::STANDARD,
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=").unwrap();
    std::fs::write(e.project.path().join("tiny.png"), png).unwrap();
    let backend = make_backend(&e.config).unwrap();
    let session = Session::create(
        &e.layout,
        SessionMeta {
            model: "scripted".into(),
        },
    )
    .unwrap();
    let mut a = Agent::interactive(
        &e.config,
        backend,
        e.layout.clone(),
        session,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut ui = Recorder::default();
    a.turn("read the image", &mut ui).unwrap();
    assert_eq!(ui.images.len(), 1);
    assert_eq!(ui.images[0]["path"], "tiny.png");
    let requests = server.requests.lock();
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("data:image/png;base64,")
    );
    let saved = std::fs::read_to_string(a.session().path().unwrap()).unwrap();
    assert!(saved.contains("sha256"));
    assert!(!saved.contains("data:image/png;base64,"));
}

#[test]
fn user_supplied_external_image_reaches_the_model_with_unknown_capabilities() {
    let server = serve(vec![Reply::Text("image received")]);
    let e = env(&server.url, "auto");
    let outside = tempfile::tempdir().unwrap();
    let image = outside.path().join("Screenshot 1.png");
    let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=";
    std::fs::write(
        &image,
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).unwrap(),
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let ui = &mut Recorder::default();
    a.turn(&format!("Inspect @\"{}\"", image.display()), ui)
        .unwrap();
    assert_eq!(ui.images.len(), 1);
    assert_eq!(ui.images[0]["path"], "Screenshot 1.png");
    let requests = server.requests.lock();
    let user = requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap();
    assert_eq!(
        user["content"][1]["image_url"]["url"],
        format!("data:image/png;base64,{encoded}")
    );
    let saved = std::fs::read_to_string(a.session().path().unwrap()).unwrap();
    assert!(saved.contains("Screenshot 1.png"));
    assert!(!saved.contains(encoded));
}

#[test]
fn loading_browser_exposes_managed_preview_tools_without_starting_a_service() {
    if skip_without_sandbox() {
        return;
    }
    let server = serve(vec![
        Reply::Call("tool_load", json!({"names":["browser"]})),
        Reply::Call("bg", json!({"action":"list"})),
        Reply::Call(
            "bg",
            json!({"action":"start","name":"preview","command":"echo READY; sleep 30"}),
        ),
        Reply::Call(
            "bg",
            json!({"action":"wait","name":"preview","until":"^READY$","timeout":2}),
        ),
        Reply::Call("bg", json!({"action":"kill","name":"preview"})),
        Reply::Text("Temporary preview process stopped."),
        Reply::Text("Temporary preview process stopped; opaque command effects reviewed."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.assistance.compact_tools = true;
    e.config.integrations = vec!["bg".into()];
    // Exercise the local-compute schema budget without starting or contacting a local model.
    e.config.model_roles.insert(
        "small".into(),
        dial::llm::model_config::ModelSelection {
            model: "unused-local-model".into(),
            ..Default::default()
        },
    );
    e.config.assistance.enabled = true;
    e.config.assistance.compact_tools = true;
    e.config.assistance.initial_tool_limit = 1;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a
        .turn("Check a temporary preview process and stop it.", &mut ui)
        .unwrap();
    assert_eq!(
        report.outcome,
        TurnOutcome::Completed,
        "results={:?}; notices={:?}; text={}",
        ui.results,
        ui.notices,
        ui.text
    );
    let requests = server.requests.lock();
    assert!(
        tool_names(&requests[0]).contains(&"bg".into()),
        "process controls are core tools"
    );
    assert!(
        requests[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "bg")
    );
    let results: Vec<_> = ui
        .results
        .iter()
        .filter(|(name, _, _)| name == "bg")
        .collect();
    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|(_, _, error)| !error));
    assert!(
        results[0].1.contains("no background processes"),
        "loading browser starts no server"
    );
    assert!(results[2].1.starts_with("ready:"));
    assert!(results[3].1.contains("killed preview"));
}

#[test]
fn pasted_external_image_attaches_and_rereads_without_a_workspace_copy() {
    let outside = tempfile::tempdir().unwrap();
    let image = outside.path().join("pasted image.png");
    let other = outside.path().join("not supplied.png");
    let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=";
    let pixels =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).unwrap();
    std::fs::write(&image, &pixels).unwrap();
    std::fs::write(&other, &pixels).unwrap();
    let server = serve(vec![
        Reply::Call("read", json!({"path":image})),
        Reply::Call("read", json!({"path":other})),
        Reply::Text("Supplied image inspected; unrelated external file unavailable."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a
        .turn(
            &format!("\"{}\" this is what it looks like", image.display()),
            &mut ui,
        )
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        ui.images.len(),
        2,
        "initial attachment and explicit reread both preview"
    );
    let reads: Vec<_> = ui
        .results
        .iter()
        .filter(|(name, _, _)| name == "read")
        .collect();
    assert!(!reads[0].2, "supplied image may be reread directly");
    assert!(
        reads[1].2,
        "no access to another file with identical pixels"
    );
    let requests = server.requests.lock();
    for request in &requests[..2] {
        assert!(
            request["messages"]
                .to_string()
                .contains(&format!("data:image/png;base64,{encoded}")),
            "the original pixels reach inference"
        );
        assert!(
            request["messages"]
                .to_string()
                .contains("\"detail\":\"high\"")
        );
    }
    assert!(
        !e.project.path().join(".cache").exists(),
        "no shell copy needed"
    );
}

#[test]
fn visual_source_edits_disclose_missing_inspection_without_a_review_round() {
    for explicit_finish in [false, true] {
        let stop = || {
            if explicit_finish {
                Reply::Call(
                    "finish",
                    json!({"outcome":"completed","summary":"style updated"}),
                )
            } else {
                Reply::Text("style updated")
            }
        };
        let server = serve(vec![
            Reply::Call(
                "write",
                json!({"path":"style.css","content":"body { color: green; }\n"}),
            ),
            stop(),
        ]);
        let mut e = env(&server.url, "auto");
        e.config.verification.enabled = false;
        let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let mut ui = Recorder::default();
        let report = a.turn("Add the requested rule.", &mut ui).unwrap();
        assert_eq!(
            std::fs::read_to_string(e.project.path().join("style.css")).unwrap(),
            "body { color: green; }\n"
        );
        let requests = server.requests.lock();
        assert_eq!(requests.len(), 2, "no visual review round is added");
        assert!(report.text.contains("Visual inspection was not completed"));
        assert!(
            ui.text.contains("Visual inspection was not completed"),
            "the limitation reaches the actual streamed UI"
        );
        assert_eq!(report.outcome, dial::agent::TurnOutcome::Completed);
        if explicit_finish {
            let finishes: Vec<_> = ui
                .results
                .iter()
                .filter(|(name, _, _)| name == "finish")
                .collect();
            assert_eq!(finishes.len(), 1, "the first completion is accepted");
            assert!(!finishes[0].2, "a completion is not a tool failure");
            assert_eq!(
                a.session().control_state()["reliability"]["stats"]["runtime_errors"],
                0
            );
        }
    }
}

#[test]
fn git_with_configured_executables_requires_approval() {
    let server = serve(vec![
        Reply::Call("bash", json!({"command":"git diff"})),
        Reply::Text("denied"),
    ]);
    let e = env(&server.url, "ask");
    std::fs::create_dir(e.project.path().join(".git")).unwrap();
    std::fs::write(
        e.project.path().join(".git/config"),
        "[diff]\nexternal = touch escaped\n[core]\nfsmonitor = touch escaped\n",
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder {
        deny: true,
        ..Recorder::default()
    };
    a.turn("inspect changes", &mut ui).unwrap();
    assert_eq!(ui.approvals, ["bash"]);
    assert!(!e.project.path().join("escaped").exists());
    assert!(
        server.requests.lock()[1]["messages"]
            .to_string()
            .contains("denied by the user")
    );
}

#[test]
fn failed_focused_check_repairs_boundedly_and_never_reports_success() {
    let server = serve(vec![
        Reply::Call("write", json!({"path":"marker.txt","content":"bad"})),
        Reply::Text("done"),
        Reply::Call("write", json!({"path":"marker.txt","content":"fixed"})),
        Reply::Text("fixed"),
    ]);
    let mut e = env(&server.url, "auto");
    // Zero repair attempts selects the bounded default; this check never passes, so pin the count explicitly.
    e.config.verification.repair_attempts = 1;
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "always-fails".into(),
        command: "false".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("create marker", &mut ui).unwrap();
    assert!(report.text.contains("Candidate work summary"));
    assert!(report.text.ends_with("fixed"));
    assert_eq!(
        report.outcome,
        TurnOutcome::Failed,
        "A failed final check must not become a successful turn"
    );
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "fixed"
    );
    assert!(
        ui.notices
            .iter()
            .any(|notice| notice.contains("verification still fails"))
    );
    let rollback = a.checkpoints().unwrap().rollback(1).unwrap();
    assert_eq!(rollback.removed, ["marker.txt"]);
    assert!(!e.project.path().join("marker.txt").exists());
}

#[cfg(unix)]
#[test]
fn automatic_verification_prerequisite_failures_do_not_request_code_repairs() {
    if skip_without_sandbox() {
        return;
    }
    for (name, command, status) in [
        (
            "python AST: probe",
            "printf 'No module named pytest\\n'; exit 1",
            "environment_unavailable",
        ),
        ("empty-suite", "printf 'Ran 0 tests\\n'", "unverified"),
    ] {
        let server = serve(vec![
            Reply::Call(
                "write",
                json!({"path":"marker.txt","content":"original work"}),
            ),
            Reply::Text("Marker written."),
            Reply::Call(
                "write",
                json!({"path":"marker.txt","content":"unrequested repair"}),
            ),
        ]);
        let mut e = env(&server.url, "auto");
        e.config.verification.automatic_checks = true;
        e.config.verification.commands.push(VerificationCommand {
            name: name.into(),
            command: command.into(),
            file_extensions: vec![],
            timeout_secs: 5,
        });
        let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let report = agent
            .turn("Create marker", &mut Recorder::default())
            .unwrap();
        assert_eq!(report.outcome, TurnOutcome::Failed);
        assert!(report.text.contains("Candidate work summary"));
        assert!(report.text.contains("Marker written."));
        assert_eq!(
            server.requests.lock().len(),
            2,
            "{status} must not prompt a code repair"
        );
        assert_eq!(agent.metrics().repair_attempts, 0);
        assert_eq!(
            agent.session().control_state()["reliability"]["verification_results"][0]["status"],
            status
        );
        assert_eq!(
            std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
            "original work"
        );
    }
}

#[test]
fn automatic_verification_repairs_share_the_turn_request_budget() {
    if skip_without_sandbox() {
        return;
    }
    for max_steps in [2, 3] {
        let server = serve(vec![
            Reply::Call("write", json!({"path":"marker.txt","content":"bad"})),
            Reply::Text("Marker written."),
            Reply::Text("The check still fails."),
            Reply::Call(
                "write",
                json!({"path":"marker.txt","content":"beyond budget"}),
            ),
        ]);
        let mut e = env(&server.url, "auto");
        e.config.harness.max_steps = max_steps;
        e.config.verification.automatic_checks = true;
        e.config.verification.commands.push(VerificationCommand {
            name: "code-failure".into(),
            command: "false".into(),
            file_extensions: vec![],
            timeout_secs: 5,
        });
        let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let report = agent
            .turn("Create marker", &mut Recorder::default())
            .unwrap();
        assert_eq!(report.outcome, TurnOutcome::Failed);
        assert_eq!(server.requests.lock().len(), max_steps as usize);
        assert_eq!(
            report.steps, max_steps,
            "report includes the initial and repair requests"
        );
        assert_eq!(agent.metrics().repair_attempts, u64::from(max_steps - 2));
        assert_eq!(
            std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
            "bad"
        );
    }
}

#[cfg(unix)]
#[test]
fn stop_hook_reentry_cannot_reset_an_exhausted_turn_budget() {
    let server = serve(vec![
        Reply::Text("Report complete."),
        Reply::Text("Report complete."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.harness.max_steps = 1;
    e.config.hooks.push(dial::config::types::HookConfig {
        event: "stop".into(),
        matcher: None,
        command: vec!["sh".into(), "-c".into(), r#"cat >/dev/null; printf '{"decision":"block","reason":"Missing acceptance condition."}'"#.into()],
        timeout_secs: 5,
        fail_closed: None,
    });
    let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = agent
        .turn("Produce a report", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert_eq!(report.steps, 1);
    assert_eq!(server.requests.lock().len(), 1);
    assert_eq!(agent.metrics().turns_completed, 0);
    assert!(report.text.contains("Candidate work summary"));
    assert!(report.text.contains("Report complete."));
}

#[cfg(unix)]
#[test]
fn stop_hook_edits_reenter_verification_and_keep_the_turn_repair_bound() {
    if skip_without_sandbox() {
        return;
    }
    for fails_again in [false, true] {
        let mut replies = vec![
            Reply::Call("write", json!({"path":"marker.txt","content":"bad"})),
            Reply::Text("Initial marker written."),
            Reply::Call("write", json!({"path":"marker.txt","content":"fixed"})),
            Reply::Text("Initial marker repaired."),
            Reply::Call(
                "write",
                json!({"path":"marker.txt","content":if fails_again { "bad again" } else { "fixed again" }}),
            ),
            Reply::Text("Hook-requested marker update written."),
        ];
        // This request must never run: a hook cannot replenish the repair allowance.
        replies.push(Reply::Text("Unexpected extra repair."));
        let server = serve(replies);
        let mut e = env(&server.url, "auto");
        e.config.harness.max_steps = 8;
        e.config.verification.automatic_checks = true;
        e.config.verification.repair_attempts = 1;
        e.config.verification.commands.push(VerificationCommand {
            name: "marker-fixed".into(),
            command: "grep -q fixed marker.txt".into(),
            file_extensions: vec![],
            timeout_secs: 5,
        });
        e.config.hooks.push(dial::config::types::HookConfig {
            event: "stop".into(),
            matcher: None,
            command: vec!["sh".into(), "-c".into(), r#"payload=$(cat); case "$payload" in *'"stop_hook_active":false'*) printf '{"decision":"block","reason":"Update marker once more."}';; *) printf '{}';; esac"#.into()],
            timeout_secs: 5,
            fail_closed: None,
        });
        let mut agent = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let mut ui = Recorder::default();
        let report = agent.turn("Create marker", &mut ui).unwrap();
        assert_eq!(
            report.outcome,
            if fails_again {
                TurnOutcome::Failed
            } else {
                TurnOutcome::Completed
            }
        );
        assert_eq!(report.steps, 6);
        assert_eq!(server.requests.lock().len(), 6);
        assert_eq!(agent.metrics().repair_attempts, 1);
        assert_eq!(
            ui.calls.iter().filter(|name| *name == "bash").count(),
            3,
            "initial candidate, repair and hook update must each be checked"
        );
        let state = agent.session().control_state();
        assert_eq!(
            state["reliability"]["verification_results"][0]["status"],
            if fails_again { "code_failed" } else { "passed" }
        );
        if fails_again {
            assert!(report.text.contains("verification still fails"));
            assert!(report.text.contains("Candidate work summary"));
            assert!(
                report
                    .text
                    .contains("Hook-requested marker update written.")
            );
        }
    }
}

#[test]
fn a_step_reuses_earlier_work_without_a_citation_ceremony() {
    // The pattern that looped in practice: all the work happens while step 1 is open, then the
    // later steps are marked done. Uncited, step 2 stays open and the error says how to cite.
    let plan =
        json!({"op":"set","tasks":[{"content":"read the marker"},{"content":"report its value"}]});
    let server = serve(vec![
        Reply::Call("todo", plan),
        Reply::Call("read", json!({"path":"marker.txt"})),
        close_todo(),
        close_todo(),
        Reply::Call(
            "todo",
            json!({"op":"done","evidence":["read marker.txt earlier this turn: five"]}),
        ),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"The marker says five."}),
        ),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("marker.txt"), "five\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("what does the marker say?", &mut ui).unwrap();
    let requests = server.requests.lock();
    assert!(
        !requests[4]["messages"]
            .to_string()
            .contains("call done again with evidence")
    );
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert!(!a.todo().has_open(), "the cited done closed the step");
}

#[test]
fn todo_updates_do_not_run_checks_but_completion_runs_and_repairs_them() {
    if skip_without_sandbox() {
        return;
    }
    let server = serve(vec![
        open_todo(),
        Reply::Call("write", json!({"path":"marker.txt","content":"bad"})),
        close_todo(),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Marker fixed."}),
        ),
        Reply::Call("write", json!({"path":"marker.txt","content":"fixed"})),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Marker fixed."}),
        ),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "marker-fixed".into(),
        command: "grep -q fixed marker.txt".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("Create marker", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = server.requests.lock();
    assert!(
        !requests[3]["messages"]
            .to_string()
            .contains("Step not closed")
    );
    assert!(
        requests[4]["messages"]
            .to_string()
            .contains("verification failed")
    );
    assert_eq!(ui.calls.iter().filter(|name| *name == "bash").count(), 2);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "fixed"
    );
    assert!(
        a.session().control_state()["reliability"]["edit_loop"]["failed_checks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn todo_updates_cannot_bypass_failed_completion_verification() {
    if skip_without_sandbox() {
        return;
    }
    let server = serve(vec![
        open_todo(),
        Reply::Call("write", json!({"path":"marker.txt","content":"bad"})),
        close_todo(),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Marker written."}),
        ),
        close_todo(),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Marker written."}),
        ),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Verification failed","reason":"Configured check did not pass."}),
        ),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "always-fails".into(),
        command: "false".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("Create marker", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    assert_eq!(
        a.session().control_state()["reliability"]["edit_loop"]["failed_checks"],
        json!(["always-fails"])
    );
    assert_eq!(
        ui.calls.iter().filter(|name| *name == "bash").count(),
        1,
        "An unchanged failed check is not repeated by todo updates or another finish"
    );
}

#[test]
fn finish_reruns_failed_checks_after_a_repair_edit() {
    let server = serve(vec![
        Reply::Call("write", json!({"path":"marker.txt","content":"bad"})),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Created marker."}),
        ),
        Reply::Call("write", json!({"path":"marker.txt","content":"fixed"})),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Repaired marker."}),
        ),
    ]);
    let mut e = env(&server.url, "auto");
    // The disposable fixture's verification command must run even inside a host sandbox.
    e.config.sandbox.mode = dial::config::types::SandboxMode::Off;
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "marker-fixed".into(),
        command: "grep -q fixed marker.txt".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("create marker", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "fixed"
    );
    let requests = server.requests.lock();
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("verification failed")
    );
    assert_eq!(requests.len(), 4);
    assert_eq!(ui.calls.iter().filter(|call| *call == "bash").count(), 2);
    assert!(
        a.session().control_state()["reliability"]["edit_loop"]["failed_checks"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
}

#[test]
fn the_current_step_rides_at_the_end_of_requests_without_entering_history() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"inspect notes"},{"content":"summarize notes"}]}),
        ),
        Reply::Call("read", json!({"path":"notes.txt"})),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Paused","reason":"Which format?"}),
        ),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.efficiency.task_packet = true;
    std::fs::write(e.project.path().join("notes.txt"), "hello\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("summarize notes", &mut Recorder::default()).unwrap();
    let requests = server.requests.lock();
    let (first, second) = (
        requests[1]["messages"].as_array().unwrap(),
        requests[2]["messages"].as_array().unwrap(),
    );
    let focus_at = first
        .iter()
        .rposition(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains("Current task evidence packet"))
        })
        .expect("the focus rides in the request");
    let focus = &first[focus_at];
    assert!(
        first.len() - focus_at <= 2,
        "the focus sits in the trailing transient slots"
    );
    assert_eq!(focus["role"], "user");
    assert!(
        focus["content"]
            .as_str()
            .unwrap()
            .contains("Current task evidence packet"),
        "{focus}"
    );
    assert!(focus["content"].as_str().unwrap().contains("inspect notes"));
    assert!(!focus["content"].as_str().unwrap().contains("[Task focus"));
    assert_eq!(
        first[..focus_at],
        second[..focus_at],
        "everything before the focus stays a stable prefix"
    );
    assert_eq!(
        second.last().unwrap()["role"],
        "user",
        "the next request carries its own focus after the new tool result"
    );
    assert!(
        !a.session()
            .messages()
            .iter()
            .any(|m| m.to_string().contains("Task focus")),
        "the focus is never stored"
    );
}

#[test]
fn approval_denied_edit_does_not_trigger_focused_checks() {
    let server = serve(vec![
        Reply::Call("write", json!({"path":"marker.txt","content":"no"})),
        Reply::Text("unchanged"),
    ]);
    let mut e = env(&server.url, "ask");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "always-fails".into(),
        command: "false".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder {
        deny: true,
        ..Recorder::default()
    };
    a.turn("create marker", &mut ui)
        .unwrap_or_else(|error| panic!("{error:#}"));
    assert!(!e.project.path().join("marker.txt").exists());
    assert_eq!(ui.approvals, ["write"]);
    assert!(
        !ui.notices
            .iter()
            .any(|notice| notice.contains("verification still fails"))
    );
}

#[test]
fn dynamic_extension_sdk_executes_with_approval_and_evidence() {
    use dial::tools::ext::{Tool, ToolContext};
    struct Inspect;
    impl Tool for Inspect {
        fn name(&self) -> &str {
            "sdk_inspect"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":"sdk_inspect","description":"Read the SDK consumer fixture","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            true
        }
        fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
            let result = ctx
                .workspace
                .resolve(args["path"].as_str().unwrap())
                .and_then(|path| std::fs::read_to_string(path).map_err(|e| e.to_string()));
            match result {
                Ok(text) => ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text,
                    is_error: false,
                },
                Err(text) => ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text,
                    is_error: true,
                },
            }
        }
    }
    struct Consumer;
    impl dial::extensions::Extension for Consumer {
        fn name(&self) -> &str {
            "consumer"
        }
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            vec![Arc::new(Inspect)]
        }
    }
    let server = serve(vec![
        Reply::Call("tool_search", json!({"query":"sdk_inspect"})),
        Reply::Call("tool_load", json!({"names":["sdk_inspect"]})),
        Reply::Call("sdk_inspect", json!({"path":"fixture.txt"})),
        Reply::Text("SDK result received"),
    ]);
    let mut e = env(&server.url, "ask");
    e.config.assistance.compact_tools = true;
    std::fs::write(
        e.project.path().join("fixture.txt"),
        "real consumer evidence",
    )
    .unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut host = dial::extensions::ExtensionHost::default();
    host.install(&mut a, &Consumer).unwrap();
    assert!(host.install(&mut a, &Consumer).is_err());
    assert!(a.register_tool(Arc::new(Inspect)).is_err());
    struct Unsupported;
    impl dial::extensions::Extension for Unsupported {
        fn name(&self) -> &str {
            "unsupported"
        }
        fn api_version(&self) -> u32 {
            dial::extensions::SDK_VERSION + 1
        }
    }
    assert!(host.install(&mut a, &Unsupported).is_err());
    let mut ui = Recorder::default();
    a.turn("inspect SDK fixture", &mut ui).unwrap();
    assert_eq!(ui.approvals, ["sdk_inspect"]);
    let requests = server.requests.lock();
    assert!(
        !requests[0]["tools"]
            .to_string()
            .contains("\"name\":\"sdk_inspect\"")
    );
    assert!(
        requests[2]["tools"]
            .to_string()
            .contains("\"name\":\"sdk_inspect\"")
    );
    assert!(
        requests[3]["messages"]
            .to_string()
            .contains("real consumer evidence")
    );
    assert!(
        serde_json::to_string(&a.session().messages())
            .unwrap()
            .contains("artifact://")
    );
    assert_eq!(a.metrics().dynamic_tools_used, 1);
    let path = a.session().path().unwrap().to_owned();
    a.swap_session(Session::ephemeral());
    let resumed = Session::open(&path).unwrap();
    a.swap_session(resumed);
    assert!(a.tool_names().iter().any(|n| n == "sdk_inspect"));
}

#[test]
fn compatible_read_aliases_execute_with_canonical_arguments() {
    let server = serve(vec![
        Reply::Call(
            "functions.read_file",
            json!({"file_path":"f.txt","max_lines":"1"}),
        ),
        Reply::Text("Read successfully."),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("f.txt"), "hello").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("read f.txt", &mut ui).unwrap();
    assert_eq!(ui.calls, ["read"]);
    assert_eq!(a.metrics().tool_contract_errors, 0);
    assert!(
        ui.results
            .iter()
            .any(|(_, text, failed)| !failed && text.contains("hello"))
    );
}

#[test]
fn known_unoffered_tools_load_without_execution_until_the_next_request() {
    let server = serve(vec![
        Reply::Call("search_web", json!({"search_query":"Rust release notes"})),
        Reply::Call("search_web", json!({"search_query":"Rust release notes"})),
        Reply::Text("Network access is disabled."),
    ]);
    let mut e = env(&server.url, "ask");
    e.config.integrations = vec!["web_search".into()];
    e.config.sandbox.allow_network = false;
    e.config.assistance.compact_tools = true;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("search the web for Rust release notes", &mut ui)
        .unwrap();
    assert_eq!(ui.approvals, ["web_search"]);
    let requests = server.requests.lock();
    assert!(
        !requests[0]["tools"]
            .to_string()
            .contains("\"name\":\"web_search\"")
    );
    assert!(
        requests[1]["tools"]
            .to_string()
            .contains("\"name\":\"web_search\"")
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("This call was not executed")
    );
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("network access is disabled")
    );
}

#[test]
fn web_search_is_discoverable_loadable_and_respects_network_restrictions() {
    let server = serve(vec![
        Reply::Call("tool_search", json!({"query":"web_search"})),
        Reply::Call("tool_load", json!({"names":["web_search","web_fetch"]})),
        Reply::Call("web_search", json!({"query":"Rust release notes"})),
        Reply::Text("Network access is disabled; no search results were retrieved."),
    ]);
    let mut e = env(&server.url, "ask");
    e.config.assistance.compact_tools = true;
    e.config.integrations = vec!["web_search".into(), "web_fetch".into()];
    e.config.sandbox.allow_network = false;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("search the web for Rust release notes", &mut ui)
        .unwrap();
    assert_eq!(ui.approvals, ["web_search"]);
    let requests = server.requests.lock();
    assert!(
        requests[0]["messages"]
            .to_string()
            .contains("For live web research, discover web_search and web_fetch")
    );
    assert!(
        !requests[0]["tools"]
            .to_string()
            .contains("\"name\":\"web_search\"")
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Search the web and return numbered results")
    );
    for name in ["web_search", "web_fetch"] {
        assert!(
            requests[2]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["function"]["name"] == name)
        );
    }
    assert!(
        requests[3]["messages"]
            .to_string()
            .contains("web_search is unavailable: network access is disabled")
    );
    assert_eq!(a.metrics().tool_contract_errors, 0);
}

#[test]
fn discovery_enables_only_next_round_and_rejects_invalid_names_atomically() {
    let server = serve(vec![
        Reply::Calls(vec![
            ("tool_load", json!({"names":["web_fetch"]})),
            ("web_fetch", json!({"url":"https://example.invalid"})),
        ]),
        Reply::Call("tool_load", json!({"names":["web_search","not_a_tool"]})),
        Reply::Text("finished"),
    ]);
    let mut e = env(&server.url, "ask");
    e.config.integrations = vec!["web_fetch".into(), "web_search".into()];
    e.config.sandbox.allow_network = false;
    e.config.assistance.compact_tools = true;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("discover specialist tools", &mut Recorder::default())
        .unwrap();
    assert_eq!(a.metrics().tool_contract_errors, 1);
    assert!(a.tool_names().iter().any(|n| n == "web_fetch"));
    assert!(!a.tool_names().iter().any(|n| n == "web_search"));
}

#[test]
fn specialist_schema_can_be_released_and_loaded_again() {
    let server = serve(vec![
        Reply::Call("tool_load", json!({"names":["web_fetch"]})),
        Reply::Call("tool_load", json!({"unload":["web_fetch"]})),
        Reply::Call("tool_load", json!({"names":["web_fetch"]})),
        Reply::Text("finished"),
    ]);
    let mut e = env(&server.url, "ask");
    e.config.integrations = vec!["web_fetch".into()];
    e.config.sandbox.allow_network = false;
    e.config.assistance.compact_tools = true;
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Manage specialist tools", &mut Recorder::default())
        .unwrap();
    let requests = server.requests.lock();
    let has = |i: usize| {
        requests[i]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["function"]["name"] == "web_fetch")
    };
    assert!(!has(0) && has(1) && !has(2) && has(3));
    assert!(a.tool_names().iter().any(|n| n == "web_fetch"));
    // Loading/unloading changes the active contract, not unrelated schema definitions.
    let base_tools = requests[0]["tools"].as_array().unwrap();
    for request in requests.iter().skip(1) {
        for base in base_tools {
            let current = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["function"]["name"] == base["function"]["name"])
                .unwrap();
            assert_eq!(
                current, base,
                "loading a specialist rewrote an existing schema"
            );
        }
    }
}

#[test]
fn working_state_and_read_evidence_survive_compaction_without_mutation_loss() {
    let server = serve(vec![
        Reply::Call(
            "working_state",
            json!({"action":"update","goal":"preserve exact goal","decisions":["keep public API"],"open_work":["finish regression tests"]}),
        ),
        Reply::Call("read", json!({"path":"evidence.txt"})),
        Reply::Call(
            "write",
            json!({"path":"evidence.txt","content":"new evidence\n"}),
        ),
        Reply::Call("read", json!({"path":"evidence.txt"})),
        // Enough compressible history to offset the irreducible exact state/constraint carry.
        Reply::Say(format!(
            "first done {}",
            "historical read details ".repeat(100)
        )),
        Reply::Text("second done"),
        Reply::Text("third done"),
        Reply::Text("compact summary"),
        Reply::Text("continued"),
    ]);
    let mut e = env(&server.url, "ask");
    // The default 20k keep-recent budget covers this whole short history; keep only the newest group.
    e.config.compaction.keep_recent_tokens = 100;
    std::fs::write(e.project.path().join("evidence.txt"), "old evidence\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("first goal", &mut ui).unwrap();
    let before = server.requests.lock()[4]["messages"].to_string();
    assert!(before.contains("old evidence") && !before.contains("Superseded observation"));
    assert!(before.contains("new evidence"));
    assert!(a.session().messages().iter().any(|m| {
        m["content"]
            .as_str()
            .is_some_and(|s| s.contains("old evidence"))
    }));
    a.turn("second goal", &mut ui).unwrap();
    a.turn("third goal", &mut ui).unwrap();
    a.compact(&mut ui).unwrap();
    a.turn("continue", &mut ui).unwrap();
    let last = server.requests.lock().last().unwrap()["messages"].to_string();
    assert!(last.contains("preserve exact goal") && last.contains("finish regression tests"));
    assert_eq!(a.metrics().compactions, 1);
}

#[test]
fn live_worker_coordination_during_generation_discards_stale_writes() {
    let main = serve(vec![Reply::Text("unused")]);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let side = serve_fn(
        Arc::new(move |request| {
            if request["messages"]
                .to_string()
                .contains("Stop writing; answer four")
            {
                return Reply::Text("Four.");
            }
            started_tx.send(()).unwrap();
            release_rx
                .lock()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            Reply::Call(
                "write",
                json!({"path":"stale.txt","content":"must not run"}),
            )
        }),
        None,
        None,
    );
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let started = a
        .start_subagents(
            &json!({"agent":"worker","task":"Write stale.txt","solution_space":"s"}),
            &mut ui,
        )
        .unwrap();
    let id = started[0]["id"].as_str().unwrap();
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    a.subagent_message(id, "Stop writing; answer four").unwrap();
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = a.subagent_status(Some(id), &mut ui).unwrap();
        if state["status"] != "running" {
            assert_eq!(state["status"], "done", "{state}");
            assert_eq!(state["result"], "Four.");
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!e.project.path().join("stale.txt").exists());
    assert_eq!(side.requests.lock().len(), 2);
}

#[test]
fn live_subagent_messages_results_and_cancel_are_real_independent_runs() {
    let main = serve(vec![Reply::Text("parent independent")]);
    let side = serve_fn(
        Arc::new(|request| {
            thread::sleep(Duration::from_millis(30));
            if request["messages"]
                .to_string()
                .contains("steered directive")
            {
                Reply::Text("received steering")
            } else {
                Reply::Call("read", json!({"path":"shared.txt"}))
            }
        }),
        None,
        None,
    );
    let mut e = env(&main.url, "ask");
    with_subagent_role(&mut e, &side.url);
    std::fs::write(e.project.path().join("shared.txt"), "shared evidence").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let started = a
        .start_subagents(
            &json!({"task":"inspect shared files","solution_space":"s","name":"live"}),
            &mut ui,
        )
        .unwrap();
    let id = started[0]["id"].as_str().unwrap().to_owned();
    a.subagent_message(&id, "steered directive").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let final_state = loop {
        let state = a.subagent_status(Some(&id), &mut ui).unwrap();
        if state["status"] != "running" {
            break state;
        }
        assert!(Instant::now() < deadline, "live job failed to finish");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(final_state["status"], "done");
    assert_eq!(final_state["result"], "received steering");
    assert!(
        final_state["artifact"]
            .as_str()
            .unwrap()
            .starts_with("artifact://")
    );
    assert!(
        a.session().messages().is_empty(),
        "children must not contaminate the parent conversation"
    );
    assert!(
        side.requests
            .lock()
            .iter()
            .any(|r| r["messages"].to_string().contains("steered directive"))
    );
    let started = a
        .start_subagents(
            &json!({"task":"cancel this investigation","solution_space":"s"}),
            &mut ui,
        )
        .unwrap();
    let id = started[0]["id"].as_str().unwrap();
    a.cancel_subagent(id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = a.subagent_status(Some(id), &mut ui).unwrap();
        if state["status"] != "running" {
            assert_eq!(state["status"], "cancelled");
            break;
        }
        assert!(Instant::now() < deadline, "cancelled job failed to stop");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(a.subagent_status(Some("not-a-job"), &mut ui).is_err());
}

#[test]
fn live_worker_preserves_read_only_and_per_operation_approval_boundaries() {
    let main = serve(vec![Reply::Text("unused")]);
    let side = serve(vec![
        Reply::Call(
            "write",
            json!({"path":"worker.txt","content":"approved worker output"}),
        ),
        Reply::Text("done"),
    ]);
    let mut e = env(&main.url, "ask");
    with_subagent_role(&mut e, &side.url);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.set_plan_mode(true);
    assert!(
        a.start_subagents(
            &json!({"agent":"worker","task":"write worker.txt","solution_space":"s"}),
            &mut ui
        )
        .is_err()
    );
    assert!(!e.project.path().join("worker.txt").exists());
    a.set_plan_mode(false);
    let started = a
        .start_subagents(
            &json!({"agent":"worker","task":"write worker.txt","solution_space":"s"}),
            &mut ui,
        )
        .unwrap();
    let id = started[0]["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = a.subagent_status(Some(id), &mut ui).unwrap();
        if state["status"] != "running" {
            assert_eq!(state["status"], "done");
            break;
        }
        assert!(Instant::now() < deadline, "worker did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(ui.approvals, ["task", "write"]);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("worker.txt")).unwrap(),
        "approved worker output"
    );
}

#[test]
fn rpc_admission_abort_is_not_cleared_by_the_agent_turn() {
    let server = serve(vec![Reply::Text("must not be requested")]);
    let e = env(&server.url, "auto");
    let interrupt = Arc::new(AtomicBool::new(true));
    let mut a = agent_with(&e, None, interrupt);
    let report = a
        .turn_preserving_interrupt("cancel before dispatch", &mut Recorder::default())
        .unwrap();
    assert!(report.interrupted);
    assert!(server.requests.lock().is_empty());
    assert_eq!(a.metrics().turns_completed, 0);
}

#[test]
fn plan_mode_blocks_future_mutations_from_preexisting_live_workers() {
    let main = serve(vec![Reply::Text("unused")]);
    let side = serve(vec![
        Reply::Call(
            "write",
            json!({"path":"blocked.txt","content":"must not be written"}),
        ),
        Reply::Text("write denied"),
    ]);
    let mut e = env(&main.url, "ask");
    with_subagent_role(&mut e, &side.url);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let started = a
        .start_subagents(
            &json!({"agent":"worker","task":"write blocked.txt","solution_space":"s"}),
            &mut ui,
        )
        .unwrap();
    a.set_plan_mode(true);
    let id = started[0]["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if a.subagent_status(Some(id), &mut ui).unwrap()["status"] != "running" {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!e.project.path().join("blocked.txt").exists());
    assert_eq!(ui.approvals, ["task"]);
    assert!(
        side.requests
            .lock()
            .iter()
            .any(|r| r["messages"].to_string().contains("read-only"))
    );
}

#[test]
fn out_of_band_live_cancellation_unblocks_pending_approval_without_mutation() {
    struct Waiting {
        control: dial::agent::LiveTaskControl,
        entered: bool,
    }
    impl Ui for Waiting {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            Decision::Allow
        }
        fn approve_with_cancel(&mut self, tool: &str, _: &str, cancel: &AtomicBool) -> Decision {
            if tool == "task" {
                return Decision::Allow;
            }
            assert_eq!(tool, "write");
            self.entered = true;
            let control = self.control.clone();
            thread::spawn(move || {
                control.cancel("task-1").unwrap();
            });
            let until = Instant::now() + Duration::from_secs(2);
            while !cancel.load(Ordering::SeqCst) {
                assert!(Instant::now() < until);
                thread::sleep(Duration::from_millis(5));
            }
            Decision::Deny
        }
    }
    let main = serve(vec![Reply::Text("unused")]);
    let side = serve(vec![
        Reply::Call("write", json!({"path":"cancelled.txt","content":"no"})),
        Reply::Text("unused"),
    ]);
    let mut e = env(&main.url, "ask");
    with_subagent_role(&mut e, &side.url);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let control = a.live_task_control();
    let mut ui = Waiting {
        control: control.clone(),
        entered: false,
    };
    let started = a
        .start_subagents(
            &json!({"agent":"worker","task":"write cancelled.txt","solution_space":"s"}),
            &mut ui,
        )
        .unwrap();
    let id = started[0]["id"].as_str().unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let state = a.subagent_status(Some(id), &mut ui).unwrap();
        if state["status"] != "running" {
            assert_eq!(state["status"], "cancelled");
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ui.entered);
    assert!(!e.project.path().join("cancelled.txt").exists());
    assert!(control.send(id, "too late").is_err());
}

#[test]
fn extension_batch_registration_is_atomic_including_unloaded_and_core_collisions() {
    use dial::tools::ext::{Tool, ToolContext};
    struct ReadFixture {
        name: &'static str,
        version: u32,
    }
    impl Tool for ReadFixture {
        fn name(&self) -> &str {
            self.name
        }
        fn extension_api_version(&self) -> u32 {
            self.version
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":self.name,"description":"Read consumer fixture through the workspace boundary","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            true
        }
        fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
            dial::tools::execute(ctx.workspace, "read", args, 120)
        }
    }
    let server = serve(vec![
        Reply::Call("tool_load", json!({"names":["atomic-good"]})),
        Reply::Call("atomic-good", json!({"path":"fixture.txt"})),
        Reply::Text("atomic SDK complete"),
    ]);
    let e = env(&server.url, "ask");
    std::fs::write(e.project.path().join("fixture.txt"), "atomic real fixture").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let valid = || {
        Arc::new(ReadFixture {
            name: "atomic-good",
            version: 1,
        }) as Arc<dyn Tool>
    };
    assert!(
        a.register_tools(vec![
            valid(),
            Arc::new(ReadFixture {
                name: "read",
                version: 1
            })
        ])
        .is_err()
    );
    assert!(!a.available_tool_names().iter().any(|n| n == "atomic-good"));
    assert!(a.register_tools(vec![valid(), valid()]).is_err());
    assert!(!a.available_tool_names().iter().any(|n| n == "atomic-good"));
    assert!(
        a.register_tools(vec![
            valid(),
            Arc::new(ReadFixture {
                name: "future-tool",
                version: 2
            })
        ])
        .is_err()
    );
    assert!(!a.available_tool_names().iter().any(|n| n == "atomic-good"));
    a.register_tools(vec![valid()]).unwrap();
    assert!(
        a.register_tools(vec![
            Arc::new(ReadFixture {
                name: "second-good",
                version: 1
            }),
            valid()
        ])
        .is_err()
    );
    assert!(!a.available_tool_names().iter().any(|n| n == "second-good"));
    let mut ui = Recorder::default();
    a.turn("use atomically installed fixture tool", &mut ui)
        .unwrap();
    assert_eq!(ui.approvals, ["atomic-good"]);
    assert!(
        server.requests.lock()[2]["messages"]
            .to_string()
            .contains("atomic real fixture")
    );
}

#[test]
fn legacy_admitted_memory_selection_migrates_runtime_without_rewriting_history() {
    let server = serve(vec![
        Reply::Call("recall", json!({"query":"durable fixture"})),
        Reply::Text("recalled"),
        Reply::Text("resumed"),
    ]);
    let e = env(&server.url, "ask");
    let mut legacy = e.config.harness.clone();
    legacy.tools.push("memory".into());
    let store = Store::open(&e.layout.state_dir, "scripted", "legacy-consumer", &legacy).unwrap();
    let memory = dial::tools::provenance::ProvenanceMemory::open(&e.layout).unwrap();
    memory
        .retain(
            dial::tools::provenance::Scope::Project,
            "durable fixture fact",
            "user-confirmed fixture",
            "primer-session",
            None,
        )
        .unwrap();
    let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("recall durable fixture", &mut ui).unwrap();
    assert_eq!(ui.calls, ["recall"]);
    let requests = server.requests.lock();
    let names = tool_names(&requests[0]);
    assert!(names.iter().any(|n| n == "recall") && !names.iter().any(|n| n == "memory"));
    assert!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .any(|m| m["content"]
                .as_str()
                .unwrap_or("")
                .contains("durable fixture fact"))
    );
    drop(requests);
    let path = a.session().path().unwrap().to_owned();
    a.swap_session(Session::ephemeral());
    let resumed = Session::open(&path).unwrap();
    a.swap_session(resumed);
    a.turn("continue", &mut ui).unwrap();
    assert_eq!(
        ui.notices
            .iter()
            .filter(|n| n.contains("Migrated legacy memory tool selection"))
            .count(),
        1
    );
    assert!(
        store
            .active()
            .unwrap()
            .harness
            .tools
            .iter()
            .any(|n| n == "memory"),
        "immutable admitted harness must not be rewritten"
    );
}

#[test]
fn streamed_tool_arguments_are_observable_before_validated_invocation() {
    #[derive(Default)]
    struct Progress {
        order: Vec<String>,
        arguments: Vec<(u64, String, String, String)>,
    }
    impl Ui for Progress {
        fn tool_arguments_delta(&mut self, index: u64, id: &str, name: &str, delta: &str) {
            self.order.push(format!("arguments:{name}"));
            self.arguments
                .push((index, id.into(), name.into(), delta.into()));
        }
        fn tool_call(&mut self, name: &str, _: &str) {
            self.order.push(format!("call:{name}"));
        }
    }
    let server = serve(vec![
        Reply::Calls(vec![
            ("read", json!({"path":"first.txt"})),
            ("read", json!({"path":"second.txt"})),
        ]),
        Reply::Text("both files read"),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("first.txt"), "first evidence").unwrap();
    std::fs::write(e.project.path().join("second.txt"), "second evidence").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Progress::default();
    a.turn("read both fixtures", &mut ui).unwrap();
    assert_eq!(ui.arguments.len(), 2);
    assert_eq!(
        (
            &ui.arguments[0].0,
            ui.arguments[0].1.as_str(),
            ui.arguments[0].2.as_str()
        ),
        (&0, "call_0", "read")
    );
    assert_eq!(
        (
            &ui.arguments[1].0,
            ui.arguments[1].1.as_str(),
            ui.arguments[1].2.as_str()
        ),
        (&1, "call_1", "read")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&ui.arguments[0].3).unwrap(),
        json!({"path":"first.txt"})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&ui.arguments[1].3).unwrap(),
        json!({"path":"second.txt"})
    );
    assert_eq!(
        ui.order,
        ["arguments:read", "arguments:read", "call:read", "call:read"]
    );
    let requests = server.requests.lock();
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("first evidence")
            && requests[1]["messages"]
                .to_string()
                .contains("second evidence")
    );
}

/// A ranking request carries no tools; the wire format omits an empty `tools` list entirely.
fn is_rerank(request: &Value) -> bool {
    request
        .get("tools")
        .and_then(Value::as_array)
        .is_none_or(|tools| tools.is_empty())
}

fn semantic_fixture_provider(request: &Value) -> Reply {
    assert!(
        is_rerank(request),
        "reranking cannot call tools or spawn agents"
    );
    let payload: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    let candidates = payload["candidates"].as_array().unwrap();
    let hit = candidates
        .iter()
        .find(|c| {
            c["source_evidence"]
                .as_str()
                .unwrap()
                .contains("memo.get(key)")
        })
        .expect("semantic candidates include a zero-overlap declaration");
    assert_eq!(hit["path"], "cache.rs");
    // Any token overlap awards at least 10 points; a bare declaration only gets its small structural baseline.
    assert!(
        hit["lexical_points"].as_u64().unwrap() < 10,
        "this synonym case must not be solved by lexical overlap: {hit}"
    );
    Reply::Say(json!({"indices":[hit["index"]]}).to_string())
}

fn write_semantic_fixtures(e: &Env) {
    std::fs::write(e.project.path().join("cache.rs"), "fn cached_lookup(key: &str) {\n    if let Some(hit) = memo.get(key) { return hit; }\n    calculate(key)\n}\n").unwrap();
    std::fs::write(
        e.project.path().join("render.py"),
        "def paint():\n    canvas.flush()\n",
    )
    .unwrap();
}

#[test]
fn semantic_find_uses_current_model_and_preserves_native_tool_pairs() {
    let server = serve_fn(
        Arc::new(|request| {
            if is_rerank(request) {
                return semantic_fixture_provider(request);
            }
            if has_tool_result(request) {
                Reply::Text("cache implementation located")
            } else {
                Reply::Call(
                    "find",
                    json!({"query":"reuse an existing value without doing the expensive work again","ranking":"semantic"}),
                )
            }
        }),
        None,
        None,
    );
    let mut e = env(&server.url, "auto");
    e.config.harness.tools.push("find".into());
    write_semantic_fixtures(&e);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("locate reusable value behavior", &mut ui).unwrap();
    assert_eq!(report.text, "cache implementation located");
    assert_eq!(ui.calls, ["find"]);
    let usage = a.usage().0;
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Retrieval].requests,
        1
    );
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Agent].requests,
        2
    );
    assert_eq!(usage.workers.requests, 1);
    let requests = server.requests.lock();
    assert_eq!(
        requests.len(),
        3,
        "the serving backend handles the one isolated rank request"
    );
    assert!(requests.iter().all(|r| r["model"] == "scripted"));
    assert!(
        !requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m.get("tool_calls").is_some()),
        "rerank context is isolated from pending native calls"
    );
    let results: Vec<&Value> = requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "call_1");
    let evidence = results[0]["content"].as_str().unwrap();
    assert!(
        evidence.contains("cache.rs:1-4 semantic_rank=1"),
        "{evidence}"
    );
    assert!(!evidence.contains("render.py:"));
}

#[test]
fn semantic_find_rejects_fabricated_ranking_and_exact_mode_bypasses_inference() {
    let server = serve(vec![
        Reply::Call(
            "find",
            json!({"query":"reuse an existing value without doing the expensive work again","ranking":"semantic"}),
        ),
        Reply::Text("{\"indices\":[999]}"),
        Reply::Text("ranking rejected"),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.harness.tools.push("find".into());
    write_semantic_fixtures(&e);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("semantic query", &mut Recorder::default()).unwrap();
    let requests = server.requests.lock();
    let error = requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(error.contains("outside captured evidence") && !error.contains("cache.rs:"));
    drop(requests);
    let usage = a.usage().0;
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Retrieval].requests,
        1
    );
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Agent].requests,
        2
    );
    let exact = serve(vec![
        Reply::Call("find", json!({"query":"memo.get","mode":"exact"})),
        Reply::Text("exact located"),
    ]);
    let mut e = env(&exact.url, "auto");
    e.config.harness.tools.push("find".into());
    write_semantic_fixtures(&e);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("literal query", &mut Recorder::default()).unwrap();
    let requests = exact.requests.lock();
    assert_eq!(requests.len(), 2, "exact retrieval has no ranking request");
    assert!(requests[1]["messages"].to_string().contains("cache.rs:2-2"));
    assert!(
        !a.usage()
            .0
            .by_purpose
            .contains_key(&dial::llm::usage::Purpose::Retrieval)
    );
}

#[test]
fn delegated_semantic_find_reuses_the_child_backend_and_request_budget() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"task":"locate reuse behavior","solution_space":"s","max_steps":3}),
        ),
        Reply::Text("delegated evidence received"),
    ]);
    let side = serve_fn(
        Arc::new(|request| {
            if is_rerank(request) {
                return semantic_fixture_provider(request);
            }
            if has_tool_result(request) {
                Reply::Text("cache.rs contains cached_lookup")
            } else {
                Reply::Call(
                    "find",
                    json!({"query":"reuse an existing value without doing the expensive work again","ranking":"semantic"}),
                )
            }
        }),
        None,
        None,
    );
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    e.config.harness.tools.push("find".into());
    write_semantic_fixtures(&e);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    assert_eq!(
        a.turn("delegate this behavioral query", &mut ui)
            .unwrap()
            .text,
        "delegated evidence received"
    );
    let usage = a.usage().0;
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Retrieval].requests,
        1
    );
    assert_eq!(
        usage.by_purpose[&dial::llm::usage::Purpose::Agent].requests,
        4
    );
    assert_eq!(usage.workers.requests, 3);
    let requests = side.requests.lock();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| r["model"] == "side-model"));
    assert!(
        requests[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .any(|m| m["content"]
                .as_str()
                .unwrap()
                .contains("cache.rs:1-4 semantic_rank=1"))
    );
}

#[test]
fn live_training_targets_match_actual_requests_across_turns_and_harvest_final_solution() {
    let server = serve(vec![
        Reply::Call(
            "working_state",
            json!({"action":"update","goal":"preserve SQLite constraints"}),
        ),
        Reply::Call(
            "retain",
            json!({"scope":"project","content":"SQLite uses transactions for atomic updates.","source":"test fixture"}),
        ),
        Reply::Call("read", json!({"path":"facts.txt"})),
        Reply::Text("first answer"),
        Reply::Text("continued answer"),
    ]);
    let mut e = env(&server.url, "auto");
    if !e.config.harness.tools.iter().any(|t| t == "retain") {
        e.config.harness.tools.push("retain".into());
    }
    e.config.training.max_seq_length = 131072;
    std::fs::write(e.project.path().join("facts.txt"), "SQLite evidence").unwrap();
    let store = Store::open(&e.layout.state_dir, "scripted", "t", &e.config.harness).unwrap();
    let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let first = a
        .turn("Read facts.txt; preserve SQLite constraints", &mut ui)
        .unwrap();
    let second = a.turn("Continue the SQLite task", &mut ui).unwrap();
    let requests = server.requests.lock();
    let first_payload = store
        .trajectory_messages(first.trajectory.unwrap())
        .unwrap()
        .unwrap();
    let snapshots = first_payload["examples"].as_array().unwrap();
    assert_eq!(snapshots.len(), 4);
    for (snapshot, request) in snapshots.iter().zip(requests.iter()) {
        let messages = snapshot["messages"].as_array().unwrap();
        assert_eq!(json!(&messages[..messages.len() - 1]), request["messages"]);
        assert_eq!(snapshot["tools"], request["tools"]);
        assert!(messages.iter().all(|m| m.get("dial_evidence").is_none()));
    }
    let second_payload = store
        .trajectory_messages(second.trajectory.unwrap())
        .unwrap()
        .unwrap();
    let messages = second_payload["examples"][0]["messages"]
        .as_array()
        .unwrap();
    assert_eq!(
        json!(&messages[..messages.len() - 1]),
        requests[4]["messages"]
    );
    let context = json!(&messages[..messages.len() - 1]).to_string();
    assert!(context.contains("first answer") && context.contains("preserve SQLite constraints"));
    // Recalled notes travel with the user message they were recalled for; the working state the model wrote is
    // visible in its own earlier call instead of being re-inserted before the conversation on every request.
    assert!(context.contains("Relevant persistent notes"));
    let system_count = messages.iter().filter(|m| m["role"] == "system").count();
    assert_eq!(
        system_count,
        4,
        "frozen guidance, stable context, the tool-search addendum and the memory guidance: {:?}",
        messages
            .iter()
            .filter(|m| m["role"] == "system")
            .map(|m| m["content"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(120)
                .collect::<String>())
            .collect::<Vec<_>>()
    );
    assert!(
        messages[..system_count]
            .iter()
            .all(|m| m["role"] == "system")
    );
    assert!(messages.iter().any(|m| {
        m["tool_calls"].to_string().contains("working_state")
            && m["tool_calls"]
                .to_string()
                .contains("preserve SQLite constraints")
    }));
    assert_eq!(messages.last().unwrap()["content"], "continued answer");
    drop(requests);
    // No self-label: only the externally accepted second turn is harvested.
    assert!(store.verified_successes(100.0, 10).unwrap().is_empty());
    store
        .outcome(&dial::config::types::Outcome {
            trajectory: second.trajectory.unwrap(),
            score: 100.0,
            source: "user".into(),
            reference: None,
        })
        .unwrap();
    let report = dial::learn::harvest::harvest(&e.config, &store, &Default::default()).unwrap();
    assert_eq!(report.from_successes, 1, "{report:?}");
    let records: Vec<Value> = [&e.config.training.train_data, &e.config.training.valid_data]
        .iter()
        .flat_map(|p| {
            std::fs::read_to_string(p)
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]["messages"].as_array().unwrap().last().unwrap()["content"],
        "continued answer"
    );
    assert!(
        records[0]["messages"]
            .to_string()
            .contains("SQLite evidence")
    );
}

#[test]
fn every_request_extends_the_previous_one_so_prompt_caches_stay_valid() {
    // Recalled notes, working state and superseded reads used to be rewritten in front of the whole conversation
    // whenever a new user turn arrived, forcing the provider (or LM Studio) to prefill the history again.
    // (A deliberate one-time supersession of a repeated read is covered by the `evidence` unit tests.)
    let server = serve(vec![
        Reply::Call(
            "retain",
            json!({"scope":"project","content":"SQLite uses transactions for atomic updates.","source":"fixture"}),
        ),
        Reply::Call("read", json!({"path":"facts.txt"})),
        Reply::Text("first answer"),
        Reply::Text("second answer"),
        Reply::Text("third answer"),
    ]);
    let mut e = env(&server.url, "auto");
    if !e.config.harness.tools.iter().any(|t| t == "retain") {
        e.config.harness.tools.push("retain".into());
    }
    std::fs::write(e.project.path().join("facts.txt"), "SQLite evidence").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("Remember and read the SQLite facts", &mut ui)
        .unwrap();
    a.turn("What do the notes say about SQLite transactions?", &mut ui)
        .unwrap();
    a.turn("Anything else about SQLite?", &mut ui).unwrap();
    let requests = server.requests.lock();
    assert!(requests.len() >= 5);
    for (n, pair) in requests.windows(2).enumerate() {
        let (before, after) = (
            conversation_messages(&pair[0]),
            conversation_messages(&pair[1]),
        );
        assert!(after.len() > before.len());
        assert_eq!(
            &after[..before.len()],
            &before[..],
            "request {} rewrote what request {n} had already sent",
            n + 1
        );
    }
    assert!(
        requests.last().unwrap()["messages"]
            .to_string()
            .contains("Relevant persistent notes"),
        "recall still reaches the model"
    );
}
#[test]
fn a_tool_call_with_raw_newlines_in_its_arguments_runs_instead_of_being_regenerated() {
    // Local models routinely write real line breaks into JSON strings. This verifies the raw write executes once;
    // The final diff review arrives on the request after the write; the reply below closes the turn.
    let raw = "{\"path\":\"out.txt\",\"content\":\"line one\nline two\n\"}".to_owned();
    assert!(serde_json::from_str::<Value>(&raw).is_err());
    let server = serve(vec![Reply::RawCall("write", raw), Reply::Text("done")]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("write out.txt", &mut Recorder::default()).unwrap();
    assert_eq!(report.text, "done");
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("out.txt")).unwrap(),
        "line one\nline two\n"
    );
    assert_eq!(
        server.requests.lock().len(),
        2,
        "the raw write runs once; the final diff review rides on the request after it, not a regenerated call"
    );
}
#[test]
fn a_cut_off_tool_call_is_still_refused_and_nothing_is_written() {
    let server = serve(vec![
        Reply::RawCall(
            "write",
            "{\"path\":\"out.txt\",\"content\":\"half a fi".into(),
        ),
        Reply::Text("gave up"),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("write out.txt", &mut Recorder::default()).unwrap();
    assert!(
        !e.project.path().join("out.txt").exists(),
        "a half-written call must not execute"
    );
}
fn recovery_fixture(e: &Env) -> Store {
    std::fs::create_dir_all(&e.layout.state_dir).unwrap();
    let store = Store::open(&e.layout.state_dir, "scripted", "t", &e.config.harness).unwrap();
    let version = store.active().unwrap().id;
    let trajectory = store
        .begin_trajectory(version, "coding", "prior-repair", "prompt")
        .unwrap();
    let project_root = std::fs::canonicalize(e.project.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    store.tool_recovery(trajectory, version, &json!({
        "project_root": project_root,
        "context": [{"role":"user","content":"Fix the parser for emoji filenames"}],
        "tools": [{"type":"function","function":{"name":"edit","parameters":{"type":"object"}}}],
        "corrected": {"role":"assistant","content":null,"tool_calls":[{"id":"repair-1","type":"function",
            "function":{"name":"edit","arguments":"{\"path\":\"private/secret.txt\",\"input\":\"SECRET_VALUE\"}"}}]},
        "rejected": [{"role":"assistant","content":"invalid shape"}],
        "failure_kinds": ["schema"]
    })).unwrap();
    store
}

#[test]
fn repair_hints_ride_with_the_user_turn_so_the_system_prompt_never_changes() {
    // Hints used to be appended to the system prompt for one request and dropped from the next, so message 0 differed
    // between the first and second step and a cached prefix could never be reused past it.
    let server = serve(vec![
        Reply::Call("read", json!({"path":"notes.txt"})),
        Reply::Text("first"),
        Reply::Text("second"),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.assistance.repair_hints = true;
    std::fs::write(e.project.path().join("notes.txt"), "parser notes").unwrap();
    let store = recovery_fixture(&e);
    let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("Please fix the parser and emoji filenames", &mut ui)
        .unwrap();
    a.turn("Fix the parser and emoji filenames once more", &mut ui)
        .unwrap();
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 3);
    let system = requests[0]["messages"][0]["content"].as_str().unwrap();
    assert!(
        !system.contains("verified repair patterns"),
        "hints no longer live in the system prompt"
    );
    for request in requests.iter() {
        assert_eq!(
            request["messages"][0]["content"], system,
            "the system prompt is identical on every request"
        );
    }
    let messages = requests[0]["messages"].as_array().unwrap();
    let advisory = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .find_map(|m| {
            m["content"]
                .as_str()
                .filter(|c| c.contains("Relevant verified repair patterns"))
        })
        .unwrap_or_else(|| panic!("no user turn carries the repair advisory: {messages:?}"));
    assert!(
        advisory.contains("edit") && !advisory.contains("SECRET_VALUE"),
        "{advisory}"
    );
    assert!(
        messages
            .iter()
            .filter(|m| m["role"] == "system")
            .all(|m| !m["content"]
                .as_str()
                .unwrap_or("")
                .contains("Relevant verified repair patterns")),
        "the system prompt must not carry repair hints"
    );
    for (n, pair) in requests.windows(2).enumerate() {
        let (before, after) = (
            conversation_messages(&pair[0]),
            conversation_messages(&pair[1]),
        );
        assert_eq!(
            &after[..before.len()],
            &before[..],
            "request {} rewrote what request {n} had already sent",
            n + 1
        );
    }
}

#[test]
fn zero_step_limit_runs_past_64_and_overrides_a_finite_admitted_harness() {
    for unlimited in [false, true] {
        let mut replies: Vec<_> = (0..65)
            .map(|n| Reply::Call("read", json!({"path":format!("f{n}.txt")})))
            .collect();
        replies.push(Reply::Text("done"));
        let server = serve(replies);
        let mut e = env(&server.url, "auto");
        e.config.harness.tools = vec!["read".into()];
        e.config.harness.max_steps = 2;
        let store = Store::open(&e.layout.state_dir, "scripted", "t", &e.config.harness).unwrap();
        if unlimited {
            e.config.harness.max_steps = 0;
        }
        e.config.reasoning_guard.enabled = false;
        for n in 0..65 {
            std::fs::write(
                e.project.path().join(format!("f{n}.txt")),
                format!("value {n}"),
            )
            .unwrap();
        }
        let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
        assert_eq!(a.version().harness.max_steps, 2);
        let report = a
            .turn(
                "Read the numbered files and report the result.",
                &mut Recorder::default(),
            )
            .unwrap();
        if unlimited {
            assert_eq!(
                (report.text.as_str(), report.outcome),
                ("done", TurnOutcome::Completed)
            );
            assert_eq!(server.requests.lock().len(), 66);
        } else {
            assert_eq!(report.outcome, TurnOutcome::Incomplete);
            assert!(report.text.contains("2-step limit"), "{}", report.text);
            assert_eq!(server.requests.lock().len(), 2);
        }
    }
}

#[test]
fn zero_step_limit_keeps_sealed_evaluation_finite() {
    let mut replies: Vec<_> = (0..25)
        .map(|n| Reply::Call("read", json!({"path":format!("f{n}.txt")})))
        .collect();
    replies.push(Reply::Text("done"));
    let server = serve(replies);
    let mut e = env(&server.url, "auto");
    e.config.harness.tools = vec!["read".into()];
    e.config.harness.max_steps = 0;
    let version = dial::config::types::Version {
        id: 1,
        parent: None,
        kind: "initial".into(),
        harness: e.config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    let files = (0..25)
        .map(|n| (format!("f{n}.txt"), format!("value {n}")))
        .collect();
    let passed = dial::agent::check_case(
        &mut dial::agent::Runtime::new(),
        &e.config,
        &version,
        "Read the numbered files and report the result.",
        &files,
        &dial::learn::verify::Check::FileEquals {
            path: "f0.txt".into(),
            content: "value 0".into(),
        },
    )
    .unwrap();
    assert!(!passed);
    assert_eq!(server.requests.lock().len(), 24);
}

#[test]
fn cut_off_replies_do_not_spend_the_step_budget() {
    // Two truncated generations, then one tool step and the answer: two real steps. With a budget of two, every
    // model call used to count and this ended at the step limit.
    let server = serve(vec![
        Reply::Cut,
        Reply::Cut,
        Reply::Call("read", json!({"path":"f.txt"})),
        Reply::Text("done"),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.harness.max_steps = 2;
    e.config.reasoning_guard.enabled = false;
    std::fs::write(e.project.path().join("f.txt"), "x").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("read f.txt", &mut Recorder::default()).unwrap();
    assert_eq!(
        (report.text.as_str(), report.outcome),
        ("done", TurnOutcome::Completed)
    );
    assert_eq!(server.requests.lock().len(), 4);
}

#[test]
fn a_tool_that_keeps_failing_with_a_new_guess_each_time_is_warned_then_the_turn_ends_incomplete() {
    // Read a distinct range each round so this exercises the failing-tool streak,
    // independently of the unchanged-observation guard. No whole round fails.
    let guesses = (1..=12)
        .map(|n| {
            Reply::Calls(vec![
                ("read", json!({"path":format!("f.txt:{n}-{n}")})),
                (
                    "edit",
                    json!({"path":"f.txt","old_string":format!("guess number {n}"),"new_string":"x"}),
                ),
            ])
        })
        .collect();
    let server = serve(guesses);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    e.config.edit_mode_override = Some(dial::config::types::EditMode::Replace);
    std::fs::write(e.project.path().join("f.txt"), "the real text\n".repeat(12)).unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a
        .turn("fix f.txt", &mut ui)
        .expect("a stalled turn reports itself instead of erroring");
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    let requests = server.requests.lock();
    assert!(
        requests.last().unwrap()["messages"]
            .to_string()
            .contains("times in a row without one success")
    );
    assert!(
        !requests[4]["messages"]
            .to_string()
            .contains("5 times in a row"),
        "not before the fifth failure"
    );
    assert!(
        requests[5]["messages"]
            .to_string()
            .contains("5 times in a row, with different arguments"),
        "warned on the fifth"
    );
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("f.txt")).unwrap(),
        "the real text\n".repeat(12)
    );
    assert_eq!(a.metrics().turns_completed, 1);
}

#[test]
fn rounds_in_which_nothing_worked_end_the_turn_even_when_no_single_tool_streak_does() {
    // Eight different reads of files that do not exist: each tool call is a new guess and the `read` streak (8) is
    // below its own limit, but eight whole rounds without one success is a model that is not getting anywhere.
    let server = serve(
        (1..=12)
            .map(|n| Reply::Call("read", json!({"path":format!("missing-{n}.txt")})))
            .collect(),
    );
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    e.config.reliability.enabled = false; // Isolate the round guard from earlier rejected checkpoint calls.
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("read the notes", &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    let requests = server.requests.lock();
    assert!(
        requests[8]["messages"]
            .to_string()
            .contains("every tool call has failed for 8 rounds in a row")
    );
    let after_fourth = requests[4]["messages"].as_array().unwrap();
    assert!(
        after_fourth.iter().any(|m| m["role"] == "user"
            && m["content"]
                .as_str()
                .is_some_and(|c| c.contains("4 rounds of tool calls"))),
        "warned after the fourth round"
    );
    assert!(
        !requests[3]["messages"]
            .to_string()
            .contains("rounds of tool calls in a row")
    );
    assert!(
        !requests[5]["messages"]
            .to_string()
            .contains("4 rounds of tool calls"),
        "the warning reaches one request without persisting in history"
    );
}

#[test]
fn a_big_tool_result_triggers_compaction_before_the_request_it_would_overflow() {
    // Fixed prompt and tool schemas cost about 9.4k tokens. The big reply (~10.5k) keeps `three?` under the
    // 65% threshold of a 32k window; the probe (~2.2k) crosses it. Its compaction summarizes the big reply and
    // lands at ~13.7k, inside the 80% continuation band (~17k), so the probe still answers in the same turn.
    let chat = Mutex::new(std::collections::VecDeque::from([
        Reply::Text("one-answer"),
        Reply::Say(
            (0..2000)
                .map(|i| format!("{i:04} -- result {i:04}\n"))
                .collect(),
        ),
        Reply::Text("three-answer"),
        Reply::Text("third"),
    ]));
    // The summarizer folds a large transcript into several windows; every window gets the same summary.
    let server = serve_fn(
        Arc::new(move |request: &Value| {
            let summarizing = request["messages"][0]["content"]
                .as_str()
                .is_some_and(|text| text.starts_with("Summarize user-AI"));
            if summarizing {
                return Reply::Text("SUMMARY");
            }
            chat.lock()
                .pop_front()
                .unwrap_or(Reply::Text("(script exhausted)"))
        }),
        None,
        Some(32768),
    );
    let mut e = env(&server.url, "auto");
    e.config.compaction.threshold_percent = Some(65.0);
    e.config.compaction.keep_recent_tokens = 1000;
    e.config.compaction.reserve_tokens = Some(4096);
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("one?", &mut ui).unwrap();
    a.turn("two?", &mut ui).unwrap();
    a.turn("three?", &mut ui).unwrap();
    let report = a.turn(&"c".repeat(8_800), &mut ui).unwrap();
    assert_eq!(report.text, "third");
    assert!(
        summary_requests(&server) >= 1,
        "compacted first, from the estimate, not from a stale report"
    );
    assert_eq!(a.metrics().compactions, 1);
    let messages = a.session().messages();
    assert!(messages[0]["content"].as_str().unwrap().contains("SUMMARY"));
}

#[test]
fn a_session_switch_forgets_previous_context_and_token_count() {
    let server = serve_with(
        vec![Reply::Text("old-answer"), Reply::Text("fresh-answer")],
        None,
        Some(16384),
    );
    let e = env(&server.url, "auto");
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    a.turn("old-conversation-marker", &mut Recorder::default())
        .unwrap();
    assert!(!a.session().messages().is_empty());
    a.record_prompt_usage(4000);
    a.swap_session(
        Session::create(
            &e.layout,
            SessionMeta {
                model: e.config.model.clone(),
            },
        )
        .unwrap(),
    );
    assert_eq!(a.last_prompt_tokens, 0);
    assert!(a.session().messages().is_empty());
    assert!(!a.todo().has_open());
    a.turn("fresh-conversation-marker", &mut Recorder::default())
        .unwrap();
    let requests = server.requests.lock();
    let messages = requests.last().unwrap()["messages"].to_string();
    assert!(messages.contains("fresh-conversation-marker"));
    assert!(!messages.contains("old-conversation-marker"));
    assert!(!messages.contains("old-answer"));
}

#[test]
fn extended_inspection_keeps_every_tool_without_a_plan() {
    let mut script: Vec<_> = (0..6)
        .map(|n| Reply::Call("read", json!({"path":format!("{n}.txt")})))
        .collect();
    script.push(Reply::Text("Inspected all six files."));
    let server = serve(script);
    let e = env(&server.url, "auto");
    for n in 0..6 {
        std::fs::write(
            e.project.path().join(format!("{n}.txt")),
            format!("evidence {n}"),
        )
        .unwrap();
    }
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("Investigate the checks", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 7);
    let last = requests[6]["tools"].as_array().unwrap();
    for name in ["read", "write", "bash"] {
        assert!(
            last.iter().any(|s| s["function"]["name"] == name),
            "{name} must stay offered after many unplanned reads"
        );
    }
}

#[test]
fn independent_training_failure_gets_one_repair_and_only_success_is_harvestable() {
    if skip_without_sandbox() {
        return;
    }
    let server = serve(vec![Reply::Text("wrong"), Reply::Text("42")]);
    let state = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.endpoint = Some(dial::config::types::EndpointConfig {
        headers: None,
        base_url: server.url.clone(),
        api_key_env: None,
        allow_insecure: false,
        protocol: None,
        account_id: None,
    });
    config.model = "test-model".into();
    config.state_dir = state.path().into();
    config.project_root = project.path().into();
    let store = Store::open(state.path(), &config.model, "test", &config.harness).unwrap();
    let task = dial::learn::curriculum::Task {
        family: "repair-test",
        prompt: "Return 42".into(),
        files: Default::default(),
        check: dial::learn::verify::Check::AnswerExact {
            expected: "42".into(),
        },
    };
    let passed = dial::agent::selfplay_run(
        &mut dial::agent::Runtime::new(),
        &config,
        &store,
        &store.active().unwrap(),
        &task,
    )
    .unwrap();
    assert!(passed);
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]["messages"].as_array().unwrap().iter().any(|m| {
        m["content"]
            .as_str()
            .is_some_and(|s| s.contains("independent training-task verifier rejected"))
    }));
    let successes = store.verified_successes(100.0, 10).unwrap();
    assert_eq!(successes.len(), 1);
}

#[test]
fn isolated_search_applies_smallest_verified_patch_without_touching_other_files() {
    if skip_without_sandbox() {
        return;
    }
    let main_step = AtomicUsize::new(0);
    let server = serve_fn(
        Arc::new(move |request| {
            let main = request["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|t| t["function"]["name"] == "solve"))
                || request["messages"].as_array().is_some_and(|ms| {
                    ms.iter().any(|m| {
                        m["role"] == "user"
                            && m["content"].as_str().is_some_and(|s| {
                                s.contains("Fix the value using verified candidates")
                            })
                    })
                });
            if main {
                // The first call only loads the deferred tool; the retry executes it.
                return match main_step.fetch_add(1, Ordering::SeqCst) {
                    0 | 1 => Reply::Call(
                        "solve",
                        json!({
                            "goal":"Make value.txt begin with fixed", "allowed_paths":["value.txt"],
                            "candidates":["minimal fix", "include extra explanatory content"],
                            "checks":[{"command":"test \"$(head -n 1 value.txt)\" = fixed","timeout_secs":5}]
                        }),
                    ),
                    _ => {
                        Reply::Text("Both candidates were checked; the smaller patch was applied.")
                    }
                };
            }
            let messages = request["messages"].as_array().unwrap();
            let results = messages.iter().filter(|m| m["role"] == "tool").count();
            // Overwriting an existing file requires a prior read of it (read witness).
            if results == 0 {
                return Reply::Call("read", json!({"path":"value.txt"}));
            }
            if results > 1 {
                return Reply::Text("Patch implemented.");
            }
            let extra = messages.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("include extra explanatory content"))
            });
            Reply::Call(
                "write",
                json!({"path":"value.txt","content":if extra {"fixed\nextra\n"} else {"fixed\n"}}),
            )
        }),
        None,
        None,
    );
    let mut e = env(&server.url, "auto");
    e.config.solve.enabled = true;
    e.config.harness.tools.push("solve".into());
    std::fs::write(e.project.path().join("value.txt"), "broken\n").unwrap();
    std::fs::write(e.project.path().join("unrelated.txt"), "dirty user work\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn(
            "Fix the value using verified candidates",
            &mut Recorder::default(),
        )
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("value.txt")).unwrap(),
        "fixed\n"
    );
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("unrelated.txt")).unwrap(),
        "dirty user work\n"
    );
    let requests = server.requests.lock();
    let last = requests.last().unwrap()["messages"].to_string();
    assert!(last.contains("\\\"winner\\\":1"), "{last}");
}

#[test]
fn escalation_rejects_nonlocal_route_before_disclosing_evidence() {
    let call = || {
        Reply::Call(
            "escalate",
            json!({"goal":"Fix behavior","question":"Which invariant is missing?",
            "evidence":["PRIVATE_PROJECT_EVIDENCE"],"rejected_hypotheses":["syntax alone"]}),
        )
    };
    // The first call only loads the deferred tool; the retry executes and is refused.
    let server = serve(vec![
        call(),
        call(),
        Reply::Text("Remote escalation requires operator opt-in."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.escalation.enabled = true;
    e.config.harness.tools.push("escalate".into());
    e.config.model_roles.insert(
        "coach".into(),
        dial::llm::model_config::ModelSelection {
            provider: "endpoint".into(),
            model: "stronger".into(),
            base_url: Some("https://must-not-contact.invalid/v1".into()),
            ..Default::default()
        },
    );
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Ask a narrow question", &mut Recorder::default())
        .unwrap();
    assert_eq!(server.requests.lock().len(), 3);
    let tools: Vec<String> = a
        .session()
        .messages()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].to_string())
        .collect();
    assert!(
        tools.iter().any(|s| s.contains("allow_remote")),
        "{tools:?}"
    );
}

#[test]
fn behavioral_repairs_retain_failure_and_pass_without_marking_task_success() {
    if skip_without_sandbox() {
        return;
    }
    let command = "test \"$(cat value.txt)\" = fixed";
    let server = serve(vec![
        Reply::Call("bash", json!({"command":command})),
        Reply::Call("read", json!({"path":"value.txt"})),
        Reply::Call("write", json!({"path":"value.txt","content":"fixed\n"})),
        Reply::Call("bash", json!({"command":command})),
        Reply::Text("The declared behavioral check passes."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "value_behavior".into(),
        command: command.into(),
        file_extensions: vec!["txt".into()],
        timeout_secs: 5,
    });
    e.config
        .verification
        .behavioral_checks
        .push("value_behavior".into());
    std::fs::write(e.project.path().join("value.txt"), "broken\n").unwrap();
    let store = Store::open(&e.layout.state_dir, "scripted", "coding", &e.config.harness).unwrap();
    let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
    a.turn("Fix the value", &mut Recorder::default()).unwrap();
    let records = store
        .recent_executable_repairs(
            &e.project.path().canonicalize().unwrap().to_string_lossy(),
            10,
        )
        .unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0].1;
    assert_eq!(record["kind"], "behavioral");
    assert_eq!(record["before"]["exit_code"], 1);
    assert_eq!(record["after"]["exit_code"], 0);
    assert_ne!(
        record["changes"][0]["before_sha256"],
        record["changes"][0]["after_sha256"]
    );
    let before_ref = record["changes"][0]["before_ref"].as_str().unwrap();
    let artifacts =
        dial::storage::artifact::ArtifactStore::new(&e.layout.state_dir.join("artifacts")).unwrap();
    assert!(
        artifacts
            .retrieve(before_ref, 1, 10)
            .unwrap()
            .contains("1:broken\n")
    );
}

#[test]
fn harness_records_behavioral_failure_before_the_model_edits() {
    if skip_without_sandbox() {
        return;
    }
    let command = "test \"$(cat value.txt)\" = fixed";
    let server = serve(vec![
        Reply::Call("read", json!({"path":"value.txt"})),
        Reply::Call("write", json!({"path":"value.txt","content":"fixed\n"})),
        Reply::Text("Fixed."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "behavior".into(),
        command: command.into(),
        file_extensions: vec!["txt".into()],
        timeout_secs: 5,
    });
    e.config
        .verification
        .behavioral_checks
        .push("behavior".into());
    std::fs::write(e.project.path().join("value.txt"), "broken\n").unwrap();
    let store = Store::open(&e.layout.state_dir, "scripted", "coding", &e.config.harness).unwrap();
    let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
    a.turn("Fix the value", &mut Recorder::default()).unwrap();
    assert!(
        server.requests.lock()[0]["messages"]
            .to_string()
            .contains("failures predate this turn")
    );
    let records = store
        .recent_executable_repairs(
            &e.project.path().canonicalize().unwrap().to_string_lossy(),
            10,
        )
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].1["before"]["exit_code"], 1);
    assert_eq!(records[0].1["after"]["exit_code"], 0);
}

#[test]
fn silent_serial_and_parallel_subagents_keep_ui_alive_and_cancel_before_late_tools() {
    struct CancelOnHeartbeat {
        interrupt: Arc<AtomicBool>,
        child_started: bool,
        ticks: usize,
    }
    impl Ui for CancelOnHeartbeat {
        fn notice(&mut self, text: &str) {
            if text.starts_with("subagent ") && text.contains("(1/") {
                self.child_started = true;
            }
        }
        fn heartbeat(&mut self) {
            if self.child_started {
                self.ticks += 1;
                if self.ticks >= 3 {
                    self.interrupt.store(true, Ordering::SeqCst);
                }
            }
        }
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            Decision::Allow
        }
    }
    for task in [
        json!({"agent":"worker","task":"Implement a fix","solution_space":"s"}),
        json!({"agent":"scout","task":"Inspect the code","solution_space":"s"}),
        json!({"tasks":[{"agent":"scout","task":"Inspect A","solution_space":"s"},{"agent":"scout","task":"Inspect B","solution_space":"s"}]}),
    ] {
        let request_number = AtomicUsize::new(0);
        let server = serve_fn(
            Arc::new(move |_| {
                if request_number.fetch_add(1, Ordering::SeqCst) == 0 {
                    Reply::Call("task", task.clone())
                } else {
                    Reply::Think {
                        reasoning: "Considering the request".into(),
                        pause: Duration::from_secs(3),
                    }
                }
            }),
            None,
            None,
        );
        let e = env(&server.url, "auto");
        let interrupt = Arc::new(AtomicBool::new(false));
        let mut ui = CancelOnHeartbeat {
            interrupt: interrupt.clone(),
            child_started: false,
            ticks: 0,
        };
        let mut agent = agent_with(&e, None, interrupt);
        let started = Instant::now();
        let report = agent.turn("Delegate the work", &mut ui).unwrap();
        assert!(
            report.interrupted,
            "heartbeat-triggered cancellation must reach the child"
        );
        assert!(ui.ticks >= 3, "silent child must not block the UI owner");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancel must not wait for a child reply"
        );
        assert!(
            !e.project.path().join("gate-leak.txt").exists(),
            "late streamed writes cannot execute"
        );
    }
}

fn experience_checks(e: &mut Env) {
    e.config.verification.enabled = false; // Explicit tool checks still record their observed result.
    e.config.verification.commands = vec![VerificationCommand {
        name: "value check".into(),
        command: "test \"$(cat value.txt)\" = fixed".into(),
        file_extensions: vec!["txt".into()],
        timeout_secs: 5,
    }];
    e.config.verification.behavioral_checks = vec!["value check".into()];
}
#[test]
fn experience_capture_replay_and_stale_refusal_use_real_tools() {
    if skip_without_sandbox() {
        return;
    }
    let server = serve(vec![
        Reply::Call("read", json!({"path":"value.txt"})),
        Reply::Call("write", json!({"path":"value.txt","content":"fixed"})),
        Reply::Call(
            "bash",
            json!({"command":"test \"$(cat value.txt)\" = fixed"}),
        ),
        Reply::Text("Repaired."),
    ]);
    let mut e = env(&server.url, "auto");
    experience_checks(&mut e);
    std::fs::write(e.project.path().join("value.txt"), "old").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("repair value", &mut Recorder::default()).unwrap();
    let store = dial::storage::experience::Experience::open(&e.layout).unwrap();
    let entries = store.inspect().unwrap();
    let recipe = entries["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["kind"] == "recipe")
        .expect("observed edit with passing behavioral check should save a recipe");
    let id = recipe["id"].as_str().unwrap().to_owned();
    let replay = serve(vec![
        Reply::Call("experience", json!({"action":"run","id":id})),
        Reply::Call("experience", json!({"action":"run","id":id})),
        Reply::Text("Done."),
    ]);
    let mut resumed = env(&replay.url, "auto");
    resumed.layout = e.layout.clone();
    resumed.config = e.config.clone();
    resumed.config.endpoint.as_mut().unwrap().base_url = replay.url.clone();
    std::fs::write(e.project.path().join("value.txt"), "old").unwrap();
    let mut a = agent_with(&resumed, None, Arc::new(AtomicBool::new(false)));
    a.turn("run recorded recipe", &mut Recorder::default())
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("value.txt")).unwrap(),
        "fixed"
    );
    drop(a);
    let stale = serve(vec![
        Reply::Call("experience", json!({"action":"run","id":id})),
        Reply::Call("experience", json!({"action":"run","id":id})),
        Reply::Text("Stale; investigate."),
    ]);
    resumed.config.endpoint.as_mut().unwrap().base_url = stale.url.clone();
    std::fs::write(e.project.path().join("value.txt"), "concurrent edit").unwrap();
    let mut a = agent_with(&resumed, None, Arc::new(AtomicBool::new(false)));
    a.turn("reuse recipe", &mut Recorder::default()).unwrap();
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("value.txt")).unwrap(),
        "concurrent edit"
    );
    assert!(
        stale.requests.lock()[2]["messages"]
            .to_string()
            .contains("preconditions changed")
    );
}
#[test]
fn experience_requires_current_behavioral_check_after_last_write() {
    let server = serve(vec![
        Reply::Call("write", json!({"path":"value.txt","content":"fixed"})),
        Reply::Call(
            "bash",
            json!({"command":"test \"$(cat value.txt)\" = fixed"}),
        ),
        Reply::Call("write", json!({"path":"value.txt","content":"unchecked"})),
        Reply::Text("Unverified."),
    ]);
    let mut e = env(&server.url, "auto");
    experience_checks(&mut e);
    std::fs::write(e.project.path().join("value.txt"), "old").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("repair value", &mut Recorder::default()).unwrap();
    let report = dial::storage::experience::Experience::open(&e.layout)
        .unwrap()
        .inspect()
        .unwrap();
    assert!(
        !report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "recipe")
    );
}
#[test]
fn experience_ledger_survives_session_resume_and_unrelated_source_changes() {
    let server = serve(vec![
        Reply::Call("read", json!({"path":"a.rs"})),
        Reply::Text("Observed."),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("a.rs"), "fn a() {}\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("where is entry point", &mut Recorder::default())
        .unwrap();
    let state = a.session().control_state();
    assert_eq!(
        state["investigation_ledger"]["observations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let path = a.session().path().unwrap().to_owned();
    drop(a);
    let resumed = serve(vec![
        Reply::Call("experience", json!({"action":"ledger"})),
        Reply::Text("Reused."),
    ]);
    let mut e = e;
    e.config.endpoint.as_mut().unwrap().base_url = resumed.url.clone();
    std::fs::write(e.project.path().join("unrelated.rs"), "fn unrelated() {}\n").unwrap();
    let session = Session::open(&path).unwrap();
    let mut a = Agent::interactive(
        &e.config,
        make_backend(&e.config).unwrap(),
        e.layout.clone(),
        session,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    a.turn("where is entry point", &mut Recorder::default())
        .unwrap();
    assert!(
        resumed.requests.lock()[0]["messages"]
            .to_string()
            .contains("unchanged source evidence")
    );
}
#[test]
fn experience_teacher_is_opt_in_and_cannot_replace_recipe_evidence() {
    if skip_without_sandbox() {
        return;
    }
    let main = serve(vec![
        Reply::Call("read", json!({"path":"value.txt"})),
        Reply::Call("write", json!({"path":"value.txt","content":"fixed"})),
        Reply::Call(
            "bash",
            json!({"command":"test \"$(cat value.txt)\" = fixed"}),
        ),
        Reply::Text("Fixed."),
    ]);
    let teacher = serve(vec![Reply::Text(
        "{\"lesson\":\"Check the changed value using the configured assertion.\"}",
    )]);
    let mut e = env(&main.url, "auto");
    experience_checks(&mut e);
    e.config.assistance.teacher_enabled = true;
    e.config.model_roles.insert(
        "coach".into(),
        dial::llm::model_config::ModelSelection {
            provider: "teacher".into(),
            model: "teacher-model".into(),
            base_url: Some(teacher.url.clone()),
            ..Default::default()
        },
    );
    std::fs::write(e.project.path().join("value.txt"), "old").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("repair value", &mut Recorder::default()).unwrap();
    assert_eq!(teacher.requests.lock().len(), 1);
    assert!(
        teacher.requests.lock()[0]["tools"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
    let report = dial::storage::experience::Experience::open(&e.layout)
        .unwrap()
        .inspect()
        .unwrap();
    let recipe = report["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["kind"] == "recipe")
        .unwrap();
    assert!(
        recipe["payload"]["lesson"]
            .as_str()
            .unwrap()
            .contains("configured assertion")
    );
    assert_eq!(
        recipe["payload"]["checks"][0],
        "test \"$(cat value.txt)\" = fixed"
    );
}

#[test]
fn experience_replay_still_requires_each_write_approval() {
    struct DenyWrites;
    impl Ui for DenyWrites {
        fn approve(&mut self, name: &str, _: &str) -> Decision {
            if name == "write" {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }
    }
    let server = serve(vec![Reply::Text("placeholder")]);
    let mut e = env(&server.url, "ask");
    experience_checks(&mut e);
    std::fs::write(e.project.path().join("value.txt"), "old").unwrap();
    let ws = dial::tools::Workspace::new(e.project.path()).unwrap();
    let store = dial::storage::experience::Experience::open(&e.layout).unwrap();
    let id = store
        .recipe(&dial::storage::experience::Recipe {
            objective: "repair value".into(),
            model: e.config.model.clone(),
            session: "fixture".into(),
            steps: vec![dial::storage::experience::Step {
                tool: "write".into(),
                args: json!({"path":"value.txt","content":"fixed"}),
                sources: dial::storage::experience::preconditions(
                    &ws,
                    "write",
                    &json!({"path":"value.txt"}),
                ),
            }],
            checks: vec![e.config.verification.commands[0].command.clone()],
            lesson: None,
        })
        .unwrap();
    let replay = serve(vec![
        Reply::Call("experience", json!({"action":"run","id":id})),
        Reply::Call("experience", json!({"action":"run","id":id})),
        Reply::Text("Write denied."),
    ]);
    e.config.endpoint.as_mut().unwrap().base_url = replay.url.clone();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("reuse recipe", &mut DenyWrites).unwrap();
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("value.txt")).unwrap(),
        "old"
    );
    assert!(
        replay.requests.lock()[2]["messages"]
            .to_string()
            .contains("denied")
    );
}

#[test]
fn experience_does_not_capture_untracked_shell_mutations() {
    if skip_without_sandbox() {
        return;
    }
    let server = serve(vec![
        Reply::Call("write", json!({"path":"value.txt","content":"fixed"})),
        Reply::Call("bash", json!({"command":"printf other > untracked.txt"})),
        Reply::Call(
            "bash",
            json!({"command":"test \"$(cat value.txt)\" = fixed"}),
        ),
        Reply::Text("Finished."),
    ]);
    let mut e = env(&server.url, "auto");
    experience_checks(&mut e);
    std::fs::write(e.project.path().join("value.txt"), "old").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("repair value", &mut Recorder::default()).unwrap();
    let report = dial::storage::experience::Experience::open(&e.layout)
        .unwrap()
        .inspect()
        .unwrap();
    assert!(
        !report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "recipe")
    );
    assert!(e.project.path().join("untracked.txt").is_file());
}

#[test]
fn adaptive_settings_follow_real_reads_edits_checks_and_review_handoff() {
    if skip_without_sandbox() {
        return;
    }
    let server = serve(vec![
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Call("write", json!({"path":"marker.txt","content":"after\n"})),
        Reply::Call(
            "bash",
            json!({"command":"python3 -m unittest discover -s tests"}),
        ),
        Reply::Call("read", json!({"path":"missing.txt"})),
        Reply::Text("Updated and checked the marker."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.harness.max_tokens = 8192;
    e.config.verification.enabled = false;
    e.config.adaptive.sampling = true;
    e.config.adaptive.output_limits = true;
    let profile = dial::llm::profiles::ModelProfile {
        provider: e.config.selection().provider,
        model: e.config.model.clone(),
        capabilities: dial::llm::profiles::ModelCapabilities {
            temperature: true,
            top_p: true,
            seed: true,
            ..Default::default()
        },
        ..Default::default()
    };
    e.config.model_profiles.insert("sampling".into(), profile);
    e.config.model_profile = Some("sampling".into());
    std::fs::write(e.project.path().join("marker.txt"), "before\n").unwrap();
    std::fs::create_dir(e.project.path().join("tests")).unwrap();
    std::fs::write(e.project.path().join("tests/test_ok.py"),"import unittest\nclass Check(unittest.TestCase):\n def test_ok(self): self.assertEqual(1, 1)\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Implement the marker change", &mut Recorder::default())
        .unwrap();
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0]["temperature"], 0.2);
    assert_eq!(requests[2]["temperature"], 0.15);
    assert_eq!(requests[3]["temperature"], 0.05);
    assert_eq!(requests[4]["temperature"], 0.1);
    assert_eq!(requests[0]["max_tokens"], 2048);
    assert_eq!(requests[2]["max_tokens"], 8192);
    assert_eq!(requests[3]["max_tokens"], 1536);
    let control = a.session().control_state();
    assert_eq!(control["adaptive_policy"]["selection"]["phase"], "recovery");
    assert_eq!(control["adaptive_history"].as_array().unwrap().len(), 4);
}

#[test]
fn reliability_capability_check_executes_only_disposable_expected_calls() {
    if skip_without_sandbox() {
        return;
    }
    let server = serve_fn(
        Arc::new(|request| {
            if request["tools"].as_array().unwrap().len() == 2 {
                return Reply::Calls(vec![
                    ("read", json!({"path":"fixture.py"})),
                    ("glob", json!({"pattern":"*.py"})),
                ]);
            }
            let name = request["tools"][0]["function"]["name"].as_str().unwrap();
            let content = request["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap();
            let args: Value =
                serde_json::from_str(content.split_once("arguments: ").unwrap().1).unwrap();
            match name {
                "read" => Reply::Call("read", args),
                "write" => Reply::Call("write", args),
                "edit" => Reply::Call("edit", args),
                "grep" => Reply::Call("grep", args),
                "glob" => Reply::Call("glob", args),
                "todo" => Reply::Call("todo", args),
                "bash" => Reply::Call("bash", args),
                _ => panic!("unexpected probe tool"),
            }
        }),
        None,
        None,
    );
    let mut e = env(&server.url, "auto");
    std::fs::write(
        e.project.path().join("fixture.py"),
        "user source must remain unchanged",
    )
    .unwrap();
    let report = dial::reliability::check(&e.config).unwrap();
    assert_eq!(report["passed"], 8, "{report}");
    assert_eq!(report["batch_passed"], true);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("fixture.py")).unwrap(),
        "user source must remain unchanged"
    );
    assert!(!e.project.path().join("created.txt").exists());
    assert!(dial::reliability::measured(&e.config).is_some());
    let profile = dial::llm::profiles::ModelProfile {
        provider: e.config.selection().provider,
        model: e.config.model.clone(),
        capabilities: dial::llm::profiles::ModelCapabilities {
            parallel_tool_calls: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    e.config.model_profiles.insert("single".into(), profile);
    e.config.model_profile = Some("single".into());
    let report = dial::reliability::check(&e.config).unwrap();
    assert_eq!(report["batch_disabled"], true);
    assert_eq!(report["passed"], 7);
    assert_eq!(report["total"], 7);
    assert!(report["cases"].as_array().unwrap().last().unwrap()["skipped"] == true);
}

#[test]
fn reliability_stops_different_contract_failures_without_executing_them() {
    let server = serve(vec![
        Reply::Call("read", json!({"path":5})),
        Reply::Call("write", json!({"path":"must-not-exist.txt"})),
        Reply::Calls(vec![
            (
                "write",
                json!({"path":"must-not-exist.txt","content":"bad batch"}),
            ),
            ("read", json!({"path":"existing.txt"})),
        ]),
        Reply::Text("must never be requested"),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.reliability.auto_continue_rounds = 0; // this test exercises the stop itself
    e.config.reliability.measured_interfaces = true;
    std::fs::write(e.project.path().join("existing.txt"), "evidence").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("Fix a fixture", &mut Recorder::default()).unwrap();
    // Contract failures escalate; the turn is no longer ended for them.
    assert_eq!(report.outcome, TurnOutcome::Completed, "{}", report.text);
    assert!(!e.project.path().join("must-not-exist.txt").exists());
    assert_eq!(server.requests.lock().len(), 4);
    let control = a.session().control_state();
    assert_eq!(control["reliability"]["interface"], "compact single call");
    assert_eq!(control["reliability"]["stats"]["successes"], 0);
}

#[test]
fn strict_tool_wire_schema_and_optional_nulls_preserve_execution_contract() {
    let server = serve(vec![
        Reply::Call(
            "read",
            json!({"path":"fixture.txt","limit":null,"offset":null}),
        ),
        Reply::Text("Read succeeded"),
    ]);
    let mut e = env(&server.url, "auto");
    let profile = dial::llm::profiles::ModelProfile {
        provider: e.config.selection().provider,
        model: e.config.model.clone(),
        capabilities: dial::llm::profiles::ModelCapabilities {
            temperature: true,
            strict_tools: true,
            parallel_tool_calls: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    e.config.model_profiles.insert("strict".into(), profile);
    e.config.model_profile = Some("strict".into());
    std::fs::write(e.project.path().join("fixture.txt"), "real evidence").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Read fixture.txt", &mut Recorder::default())
        .unwrap();
    assert_eq!(a.metrics().tool_contract_errors, 0);
    let requests = server.requests.lock();
    let read = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "read")
        .unwrap();
    assert_eq!(read["function"]["strict"], true);
    assert_eq!(
        read["function"]["parameters"]["properties"]["limit"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(requests[0]["parallel_tool_calls"], false);
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("real evidence")
    );
}

#[test]
fn finish_cannot_accept_or_reuse_failed_behavioral_verification() {
    let server = serve(vec![
        Reply::Call("write", json!({"path":"value.txt","content":"broken"})),
        Reply::Call("finish", json!({"outcome":"completed","summary":"Fixed"})),
        Reply::Call("finish", json!({"outcome":"completed","summary":"Fixed"})),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Check fails","reason":"The required value is not established"}),
        ),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "value_behavior".into(),
        command: "test \"$(cat value.txt)\" = fixed".into(),
        file_extensions: vec!["txt".into()],
        timeout_secs: 5,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("Fix the value", &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    assert!(
        server.requests.lock()[2]["messages"]
            .to_string()
            .contains("focused verification failed")
    );
    assert!(
        server.requests.lock()[3]["messages"]
            .to_string()
            .contains("focused checks failed and no new edits are awaiting a fresh check")
    );
    assert!(
        a.session().control_state()["reliability"]["automatic_failing_checks"]
            .as_u64()
            .unwrap()
            > 0
    );
}

fn behavioral_outcome_turn(
    content: &str,
    check_passes: bool,
) -> (Store, Scripted, Option<dial::agent::TurnReport>) {
    let command = "test \"$(cat value.txt)\" = fixed";
    let server = serve(vec![
        Reply::Call("read", json!({"path":"value.txt"})),
        Reply::Call("write", json!({"path":"value.txt","content":content})),
        Reply::Text(if check_passes {
            "Fixed and the behavioral check passes."
        } else {
            "Done."
        }),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "value_behavior".into(),
        command: command.into(),
        file_extensions: vec!["txt".into()],
        timeout_secs: 5,
    });
    e.config
        .verification
        .behavioral_checks
        .push("value_behavior".into());
    std::fs::write(e.project.path().join("value.txt"), "broken\n").unwrap();
    let store = Store::open(&e.layout.state_dir, "scripted", "coding", &e.config.harness).unwrap();
    let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
    let report = a.turn("Fix the value", &mut Recorder::default()).ok();
    drop(a);
    (store, server, report)
}

#[test]
fn clean_turn_with_passing_behavioral_check_records_a_verified_outcome() {
    if skip_without_sandbox() {
        return;
    }
    let (store, _server, report) = behavioral_outcome_turn("fixed\n", true);
    assert_eq!(report.map(|r| r.outcome), Some(TurnOutcome::Completed));
    assert_eq!(store.verified_live_turns().unwrap(), 1);
}

#[test]
fn turn_with_failing_behavioral_check_records_no_verified_outcome() {
    if skip_without_sandbox() {
        return;
    }
    let (store, _server, _report) = behavioral_outcome_turn("broken\n", false);
    assert_eq!(store.verified_live_turns().unwrap(), 0);
}

#[test]
fn subagent_roles_reach_distinct_backends_and_custom_base_roles_keep_their_thinking() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"tasks":[
                {"agent":"custom-review","task":"review-only","solution_space":"s"},
                {"agent":"custom-plan","task":"plan-only","solution_space":"s"},
                {"agent":"test-runner","task":"test-only","solution_space":"s"}
            ]}),
        ),
        Reply::Text("Role routing verified."),
    ]);
    let review = serve_fn(
        Arc::new(|_| Reply::Text("Review/test complete.")),
        None,
        None,
    );
    let plan = serve(vec![Reply::Text("Plan complete.")]);
    let mut e = env(&main.url, "auto");
    for (role, model, url) in [
        ("tester", "review-model", &review.url),
        ("planner", "plan-model", &plan.url),
    ] {
        e.config.model_roles.insert(
            role.into(),
            dial::llm::model_config::ModelSelection {
                provider: "reviewer".into(),
                model: model.into(),
                base_url: Some(url.clone()),
                ..Default::default()
            },
        );
    }
    e.config.agents.insert(
        "custom-review".into(),
        serde_json::from_value(
            json!({"base":"reviewer","prompt":"Review only.","thinking_level":"high"}),
        )
        .unwrap(),
    );
    e.config.agents.insert(
        "custom-plan".into(),
        serde_json::from_value(json!({"base":"planner","prompt":"Plan only."})).unwrap(),
    );
    e.config.model_profiles.insert(
        "review-controls".into(),
        dial::llm::profiles::ModelProfile {
            provider: "reviewer".into(),
            model: "review-model".into(),
            capabilities: dial::llm::profiles::ModelCapabilities {
                reasoning: dial::llm::profiles::ReasoningApi::OpenaiEffort,
                reasoning_efforts: vec![dial::llm::profiles::ReasoningEffort::High],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert_eq!(
        run_turn(&e, &mut Recorder::default()).text,
        "Role routing verified."
    );
    let review_requests = review.requests.lock();
    assert_eq!(
        review_requests.len(),
        2,
        "reviewer and test runner must both use tester"
    );
    assert!(
        review_requests
            .iter()
            .all(|request| request["model"] == "review-model")
    );
    let custom = review_requests
        .iter()
        .find(|request| prompt_of(request) == "review-only")
        .unwrap();
    assert_eq!(custom["reasoning_effort"], "high");
    assert_eq!(plan.requests.lock()[0]["model"], "plan-model");
    let result = task_result(&main);
    assert!(
        result.contains("role tester, model reviewer/review-model"),
        "{result}"
    );
    assert!(
        result.contains("role planner, model reviewer/plan-model"),
        "{result}"
    );
}

#[test]
fn custom_subagents_in_one_role_keep_distinct_model_overrides_in_sync_and_background_runs() {
    for action in ["run", "start"] {
        let mut replies = vec![Reply::Call(
            "task",
            json!({"action":action,"tasks":[
                {"agent":"alpha","task":"alpha slice","solution_space":"s"},
                {"agent":"beta","task":"beta slice","solution_space":"s"}
            ]}),
        )];
        if action == "start" {
            replies.push(Reply::Call("task", json!({"action":"wait"})));
        }
        replies.push(Reply::Text("Both slices complete."));
        let main = serve(replies);
        let side = serve_fn(Arc::new(|_| Reply::Text("Slice complete.")), None, None);
        let mut e = env(&main.url, "auto");
        with_subagent_role(&mut e, &side.url);
        for (name, model) in [("alpha", "alpha-model"), ("beta", "beta-model")] {
            e.config.agents.insert(
                name.into(),
                serde_json::from_value(
                    json!({"base":"scout","prompt":"Investigate your slice.","model":[model]}),
                )
                .unwrap(),
            );
        }
        assert_eq!(
            run_turn(&e, &mut Recorder::default()).text,
            "Both slices complete."
        );
        let mut models: Vec<String> = side
            .requests
            .lock()
            .iter()
            .map(|request| request["model"].as_str().unwrap().to_owned())
            .collect();
        models.sort();
        assert_eq!(models, ["alpha-model", "beta-model"], "{action}");
        if action == "start" {
            let root = e.layout.state_dir.join("subagents");
            let files: Vec<_> = std::fs::read_dir(root)
                .unwrap()
                .flat_map(|entry| std::fs::read_dir(entry.unwrap().path()).unwrap())
                .map(|entry| entry.unwrap().path())
                .collect();
            let mut recorded: Vec<_> = files
                .iter()
                .map(|path| {
                    let text = std::fs::read_to_string(path).unwrap();
                    let end: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
                    assert_eq!(end["role"], "small");
                    assert_eq!(end["model_source"], "definition");
                    end["model"].as_str().unwrap().to_owned()
                })
                .collect();
            recorded.sort();
            assert_eq!(recorded, ["reviewer/alpha-model", "reviewer/beta-model"]);
        }
    }
}

#[test]
fn markdown_subagents_use_role_and_thinking_selectors_in_both_launch_paths() {
    for action in ["run", "start"] {
        let mut replies = vec![Reply::Call(
            "task",
            json!({
                "action":action,"agent":"file-review","task":"Review the slice","solution_space":"s"
            }),
        )];
        if action == "start" {
            replies.push(Reply::Call("task", json!({"action":"wait"})));
        }
        replies.push(Reply::Text("File reviewer complete."));
        let main = serve(replies);
        let side = serve(vec![Reply::Text("No findings in the assigned slice.")]);
        let mut e = env(&main.url, "auto");
        with_subagent_role(&mut e, &side.url);
        let mut selection = e.config.model_roles["small"].clone();
        selection.model = "file-review-model".into();
        e.config.model_roles.insert("tester".into(), selection);
        e.config.model_profiles.insert(
            "file-review-controls".into(),
            dial::llm::profiles::ModelProfile {
                provider: "reviewer".into(),
                model: "file-review-model".into(),
                capabilities: dial::llm::profiles::ModelCapabilities {
                    reasoning: dial::llm::profiles::ReasoningApi::OpenaiEffort,
                    reasoning_efforts: vec![dial::llm::profiles::ReasoningEffort::None],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let defs = e.project.path().join(".dial/agents");
        std::fs::create_dir_all(&defs).unwrap();
        std::fs::write(defs.join("file-review.md"),
            "---\nname: file-review\ndescription: reviews a slice\nmodel: ['@tester']\nthinking_level: off\n---\nReview your assigned slice."
        ).unwrap();
        assert_eq!(
            run_turn(&e, &mut Recorder::default()).text,
            "File reviewer complete."
        );
        let requests = side.requests.lock();
        assert_eq!(requests.len(), 1, "{action}: {}", task_result(&main));
        assert_eq!(requests[0]["model"], e.config.model_roles["tester"].model);
        assert_eq!(requests[0]["reasoning_effort"], "none");
        if action == "start" {
            let transcript = std::fs::read_dir(e.layout.state_dir.join("subagents"))
                .unwrap()
                .flat_map(|entry| std::fs::read_dir(entry.unwrap().path()).unwrap())
                .next()
                .unwrap()
                .unwrap()
                .path();
            let contents = std::fs::read_to_string(transcript).unwrap();
            let header: Value = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
            assert_eq!(header["role"], "tester");
            assert_eq!(header["model"], "reviewer/file-review-model");
        } else {
            assert!(task_result(&main).contains("No findings in the assigned slice."));
        }
    }
}

#[test]
fn assigned_local_subagent_model_never_reuses_the_parent_endpoint() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"agent":"reviewer","task":"Review code","solution_space":"s"}),
        ),
        Reply::Text("The requested local model is unavailable."),
    ]);
    let mut e = env(&main.url, "auto");
    let absent = e.project.path().join("absent-local-model");
    e.config.model_roles.insert(
        "tester".into(),
        dial::llm::model_config::ModelSelection {
            provider: "mlx".into(),
            model: absent.to_string_lossy().into_owned(),
            ..Default::default()
        },
    );
    assert_eq!(
        run_turn(&e, &mut Recorder::default()).text,
        "The requested local model is unavailable."
    );
    assert_eq!(
        main.requests.lock().len(),
        2,
        "The child must never run on the parent's endpoint"
    );
    let result = task_result(&main);
    assert!(result.contains("subagent provider unavailable"), "{result}");
    assert!(
        result.contains(&absent.to_string_lossy().to_string()),
        "{result}"
    );
}

#[test]
fn configured_subagent_fallback_is_used_before_work_but_never_replays_completed_tools() {
    for progressed in [false, true] {
        let main = serve(vec![
            Reply::Call(
                "task",
                json!({"agent":"routed","task":"Find the marker","solution_space":"s"}),
            ),
            Reply::Text("Worker result received."),
        ]);
        let mut responses = vec![];
        if progressed {
            responses.push(Reply::Call("read", json!({"path":"marker.txt"})));
        }
        responses.push(Reply::Http(402, "Insufficient credits"));
        let primary = serve(responses);
        let backup = serve(vec![Reply::Text("The marker is ORCHID.")]);
        let mut e = env(&main.url, "auto");
        std::fs::write(e.project.path().join("marker.txt"), "ORCHID\n").unwrap();
        e.config.retry.max_attempts = 1;
        for (role, url, model) in [
            ("small", &primary.url, "primary"),
            ("tester", &backup.url, "backup"),
        ] {
            e.config.model_roles.insert(
                role.into(),
                dial::llm::model_config::ModelSelection {
                    provider: "reviewer".into(),
                    model: model.into(),
                    base_url: Some(url.clone()),
                    ..Default::default()
                },
            );
        }
        e.config.agents.insert(
            "routed".into(),
            serde_json::from_value(
                json!({"base":"scout","prompt":"Find the marker.","model":["@small","@tester"]}),
            )
            .unwrap(),
        );
        run_turn(&e, &mut Recorder::default());
        if progressed {
            assert!(
                backup.requests.lock().is_empty(),
                "fallback must not restart a child after it has run tools"
            );
        } else {
            assert_eq!(backup.requests.lock().len(), 1);
            let result = task_result(&main);
            assert!(result.contains("ORCHID"), "{result}");
        }
    }
}

#[test]
fn permitted_descendants_share_one_slot_and_stop_at_the_depth_boundary() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"agent":"lead","task":"outer","solution_space":"s"}),
        ),
        Reply::Text("Delegation complete."),
    ]);
    let side = serve(vec![
        Reply::Call(
            "task",
            json!({"agent":"scout","task":"inner","solution_space":"s"}),
        ),
        Reply::Text("Nested evidence collected."),
        Reply::Text("Evidence reviewed."),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    e.config.task.max_concurrency = 1;
    e.config.task.max_recursion_depth = 2;
    e.config.agents.insert("lead".into(), serde_json::from_value(json!({"base":"scout","prompt":"Coordinate permitted scouts.","tools":["read","task"],"spawns":["scout"]})).unwrap());
    run_turn(&e, &mut Recorder::default());
    let requests = side.requests.lock();
    assert_eq!(requests.len(), 3);
    assert!(tool_names(&requests[0]).contains(&"task".into()));
    assert!(!tool_names(&requests[1]).contains(&"task".into()));
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("Nested evidence collected.")
    );
}

#[test]
fn finished_worker_resumes_its_saved_conversation_under_the_same_identity() {
    let main = serve(vec![Reply::Text("unused")]);
    let side = serve(vec![
        Reply::Text("Original finding: marker-731."),
        Reply::Text("Follow-up completed using marker-731."),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let start = a
        .start_subagents(
            &json!({"task":"record a finding","solution_space":"s"}),
            &mut ui,
        )
        .unwrap();
    let id = start[0]["id"].as_str().unwrap().to_owned();
    for round in 0..2 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = a.subagent_status(Some(&id), &mut ui).unwrap();
            if state["status"] != "running" {
                assert_eq!(state["status"], "done", "{state}");
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
        if round == 0 {
            let session_path = a.session().path().unwrap().to_owned();
            drop(a);
            a = Agent::interactive(
                &e.config,
                make_backend(&e.config).unwrap(),
                e.layout.clone(),
                Session::open(&session_path).unwrap(),
                None,
                None,
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
            let resumed = a.start_subagents(&json!({"action":"resume","id":id,"message":"Use the previous finding for the follow-up."}), &mut ui).unwrap();
            assert_eq!(resumed[0]["id"], id);
        }
    }
    let requests = side.requests.lock();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Original finding: marker-731.")
    );
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Use the previous finding")
    );
    assert_eq!(tool_names(&requests[0]), tool_names(&requests[1]));
}

/// Extraction requests carry the memory role's fixed system prompt and no tool list.
fn is_semantic_extraction(request: &Value) -> bool {
    request["messages"]
        .to_string()
        .contains("Extract durable facts")
}

/// Reports whether any semantic project database under `root` holds a live fact containing `needle`.
/// Unreadable or still-initializing databases count as "not yet".
fn semantic_fact_stored(root: &std::path::Path, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.is_dir() {
            if semantic_fact_stored(&path, needle) {
                return true;
            }
        } else if path
            .file_name()
            .is_some_and(|name| name == "memory.sqlite3")
            && path.components().any(|part| part.as_os_str() == "semantic")
        {
            let Ok(conn) = rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) else {
                continue;
            };
            let count: rusqlite::Result<i64> = conn.query_row(
                "SELECT count(*) FROM facts WHERE forgotten = 0 AND content LIKE ?1",
                [format!("%{needle}%")],
                |row| row.get(0),
            );
            if count.is_ok_and(|count| count > 0) {
                return true;
            }
        }
    }
    false
}

#[test]
fn semantic_memory_retains_a_turn_and_recalls_it_in_a_later_request() {
    const FACT: &str = "The billing sandbox database is named ledger-staging.";
    let server = serve_fn(
        Arc::new(|request| {
            if is_semantic_extraction(request) {
                return Reply::Say(
                    json!({"facts":[{"content":FACT,"scope":"project"}]}).to_string(),
                );
            }
            Reply::Text("Noted.")
        }),
        None,
        None,
    );
    let mut e = env(&server.url, "auto");
    e.config.memory.backend = dial::config::memory_backend::MemoryBackendId::Semantic;
    e.config.memory.backend_explicit = true;
    e.config.model_roles.insert(
        "memory".into(),
        dial::llm::model_config::ModelSelection {
            provider: "reviewer".into(),
            model: "scripted".into(),
            base_url: Some(server.url.clone()),
            ..Default::default()
        },
    );
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn(
        "Remember this for later: the billing sandbox database is named ledger-staging for this project.",
        &mut ui,
    )
    .unwrap();
    // Retention runs on the memory maintenance worker; drive it until the database shows the fact.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !semantic_fact_stored(e._home.path(), "ledger-staging")
        && !semantic_fact_stored(e.project.path(), "ledger-staging")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "turn 1 never retained the fact; {} requests were served; notices: {:?}",
            server.requests.lock().len(),
            ui.notices
        );
        a.maintenance_tick(&mut ui, false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    a.turn("Which database does the billing sandbox use?", &mut ui)
        .unwrap();
    let requests = server.requests.lock();
    assert!(
        requests.iter().any(is_semantic_extraction),
        "the memory role must have performed the extraction"
    );
    let answering = requests
        .iter()
        .rev()
        .find(|request| !is_semantic_extraction(request))
        .unwrap();
    assert!(
        answering["messages"].to_string().contains("ledger-staging"),
        "the later turn must receive the recalled fact: {answering}"
    );
}

#[test]
fn completion_evidence_keeps_pending_obligations_without_claiming_test_coverage() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[
                {"content":"Summarize the investigation"},
                {"content":"Implement separately requested change"}
            ]}),
        ),
        Reply::Text("The investigation found the owning implementation."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a
        .turn("Investigate the implementation", &mut Recorder::default())
        .unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    assert!(report.text.contains("progress item(s) remain open"));
    let control = a.session().control_state();
    let evidence = &control["completion_evidence"];
    assert_eq!(evidence["execution_outcome"], "completed");
    assert_eq!(evidence["verification_coverage"], "not_observed");
    assert_eq!(evidence["deliverable_coverage"], "open_obligations");
    assert_eq!(evidence["outstanding_tasks"].as_array().unwrap().len(), 1);
    assert_eq!(control["efficiency"]["completion_evidence"], *evidence);
    assert!(a.turn("", &mut Recorder::default()).is_err());
    assert!(a.session().control_state()["completion_evidence"].is_null());
}

#[test]
fn completion_evidence_repaired_shell_effects_replace_failing_baseline() {
    if skip_without_sandbox() {
        return;
    }
    let command = "test \"$(cat value.txt)\" = fixed";
    let server = serve(vec![
        Reply::Call("bash", json!({"command":"printf 'fixed\\n' > value.txt"})),
        Reply::Text("Repaired the value."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.automatic_checks = true;
    e.config.verification.commands.push(VerificationCommand {
        name: "value_behavior".into(),
        command: command.into(),
        file_extensions: vec!["txt".into()],
        timeout_secs: 5,
    });
    e.config
        .verification
        .behavioral_checks
        .push("value_behavior".into());
    std::fs::write(e.project.path().join("value.txt"), "broken\n").unwrap();
    let store = Store::open(&e.layout.state_dir, "scripted", "coding", &e.config.harness).unwrap();
    let mut a = agent_with(&e, Some(&store), Arc::new(AtomicBool::new(false)));
    let report = a.turn("Fix the value", &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Completed);
    let evidence = &a.session().control_state()["completion_evidence"]["checks"];
    assert_eq!(evidence["baseline"][command]["status"], "code_failed");
    assert_eq!(evidence["latest"][command]["status"], "passed");
    assert_eq!(evidence["passing_behavioral_checks"], 1);
    assert_eq!(store.verified_live_turns().unwrap(), 1);
}

#[test]
fn completion_evidence_surfaces_session_recovery_once_without_injecting_damaged_bytes() {
    let server = serve(vec![
        Reply::Text("First answer."),
        Reply::Text("Second answer."),
    ]);
    let e = env(&server.url, "auto");
    let mut session = Session::create(
        &e.layout,
        SessionMeta {
            model: "scripted".into(),
        },
    )
    .unwrap();
    session
        .append(json!({"role":"user","content":"Earlier question"}))
        .unwrap();
    let path = session.path().unwrap().to_owned();
    drop(session);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"DO_NOT_TRUST_JOURNAL\n").unwrap();
    }
    let resumed = Session::open(&path).unwrap();
    let mut a = Agent::interactive(
        &e.config,
        make_backend(&e.config).unwrap(),
        e.layout.clone(),
        resumed,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let mut ui = Recorder::default();
    a.turn("Answer the first question", &mut ui).unwrap();
    a.turn("Answer the second question", &mut ui).unwrap();
    assert_eq!(
        ui.notices
            .iter()
            .filter(|text| text.starts_with("Session recovery:"))
            .count(),
        1
    );
    assert_eq!(
        a.session()
            .messages()
            .iter()
            .filter(|m| m["dial_session_recovery"] == true)
            .count(),
        1
    );
    let requests = server.requests.lock();
    assert!(
        requests[0]["messages"]
            .to_string()
            .contains("resumed history may be incomplete")
    );
    assert!(
        !requests[0]["messages"]
            .to_string()
            .contains("DO_NOT_TRUST_JOURNAL")
    );
}
