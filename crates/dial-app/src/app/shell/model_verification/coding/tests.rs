use super::*;
use crate::llm::{fake, model_config::ModelSelection};
use serde_json::Value;
use std::io::{Read, Write};

fn fixture_config(url: &str) -> (tempfile::TempDir, tempfile::TempDir, Config) {
    let (home, project, _layout, config) = crate::app::tests::config();
    let mut selection = ModelSelection::new("fixture", "test");
    selection.base_url = Some(format!("{url}/v1"));
    (home, project, config.with_selection(&selection))
}

#[test]
fn coding_failures_do_not_count_as_roundtrips() {
    for (message, expected) in [
        (
            json!({"role":"assistant","content":"OK"}),
            Outcome::TextOnly,
        ),
        (
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"probe","type":"function","function":{"name":"dial_diagnostic_echo","arguments":"{\"value\":\"wrong\"}"}}]}),
            Outcome::InvalidInvocation,
        ),
    ] {
        let server = fake::serve(vec![fake::json(
            200,
            &json!({"choices":[{"message":message,"finish_reason":"stop"}]}),
        )]);
        let (_home, _project, config) = fixture_config(&server.url);
        assert_eq!(
            test(
                &config,
                Arc::new(AtomicBool::new(false)),
                Duration::from_secs(2)
            )
            .unwrap(),
            expected
        );
        assert_eq!(server.requests().len(), 1);
    }
}

#[test]
fn cancellation_and_deadline_do_not_record_coding_success() {
    let (_home, _project, config) = fixture_config("http://127.0.0.1:1");
    assert!(
        test(
            &config,
            Arc::new(AtomicBool::new(true)),
            Duration::from_secs(1)
        )
        .unwrap_err()
        .to_string()
        .contains("cancelled")
    );
    let server = fake::serve(vec![fake::Script::Stall {
        silence: Duration::from_secs(2),
    }]);
    let (_home, _project, config) = fixture_config(&server.url);
    let started = Instant::now();
    assert!(
        test(
            &config,
            Arc::new(AtomicBool::new(false)),
            Duration::from_millis(200)
        )
        .unwrap_err()
        .to_string()
        .contains("timed out")
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

// A local HTTP peer reads the synthetic result, instead of guessing a receipt from the prompt.
#[test]
fn continuation_must_consume_the_synthetic_result() {
    for (correct, expected) in [
        (true, Outcome::StructuredRoundtrip),
        (false, Outcome::StructuredInvocation),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let (_home, _project, config) =
            fixture_config(&format!("http://{}", listener.local_addr().unwrap()));
        std::thread::scope(|scope| {
            let peer = scope.spawn(move || {
                let mut requests = Vec::new();
                for index in 0..2 {
                    let deadline = Instant::now() + Duration::from_secs(4);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => std::thread::yield_now(),
                            Err(e) => panic!("fixture accept: {e}"),
                        }
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut bytes = Vec::new();
                    let (header_end, length) = loop {
                        let mut chunk = [0; 4096];
                        let n = stream.read(&mut chunk).unwrap();
                        assert!(n > 0);
                        bytes.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..pos]).to_ascii_lowercase();
                            let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                            break (pos + 4, length);
                        }
                    };
                    while bytes.len() < header_end + length {
                        let mut chunk = [0; 4096];
                        let n = stream.read(&mut chunk).unwrap();
                        assert!(n > 0);
                        bytes.extend_from_slice(&chunk[..n]);
                    }
                    let body: Value = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                    let message = if index == 0 {
                        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
                        json!({"role":"assistant","content":null,"tool_calls":[{"id":"probe","type":"function","function":{"name":"dial_diagnostic_echo","arguments":"{\"value\":\"dial-probe\"}"}}]})
                    } else {
                        let result: Value = serde_json::from_str(body["messages"][2]["content"].as_str().unwrap()).unwrap();
                        assert_eq!(body["messages"][2]["tool_call_id"], "probe");
                        json!({"role":"assistant","content":if correct { result["receipt"].as_str().unwrap() } else { "ignored result" }})
                    };
                    assert!(body["max_tokens"].as_u64().unwrap() <= 128);
                    requests.push(body);
                    let response = json!({"choices":[{"message":message,"finish_reason":"stop"}]}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
                }
                requests
            });
            assert_eq!(
                test(
                    &config,
                    Arc::new(AtomicBool::new(false)),
                    Duration::from_secs(3)
                )
                .unwrap(),
                expected
            );
            assert_eq!(peer.join().unwrap().len(), 2, "no automatic retry");
        });
    }
}

#[test]
fn coding_evidence_is_separate_and_exact_configuration_scoped() {
    let (_home, _project, app, mut shell) = crate::app::shell::testkit::shell();
    shell.coding_verification = Some((
        identity(&app.config).unwrap(),
        Some(Outcome::StructuredRoundtrip),
    ));
    assert!(
        shell
            .model_response_status(&app.config)
            .starts_with("Unknown")
    );
    assert!(
        shell
            .model_coding_status(&app.config)
            .starts_with("Structured tool roundtrip verified")
    );
    let mut changed = app.config.clone();
    changed.model = "other".into();
    assert!(shell.model_coding_status(&changed).starts_with("Unknown"));
    shell.status.phase = crate::ui::status::Phase::Working;
    shell.verify_coding_model(&app, None).unwrap();
    assert!(shell.deferred.is_empty());
}
