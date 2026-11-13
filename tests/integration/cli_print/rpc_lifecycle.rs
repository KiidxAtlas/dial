//! Protocol lifecycle regressions use the existing local provider fixture, never real accounts.
use super::*;

/// Upper bound for a spawned RPC process to start or settle. A passing run returns as soon as the
/// child settles; the bound only has to outlast process startup in a loaded parallel suite.
const SETTLE: Duration = Duration::from_secs(30);

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
    if !dial::platform::process::wait_exited(&child, SETTLE).unwrap() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "RPC did not settle after EOF: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
    if let Err(error) = server.wait_for_request(SETTLE) {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "inference did not start ({error}): {}",
            String::from_utf8_lossy(&output.stderr)
        );
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

/// The response to `id`, which must be an error. Other responses are kept for `result`.
fn error_response(rpc: &RpcChild, id: u64) -> Value {
    if let Some(event) = rpc.pending.borrow_mut().remove(&id) {
        assert!(event.get("error").is_some(), "{event}");
        return event;
    }
    loop {
        let event = rpc.next();
        if event["id"] == id {
            assert!(event.get("error").is_some(), "{event}");
            return event;
        }
        if let Some(other) = event["id"].as_u64() {
            rpc.pending.borrow_mut().insert(other, event);
        }
    }
}

fn listed(page: &Value) -> Vec<String> {
    page["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn native_session_model_and_settings_methods_answer_during_a_turn() {
    let env = Env::new();
    let server = Server::new(vec![
        text("first answer"),
        json!({"wait_for_interrupt":true}),
    ]);
    let mut rpc = RpcChild::start(&env, &server);
    rpc.send(1, "send", json!({"text":"first prompt"}));
    rpc.result(1);
    rpc.send(2, "state", json!({}));
    let state = rpc.result(2);
    let a = state["session_id"].as_str().unwrap().to_owned();
    assert_eq!(state["current_model"]["model_id"], "scripted");
    rpc.send(3, "new_session", json!({}));
    let b = rpc.result(3)["session_id"].as_str().unwrap().to_owned();

    rpc.send(4, "send", json!({"text":"wait"}));
    server.wait_for_request(SETTLE).unwrap();
    server.wait_for_request(SETTLE).unwrap();
    // These are controller methods: they answer while the turn holds the actor.
    rpc.send(5, "list_sessions", json!({}));
    let page = rpc.result(5);
    let ids = listed(&page);
    assert!(ids.contains(&a) && ids.contains(&b), "{page}");
    rpc.send(6, "pin_session", json!({"session_id":a,"pinned":true}));
    assert_eq!(rpc.result(6)["changed"], true);
    rpc.send(7, "list_sessions", json!({}));
    assert_eq!(listed(&rpc.result(7))[0], a);
    rpc.send(8, "list_models", json!({}));
    let models = rpc.result(8)["models"].as_array().unwrap().clone();
    assert!(
        models
            .iter()
            .any(|model| model["model_id"] == "scripted" && model["current"] == true),
        "{models:?}"
    );
    rpc.send(9, "delete_session", json!({"session_id":b}));
    let refused = error_response(&rpc, 9);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("active session"),
        "{refused}"
    );
    rpc.send(
        10,
        "set_setting",
        json!({"key":"harness.max_steps","value":5}),
    );
    assert_eq!(rpc.result(10)["saved"], true);
    rpc.send(11, "get_settings", json!({"key":"harness.max_steps"}));
    assert_eq!(rpc.result(11)["value"], 5);
    rpc.send(
        12,
        "set_setting",
        json!({"key":"harness.max_steps","value":"many"}),
    );
    error_response(&rpc, 12);

    rpc.send(13, "abort", json!({}));
    rpc.result(13);
    assert_eq!(rpc.result(4)["interrupted"], true);

    rpc.send(14, "delete_session", json!({"session_id":a}));
    assert_eq!(rpc.result(14)["deleted"], true);
    rpc.send(15, "fork_session", json!({}));
    let forked = rpc.result(15);
    assert_eq!(forked["forked_from"], b.as_str());
    assert_ne!(forked["session_id"], b.as_str());
    rpc.send(16, "list_sessions", json!({}));
    let ids = listed(&rpc.result(16));
    assert!(!ids.contains(&a));
    rpc.send(17, "reset_setting", json!({"key":"harness.max_steps"}));
    assert_eq!(rpc.result(17)["removed"], true);
    rpc.send(1000, "shutdown", json!({}));
    rpc.result(1000);
}

