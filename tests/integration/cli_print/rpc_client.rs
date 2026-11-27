//! `dial-client` against the real executable: the lifecycle a frontend depends on (negotiation,
//! approvals, coded errors, disconnect accounting and reconnect-by-resume), with the local
//! scripted provider and no real accounts.
use super::*;
use dial_client::{Client, ClientError, Decision, Effect, HandoffPoll, Launch, Message};

const WAIT: Duration = Duration::from_secs(30);

fn launch(env: &Env, server: &Server) -> Launch {
    Launch::new(env!("CARGO_BIN_EXE_dial"))
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
}

fn ask_before_writes(env: &Env) {
    let config = env.home.path().join("config.toml");
    let previous = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("approval = \"ask\"\n{previous}")).unwrap();
}

fn code(error: ClientError) -> String {
    match error {
        ClientError::Rpc { error, .. } => error.code.unwrap_or_default(),
        other => panic!("expected an engine error, got {other}"),
    }
}

/// Next message other than streaming noise, failing the test on timeout.
fn next(client: &mut Client) -> Message {
    client
        .poll(WAIT)
        .unwrap_or_else(|| panic!("no message within {WAIT:?}"))
}

#[test]
fn private_ask_requires_consent_and_leaves_messages_unchanged() {
    let env = Env::new();
    let server = Server::new(vec![text("private reply")]);
    let mut client = Client::connect(&launch(&env, &server), WAIT).unwrap();
    let before = client.call("messages", json!({}), WAIT).unwrap();
    assert_eq!(
        code(
            client
                .call("private_ask", json!({"text":"private question"}), WAIT)
                .unwrap_err()
        ),
        "refused"
    );
    assert!(server.requests.lock().is_empty());
    let result = client
        .call(
            "private_ask",
            json!({"text":"private question","confirm":true}),
            WAIT,
        )
        .unwrap();
    assert_eq!(result["answer"], "private reply");
    assert_eq!(client.call("messages", json!({}), WAIT).unwrap(), before);
}

