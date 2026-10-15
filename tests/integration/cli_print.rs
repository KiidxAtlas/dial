//! Exercise the real executable and its stdout protocol with a local scripted provider.
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub(super) struct Server {
    pub(super) url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    pub(super) fn new(replies: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (done, seen) = (stop.clone(), requests.clone());
        let worker = thread::spawn(move || {
            let mut replies: VecDeque<_> = replies.into();
            while !done.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => respond(stream, &mut replies, &seen, &done),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn respond(
    mut stream: TcpStream,
    replies: &mut VecDeque<Value>,
    requests: &Mutex<Vec<Value>>,
    stop: &AtomicBool,
) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader
        .read_line(&mut first)
        .expect("reading HTTP request line")
        == 0
    {
        return;
    }
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap();
        }
    }
    if first.starts_with("GET ") {
        let _ = write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        return;
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let request: Value = serde_json::from_slice(&body).unwrap();
    let streaming = request["stream"] == true;
    requests.lock().push(request);
    let delta = replies
        .pop_front()
        .unwrap_or_else(|| json!({"role":"assistant", "content":"done"}));
    if delta["wait_for_interrupt"] == true {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
        );
        // Request-side EOF is not response cancellation: clients may half-close after sending.
        // Keep this intentionally unfinished SSE response alive until the fixture is dropped.
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(5));
        }
        return;
    }
    if delta["http_error"] == true {
        let payload = "{\"error\":{\"message\":\"scripted failure\"}}";
        let _ = write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        return;
    }
    let finish = if delta.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    let usage = json!({"prompt_tokens":7,"completion_tokens":3});
    let (kind, payload) = if streaming {
        (
            "text/event-stream",
            format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}),
                json!({"choices":[],"usage":usage})
            ),
        )
    } else {
        (
            "application/json",
            json!({"choices":[{"message":delta,"finish_reason":finish}],"usage":usage}).to_string(),
        )
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
}
pub(super) fn text(value: &str) -> Value {
    json!({"role":"assistant","content":value})
}
pub(super) fn call(name: &str, args: Value) -> Value {
    json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":name,"arguments":args.to_string()}}]})
}
struct Env {
    home: tempfile::TempDir,
    project: tempfile::TempDir,
}
impl Env {
    fn new() -> Self {
        let env = Self {
            home: tempfile::tempdir().unwrap(),
            project: tempfile::tempdir().unwrap(),
        };
        std::fs::write(
            env.home.path().join("config.toml"),
            "integrations = []\n[improve]\nenabled = false\n",
        )
        .unwrap();
        env
    }
    fn run(&self, server: &Server, args: &[&str], stdin: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dial"))
            .env("DIAL_HOME", self.home.path())
            .env("NO_COLOR", "1")
            .current_dir(self.project.path())
            .args([
                "-p",
                "--endpoint",
                &server.url,
                "--model",
                "scripted",
                "--sandbox",
                "off",
            ])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}
fn events(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("non-JSON stdout: {line:?}: {e}"))
        })
        .collect()
}

#[test]
fn print_results_distinguish_completed_blocked_and_unfinished_work() {
    for (outcome, expected_code, expected_text) in [
        ("completed", 0, "Finished."),
        ("blocked", 2, "Blocked:"),
        ("needs_input", 2, "Needs input:"),
    ] {
        let e = Env::new();
        let server = Server::new(vec![call(
            "finish",
            json!({"outcome":outcome,"summary":"Finished.","reason":"Which destination?"}),
        )]);
        let output = e.run(&server, &["--output-format", "json", "status"], "");
        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result = &events(&output)[0];
        assert_eq!(result["outcome"], outcome);
        assert_eq!(result["ok"], outcome == "completed");
        assert!(result["text"].as_str().unwrap().contains(expected_text));
    }
    let e = Env::new();
    let server = Server::new(vec![
        call(
            "todo",
            json!({"op":"set","tasks":[{"content":"create marker"}]}),
        ),
        text("I will get started."),
        text("Working on it."),
        text("Working on it."),
        text("Working on it."),
    ]);
    let output = e.run(&server, &["--output-format", "json", "create marker"], "");
    assert_eq!(output.status.code(), Some(2));
    let result = &events(&output)[0];
    assert_eq!(result["outcome"], "incomplete");
    assert_eq!(result["ok"], false);
    assert!(
        result["text"]
            .as_str()
            .unwrap()
            .contains("Stopped with incomplete work")
    );
    // Tool call, one actionable promise, then a vague final reply with an unfinished plan.
    assert_eq!(server.requests.lock().len(), 5);
}

