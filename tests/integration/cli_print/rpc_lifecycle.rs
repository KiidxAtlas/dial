//! Protocol lifecycle regressions use the existing local provider fixture, never real accounts.
use super::*;

fn start(env: &Env, server: &Server, args: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_dial"))
        .args(args)
        .args([
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
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn finish(mut child: std::process::Child) -> Output {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "RPC did not settle after EOF: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn finite_piped_state_and_shutdown_receive_responses_before_eof_cleanup() {
    let env = Env::new();
    let server = Server::new(vec![]);
    for payload in [
        "{\"id\":1,\"method\":\"state\"}\n",
        "{\"id\":1,\"method\":\"state\"}\n{\"id\":2,\"method\":\"shutdown\"}\n",
        "{\"id\":1,\"method\":\"new_session\"}",
    ] {
        let mut child = start(&env, &server, &["rpc"]);
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let output = finish(child);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows = events(&output);
        let first = rows
            .iter()
            .find(|row| row["id"] == 1)
            .expect("accepted request must receive a response");
        assert!(first["result"]["session_id"].is_string(), "{first}");
        if payload.contains("shutdown") {
            assert!(
                rows.iter()
                    .any(|row| row["id"] == 2 && row["result"]["closing"] == true)
            );
        }
    }
}

#[test]
fn eof_cancels_active_silent_inference_without_discarding_its_response() {
    let env = Env::new();
    let server = Server::new(vec![json!({"wait_for_interrupt":true})]);
    let mut child = start(&env, &server, &["rpc"]);
    let mut input = child.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        json!({"id":1,"method":"send","params":{"text":"wait"}})
    )
    .unwrap();
    input.flush().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.requests.lock().is_empty() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("inference did not start");
        }
        thread::sleep(Duration::from_millis(5));
    }
    drop(input);
    let output = finish(child);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        events(&output)
            .iter()
            .any(|row| row["id"] == 1 && row["result"]["interrupted"] == true)
    );
}

#[test]
fn protocol_plan_launch_is_explicitly_rejected_for_commands_and_mode_routes() {
    let env = Env::new();
    let server = Server::new(vec![]);
    for args in [
        // ModeArgs are root-only. Its optional model value must also stay empty rather
        // than consuming the following subcommand as a named planner model.
        vec!["--plan=", "rpc"],
        vec!["--plan=", "acp"],
        vec!["--mode", "rpc", "--plan"],
        vec!["--mode", "acp", "--plan"],
    ] {
        let mut child = start(&env, &server, &args);
        drop(child.stdin.take());
        let output = finish(child);
        assert!(!output.status.success());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains("--plan") && diagnostic.contains("cannot be used with"),
            "{diagnostic}"
        );
        assert!(
            output.stdout.is_empty(),
            "must reject before protocol session creation"
        );
    }
    assert!(server.requests.lock().is_empty());
}

fn initialize_acp(rpc: &mut RpcChild) {
    rpc.send(
        1,
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{}}),
    );
    let initialized = rpc.result(1);
    assert_eq!(initialized["protocolVersion"], 1);
    assert_eq!(initialized["agentCapabilities"]["loadSession"], true);
}

fn session_params(env: &Env, session: Option<&str>) -> Value {
    let mut params = json!({"cwd":env.project.path(),"mcpServers":[]});
    if let Some(session) = session {
        params["sessionId"] = json!(session);
    }
    params
}

fn assert_replayed_history(updates: &[Value], session: &str, prompt: &str, answer: &str) {
    for (kind, text) in [
        ("user_message_chunk", prompt),
        ("agent_message_chunk", answer),
    ] {
        assert!(
            updates.iter().any(|event| {
                event["method"] == "session/update"
                    && event["params"]["sessionId"] == session
                    && event["params"]["update"]["sessionUpdate"] == kind
                    && event["params"]["update"]["content"]["text"] == text
            }),
            "missing {kind} {text:?} for {session}: {updates:?}"
        );
    }
}