#[test]
fn first_run_client_can_connect_and_list_sign_in_providers_without_a_model() {
    let env = Env::new();
    let launch = Launch::new(env!("CARGO_BIN_EXE_dial"))
        .env("DIAL_HOME", env.home.path())
        .env("DIAL_SUBSCRIPTION_AUTH_HOME", env.home.path())
        .current_dir(env.project.path());
    let mut client = Client::connect(&launch, WAIT).unwrap();
    assert!(client.ready().model.is_empty());
    let list = client.call("auth_providers", json!({}), WAIT).unwrap();
    let providers = list["providers"].as_array().unwrap();
    for provider in ["anthropic", "openai"] {
        assert!(providers.iter().any(|p| p["provider"] == provider));
    }
    let key_provider = "api-key:abliteration";
    assert!(
        list["key_providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["provider"] == key_provider
                && p["key_saved"] == false
                && p["can_remove"] == true)
    );
    let login = client
        .request("login", json!({"provider":key_provider}))
        .unwrap();
    let prompt = loop {
        if let Message::Event(e) = next(&mut client)
            && e.kind == "auth_prompt"
        {
            assert_eq!(e.body["secret"], true);
            break e.body["prompt_id"].clone();
        }
    };
    let removal = json!({"provider":key_provider,"confirm":true});
    assert_eq!(
        code(
            client
                .call("remove_api_key", removal.clone(), WAIT)
                .unwrap_err()
        ),
        "busy"
    );
    client
        .call(
            "auth_code",
            json!({"prompt_id":prompt,"code":"fixture-key"}),
            WAIT,
        )
        .unwrap();
    loop {
        if let Message::Response { id, result, .. } = next(&mut client)
            && id == login
        {
            assert!(!result.unwrap().to_string().contains("fixture-key"));
            break;
        }
    }
    for refused in [
        json!({"provider":key_provider}),
        json!({"provider":key_provider,"confirm":false}),
        json!({"provider":"openai","confirm":true}),
        json!({"provider":"api-key:https://example.invalid","confirm":true}),
    ] {
        assert_eq!(
            code(client.call("remove_api_key", refused, WAIT).unwrap_err()),
            "invalid_params"
        );
    }
    let list = client.call("auth_providers", json!({}), WAIT).unwrap();
    assert!(
        list["key_providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["provider"] == key_provider && p["key_saved"] == true)
    );
    assert!(
        client
            .call(
                "login",
                json!({"provider":"api-key:https://example.invalid"}),
                WAIT
            )
            .is_err()
    );
    // The store uses exact URLs. A custom endpoint with a trailing slash is a distinct key.
    let canonical = &dial::llm::catalog::builtin_provider("abliteration")
        .unwrap()
        .base_url;
    let other = format!("{canonical}/");
    dial::llm::catalog::save_credential(env.home.path(), &other, "another-fixture").unwrap();
    assert_eq!(
        client.call("remove_api_key", removal, WAIT).unwrap(),
        json!({"provider":key_provider})
    );
    let list = client.call("auth_providers", json!({}), WAIT).unwrap();
    assert!(
        list["key_providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["provider"] == key_provider && p["key_saved"] == false)
    );
    assert!(dial::llm::catalog::stored_credential_urls(env.home.path()).contains(&other));
    assert!(client.shutdown(WAIT).unwrap().success());
}

#[test]
fn client_negotiates_and_drives_an_approved_turn_with_coded_refusals() {
    let env = Env::new();
    ask_before_writes(&env);
    let server = Server::new(vec![
        call("write", json!({"path":"result.txt","content":"saved"})),
        text("written"),
    ]);
    let mut client = Client::connect(&launch(&env, &server), WAIT).unwrap();
    assert_eq!(client.protocol_version(), 2);
    assert!(client.supports("approve") && client.supports("steer"));
    assert!(!client.ready().session_id.is_empty());

    // Refusals carry stable codes a frontend can branch on.
    assert_eq!(
        code(
            client
                .approve("approval-0", Decision::Allow, WAIT)
                .unwrap_err()
        ),
        "not_pending"
    );
    assert_eq!(
        code(client.call("steer", json!({"text":"x"}), WAIT).unwrap_err()),
        "not_pending"
    );
    assert_eq!(
        code(
            client
                .call("messages_page", json!({"offset":"zero"}), WAIT)
                .unwrap_err()
        ),
        "invalid_params"
    );
    assert!(matches!(
        client.call("no_such_method", json!({}), WAIT),
        Err(ClientError::Unsupported(method)) if method == "no_such_method"
    ));
    let turn = client
        .request("send", json!({"text":"write the result"}))
        .unwrap();
    let approval = loop {
        if let Message::Event(event) = next(&mut client)
            && let Some(approval) = event.approval()
        {
            break approval;
        }
    };
    assert_eq!(approval.tool, "write");
    assert!(!env.project.path().join("result.txt").exists());
    assert_eq!(
        code(
            client
                .call("send", json!({"text":"again"}), WAIT)
                .unwrap_err()
        ),
        "busy"
    );
    assert_eq!(
        code(
            client
                .call(
                    "approve",
                    json!({"approval_id":approval.approval_id,"decision":"maybe"}),
                    WAIT
                )
                .unwrap_err()
        ),
        "invalid_params"
    );
    client
        .approve(&approval.approval_id, Decision::Allow, WAIT)
        .unwrap();
    let reply = loop {
        if let Message::Response { id, result, .. } = next(&mut client)
            && id == turn
        {
            break result.unwrap();
        }
    };
    assert_eq!(reply["text"], "written");
    assert_eq!(
        std::fs::read_to_string(env.project.path().join("result.txt")).unwrap(),
        "saved"
    );
    assert_eq!(
        code(
            client
                .approve(&approval.approval_id, Decision::Allow, WAIT)
                .unwrap_err()
        ),
        "not_pending",
        "approval decisions are one-shot"
    );
    assert!(client.outstanding().is_empty());
    assert!(client.shutdown(WAIT).unwrap().success());
}

#[test]
fn a_lost_engine_reports_unknown_outcomes_and_a_new_connection_resumes_the_session() {
    let env = Env::new();
    let server = Server::new(vec![
        text("first answer"),
        json!({"wait_for_interrupt":true}),
    ]);
    let mut client = Client::connect(&launch(&env, &server), WAIT).unwrap();
    let session = client.ready().session_id.clone();
    assert_eq!(
        client
            .call("send", json!({"text":"first prompt"}), WAIT)
            .unwrap()["text"],
        "first answer"
    );

    let turn = client.request("send", json!({"text":"wait"})).unwrap();
    server.wait_for_request(WAIT).expect("first inference");
    server.wait_for_request(WAIT).expect("silent inference");
    client.kill().unwrap();
    let outstanding = loop {
        if let Message::Disconnected { outstanding } = next(&mut client) {
            break outstanding;
        }
    };
    assert_eq!(outstanding.len(), 1, "{outstanding:?}");
    assert_eq!(outstanding[0].id, turn);
    assert_eq!(outstanding[0].effect, Effect::MayHaveApplied);
    assert!(client.is_closed());
    assert!(client.poll(Duration::from_millis(10)).is_none());
    assert!(matches!(
        client.request("state", json!({})),
        Err(ClientError::Disconnected { .. })
    ));
    drop(client);

    // Reconnect: a fresh process, negotiated again, resumes the persisted session.
    let mut client = Client::connect(&launch(&env, &server), WAIT).unwrap();
    client
        .call("resume", json!({"session_id": session}), WAIT)
        .unwrap();
    let history = client
        .call("messages_page", json!({"offset":0,"limit":50}), WAIT)
        .unwrap();
    assert!(
        history["messages"].to_string().contains("first answer"),
        "{history}"
    );
    assert!(client.shutdown(WAIT).unwrap().success());
}

#[test]
fn events_are_numbered_and_attributed_and_undo_restores_files_and_history() {
    let env = Env::new();
    let server = Server::new(vec![
        call("write", json!({"path":"notes.txt","content":"after"})),
        text("edited"),
    ]);
    let mut client = Client::connect(&launch(&env, &server), WAIT).unwrap();
    assert!(
        client
            .ready()
            .engine_version
            .as_deref()
            .is_some_and(|v| !v.is_empty())
    );
    assert_eq!(client.ready().raw["seq"], 1);
    let session = client.ready().session_id.clone();

    let turn = client.send("edit the notes").unwrap();
    let mut seqs = Vec::new();
    let mut cards = 0;
    let reply = loop {
        match next(&mut client) {
            Message::Event(event) => {
                seqs.push(event.seq().expect("every event is numbered"));
                assert_eq!(event.session_id(), Some(session.as_str()));
                if event.kind == "tool_call" {
                    assert_eq!(event.request_id(), Some(turn), "{:?}", event.body);
                }
                if event.kind == "tool_card" {
                    assert!(event.body["label"].is_string() && event.body["call_id"].is_string());
                    cards += 1;
                }
                if event.kind == "tool_result" {
                    assert_eq!(event.body["status"], "ok", "{:?}", event.body);
                    let changed = event.body["changed_paths"][0].as_str().unwrap_or_default();
                    assert!(changed.ends_with("notes.txt"), "{:?}", event.body);
                }
            }
            Message::Response { id, result, .. } if id == turn => break result.unwrap(),
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(reply["text"], "edited");
    assert!(
        seqs.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{seqs:?}"
    );
    assert_eq!(client.lost_events(), 0);
    assert!(cards > 0, "the engine builds a card for each tool call");
    assert_eq!(
        std::fs::read_to_string(env.project.path().join("notes.txt")).unwrap(),
        "after",
        "{reply}"
    );

    // Review the turn's change, then undo it like `/undo`.
    let listed = client.list_checkpoints(WAIT).unwrap();
    let checkpoint = &listed["checkpoints"][0];
    let checkpoint_turn = checkpoint["turn"].as_u64().unwrap();
    let diff = client
        .checkpoint_diff(checkpoint_turn, "notes.txt", WAIT)
        .unwrap();
    assert!(diff["diff"].as_str().unwrap().contains("after"), "{diff}");
    assert_eq!(
        code(
            client
                .checkpoint_diff(checkpoint_turn, "missing.txt", WAIT)
                .unwrap_err()
        ),
        "not_found"
    );
    let undone = client.undo(1, WAIT).unwrap();
    assert_eq!(undone["conversation_kept"], false, "{undone}");
    assert!(!env.project.path().join("notes.txt").exists(), "{undone}");
    let history = client
        .call("messages_page", json!({"offset":0,"limit":50}), WAIT)
        .unwrap();
    assert!(
        !history["messages"].to_string().contains("edit the notes"),
        "{history}"
    );
    // Nothing left to undo: the owner reports an empty rewind, as `/undo` does.
    let again = client.undo(1, WAIT).unwrap();
    assert_eq!(
        (again["dropped"].clone(), again["files"].clone()),
        (json!(0), Value::Null)
    );
    assert!(client.call("list_extensions", json!({}), WAIT).unwrap()["mcp"].is_array());
    assert!(client.shutdown(WAIT).unwrap().success());
}

#[test]
fn reopened_turn_keeps_its_check_and_refuses_to_restore_later_work() {
    let env = Env::new();
    const CHECK: &str = "test \"$(cat a.txt)\" = alpha && test \"$(cat b.txt)\" = beta";
    let server = Server::new(vec![
        call(
            "write",
            json!({"files":[{"path":"a.txt","content":"alpha"},{"path":"b.txt","content":"beta"}]}),
        ),
        call(
            "bash",
            json!({"command":CHECK, "check":true, "check_paths":["a.txt","b.txt"]}),
        ),
        text("Checked both files."),
    ]);
    let launch = launch(&env, &server);
    let mut client = Client::connect(&launch, WAIT).unwrap();
    let session = client.ready().session_id.clone();
    let id = client
        .send("Write a.txt and b.txt, then check their contents")
        .unwrap();
    loop {
        match next(&mut client) {
            Message::Response {
                id: got, result, ..
            } if got == id => {
                result.unwrap();
                break;
            }
            Message::Event(e) if e.kind == "tool_result" => {
                assert_eq!(e.body["status"], "ok", "{:?}", e.body)
            }
            Message::Event(e) if e.kind == "approval_request" => {
                client
                    .approve(
                        e.body["approval_id"].as_str().unwrap(),
                        Decision::Allow,
                        WAIT,
                    )
                    .unwrap();
            }
            Message::Event(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(client.shutdown(WAIT).unwrap().success());
    std::fs::write(env.project.path().join("b.txt"), "later work").unwrap();
    let mut client = Client::connect(&launch, WAIT).unwrap();
    client
        .call("resume", json!({"session_id":session}), WAIT)
        .unwrap();
    let evidence = client
        .call("evidence", json!({"action":"turns"}), WAIT)
        .unwrap();
    let record = &evidence["turns"][0];
    assert_eq!(record["files"].as_array().unwrap().len(), 2, "{record}");
    assert!(
        record["details"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().is_some_and(|line| line.contains(CHECK))),
        "{record}"
    );
    for (path, eligible) in [("b.txt", false), ("a.txt", true)] {
        let params = json!({"session_id":session,"turn":record["turn"],"path":path});
        let diff = client.call("checkpoint_diff", params, WAIT).unwrap();
        assert_eq!(diff["safe_restore"], eligible, "{diff}");
        let restore = client.call("restore_checkpoint_files", json!({"session_id":session,"turn":record["turn"],"files":[{"path":path,"token":diff["token"]}]}), WAIT);
        if eligible {
            restore.unwrap();
        } else {
            assert_eq!(code(restore.unwrap_err()), "conflict");
        }
    }
    assert!(!env.project.path().join("a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(env.project.path().join("b.txt")).unwrap(),
        "later work"
    );
    assert!(client.shutdown(WAIT).unwrap().success());
}

#[test]
fn saved_settings_apply_by_restarting_into_the_same_session() {
    let env = Env::new();
    let server = Server::new(vec![text("hello")]);
    let launch = launch(&env, &server);
    let mut client = Client::connect(&launch, WAIT).unwrap();
    client.send("hi").unwrap();
    loop {
        if let Message::Response { result, .. } = next(&mut client) {
            result.unwrap();
            break;
        }
    }
    let session = client.state(WAIT).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let saved = client
        .set_setting("harness.max_steps", json!(7), false, WAIT)
        .unwrap();
    assert_eq!(saved["restart_required"], true);
    let mut restarted = client.restart_and_resume(&launch, WAIT).unwrap();
    // The old engine wrote nothing after its last reply, so its handoff holds only its end marker.
    let mut old = Vec::new();
    loop {
        match restarted.handoff.poll(WAIT) {
            HandoffPoll::Message(message) => old.push(message),
            HandoffPoll::Finished => break,
            HandoffPoll::TimedOut => panic!("the old engine never ended: {old:?}"),
        }
    }
    assert!(
        matches!(old.as_slice(), [Message::Disconnected { outstanding }] if outstanding.is_empty()),
        "{old:?}"
    );
    let mut client = restarted.client;
    assert_eq!(client.state(WAIT).unwrap()["session_id"], session.as_str());
    assert_eq!(
        client
            .call("get_settings", json!({"key":"harness.max_steps"}), WAIT)
            .unwrap()["value"],
        7
    );
    assert!(client.shutdown(WAIT).unwrap().success());
}

#[test]
fn a_session_has_one_writer_process_and_a_second_resume_forks() {
    let env = Env::new();
    let server = Server::new(vec![text("one"), text("two")]);
    let launch = launch(&env, &server);
    let mut owner = Client::connect(&launch, WAIT).unwrap();
    owner.call("send", json!({"text":"first"}), WAIT).unwrap();
    let session = owner.ready().session_id.clone();

    // A second process may read the session, but it never shares the journal writer.
    let mut other = Client::connect(&launch, WAIT).unwrap();
    let listed = other.call("list_sessions", json!({}), WAIT).unwrap();
    let row = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session_id"] == session.as_str())
        .cloned()
        .unwrap();
    assert_eq!(row["open_elsewhere"], true, "{row}");
    let resumed = other.resume(&session, WAIT).unwrap();
    assert_eq!(resumed["forked_from"], session.as_str(), "{resumed}");
    assert_ne!(resumed["session_id"], session.as_str());
    assert_eq!(
        code(
            other
                .call("delete_session", json!({"session_id": session}), WAIT)
                .unwrap_err()
        ),
        "busy",
        "a journal another writer holds is not deleted"
    );

    // The owner keeps writing its session undisturbed.
    assert_eq!(
        owner.call("send", json!({"text":"second"}), WAIT).unwrap()["text"],
        "two"
    );
    assert!(other.shutdown(WAIT).unwrap().success());
    assert!(owner.shutdown(WAIT).unwrap().success());
}

#[test]
fn an_engine_whose_stdout_is_not_read_queues_events_without_loss() {
    use std::io::{BufRead, BufReader, Write};
    let env = Env::new();
    // ~900 KiB of non-repeating text (under the provider's 1 MiB line limit; the repetition guard
    // discards cyclic text). The delta and the reply together far exceed the pipe buffer.
    let mut state = 1u64;
    let answer: String = (0..900 * 1024)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            char::from(b'a' + ((state >> 33) % 26) as u8)
        })
        .collect();
    let server = Server::new(vec![text(&answer)]);
    // Drive the raw process: a client-side reader thread would drain the pipe and hide the
    // engine's own backpressure handling.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_dial"))
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
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"id":1,"method":"send","params":{"text":"say a lot"}})
    )
    .unwrap();
    writeln!(stdin, "{}", json!({"id":2,"method":"state","params":{}})).unwrap();
    stdin.flush().unwrap();
    server.wait_for_request(WAIT).expect("inference");
    // Nobody reads stdout while the engine produces the turn.
    std::thread::sleep(Duration::from_secs(2));
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut streamed = String::new();
    let mut seqs = Vec::new();
    let mut replies = std::collections::BTreeMap::new();
    while replies.len() < 2 {
        let line = lines.next().expect("engine output ended early").unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        if frame["event"].is_string() {
            seqs.push(frame["seq"].as_u64().expect("numbered event"));
            if frame["event"] == "text" {
                streamed.push_str(frame["delta"].as_str().unwrap());
            }
        } else if let Some(id) = frame["id"].as_u64() {
            replies.insert(id, frame);
        }
    }
    assert!(
        streamed == answer,
        "streamed text arrives intact and in order ({} bytes)",
        streamed.len()
    );
    assert_eq!(
        replies[&1]["result"]["text"].as_str().map(str::len),
        Some(answer.len())
    );
    assert!(
        replies[&2]["result"].is_object() || replies[&2]["error"]["code"] == "busy",
        "{}",
        replies[&2]
    );
    assert_eq!(seqs.first(), Some(&1), "ready is the first event");
    assert!(
        seqs.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{seqs:?}"
    );
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[cfg(unix)]
#[test]
fn attached_cli_displays_the_owner_reply_and_detaches_without_stopping_it() {
    use dial::platform::terminal;
    use std::time::Instant;
    let env = Env::new();
    let server = Server::new(vec![text("original-reply"), text("attached-reply")]);
    let mut owner = Client::connect(&launch(&env, &server), WAIT).unwrap();
    owner
        .call("send", json!({"text":"original-prompt"}), WAIT)
        .unwrap();
    let (mut master, slave) = terminal::open(32, 120).unwrap();
    let mut input = master.try_clone().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dial"));
    cmd.args(["attach", &owner.ready().session_id])
        .current_dir(env.project.path())
        .env("DIAL_HOME", env.home.path())
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    terminal::controlling_child(&mut cmd);
    let mut viewer = cmd.spawn().unwrap();
    let mut terminal = vt100::Parser::new(32, 120, 0);
    let mut wait_for = |needle: &str| {
        let deadline = Instant::now() + WAIT;
        let mut buf = [0; 8192];
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => terminal.process(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(e) => panic!("PTY: {e}"),
            }
            if terminal.screen().contents().contains(needle) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        panic!(
            "missing {needle:?} on screen:\n{}",
            terminal.screen().contents()
        );
    };
    wait_for("original-reply");
    input.write_all(b"continue-from-cli\r").unwrap();
    wait_for("attached-reply");
    input.write_all(b"\x03").unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        // A real terminal keeps consuming output during teardown; tcsetattr can wait for it.
        let mut tail = [0; 8192];
        if let Ok(n) = master.read(&mut tail) {
            terminal.process(&tail[..n]);
        }
        if let Some(status) = viewer.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "detach timed out:\n{}",
            terminal.screen().contents()
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        owner
            .call("messages", json!({}), WAIT)
            .unwrap()
            .to_string()
            .contains("continue-from-cli")
    );
}

#[test]
fn attached_prompt_continues_original_session_with_approval_on_owner() {
    let env = Env::new();
    ask_before_writes(&env);
    let server = Server::new(vec![
        text("first answer"),
        call("write", json!({"path":"result.txt","content":"approved"})),
        text("continued"),
    ]);
    let launch = launch(&env, &server);
    let mut owner = Client::connect(&launch, WAIT).unwrap();
    let mut viewer = Client::connect(&launch, WAIT).unwrap();
    let session = owner.ready().session_id.clone();
    owner
        .call("send", json!({"text":"first prompt"}), WAIT)
        .unwrap();
    let before = viewer.call("messages", json!({}), WAIT).unwrap();
    let prompt = json!({"session_id":session,"text":"write the result"});
    let foreign_project = tempfile::tempdir().unwrap();
    let mut outsider =
        Client::connect(&launch.clone().current_dir(foreign_project.path()), WAIT).unwrap();
    assert_eq!(
        code(
            outsider
                .call("send_attached_prompt", prompt.clone(), WAIT)
                .unwrap_err()
        ),
        "refused"
    );
    assert_eq!(
        viewer.call("send_attached_prompt", prompt, WAIT).unwrap(),
        json!({"session_id":session,"queued":true})
    );
    let approval = loop {
        if let Message::Event(e) = next(&mut owner)
            && let Some(a) = e.approval()
        {
            break a;
        }
    };
    assert!(!env.project.path().join("result.txt").exists());
    assert_eq!(
        code(
            viewer
                .approve(&approval.approval_id, Decision::Allow, WAIT)
                .unwrap_err()
        ),
        "not_pending"
    );
    owner
        .approve(&approval.approval_id, Decision::Allow, WAIT)
        .unwrap();
    loop {
        if let Message::Event(e) = next(&mut owner)
            && e.kind == "collaboration_turn"
        {
            assert_eq!(e.body["result"]["text"], "continued", "{}", e.body);
            break;
        }
    }
    let messages = owner.call("messages", json!({}), WAIT).unwrap();
    for text in [
        "first prompt",
        "first answer",
        "write the result",
        "continued",
    ] {
        assert!(messages.to_string().contains(text));
    }
    assert_eq!(viewer.call("messages", json!({}), WAIT).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(env.project.path().join("result.txt")).unwrap(),
        "approved"
    );
}

#[test]
fn native_peer_board_and_human_mail_are_session_bound_even_during_inference() {
    let env = Env::new();
    let server = Server::new(vec![json!({"wait_for_interrupt":true})]);
    let launch = launch(&env, &server);
    let mut sender = Client::connect(&launch, WAIT).unwrap();
    let mut recipient = Client::connect(&launch, WAIT).unwrap();
    let from = sender.ready().session_id.clone();
    let old = recipient.ready().session_id.clone();
    let board = |client: &mut Client, session: &str| {
        client
            .call("list_peers", json!({"session_id":session}), WAIT)
            .unwrap()
    };
    let old_peer = board(&mut sender, &from)["rows"][0]["id"].clone();
    let session = recipient.call("new_session", json!({}), WAIT).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let roster = board(&mut sender, &from);
    assert_eq!(roster["rows"].as_array().unwrap().len(), 1, "{roster}");
    let live = &roster["rows"][0]["id"];
    assert_ne!(live, &old_peer);
    let my_peer = board(&mut recipient, &session)["rows"][0]["id"].clone();
    for (method, params, expected) in [
        ("list_peers", json!({}), "invalid_params"),
        ("list_peers", json!({"session_id":old}), "conflict"),
        (
            "send_peer_message",
            json!({"session_id":old,"to":my_peer,"text":"stale"}),
            "conflict",
        ),
        (
            "send_peer_message",
            json!({"session_id":session,"to":"../fake","text":"invalid"}),
            "refused",
        ),
        (
            "send_peer_message",
            json!({"session_id":session,"to":live,"text":"self"}),
            "refused",
        ),
    ] {
        assert_eq!(
            code(recipient.call(method, params, WAIT).unwrap_err()),
            expected
        );
    }
    assert_eq!(
        code(
            sender
                .call(
                    "send_peer_message",
                    json!({"session_id":from,"to":old_peer,"text":"departed"}),
                    WAIT
                )
                .unwrap_err()
        ),
        "refused"
    );
    // Sharing a Dial home must not let a different project inspect or write this inbox.
    let foreign_project = tempfile::tempdir().unwrap();
    let mut outsider =
        Client::connect(&launch.clone().current_dir(foreign_project.path()), WAIT).unwrap();
    let foreign = outsider.ready().session_id.clone();
    assert_eq!(board(&mut outsider, &foreign)["rows"], json!([]));
    assert_eq!(
        code(
            outsider
                .call(
                    "send_peer_message",
                    json!({"session_id":foreign,"to":live,"text":"cross-project"}),
                    WAIT,
                )
                .unwrap_err()
        ),
        "refused"
    );
    assert!(outsider.shutdown(WAIT).unwrap().success());
    assert!(server.requests.lock().is_empty());

    sender.request("send", json!({"text":"wait"})).unwrap();
    server.wait_for_request(WAIT).expect("turn owns the agent");
    let text = "Human message\nwith the entire second line";
    let receipt = sender
        .call(
            "send_peer_message",
            json!({"session_id":from,"to":live,"text":text}),
            WAIT,
        )
        .unwrap();
    assert_eq!(receipt, json!({"session_id":from,"to":live,"queued":true}));
    let sent = &board(&mut sender, &from)["rows"][0]["messages"][0];
    assert_eq!(
        (
            sent["by_user"].as_bool(),
            sent["outgoing"].as_bool(),
            sent["text"].as_str()
        ),
        (Some(true), Some(true), Some(text))
    );
    // Inspecting the same inbox twice does not consume the letter or start a turn.
    for _ in 0..2 {
        let incoming = board(&mut recipient, &session);
        assert_eq!(incoming["queued"], 1);
        let msg = &incoming["rows"][0]["messages"][0];
        assert_eq!(msg["text"], text);
        assert_eq!(msg["unread"], true);
        assert_eq!(msg["outgoing"], false);
    }
    assert_eq!(server.requests.lock().len(), 1);
    sender.kill().unwrap();
    // Let the single-threaded fixture finish serving the interrupted sender.
    server.stopped.send(()).unwrap();
    let turn = recipient.send("read queued mail").unwrap();
    let mut deliveries = Vec::new();
    loop {
        match next(&mut recipient) {
            Message::Event(event) if event.body.get("received_message").is_some() => {
                assert_eq!(event.kind, "notice");
                assert_eq!(event.session_id(), Some(session.as_str()));
                assert_eq!(event.request_id(), Some(turn));
                assert_eq!(
                    event.body["message"],
                    format!(
                        "Project peer {} sent a coordination message.",
                        my_peer.as_str().unwrap()
                    )
                );
                deliveries.push(event.body["received_message"].clone());
            }
            Message::Event(_) => {}
            Message::Response { id, result, .. } if id == turn => {
                result.unwrap();
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(deliveries, [json!({"from":my_peer,"text":text})]);
    assert!(recipient.shutdown(WAIT).unwrap().success());
}