#[test]
fn ordinary_print_turn_keeps_disabled_learning_and_internal_policy_out_of_output() {
    let env = Env::new();
    let server = Server::new(vec![text("4")]);
    let output = env.run(
        &server,
        &["--output-format", "stream-json", "What is 2 + 2?"],
        "",
    );
    assert_eq!(output.status.code(), Some(0));
    let frames = events(&output);
    assert_eq!(frames.last().unwrap()["text"], "4");
    assert!(
        frames.iter().all(|frame| frame["type"] != "notice"),
        "unexpected coding notices: {frames:?}"
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn text_json_and_stream_json_preserve_control_characters_and_stdout_contract() {
    let answer = "quote \" slash \\ newline\n tab\t control\u{1}";
    for mode in ["text", "json", "stream-json"] {
        let e = Env::new();
        let server = Server::new(vec![text(answer)]);
        let out = e.run(&server, &["--output-format", mode, "hello"], "");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if mode == "text" {
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                format!("{answer}\n")
            );
            continue;
        }
        let rows = events(&out);
        let result = rows.last().unwrap();
        assert_eq!(result["type"], "result");
        assert_eq!(result["ok"], true);
        assert_eq!(result["text"], answer);
        assert_eq!(result["usage"]["prompt_tokens"], 7);
        assert_eq!(result["usage"]["output_tokens"], 3);
        assert!(
            result["usage"]["cache_read_tokens"].is_null(),
            "unknown cache usage must not be fabricated"
        );
        assert!(result["usage"]["cache_write_tokens"].is_null());
        assert!(result["session_id"].is_string());
        if mode == "json" {
            assert_eq!(rows.len(), 1);
        } else {
            assert_eq!(rows.first().unwrap()["type"], "system");
            assert_eq!(
                rows.iter()
                    .filter(|r| r["type"] == "text")
                    .map(|r| r["delta"].as_str().unwrap())
                    .collect::<String>(),
                answer
            );
        }
    }
}

#[test]
fn named_approval_controls_mutations_and_reports_denial_exit_code() {
    for (approval, allow_flag, allowed) in [
        ("ask", false, false),
        ("ask", true, true),
        ("readonly", true, false),
    ] {
        let e = Env::new();
        let server = Server::new(vec![
            call("write", json!({"path":"out.txt","content":"approved"})),
            text("finished"),
        ]);
        let mut args = vec!["--output-format", "stream-json", "--approval", approval];
        if allow_flag {
            args.extend(["--allow", "write"]);
        }
        args.push("write out.txt");
        let out = e.run(&server, &args, "");
        assert_eq!(
            out.status.code(),
            Some(if allowed { 0 } else { 3 }),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(e.project.path().join("out.txt").exists(), allowed);
        let rows = events(&out);
        let result = rows.last().unwrap();
        assert_eq!(result["tool_calls"][0]["name"], "write");
        assert_eq!(result["tool_calls"][0]["is_error"], !allowed);
        let tool = rows.iter().find(|r| r["type"] == "tool_result").unwrap();
        assert_eq!(tool["is_error"], !allowed);
        assert!(tool["seconds"].as_f64().unwrap() >= 0.0);
        if !allowed {
            assert!(String::from_utf8_lossy(&out.stderr).contains("--allow write"));
        }
    }
}

#[test]
fn stdin_is_prompt_or_fenced_context_and_resume_reuses_conversation() {
    let e = Env::new();
    let server = Server::new(vec![text("first"), text("second"), text("third")]);
    let first = e.run(&server, &["--output-format", "json"], "piped prompt");
    assert!(first.status.success());
    let id = events(&first)[0]["session_id"].as_str().unwrap().to_owned();
    let second = e.run(
        &server,
        &["--output-format", "json", "--resume", &id, "next"],
        "inside ``` fence",
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(events(&second)[0]["session_id"], id);
    let third = e.run(
        &server,
        &["--output-format", "json", "--continue", "again"],
        "",
    );
    assert!(third.status.success());
    assert_eq!(events(&third)[0]["session_id"], id);
    let requests = server.requests.lock();
    let messages = requests.last().unwrap()["messages"].as_array().unwrap();
    let contents: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(contents.contains(&"piped prompt"));
    assert!(
        contents
            .iter()
            .any(|text| text.contains("next\n\nStdin context:\n````\ninside ``` fence\n````"))
    );
}

#[test]
fn errors_and_empty_answers_have_distinct_exit_statuses_and_json_results() {
    // One original empty stop plus EMPTY_STOP_MAX_RETRIES (3) retries reaches the cap.
    for (replies, code) in [
        (vec![text(""); 4], 2),
        (vec![json!({"http_error":true})], 1),
    ] {
        let e = Env::new();
        let server = Server::new(replies);
        let out = e.run(&server, &["--output-format", "json", "hello"], "");
        assert_eq!(
            out.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rows = events(&out);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["ok"], false);
        if code == 1 {
            assert!(rows[0]["error"].as_str().unwrap().contains("400"));
        }
    }
    let e = Env::new();
    let server = Server::new(vec![]);
    let out = e.run(
        &server,
        &[
            "--output-format",
            "json",
            "--resume",
            "not-a-session",
            "hello",
        ],
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        events(&out)[0]["error"]
            .as_str()
            .unwrap()
            .contains("no session")
    );
    let out = e.run(
        &server,
        &["--output-format", "json", "--approval", "invalid", "hello"],
        "",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(events(&out)[0]["error"].is_string());
}

#[test]
fn sessions_cli_renames_exports_and_lists_across_projects() {
    use dial::storage::{
        paths::Layout,
        session::{Session, SessionMeta},
    };
    let e = Env::new();
    let layout = Layout::with_user_dir(e.project.path(), e.home.path().into()).unwrap();
    let mut session = Session::create(
        &layout,
        SessionMeta {
            model: "scripted".into(),
        },
    )
    .unwrap();
    session
        .append(json!({"role":"user","content":"A <tag> question"}))
        .unwrap();
    session.append(text("An answer")).unwrap();
    let id = session.id().to_owned();
    // The CLI refuses to rename or resume a session another writer still owns; release ours first.
    drop(session);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dial"))
            .env("DIAL_HOME", e.home.path())
            .current_dir(e.project.path())
            .args(args)
            .output()
            .unwrap()
    };
    let renamed = run(&["sessions", "rename", &id, "Chosen title"]);
    assert!(
        renamed.status.success(),
        "{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    let list = run(&["sessions"]);
    assert!(String::from_utf8_lossy(&list.stdout).contains("Chosen title"));
    let exported = run(&["sessions", "export", &id, "conversation.md"]);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert!(
        std::fs::read_to_string(e.project.path().join("conversation.md"))
            .unwrap()
            .contains("An answer")
    );
    let duplicate = run(&["sessions", "export", &id, "conversation.md"]);
    assert_eq!(
        duplicate.status.code(),
        Some(1),
        "existing exports must not be overwritten"
    );
    let other = tempfile::tempdir().unwrap();
    let elsewhere = Layout::with_user_dir(other.path(), e.home.path().into()).unwrap();
    let mut remote = Session::create(
        &elsewhere,
        SessionMeta {
            model: "scripted".into(),
        },
    )
    .unwrap();
    remote.rename("Other project").unwrap();
    let local = run(&["sessions"]);
    assert!(!String::from_utf8_lossy(&local.stdout).contains("Other project"));
    let all = run(&["sessions", "--all"]);
    assert!(String::from_utf8_lossy(&all.stdout).contains("Other project"));
}

#[cfg(unix)]
#[test]
fn sigint_during_inference_emits_an_interrupted_result() {
    let e = Env::new();
    let server = Server::new(vec![json!({"wait_for_interrupt":true})]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_dial"))
        .env("DIAL_HOME", e.home.path())
        .current_dir(e.project.path())
        .args([
            "-p",
            "--endpoint",
            &server.url,
            "--model",
            "scripted",
            "--output-format",
            "json",
            "hello",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server.requests.lock().is_empty() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "provider request never arrived: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "SIGINT did not stop inference: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = events(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["interrupted"], true);
    assert_eq!(rows[0]["ok"], false);
}

struct RpcChild {
    child: std::process::Child,
    input: std::process::ChildStdin,
    events: std::sync::mpsc::Receiver<Value>,
    pending: std::cell::RefCell<std::collections::BTreeMap<u64, Value>>,
}
impl Drop for RpcChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl RpcChild {
    fn start(env: &Env, server: &Server) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dial"))
            .args([
                "rpc",
                "--endpoint",
                &server.url,
                "--model",
                "scripted",
                "--sandbox",
                "off",
            ])
            .env("DIAL_HOME", env.home.path())
            .current_dir(env.project.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, events) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let event: Value = serde_json::from_str(&line.unwrap()).unwrap();
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        let rpc = Self {
            child,
            input,
            events,
            pending: Default::default(),
        };
        assert_eq!(rpc.next()["event"], "ready");
        rpc
    }
    fn next(&self) -> Value {
        self.events
            .recv_timeout(Duration::from_secs(10))
            .expect("RPC event timeout")
    }
    fn send(&mut self, id: u64, method: &str, params: Value) {
        writeln!(
            self.input,
            "{}",
            json!({"id":id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
    }
    fn result(&self, id: u64) -> Value {
        if let Some(event) = self.pending.borrow_mut().remove(&id) {
            assert!(event.get("error").is_none(), "{event}");
            return event["result"].clone();
        }
        loop {
            let event = self.next();
            if event["id"] == id {
                assert!(event.get("error").is_none(), "{event}");
                return event["result"].clone();
            }
            if let Some(other) = event["id"].as_u64() {
                self.pending.borrow_mut().insert(other, event);
            }
        }
    }
}

#[test]
fn rpc_projects_shared_results_and_exposes_a_resolvable_execution_tree() {
    let env = Env::new();
    let server = Server::new(vec![
        call("todo", json!({"op":"view"})),
        text("shared answer"),
    ]);
    let mut rpc = RpcChild::start(&env, &server);
    rpc.send(1, "send", json!({"text":"inspect the todo list"}));
    let mut events = Vec::new();
    loop {
        let event = rpc.next();
        if event["id"] == 1 {
            assert_eq!(event["result"]["text"], "shared answer");
            break;
        }
        events.push(event);
    }
    let settled = events
        .iter()
        .find(|e| {
            e["event"] == "state_node"
                && e["node"]["label"] == "todo"
                && e["node"]["result"]["phase"] == "completed"
        })
        .unwrap();
    assert_eq!(settled["node"]["result"]["data"]["tasks"], json!([]));
    assert_eq!(settled["node"]["result"]["text"], "No tasks.");
    let legacy = events
        .iter()
        .find(|e| e["event"] == "tool_result" && e["name"] == "todo")
        .unwrap();
    assert_eq!(legacy["data"], settled["node"]["result"]["data"]);
    rpc.send(2, "state", json!({}));
    let state = rpc.result(2);
    let nodes = state["tree"].as_array().unwrap();
    for node in nodes {
        if let Some(parent) = node["parent"].as_str() {
            assert!(nodes.iter().any(|n| n["id"] == parent), "orphan: {node}");
        }
    }
    assert!(
        nodes
            .iter()
            .any(|n| n["kind"] == "message" && n["label"] == "user")
    );
    rpc.send(3, "shutdown", json!({}));
    rpc.result(3);
}

#[test]
fn rpc_keeps_conversation_state_and_approval_does_not_bypass_policy() {
    let env = Env::new();
    // These tests exercise the approval round trip; the default `write` tier would run the write unprompted.
    let home_config = env.home.path().join("config.toml");
    std::fs::write(
        &home_config,
        format!(
            "approval = \"ask\"\n{}",
            std::fs::read_to_string(&home_config).unwrap()
        ),
    )
    .unwrap();
    let server = Server::new(vec![
        text("first answer"),
        call("write", json!({"path":"result.txt","content":"saved"})),
        // The review nudge rides on the next request after the approved write (mod.rs review delivery),
        // so the model answers once more before the turn completes.
        text("second answer"),
    ]);
    let mut rpc = RpcChild::start(&env, &server);
    rpc.send(
        90,
        "approve",
        json!({"approval_id":"approval-1","decision":"allow"}),
    );
    let future = rpc.next();
    assert_eq!(future["id"], 90);
    assert!(
        future["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not currently pending")
    );
    rpc.send(1, "send", json!({"text":"first prompt"}));
    assert_eq!(rpc.result(1)["text"], "first answer");
    rpc.send(2, "send", json!({"text":"write the result"}));
    let approval = loop {
        let event = rpc.next();
        if event["event"] == "approval" {
            break event;
        }
    };
    assert_eq!(approval["tool"], "write");
    assert!(!env.project.path().join("result.txt").exists());
    rpc.send(
        3,
        "approve",
        json!({"approval_id":approval["approval_id"],"decision":"allow"}),
    );
    rpc.result(3);
    assert_eq!(rpc.result(2)["text"], "second answer");
    assert_eq!(
        std::fs::read_to_string(env.project.path().join("result.txt")).unwrap(),
        "saved"
    );
    rpc.send(
        91,
        "approve",
        json!({"approval_id":approval["approval_id"],"decision":"allow"}),
    );
    let replay = rpc.next();
    assert_eq!(replay["id"], 91);
    assert!(
        replay.get("error").is_some(),
        "approval decisions are one-shot"
    );
    let requests = server.requests.lock();
    assert!(requests[1]["messages"].to_string().contains("first answer"));
    drop(requests);
    rpc.send(4, "shutdown", json!({}));
    rpc.result(4);
}

#[test]
fn rpc_abort_interrupts_silent_inference_and_leaves_session_usable() {
    let env = Env::new();
    let server = Server::new(vec![json!({"wait_for_interrupt":true})]);
    let mut rpc = RpcChild::start(&env, &server);
    rpc.send(1, "send", json!({"text":"wait"}));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server.requests.lock().is_empty() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    rpc.send(2, "abort", json!({}));
    rpc.result(2);
    assert_eq!(rpc.result(1)["interrupted"], true);
    rpc.send(3, "state", json!({}));
    assert!(rpc.result(3)["session_id"].is_string());
    rpc.send(4, "shutdown", json!({}));
    rpc.result(4);
}

#[test]
fn rpc_cancels_live_worker_while_its_write_is_waiting_for_approval() {
    let env = Env::new();
    // These tests exercise the approval round trip; the default `write` tier would run the write unprompted.
    let home_config = env.home.path().join("config.toml");
    std::fs::write(
        &home_config,
        format!(
            "approval = \"ask\"\n{}",
            std::fs::read_to_string(&home_config).unwrap()
        ),
    )
    .unwrap();
    let server = Server::new(vec![
        call(
            "write",
            json!({"path":"forbidden.txt","content":"must not write"}),
        ),
        text("stopped"),
    ]);
    let mut rpc = RpcChild::start(&env, &server);
    rpc.send(
        1,
        "task_start",
        json!({"agent":"worker","task":"write forbidden.txt","solution_space":"s"}),
    );
    let launch = loop {
        let event = rpc.next();
        if event["event"] == "approval" {
            break event;
        }
    };
    rpc.send(
        2,
        "approve",
        json!({"approval_id":launch["approval_id"],"decision":"allow"}),
    );
    rpc.result(2);
    let jobs = rpc.result(1);
    let id = jobs[0]["id"].as_str().unwrap().to_owned();
    let write = loop {
        let event = rpc.next();
        if event["event"] == "approval" {
            break event;
        }
    };
    assert_eq!(write["tool"], "write");
    rpc.send(3, "task_cancel", json!({"id":id}));
    assert_eq!(rpc.result(3)["cancellation_requested"], true);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        rpc.send(4, "task_status", json!({"id":id}));
        let state = rpc.result(4);
        if state["status"] != "running" {
            assert_eq!(state["status"], "cancelled");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "live approval did not cancel"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!env.project.path().join("forbidden.txt").exists());
    rpc.send(6, "state", json!({}));
    let tree = rpc.result(6);
    let worker = tree["tree"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["kind"] == "worker")
        .unwrap();
    assert_eq!(worker["result"]["phase"], "interrupted");
    assert_eq!(worker["result"]["data"]["id"], id);
    assert_eq!(worker["result"]["data"]["status"], "cancelled");
    rpc.send(5, "shutdown", json!({}));
    rpc.result(5);
}

#[test]
fn understanding_cli_needs_no_model_and_exports_all_saved_entries() {
    use dial::{
        storage::{codebase::Codebase, paths::Layout},
        tools::Workspace,
    };
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dial"))
            .env("DIAL_HOME", home.path())
            .current_dir(project.path())
            .args(args)
            .output()
            .unwrap()
    };
    let empty = run(&["understanding", "--json"]);
    assert!(
        empty.status.success(),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
    let empty: Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(empty["indexed_files"], 0);
    std::fs::write(
        project.path().join("routing.rs"),
        "pub fn route_request() {}\n",
    )
    .unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().into()).unwrap();
    Codebase::open(&layout)
        .unwrap()
        .refresh(
            &Workspace::new(project.path()).unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
    let all = run(&["understanding", "--json"]);
    assert!(all.status.success());
    let report: Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(report["files"].as_array().unwrap().len(), 1);
    assert_eq!(report["files"][0]["status"], "fresh");
    let text = run(&["understanding", "route_request"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("routing.rs"));
    let architecture = run(&["understanding", "architecture", "--json"]);
    assert!(architecture.status.success());
    let architecture: Value = serde_json::from_slice(&architecture.stdout).unwrap();
    assert_eq!(architecture["view"], "architecture");
    assert!(architecture["architecture"]["areas"]["(root)"].is_array());
    let filtered = run(&["understanding", "missing", "--json"]);
    assert!(
        serde_json::from_slice::<Value>(&filtered.stdout).unwrap()["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reliability_status_is_available_without_a_model_request() {
    let server = Server::new(vec![]);
    let e = Env::new();
    let output = e.run(&server, &["reliability", "status", "--json"], "");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["enabled"], true);
    assert!(value["measured"].is_null());
    assert!(server.requests.lock().is_empty());
}

#[test]
fn rpc_initialization_preserves_the_actor_failure_cause() {
    let env = Env::new();
    let server = Server::new(vec![]);
    let missing = env.project.path().join("missing-benchmark-adapter.json");
    let output = Command::new(env!("CARGO_BIN_EXE_dial"))
        .env("DIAL_HOME", env.home.path())
        .env("DIAL_BENCHMARK_ADAPTER", &missing)
        .current_dir(env.project.path())
        .args([
            "rpc",
            "--endpoint",
            &server.url,
            "--model",
            "scripted",
            "--sandbox",
            "off",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostic.contains("benchmark adapters require the local MLX backend"),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("receiving on a closed channel"),
        "{diagnostic}"
    );
    assert!(
        server.requests.lock().is_empty(),
        "initialization must not infer"
    );
}