#[test]
fn acp_parked_sessions_restore_their_own_plan_mode_and_history() {
    let env = Env::new();
    let server = Server::new(vec![text("answer from A"), text("answer from B")]);
    let mut rpc = RpcChild::start_acp(&env, &server);
    initialize_acp(&mut rpc);
    rpc.send(2, "session/new", session_params(&env, None));
    let a = rpc.result(2)["sessionId"].as_str().unwrap().to_owned();
    rpc.send(
        3,
        "session/prompt",
        json!({"sessionId":a,"prompt":[{"type":"text","text":"prompt for A"}]}),
    );
    assert_eq!(rpc.result(3)["stopReason"], "end_turn");
    rpc.send(
        4,
        "session/set_mode",
        json!({"sessionId":a,"modeId":"plan"}),
    );
    rpc.result(4);

    rpc.send(5, "session/new", session_params(&env, None));
    let new_b = rpc.result(5);
    let b = new_b["sessionId"].as_str().unwrap().to_owned();
    assert_ne!(a, b);
    assert_eq!(new_b["modes"]["currentModeId"], "default");
    rpc.send(
        6,
        "session/prompt",
        json!({"sessionId":b,"prompt":[{"type":"text","text":"prompt for B"}]}),
    );
    assert_eq!(rpc.result(6)["stopReason"], "end_turn");

    rpc.send(7, "session/load", session_params(&env, Some(&a)));
    let (loaded_a, replay_a) = rpc.result_with_events(7);
    assert_eq!(loaded_a["modes"]["currentModeId"], "plan");
    assert_replayed_history(&replay_a, &a, "prompt for A", "answer from A");
    assert!(
        !replay_a
            .iter()
            .any(|event| event["params"]["update"]["content"]["text"] == "answer from B")
    );

    rpc.send(8, "session/load", session_params(&env, Some(&b)));
    let (loaded_b, replay_b) = rpc.result_with_events(8);
    assert_eq!(loaded_b["modes"]["currentModeId"], "default");
    assert_replayed_history(&replay_b, &b, "prompt for B", "answer from B");
    // Loading B parks A again; taking it out must retain the same bundled mode.
    rpc.send(9, "session/load", session_params(&env, Some(&a)));
    assert_eq!(rpc.result(9)["modes"]["currentModeId"], "plan");
    assert_eq!(
        server.requests.lock().len(),
        2,
        "loading history must not infer"
    );
}

#[test]
fn acp_forks_parked_history_and_reopens_closed_sessions_with_fresh_mode() {
    let env = Env::new();
    let server = Server::new(vec![text("original answer"), text("fork-only answer")]);
    let mut rpc = RpcChild::start_acp(&env, &server);
    initialize_acp(&mut rpc);
    rpc.send(2, "session/new", session_params(&env, None));
    let a = rpc.result(2)["sessionId"].as_str().unwrap().to_owned();
    rpc.send(
        3,
        "session/prompt",
        json!({"sessionId":a,"prompt":[{"type":"text","text":"original prompt"}]}),
    );
    assert_eq!(rpc.result(3)["stopReason"], "end_turn");
    rpc.send(
        4,
        "session/set_mode",
        json!({"sessionId":a,"modeId":"plan"}),
    );
    rpc.result(4);
    rpc.send(5, "session/new", session_params(&env, None));
    let b = rpc.result(5)["sessionId"].as_str().unwrap().to_owned();

    rpc.send(6, "session/fork", session_params(&env, Some(&a)));
    let forked = rpc.result(6);
    let fork = forked["sessionId"].as_str().unwrap().to_owned();
    assert_ne!(fork, a);
    assert_ne!(fork, b);
    assert_eq!(forked["modes"]["currentModeId"], "default");
    rpc.send(
        7,
        "session/prompt",
        json!({"sessionId":fork,"prompt":[{"type":"text","text":"fork-only prompt"}]}),
    );
    assert_eq!(rpc.result(7)["stopReason"], "end_turn");
    assert!(
        server.requests.lock()[1]["messages"]
            .to_string()
            .contains("original answer")
    );

    // Closing a parked session must release its writer and forget its transient mode.
    rpc.send(8, "session/close", json!({"sessionId":a}));
    rpc.result(8);
    rpc.send(9, "session/load", session_params(&env, Some(&a)));
    let (reopened, replay) = rpc.result_with_events(9);
    assert_eq!(reopened["modes"]["currentModeId"], "default");
    assert_replayed_history(&replay, &a, "original prompt", "original answer");
    assert!(
        !replay
            .iter()
            .any(|event| event["params"]["update"]["content"]["text"] == "fork-only prompt")
    );

    // Closing the active session leaves no current actor session; a parked fork still loads.
    rpc.send(10, "session/close", json!({"sessionId":a}));
    rpc.result(10);
    rpc.send(11, "session/load", session_params(&env, Some(&fork)));
    let (loaded_fork, replay_fork) = rpc.result_with_events(11);
    assert_eq!(loaded_fork["modes"]["currentModeId"], "default");
    assert_replayed_history(&replay_fork, &fork, "original prompt", "original answer");
    assert_replayed_history(&replay_fork, &fork, "fork-only prompt", "fork-only answer");
    assert_eq!(server.requests.lock().len(), 2);
}
