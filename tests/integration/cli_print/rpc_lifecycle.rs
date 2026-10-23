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
