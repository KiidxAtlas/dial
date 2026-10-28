use super::*;
use crate::config::types::{Config, EndpointConfig, Version};
use crate::llm::fake::{self, Script};
use crate::llm::provider::{Backend, ProviderKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn sse_text(text: &str) -> String {
    let event = json!({"choices":[{"delta":{"content":text}}]});
    format!("data: {event}\n\n")
}

/// A config whose endpoint is the local fake server, so `make_backend` resolves a plain endpoint.
fn side_config(url: &str, user_dir: &Path) -> Config {
    Config {
        provider: Some("endpoint".into()),
        model: "test".into(),
        user_dir: user_dir.to_path_buf(),
        endpoint: Some(EndpointConfig {
            headers: None,
            base_url: format!("{url}/v1"),
            api_key_env: None,
            allow_insecure: true,
            protocol: None,
            account_id: None,
        }),
        ..Config::default()
    }
}

fn hermetic_agent<'a>(config: &'a Config, workspace: &Path) -> Agent<'a> {
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let version = Version {
        id: 0,
        parent: None,
        kind: "config".into(),
        harness: config.harness.clone(),
        adapter: None,
        status: "active".into(),
    };
    Agent::hermetic(config, backend, &version, workspace, None, None).unwrap()
}

#[test]
fn pending_prompt_is_the_last_context_before_the_side_question() {
    let dir = tempfile::tempdir().unwrap();
    let config = side_config("http://127.0.0.1:1", dir.path());
    let agent = hermetic_agent(&config, dir.path());

    let snapshot = agent.side_snapshot(Some("fix the build")).unwrap();
    let request = snapshot.request(&[], "why did it fail?").unwrap();
    let messages = &request.chat.messages;
    let prompts = messages
        .iter()
        .filter(|m| m["content"] == "fix the build")
        .count();
    assert_eq!(prompts, 1, "active prompt appears exactly once");
    assert_eq!(
        messages[messages.len() - 2]["content"],
        "fix the build",
        "active prompt precedes the side question"
    );
}

#[test]
fn side_request_cancel_stops_only_the_side_stream() {
    // Text, then keepalives for about 10s: the stream only ends early if the side cancel is honored.
    let server = fake::serve(vec![Script::DripAfter {
        first: sse_text("Partial answer"),
        pause: Duration::from_millis(20),
        count: 500,
    }]);
    let dir = tempfile::tempdir().unwrap();
    let config = side_config(&server.url, dir.path());
    let agent = hermetic_agent(&config, dir.path());
    let snapshot = agent.side_snapshot(None).unwrap();
    let request = snapshot.request(&[], "what happened?").unwrap();

    // The main turn's interrupt is a separate flag that the side request never receives.
    let main_turn_interrupt = Arc::new(AtomicBool::new(false));
    let side_cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<String>();
    let cancel = Arc::clone(&side_cancel);
    let handle = std::thread::spawn(move || {
        request.run(cancel, &mut |text: &str| {
            let _ = tx.send(text.to_owned());
        })
    });

    let first = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first side text");
    assert!(first.contains("Partial"));

    main_turn_interrupt.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !handle.is_finished(),
        "main-turn interrupt must not stop the side answer"
    );

    side_cancel.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let reply = handle.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "side cancel must end the stream promptly"
    );
    if let Ok(text) = reply {
        assert!(
            text.starts_with("Partial"),
            "partial text is kept: {text:?}"
        );
    }
}
