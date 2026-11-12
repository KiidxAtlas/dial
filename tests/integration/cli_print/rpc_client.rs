//! `dial-client` against the real executable: the lifecycle a frontend depends on (negotiation,
//! approvals, coded errors, disconnect accounting and reconnect-by-resume), with the local
//! scripted provider and no real accounts.
use super::*;
use dial_client::{Client, ClientError, Decision, Effect, Launch, Message};

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
    let mut client = client.restart_and_resume(&launch, WAIT).unwrap();
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
