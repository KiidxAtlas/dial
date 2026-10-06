//! The agent loop end to end against a scripted OpenAI-compatible server (no real model).
use dial::{
    agent::{Agent, Decision, TurnOutcome, Ui, make_backend},
    config::types::{Config, VerificationCommand},
    storage::paths::Layout,
    storage::session::{Session, SessionMeta},
    storage::store::Store,
    tools::ToolOutput,
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

enum Reply {
    Text(&'static str),
    Say(String),
    Call(&'static str, Value),
    RawCall(&'static str, String),
    Calls(Vec<(&'static str, Value)>),
    Cut,
    Think { reasoning: String, pause: Duration },
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

fn serve_fn(choose: Choose, token: Option<&'static str>, lm_context: Option<u32>) -> Scripted {
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
    let say = |text: &str| (json!({"role":"assistant","content":text}), "stop");
    let (delta, finish) = match choose(&request) {
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
        Reply::Cut => (json!({"role":"assistant","content":""}), "length"),
        Reply::Call(name, args) => (
            json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function",
            "function":{"name":name,"arguments":args.to_string()}}]}),
            "tool_calls",
        ),
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
    let usage = json!({"prompt_tokens":100,"completion_tokens":5,"total_tokens":105});
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

fn env(url: &str, approval: &str) -> Env {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let overlay: toml::Table = toml::from_str(&format!(
        "model = \"scripted\"\napproval = \"{approval}\"\nedit_mode = \"hashline\"\nintegrations = []\n[endpoint]\nbase_url = \"{url}\"\n[sandbox]\nmode = \"strict\"\n")).unwrap();
    let mut config = Config::load(&layout, Some(overlay)).unwrap();
    // Most scripted providers test one agent task, not the separate architecture maintenance call.
    config.codebase.synthesize_architecture = false;
    config.codebase.architecture_model.clear(); // Scripted architecture tests use their configured HTTP provider.
    Env {
        _home: home,
        project,
        layout,
        config,
    }
}

#[derive(Default)]
struct Recorder {
    calls: Vec<String>,
    approvals: Vec<String>,
    summaries: Vec<String>,
    notices: Vec<String>,
    roles: Vec<String>,
    deny: bool,
}

impl Ui for Recorder {
    fn tool_call(&mut self, name: &str, _: &str) {
        self.calls.push(name.into());
    }
    fn tool_result(&mut self, _: &str, _: &ToolOutput) {}
    fn notice(&mut self, message: &str) {
        self.notices.push(message.into());
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
    let session = Session::create(
        &e.layout,
        SessionMeta {
            model: "scripted".into(),
        },
    )
    .unwrap();
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
    e.config.model_roles.insert(
        "architecture".into(),
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
            .starts_with("Synthesize")
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
                .starts_with("Synthesize the project's architecture")
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
    let cache = Arc::new(dial::storage::codebase::Codebase::open(&e.layout).unwrap());
    let ws = dial::tools::Workspace::new(e.project.path()).unwrap();
    cache.refresh(&ws, &AtomicBool::new(false)).unwrap();
    let tool = dial::storage::codebase::tool(cache.clone(), "old".into());
    let saved=tool.call(&dial::tools::ext::ToolContext {workspace:&ws,config:&e.config},&json!({"action":"remember","topic":"entry ownership","summary":"Main owns dispatch","sources":["main.rs"]}));
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
    assert_eq!(server.requests.lock().len(), 3);
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
    std::fs::write(e.project.path().join("main.rs"), "fn main() {}\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("Explain", &mut QueuedUi).unwrap();
    assert_eq!(server.requests.lock().len(), 1);
    let cache = dial::storage::codebase::Codebase::open(&e.layout).unwrap();
    assert!(cache.architecture_synthesis(8000, false).unwrap().is_some());
}

fn close_todo() -> Reply {
    Reply::Call("todo", json!({"op":"done"}))
}

#[test]
fn investigation_gates_findings_redirects_duplicate_reads_and_survives_resume() {
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
        Reply::Call("todo", conclusion.clone()),
        Reply::Call("read", json!({"path":"marker.txt"})),
        close_todo(),
        Reply::Call("read", json!({"path":"marker.txt"})),
        Reply::Call("todo", conclusion),
        Reply::Call(
            "finish",
            json!({"outcome":"needs_input","summary":"Found the value","reason":"What value should replace five?"}),
        ),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("marker.txt"), "five\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    assert_eq!(
        a.turn("Investigate and update the value", &mut ui)
            .unwrap()
            .outcome,
        TurnOutcome::NeedsInput
    );
    let requests = server.requests.lock();
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("no tool has run since it started")
    );
    assert!(
        requests[4]["messages"]
            .to_string()
            .contains("Investigation done needs finding")
    );
    assert!(
        requests[5]["messages"]
            .to_string()
            .contains("This observation is unchanged")
    );
    assert!(
        requests[6]["messages"]
            .to_string()
            .contains("Recorded finding (agent observation")
    );
    assert!(
        ui.notices
            .iter()
            .any(|n| n.contains("unchanged observation"))
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
fn unfinished_todos_continue_into_real_edits_and_completion() {
    let server = serve(vec![
        open_todo(),
        Reply::Text("I will create it."),
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
            .contains("You stopped with unfinished todo items")
    );
    assert_eq!(a.metrics().turns_completed, 1);
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
            request["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("UNIQUE_SKILL_CATALOG_MARKER"),
            enabled
        );
    }
}

#[test]
fn text_only_acknowledgements_do_not_repeat_completion_reminders() {
    let server = serve(vec![
        open_todo(),
        Reply::Text("I'll get started."),
        Reply::Text("Continuing now."),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("create marker", &mut Recorder::default()).unwrap();
    assert!(
        report.text.contains("Stopped with unfinished work"),
        "{}",
        report.text
    );
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert_eq!(server.requests.lock().len(), 3);
    assert_eq!(a.metrics().turns_completed, 0);
    assert_eq!(
        a.todo().tasks()[0].status,
        dial::agent::todo::Status::InProgress
    );
}

#[test]
fn explicit_blockers_and_questions_preserve_open_todos() {
    for outcome in ["blocked", "needs_input"] {
        let server = serve(vec![
            open_todo(),
            Reply::Call(
                "finish",
                json!({
                    "outcome":outcome,"summary":"Need the destination.","reason":"Which directory should contain marker.txt?"
                }),
            ),
        ]);
        let e = env(&server.url, "auto");
        let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
        let report = a.turn("create marker", &mut Recorder::default()).unwrap();
        assert!(report.text.contains("Which directory"));
        assert_eq!(
            report.outcome,
            if outcome == "blocked" {
                TurnOutcome::Blocked
            } else {
                TurnOutcome::NeedsInput
            }
        );
        assert_eq!(server.requests.lock().len(), 2);
        assert_eq!(a.metrics().turns_completed, 0);
        assert_eq!(
            a.todo().tasks()[0].status,
            dial::agent::todo::Status::InProgress
        );
    }
}

#[test]
fn finish_completed_rejects_unfinished_todos() {
    let server = serve(vec![
        open_todo(),
        Reply::Call("finish", json!({"outcome":"completed","summary":"Done"})),
        Reply::Call("write", json!({"path":"marker.txt","content":"done"})),
        close_todo(),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Actually done"}),
        ),
    ]);
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("create marker", &mut Recorder::default()).unwrap();
    assert_eq!(report.text, "Actually done");
    assert!(
        server.requests.lock()[2]["messages"]
            .to_string()
            .contains("cannot finish completed with open todos")
    );
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
            json!({"outcome":"needs_input","summary":"Need a destination","reason":"Which directory?"}),
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
        TurnOutcome::NeedsInput
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
            json!({"outcome":"needs_input","summary":"Need a destination","reason":"Which directory?"}),
        ),
    ]);
    let e = env(&server.url, "ask");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    assert_eq!(
        a.turn("create marker", &mut ui).unwrap().outcome,
        TurnOutcome::NeedsInput
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
            json!({"outcome":"needs_input","summary":"Waiting on investigation","reason":"The live investigation is still running."}),
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
    a.start_subagents(&json!({"prompt":"inspect the project"}), &mut ui)
        .unwrap();
    assert!(
        !a.release_idle_models(),
        "a live child must retain the foreground compute reservation"
    );
    let report = a.turn("finish investigation", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::NeedsInput);
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
            if text.starts_with("Continuing:") {
                self.interrupt.store(true, Ordering::SeqCst);
            }
        }
    }
    let server = serve(vec![open_todo(), Reply::Text("I will do it")]);
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
    // Training context is the state BEFORE the failure: system + user only, no correction message.
    let roles: Vec<&str> = payload["context"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user"]);
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
fn matching_verified_repairs_become_advisory_context_in_the_user_turn() {
    let server = serve(vec![Reply::Text("I'll inspect the current file first.")]);
    let e = env(&server.url, "ask");
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
    assert_eq!(report.text, "I'll inspect the current file first.");
    let requests = server.requests.lock();
    let first_request = &requests[0]["messages"];
    assert!(
        !first_request[0]["content"]
            .as_str()
            .unwrap()
            .contains("verified repair patterns"),
        "the system prompt stays fixed"
    );
    let user = first_request[1]["content"].as_str().unwrap();
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
    let server = serve(vec![
        Reply::Text("first answer"),
        Reply::Text("second answer"),
        Reply::Text("third answer"),
        Reply::Text("SUMMARY: the user asked three things"),
    ]);
    let e = env(&server.url, "auto");
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    for q in ["one?", "two?", "three?"] {
        a.turn(q, &mut ui).unwrap();
    }
    a.compact(&mut ui).unwrap();
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
    assert!(
        messages[0]["content"].as_str().unwrap().contains("one?"),
        "the original user goal is pinned in the summary"
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
fn a_single_long_turn_can_be_compacted_instead_of_failing_with_nothing_to_compact() {
    // One user request followed by many large tool results has no earlier user message to cut at.
    let mut script: Vec<Reply> = (0..6)
        .map(|i| {
            Reply::Call(
                "read",
                json!({"path": format!("big.txt:{}-{}", i * 300 + 1, (i + 1) * 300)}),
            )
        })
        .collect();
    script.push(Reply::Text("all read"));
    script.push(Reply::Text("SUMMARY: read the whole big file"));
    let server = serve(script);
    let e = env(&server.url, "auto");
    let body: String = (0..1800)
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
        "summary leads the history"
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
        Reply::Call("task", json!({"prompt":"Inspect this project."})),
        Reply::Text("main finished"),
    ]);
    let side = serve(vec![Reply::Text("independent findings")]);
    let mut e = env(&main.url, "auto");
    e.config
        .harness
        .tools
        .extend(["task".into(), "skill".into()]);
    e.config.model_roles.insert(
        "subagent".into(),
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
fn automatic_compaction_uses_the_selected_role_during_a_live_turn() {
    let main = serve(vec![
        Reply::Text("first"),
        Reply::Text("second"),
        Reply::Text("third"),
    ]);
    let side = serve(vec![Reply::Text("summary of earlier project work")]);
    let mut e = env(&main.url, "auto");
    e.config.model_roles.insert(
        "compaction".into(),
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
    a.last_prompt_tokens = 30000;
    assert_eq!(a.turn("three?", &mut ui).unwrap().text, "third");
    assert_eq!(side.requests.lock()[0]["model"], "summary-model");
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
    // LM Studio loaded this model with 4096 tokens although Dial is configured for 32768.
    let server = serve_with(
        vec![
            Reply::Text("first"),
            Reply::Text("second"),
            Reply::Text("SUMMARY of the first two turns"),
            Reply::Text("third"),
        ],
        None,
        Some(4096),
    );
    let e = env(&server.url, "auto");
    assert_eq!(e.config.context_window, 32768);
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("one?", &mut ui).unwrap();
    a.turn("two?", &mut ui).unwrap();
    assert_eq!(
        a.context_window(),
        4096,
        "the loaded size, not the configured one, is the budget"
    );
    // 3300 tokens is only ~10% of the configured 32768 but over 80% of what is actually loaded.
    a.last_prompt_tokens = 3300;
    assert_eq!(a.turn("three?", &mut ui).unwrap().text, "third");
    assert!(
        a.session().messages()[0]["content"]
            .as_str()
            .unwrap()
            .contains("SUMMARY"),
        "history must have been compacted"
    );
}

fn summary_requests(server: &Scripted) -> usize {
    server
        .requests
        .lock()
        .iter()
        .filter(|r| {
            r["messages"][0]["content"]
                .as_str()
                .is_some_and(|c| c.contains("You compress conversations"))
        })
        .count()
}

#[test]
fn compaction_that_frees_too_little_compacts_again_harder() {
    // LM Studio loaded 4096 tokens. Turn two is big, so summarising only turn one frees almost nothing
    // (the provider reported 3300 tokens used, ~1700 of it fixed prompt): a single pass would leave the next
    // request over the limit.
    let server = serve_with(
        vec![
            Reply::Say("a".repeat(400)),
            Reply::Say("b".repeat(6000)),
            Reply::Text("SUMMARY ONE"),
            Reply::Text("SUMMARY TWO"),
            Reply::Text("third"),
        ],
        None,
        Some(4096),
    );
    let e = env(&server.url, "auto");
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("one?", &mut ui).unwrap();
    a.turn("two?", &mut ui).unwrap();
    a.last_prompt_tokens = 3300;
    assert_eq!(a.turn("three?", &mut ui).unwrap().text, "third");
    assert_eq!(summary_requests(&server), 2, "one pass, then a harder one");
    assert_eq!(a.metrics().compactions, 2);
    assert!(
        ui.notices
            .iter()
            .any(|n| n.contains("compacting older messages harder")),
        "{:?}",
        ui.notices
    );
    let messages = a.session().messages();
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("SUMMARY TWO")
    );
    assert!(
        !messages
            .iter()
            .any(|m| m["content"].as_str().is_some_and(|c| c.starts_with("bbbb"))),
        "the big turn was folded into the summary"
    );
}

#[test]
fn compaction_that_cannot_help_says_why_instead_of_summarising_again_and_again() {
    // The fixed prompt alone is ~3900 of 4096 tokens: no summary of a tiny conversation can make room.
    let server = serve_with(
        vec![
            Reply::Text("first"),
            Reply::Text("second"),
            Reply::Text("SUMMARY"),
            Reply::Text("third"),
        ],
        None,
        Some(4096),
    );
    let e = env(&server.url, "auto");
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("one?", &mut ui).unwrap();
    a.turn("two?", &mut ui).unwrap();
    a.last_prompt_tokens = 3900;
    assert_eq!(
        a.turn("three?", &mut ui).unwrap().text,
        "third",
        "the turn still runs"
    );
    assert_eq!(summary_requests(&server), 1, "no second summary of nothing");
    let notice = ui
        .notices
        .iter()
        .find(|n| n.contains("could not make enough room"))
        .unwrap_or_else(|| panic!("{:?}", ui.notices));
    assert!(
        notice.contains("fixed part") && notice.contains("integrations"),
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

// ------------------------------------------------------------------------------------ subagents

fn with_subagent_role(e: &mut Env, url: &str) {
    e.config
        .harness
        .tools
        .extend(["task".into(), "skill".into()]);
    e.config.model_roles.insert(
        "subagent".into(),
        dial::llm::model_config::ModelSelection {
            provider: "reviewer".into(),
            model: "side-model".into(),
            base_url: Some(url.into()),
            ..Default::default()
        },
    );
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

/// The task prompt a subagent was started with (the user message after its system prompt).
fn prompt_of(request: &Value) -> String {
    request["messages"][1]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn has_tool_result(request: &Value) -> bool {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool")
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
        ("reviewer", &["read", "glob", "grep", "bash", "skill"]),
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
            Reply::Call("task", json!({"agent": agent_type, "prompt": "look"})),
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
        let mut expected: Vec<String> = tools.iter().map(|name| name.to_string()).collect();
        expected.push("codebase_memory".into());
        expected.push("artifact_read".into());
        assert_eq!(tool_names(&request), expected, "{agent_type}");
        let system = request["messages"][0]["content"].as_str().unwrap();
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
            json!({"tasks": [{"prompt": "fine"}, {"agent": "nope", "prompt": "x"}]}),
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
            {"prompt": "first", "name": "alpha"}, {"agent": "reviewer", "prompt": "second"}, {"agent": "planner", "prompt": "third"}]}),
        ),
        Reply::Text("main finished"),
    ]);
    let mut e = env(&main.url, "auto");
    with_subagent_role(&mut e, &side.url);
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
    assert!(at("## 1. alpha (scout) - ok") < at("answer to first"));
    assert!(at("answer to first") < at("## 2. reviewer#2 (reviewer) - ok"));
    assert!(at("## 2. reviewer#2 (reviewer) - ok") < at("answer to second"));
    assert!(at("answer to second") < at("## 3. planner#3 (planner) - ok"));
    assert!(at("## 3. planner#3 (planner) - ok") < at("answer to third"));
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
    assert_eq!(ui.roles, ["subagent", "main"]);
    // Each child ran as its own conversation: only its own task prompt, never the parent's.
    let prompts: Vec<String> = side.requests.lock().iter().map(prompt_of).collect();
    assert_eq!(prompts.len(), 3);
    assert!(
        side.requests
            .lock()
            .iter()
            .all(|r| !r["messages"].to_string().contains("delegate"))
    );
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
            json!({"tasks": [{"prompt": "good"}, {"prompt": "stuck", "max_steps": 1}]}),
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
        result.contains("subagent failed: used all 1 steps without a final answer"),
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
            {"agent": "worker", "prompt": "edit"}, {"prompt": "scan-a"}, {"prompt": "scan-b"}]}),
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
            json!({"tasks": [{"prompt": "a", "name": "one"}, {"prompt": "b", "name": "two"}]}),
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
            json!({"agent": "worker", "prompt": "rewrite f.txt"}),
        ),
        Reply::Text("main finished"),
    ]);
    let side = serve(vec![
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
                json!({"agent": "worker", "prompt": "write out.txt"}),
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
        Reply::Call("task", json!({"agent": "reviewer", "prompt": "review"})),
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
fn subagents_cannot_start_subagents() {
    let main = serve(vec![
        Reply::Call("task", json!({"prompt": "outer"})),
        Reply::Text("ok"),
    ]);
    let side = serve(vec![
        Reply::Call("task", json!({"prompt": "inner"})),
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
            .contains("subagents cannot start subagents")
    );
    drop(requests);
    assert_eq!(task_result(&main), "gave up");
}

#[test]
fn structured_output_is_validated_and_retried_once_with_the_error() {
    let schema = json!({"type": "object", "required": ["count"], "properties": {"count": {"type": "integer"}}, "additionalProperties": false});
    // Recovers: prose first, then a fenced JSON value that satisfies the schema.
    let main = serve(vec![
        Reply::Call("task", json!({"prompt": "count", "output_schema": schema})),
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
    assert!(
        requests[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("JSON Schema")
    );
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
        Reply::Call("task", json!({"prompt": "count", "output_schema": schema})),
        Reply::Text("ok"),
    ]);
    let side = serve(vec![
        Reply::Text("{\"count\": \"x\"}"),
        Reply::Text("{\"count\": \"y\"}"),
        Reply::Text("{\"count\": 3}"),
    ]);
    let mut e = env(&main.url, "auto");
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
            json!({"tasks": [{"prompt": "a"}, {"prompt": "b"}, {"prompt": "c"}]}),
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
        Reply::Call("task", json!({"prompt": "x"})),
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
        .err()
        .expect("a preset named like an agent type must be refused");
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
        fn heartbeat(&mut self) {
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
        Reply::Call("task", json!({"agent":"worker","prompt":"inspect"})),
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
    assert_eq!(e.config.reasoning_guard.max_pre_action_secs, 60);
    let overlay = toml::from_str(
        "[reasoning_guard]\nenabled = false\nmax_pre_action_secs = 120\nmax_reasoning_bytes = 4096",
    )
    .unwrap();
    let mut config = Config::load(&e.layout, Some(overlay)).unwrap();
    assert!(!config.reasoning_guard.enabled);
    assert_eq!(config.reasoning_guard.max_reasoning_bytes, 4096);
    config.reasoning_guard.max_pre_action_secs = 0;
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
    let e = env(&server.url, "ask");
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
    let newest = requests[4]["messages"]
        .as_array()
        .unwrap()
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
fn independent_child_reuses_saved_codebase_context() {
    let main = serve(vec![
        Reply::Call("task", json!({"prompt":"inspect request router"})),
        Reply::Text("received"),
    ]);
    let side = serve(vec![Reply::Text("located")]);
    let mut e = env(&main.url, "auto");
    std::fs::write(
        e.project.path().join("router.rs"),
        "pub fn request_router() {}\n",
    )
    .unwrap();
    with_subagent_role(&mut e, &side.url);
    run_turn(&e, &mut Recorder::default());
    let child = side.requests.lock()[0].clone();
    assert_eq!(child["messages"][1]["content"], "inspect request router");
    assert!(
        child["messages"].to_string().contains("request_router")
            && child["messages"]
                .to_string()
                .contains("Saved codebase understanding")
    );
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
    e.config.reliability.enabled = false; // Exercise the existing identical-call guard independently of the earlier contract-round gate.
    std::fs::write(e.project.path().join("marker.txt"), "original\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a
        .turn("fix marker", &mut ui)
        .expect("a stuck turn is reported, not raised as an error");
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert!(
        report.text.contains("same arguments failed 4 times"),
        "{}",
        report.text
    );
    assert!(
        ui.notices
            .iter()
            .any(|n| n.contains("same arguments failed 4 times")),
        "{:?}",
        ui.notices
    );
    assert_eq!(server.requests.lock().len(), 7);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("marker.txt")).unwrap(),
        "original\n"
    );
    let messages = a.session().messages();
    assert_eq!(
        messages.iter().filter(|m| m["role"] == "tool").count(),
        7,
        "all tool results must be persisted before stopping"
    );
    assert_eq!(
        messages.last().unwrap()["role"],
        "assistant",
        "the reason is part of the conversation, so resuming sees why it stopped"
    );
    assert_eq!(a.metrics().turns_completed, 0);
}

#[test]
fn automatic_focused_checks_keep_pumping_the_ui() {
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
        "successful checks with artifact footers must not start repair turns"
    );
}

#[test]
fn cancellation_during_automatic_checks_stops_without_a_repair_turn() {
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
            }
        }
    }
    let server = serve(vec![
        Reply::Call("write", json!({"path":"marker.txt","content":"ok"})),
        Reply::Text("done"),
    ]);
    let mut e = env(&server.url, "auto");
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
    assert!(first_result.contains("Superseded observation"));
    let uris = dial::tools::artifact::uris(first_result);
    let first_uri = uris
        .first()
        .expect("superseded observation retains an artifact reference");
    let artifacts =
        dial::tools::artifact::ArtifactStore::new(&e.layout.state_dir.join("artifacts")).unwrap();
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
    let reopened = Session::open(a.session().path().unwrap()).unwrap();
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
    assert_eq!(
        requests[1]["messages"].as_array().unwrap().last().unwrap()["content"],
        "Also explain the risks."
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
        Reply::Call("bash", json!({"command":"mkdir -p one"})),
        Reply::Call("bash", json!({"command":"mkdir -p two"})),
        Reply::Call("bash", json!({"command":"rm -rf one"})),
        Reply::Text("done"),
    ]);
    let e = env(&server.url, "ask");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = AlwaysOnce { prompts: vec![] };
    a.turn("make directories", &mut ui).unwrap();
    assert_eq!(ui.prompts.len(), 2);
    assert!(ui.prompts[1].starts_with("rm -rf one"));
    assert!(e.project.path().join("one").is_dir());
    assert!(e.project.path().join("two").is_dir());
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
    let backend =
        make_backend(&e.config)
            .unwrap()
            .with_metadata(dial::llm::catalog::ModelMetadata {
                vision: Some(true),
                ..Default::default()
            });
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
    a.turn("read the image", &mut Recorder::default()).unwrap();
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
    e.config.verification.commands.push(VerificationCommand {
        name: "always-fails".into(),
        command: "false".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let result = a.turn("create marker", &mut ui);
    assert!(
        result.is_err(),
        "a failed final check must not become a successful turn"
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

#[test]
fn closing_a_step_needs_evidence_runs_focused_checks_and_a_failure_keeps_it_open() {
    let plan = json!({"op":"set","tasks":[{"content":"write marker"},{"content":"report"}]});
    let server = serve(vec![
        Reply::Call("todo", plan),
        Reply::Call("write", json!({"path":"marker.txt","content":"bad"})),
        close_todo(),
        Reply::Call("write", json!({"path":"marker.txt","content":"fixed"})),
        close_todo(),
        close_todo(),
        Reply::Calls(vec![
            ("read", json!({"path":"marker.txt"})),
            ("todo", json!({"op":"done"})),
        ]),
        Reply::Call(
            "finish",
            json!({"outcome":"completed","summary":"Marker fixed."}),
        ),
    ]);
    let mut e = env(&server.url, "auto");
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
    let requests = server.requests.lock();
    assert!(
        requests[3]["messages"]
            .to_string()
            .contains("Step not closed: focused checks failed"),
        "the failed step stays open with diagnostics"
    );
    assert!(
        requests[5]["messages"]
            .to_string()
            .contains("Focused checks passed: marker-fixed")
    );
    assert!(
        requests[6]["messages"]
            .to_string()
            .contains("no tool has run since it started"),
        "a step closed with no work stays open"
    );
    // One failed and one passing run at step boundaries; the last step made no edits and the turn-end
    // verification would only repeat a passing check.
    assert_eq!(
        ui.calls.iter().filter(|c| *c == "bash").count(),
        2,
        "{:?}",
        ui.calls
    );
    assert!(!a.todo().has_open());
}
#[test]
fn repeated_done_cannot_bypass_failed_or_unavailable_verification() {
    let server = serve(vec![
        Reply::Call(
            "todo",
            json!({"op":"set","tasks":[{"content":"write marker"}]}),
        ),
        Reply::Call("write", json!({"path":"marker.txt","content":"bad"})),
        close_todo(),
        close_todo(),
        Reply::Call(
            "finish",
            json!({"outcome":"blocked","summary":"Verification failed","reason":"The configured check did not pass."}),
        ),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.verification.commands.push(VerificationCommand {
        name: "always-fails".into(),
        command: "false".into(),
        file_extensions: vec![],
        timeout_secs: 10,
    });
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a.turn("create marker", &mut ui).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Blocked);
    assert!(
        a.todo().has_open(),
        "failed verification must leave the step open"
    );
    let requests = server.requests.lock();
    assert!(
        requests[3]["messages"]
            .to_string()
            .contains("Step not closed: focused checks failed")
    );
    assert!(
        requests[4]["messages"]
            .to_string()
            .contains("Step not closed: the last focused checks failed")
    );
    assert_eq!(
        ui.calls.iter().filter(|c| *c == "bash").count(),
        1,
        "a repeat done must not rerun an unchanged failed check"
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
            json!({"outcome":"needs_input","summary":"Paused","reason":"Which format?"}),
        ),
    ]);
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("notes.txt"), "hello\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("summarize notes", &mut Recorder::default()).unwrap();
    let requests = server.requests.lock();
    let (first, second) = (
        requests[1]["messages"].as_array().unwrap(),
        requests[2]["messages"].as_array().unwrap(),
    );
    let focus = first.last().unwrap();
    assert_eq!(focus["role"], "user");
    assert!(
        focus["content"]
            .as_str()
            .unwrap()
            .contains("Current step (0/2 done): 1. inspect notes."),
        "{focus}"
    );
    assert_eq!(
        first[..first.len() - 1],
        second[..first.len() - 1],
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
                    text,
                    is_error: false,
                },
                Err(text) => ToolOutput {
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
    let e = env(&server.url, "ask");
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
    let resumed = Session::open(a.session().path().unwrap()).unwrap();
    a.swap_session(resumed);
    assert!(a.tool_names().iter().any(|n| n == "sdk_inspect"));
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
            ("tool_load", json!({"names":["skill"]})),
            ("skill", json!({"name":"missing"})),
        ]),
        Reply::Call("tool_load", json!({"names":["task","not_a_tool"]})),
        Reply::Text("finished"),
    ]);
    let e = env(&server.url, "ask");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.turn("discover specialist tools", &mut Recorder::default())
        .unwrap();
    assert_eq!(a.metrics().tool_contract_errors, 1);
    assert!(a.tool_names().iter().any(|n| n == "skill"));
    assert!(!a.tool_names().iter().any(|n| n == "task"));
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
        Reply::Text("first done"),
        Reply::Text("second done"),
        Reply::Text("third done"),
        Reply::Text("compact summary"),
        Reply::Text("continued"),
    ]);
    let e = env(&server.url, "ask");
    std::fs::write(e.project.path().join("evidence.txt"), "old evidence\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    a.turn("first goal", &mut ui).unwrap();
    let before = server.requests.lock()[4]["messages"].to_string();
    assert!(before.contains("Superseded observation"));
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
            &json!({"prompt":"inspect shared files","name":"live"}),
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
        .start_subagents(&json!({"prompt":"cancel this investigation"}), &mut ui)
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
            &json!({"agent":"worker","prompt":"write worker.txt"}),
            &mut ui
        )
        .is_err()
    );
    assert!(!e.project.path().join("worker.txt").exists());
    a.set_plan_mode(false);
    let started = a
        .start_subagents(
            &json!({"agent":"worker","prompt":"write worker.txt"}),
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
            &json!({"agent":"worker","prompt":"write blocked.txt"}),
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
            &json!({"agent":"worker","prompt":"write cancelled.txt"}),
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
    let memory = dial::storage::provenance::ProvenanceMemory::open(&e.layout).unwrap();
    memory
        .retain(
            dial::storage::provenance::Scope::Project,
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
    let resumed = Session::open(a.session().path().unwrap()).unwrap();
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
                    json!({"query":"reuse an existing value without doing the expensive work again"}),
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
            json!({"query":"reuse an existing value without doing the expensive work again"}),
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
}

#[test]
fn delegated_semantic_find_reuses_the_child_backend_and_request_budget() {
    let main = serve(vec![
        Reply::Call(
            "task",
            json!({"prompt":"locate reuse behavior","max_steps":3}),
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
                    json!({"query":"reuse an existing value without doing the expensive work again"}),
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
    e.config.harness.tools.push("retain".into());
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
    assert_eq!(
        messages.iter().filter(|m| m["role"] == "system").count(),
        1,
        "no extra system messages ahead of the conversation"
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
    assert_eq!(report.from_successes, 1);
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
    e.config.harness.tools.push("retain".into());
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
            pair[0]["messages"].as_array().unwrap(),
            pair[1]["messages"].as_array().unwrap(),
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
    // Local models routinely write a file's real line breaks into the JSON string. Strict parsing rejected the call and
    // made the model generate the whole payload again; the repair keeps what it meant.
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
        "one call, one answer: nothing was regenerated"
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
    let e = env(&server.url, "auto");
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
    let first_user = requests[0]["messages"][1]["content"].as_str().unwrap();
    assert!(
        first_user.contains("Relevant verified repair patterns")
            && first_user.contains("edit")
            && !first_user.contains("SECRET_VALUE"),
        "{first_user}"
    );
    for (n, pair) in requests.windows(2).enumerate() {
        let (before, after) = (
            pair[0]["messages"].as_array().unwrap(),
            pair[1]["messages"].as_array().unwrap(),
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
    // Each round reads the file successfully and then guesses again, so no round fails completely and only the
    // per-tool streak can catch this: the read-guess-read-guess loop.
    let guesses = (1..=12)
        .map(|n| {
            Reply::Calls(vec![
                ("read", json!({"path":"f.txt"})),
                (
                    "edit",
                    json!({"path":"f.txt","old":format!("guess number {n}"),"new":"x"}),
                ),
            ])
        })
        .collect();
    let server = serve(guesses);
    let mut e = env(&server.url, "auto");
    e.config.edit_mode = dial::config::types::EditModeConfig::Replace; // These are valid replace calls with unmatched text, not schema failures.
    std::fs::write(e.project.path().join("f.txt"), "the real text\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder::default();
    let report = a
        .turn("fix f.txt", &mut ui)
        .expect("a stalled turn reports itself instead of erroring");
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert!(
        report.text.starts_with("Stopped:")
            && report.text.contains("`edit` failed 10 times in a row"),
        "{}",
        report.text
    );
    assert!(
        ui.notices.iter().any(|n| n.starts_with("Stopped:")),
        "the user is told, not just the transcript: {:?}",
        ui.notices
    );
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 10, "no eleventh request after the stop");
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
        "the real text\n"
    );
    let last = a.session().messages().pop().unwrap();
    assert_eq!(
        (
            last["role"].as_str(),
            last["content"].as_str().map(|t| t.starts_with("Stopped:"))
        ),
        (Some("assistant"), Some(true))
    );
    assert_eq!(a.metrics().turns_completed, 0);
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
    let e = env(&server.url, "auto");
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("read the notes", &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert!(
        report
            .text
            .contains("every tool call failed for 8 rounds in a row"),
        "{}",
        report.text
    );
    let requests = server.requests.lock();
    assert_eq!(requests.len(), 8);
    let after_fourth = requests[4]["messages"].as_array().unwrap();
    assert!(
        after_fourth.iter().any(|m| m["role"] == "user"
            && m["content"]
                .as_str()
                .is_some_and(|c| c.starts_with("4 rounds of tool calls"))),
        "warned after the fourth round"
    );
    assert!(
        !requests[3]["messages"]
            .to_string()
            .contains("rounds of tool calls in a row")
    );
}

#[test]
fn a_big_tool_result_triggers_compaction_before_the_request_it_would_overflow() {
    // The server reports 100 prompt tokens for the previous request, far under 80% of the 4096 loaded. The new user
    // message alone is ~3500 tokens, which the last report knew nothing about.
    let server = serve_with(
        vec![
            Reply::Say("a".repeat(200)),
            Reply::Say("b".repeat(200)),
            Reply::Text("SUMMARY"),
            Reply::Text("third"),
        ],
        None,
        Some(4096),
    );
    let e = env(&server.url, "auto");
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    let mut ui = Recorder::default();
    a.turn("one?", &mut ui).unwrap();
    a.turn("two?", &mut ui).unwrap();
    assert!(
        a.last_prompt_tokens > 0 && a.last_prompt_tokens < 1000,
        "{}",
        a.last_prompt_tokens
    );
    let report = a.turn(&"c".repeat(14_000), &mut ui).unwrap();
    assert_eq!(report.text, "third");
    assert_eq!(
        summary_requests(&server),
        1,
        "compacted first, from the estimate, not from a stale report"
    );
    assert_eq!(a.metrics().compactions, 1);
    let messages = a.session().messages();
    assert!(messages[0]["content"].as_str().unwrap().contains("SUMMARY"));
}

#[test]
fn a_session_switch_forgets_the_previous_sessions_token_count() {
    let server = serve_with(vec![Reply::Text("hi")], None, Some(4096));
    let e = env(&server.url, "auto");
    let mut backend = make_backend(&e.config).unwrap();
    let mut a = agent(&e, &mut backend, None);
    a.record_prompt_usage(4000);
    a.swap_session(Session::ephemeral());
    assert_eq!(a.last_prompt_tokens, 0);
}

#[test]
fn extended_inspection_requires_real_todos_and_allows_discovered_work() {
    let mut script: Vec<_> = (0..6)
        .map(|n| Reply::Call("read", json!({"path":format!("{n}.txt")})))
        .collect();
    script.extend([
        Reply::Call("write", json!({"path":"must-not-exist.txt","content":"bypass plan"})),
        Reply::Call("todo", json!({"op":"set","tasks":[{"index":1,"content":"Find the check","investigation":true}]})),
        Reply::Call("todo", json!({"op":"set","tasks":[{"content":"Which check establishes correctness?","investigation":true}]})),
        Reply::Call("read", json!({"path":"0.txt"})),
        Reply::Call("todo", json!({"op":"append","tasks":[{"content":"Check the newly discovered boundary"}]})),
        Reply::Call("finish", json!({"outcome":"needs_input","summary":"Need the expected boundary","reason":"What value should the boundary accept?"})),
    ]);
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
    assert_eq!(report.outcome, TurnOutcome::NeedsInput);
    assert!(
        !e.project.path().join("must-not-exist.txt").exists(),
        "non-planning calls cannot bypass the checkpoint"
    );
    let requests = server.requests.lock();
    let names = |n: usize| {
        requests[n]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(6), ["todo"]);
    assert_eq!(
        names(7),
        ["todo"],
        "a rejected bypass does not remove the checkpoint"
    );
    assert_eq!(
        names(8),
        ["todo"],
        "invalid todo cannot release the checkpoint"
    );
    assert!(
        requests[7]["messages"]
            .to_string()
            .contains("temporarily withheld by the investigation checkpoint")
    );
    assert!(
        requests[8]["messages"]
            .to_string()
            .contains("Task numbers are assigned automatically")
    );
    assert!(
        requests[8]["messages"]
            .to_string()
            .contains("Example valid todo arguments")
    );
    assert!(
        names(9).contains(&"read"),
        "normal tools return after an actual plan is created"
    );
    assert_eq!(
        a.todo().tasks()[0].content,
        "Which check establishes correctness?"
    );
    assert_eq!(
        a.todo().tasks()[1].content,
        "Check the newly discovered boundary"
    );
}

#[test]
fn independent_training_failure_gets_one_repair_and_only_success_is_harvestable() {
    if dial::tools::sandbox::already_sandboxed() {
        return;
    }
    let server = serve(vec![Reply::Text("wrong"), Reply::Text("42")]);
    let state = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.endpoint = Some(dial::config::types::EndpointConfig {
        base_url: server.url.clone(),
        api_key_env: None,
        allow_insecure: false,
    });
    config.model = "test-model".into();
    config.state_dir = state.path().into();
    config.project_root = project.path().into();
    let store = Store::open(state.path(), &config.model, "test", &config.harness).unwrap();
    let passed = dial::agent::selfplay_run(
        &mut dial::agent::Runtime::new(),
        &config,
        &store,
        &store.active().unwrap(),
        "repair-test",
        "Return 42",
        &Default::default(),
        &dial::learn::verify::Check::AnswerExact {
            expected: "42".into(),
        },
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
    let main_step = AtomicUsize::new(0);
    let server = serve_fn(
        Arc::new(move |request| {
            let main = request["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|t| t["function"]["name"] == "solve"));
            if main {
                return match main_step.fetch_add(1, Ordering::SeqCst) {
                    0 => Reply::Call(
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
            if messages.iter().any(|m| m["role"] == "tool") {
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
    assert!(
        requests.last().unwrap()["messages"]
            .to_string()
            .contains("\\\"winner\\\":1")
    );
}

#[test]
fn escalation_rejects_nonlocal_route_before_disclosing_evidence() {
    let server = serve(vec![
        Reply::Call(
            "escalate",
            json!({"goal":"Fix behavior","question":"Which invariant is missing?",
            "evidence":["PRIVATE_PROJECT_EVIDENCE"],"rejected_hypotheses":["syntax alone"]}),
        ),
        Reply::Text("Remote escalation requires operator opt-in."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.escalation.enabled = true;
    e.config.harness.tools.push("escalate".into());
    e.config.model_roles.insert(
        "stronger".into(),
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
    assert_eq!(server.requests.lock().len(), 2);
    assert!(a.session().messages().iter().any(|m| {
        m["role"] == "tool"
            && m["content"]
                .as_str()
                .is_some_and(|s| s.contains("allow_remote"))
    }));
}

#[test]
fn behavioral_repairs_retain_failure_and_pass_without_marking_task_success() {
    let command = "test \"$(cat value.txt)\" = fixed";
    let server = serve(vec![
        Reply::Call("bash", json!({"command":command})),
        Reply::Call("write", json!({"path":"value.txt","content":"fixed\n"})),
        Reply::Call("bash", json!({"command":command})),
        Reply::Text("The declared behavioral check passes."),
    ]);
    let mut e = env(&server.url, "auto");
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
        dial::tools::artifact::ArtifactStore::new(&e.layout.state_dir.join("artifacts")).unwrap();
    assert!(
        artifacts
            .retrieve(before_ref, 1, 10)
            .unwrap()
            .contains("1:broken\n")
    );
}

#[test]
fn harness_records_behavioral_failure_before_the_model_edits() {
    if dial::tools::sandbox::already_sandboxed() {
        return;
    }
    let command = "test \"$(cat value.txt)\" = fixed";
    let server = serve(vec![
        Reply::Call("write", json!({"path":"value.txt","content":"fixed\n"})),
        Reply::Text("Fixed."),
    ]);
    let mut e = env(&server.url, "auto");
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
        json!({"agent":"worker","prompt":"Implement a fix"}),
        json!({"agent":"scout","prompt":"Inspect the code"}),
        json!({"tasks":[{"agent":"scout","prompt":"Inspect A"},{"agent":"scout","prompt":"Inspect B"}]}),
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
        let mut e = env(&server.url, "auto");
        e.config.harness.tools.push("task".into());
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
    let server = serve(vec![
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
    let stale = serve(vec![
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
        stale.requests.lock()[1]["messages"]
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
    let main = serve(vec![
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
        "experience".into(),
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
        replay.requests.lock()[1]["messages"]
            .to_string()
            .contains("denied")
    );
}

#[test]
fn experience_does_not_capture_untracked_shell_mutations() {
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
fn adaptive_settings_follow_real_reads_edits_checks_and_failures_without_extra_generations() {
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
    e.config.verification.enabled = false;
    e.config.adaptive.sampling = true;
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
    let e = env(&server.url, "auto");
    std::fs::write(e.project.path().join("existing.txt"), "evidence").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let report = a.turn("Fix a fixture", &mut Recorder::default()).unwrap();
    assert_eq!(report.outcome, TurnOutcome::Incomplete);
    assert!(report.text.contains("consecutive tool-contract failures"));
    assert!(!e.project.path().join("must-not-exist.txt").exists());
    assert_eq!(server.requests.lock().len(), 3);
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
            .contains("last focused checks failed")
    );
    assert!(
        a.session().control_state()["reliability"]["automatic_failing_checks"]
            .as_u64()
            .unwrap()
            > 0
    );
}