#[test]
fn apply_settings_rebuilds_around_the_same_session_and_gui_reads_answer_during_a_turn() {
    let env = Env::new();
    // The scripted server answers one connection at a time, and the interrupt-waiting reply holds
    // it until the fixture drops, so it comes last.
    let server = Server::new(vec![
        text("first answer"),
        text("after rebuild"),
        json!({"wait_for_interrupt":true}),
    ]);
    let mut rpc = RpcChild::start(&env, &server);
    rpc.send(1, "send", json!({"text":"first prompt"}));
    rpc.result(1);
    rpc.send(2, "state", json!({}));
    let session = rpc.result(2)["session_id"].as_str().unwrap().to_owned();

    // Nothing saved differs from the launch configuration: applied in place.
    rpc.send(3, "apply_settings", json!({}));
    let live = rpc.result(3);
    assert_eq!(live["applied"], "live", "{live}");
    assert_eq!(live["rebuilt"], false);

    rpc.send(
        4,
        "set_setting",
        json!({"key":"harness.max_steps","value":7}),
    );
    assert_eq!(rpc.result(4)["restart_required"], true);
    rpc.send(5, "apply_settings", json!({}));
    let rebuilt = rpc.result(5);
    assert_eq!(rebuilt["applied"], "rebuild", "{rebuilt}");
    assert!(rebuilt["tools"].is_array(), "{rebuilt}");
    rpc.send(6, "state", json!({}));
    let state = rpc.result(6);
    assert_eq!(state["session_id"], session.as_str(), "{state}");
    rpc.send(7, "messages", json!({}));
    assert_eq!(rpc.result(7)["messages"].as_array().unwrap().len(), 2);

    // The rebuilt agent runs turns in the same session.
    rpc.send(8, "send", json!({"text":"again"}));
    assert_eq!(rpc.result(8)["text"], "after rebuild");
    rpc.send(9, "state", json!({}));
    assert_eq!(rpc.result(9)["session_id"], session.as_str());

    rpc.send(10, "send", json!({"text":"wait"}));
    for _ in 0..3 {
        server.wait_for_request(SETTLE).unwrap();
    }
    // Controller reads a GUI polls while the turn holds the actor.
    rpc.send(11, "list_jobs", json!({}));
    assert!(rpc.result(11)["jobs"].is_array());
    rpc.send(12, "list_dirs", json!({}));
    assert!(rpc.result(12)["dirs"].is_array());
    rpc.send(13, "get_schema", json!({}));
    assert!(rpc.result(13)["$defs"]["request"]["oneOf"].is_array());
    rpc.send(14, "abort", json!({}));
    rpc.result(14);
    assert_eq!(rpc.result(10)["interrupted"], true);
    rpc.send(1000, "shutdown", json!({}));
    rpc.result(1000);
}

#[test]
fn acp_sessions_expose_and_switch_the_model_config_option() {
    let env = Env::new();
    let server = Server::new(vec![]);
    let mut rpc = RpcChild::start_acp(&env, &server);
    initialize_acp(&mut rpc);
    rpc.send(2, "session/new", session_params(&env, None));
    let opened = rpc.result(2);
    let session = opened["sessionId"].as_str().unwrap().to_owned();
    let model = opened["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "model")
        .unwrap_or_else(|| panic!("no model option: {opened}"))
        .clone();
    let current = model["currentValue"].as_str().unwrap().to_owned();
    assert!(current.ends_with("/scripted"), "{model}");
    assert!(
        model["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|option| option["value"] == current.as_str())
    );
    rpc.send(
        3,
        "session/set_config_option",
        json!({"sessionId":session,"configId":"model","value":current}),
    );
    let (switched, updates) = rpc.result_with_events(3);
    assert_eq!(
        switched["configOptions"][1]["currentValue"],
        current.as_str()
    );
    assert!(
        updates
            .iter()
            .any(|update| update["params"]["update"]["sessionUpdate"] == "config_option_update")
    );
    rpc.send(
        4,
        "session/set_config_option",
        json!({"sessionId":session,"configId":"model","value":"nope/unknown-model"}),
    );
    error_response(&rpc, 4);
}

#[test]
fn extension_actions_save_through_the_overlay_owner_and_mcp_sign_in_names_the_server() {
    let env = Env::new();
    let config = env.home.path().join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[mcp.local]\nprogram = \"true\"\nenabled = false\n");
    std::fs::write(&config, text).unwrap();
    let server = Server::new(vec![]);
    let mut rpc = RpcChild::start(&env, &server);
    rpc.send(1, "hello", json!({"protocol_version":2}));
    rpc.result(1);

    // Enabling runs at once, as in the overlay; the change is saved for `apply_settings`.
    rpc.send(
        2,
        "extension_action",
        json!({"intent":{"op":"toggle_mcp","id":"local","enable":true}}),
    );
    let enabled = rpc.result(2);
    assert_eq!(enabled["ok"], true, "{enabled}");
    assert_eq!(enabled["saved_scope"], "global");
    assert_eq!(enabled["apply_settings"], true);
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("enabled = true")
    );

    // Disabling needs the overlay's confirm step.
    rpc.send(
        3,
        "extension_action",
        json!({"intent":{"op":"toggle_mcp","id":"local","enable":false}}),
    );
    assert_eq!(error_response(&rpc, 3)["error"]["code"], "invalid_params");
    rpc.send(4, "extension_action", json!({"intent":{"op":"no_such_op"}}));
    assert_eq!(error_response(&rpc, 4)["error"]["code"], "invalid_params");

    rpc.send(5, "list_extensions", json!({}));
    let listed = rpc.result(5);
    assert!(
        listed["mcp"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == "local"),
        "{listed}"
    );

    rpc.send(6, "mcp_login", json!({"server":"missing"}));
    assert_eq!(error_response(&rpc, 6)["error"]["code"], "not_found");
    // A local program has no OAuth sign-in; the owner says so.
    rpc.send(7, "mcp_login", json!({"server":"local"}));
    let local = error_response(&rpc, 7);
    assert!(
        local["error"]["message"]
            .as_str()
            .unwrap()
            .contains("local program"),
        "{local}"
    );
    rpc.send(8, "mcp_logout", json!({"server":"local"}));
    assert_eq!(rpc.result(8), json!({"server":"local","removed":false}));
    rpc.send(1000, "shutdown", json!({}));
    rpc.result(1000);
}

#[test]
fn providers_that_needed_a_terminal_sign_in_through_masked_prompts() {
    let env = Env::new();
    let server = Server::new(vec![]);
    let mut rpc = RpcChild::start(&env, &server);
    rpc.send(1, "hello", json!({"protocol_version":2}));
    rpc.result(1);
    rpc.send(2, "login", json!({"provider":"cloudflare-ai-gateway"}));
    let prompt = loop {
        let event = rpc.next();
        assert!(event.get("error").is_none(), "{event}");
        if event["event"] == "auth_prompt" {
            break event;
        }
    };
    assert_eq!(prompt["provider"], "cloudflare-ai-gateway");
    assert_eq!(prompt["secret"], true, "{prompt}");
    rpc.send(
        3,
        "auth_cancel",
        json!({"provider":"cloudflare-ai-gateway"}),
    );
    rpc.result(3);
    let ended = error_response(&rpc, 2);
    assert!(
        ended["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cancel"),
        "{ended}"
    );
    rpc.send(1000, "shutdown", json!({}));
    rpc.result(1000);
}
