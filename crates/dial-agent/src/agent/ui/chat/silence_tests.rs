use super::*;

#[test]
fn provider_backoff_is_not_a_silent_stall() {
    let idle = Duration::from_secs(150);
    // Genuine remote silence past the limit is still bounded.
    assert!(silence_exceeded(120, false, false, false, idle));
    // A retry delay longer than the limit is an explicit wait, not silence.
    assert!(!silence_exceeded(120, false, false, true, idle));
    // Local prefill stays unarmed until generation is observed.
    assert!(!silence_exceeded(120, true, false, false, idle));
    assert!(silence_exceeded(120, true, true, false, idle));
    // Disabled limit, and idle time below the limit.
    assert!(!silence_exceeded(0, false, true, false, idle));
    assert!(!silence_exceeded(
        120,
        false,
        true,
        false,
        Duration::from_secs(119)
    ));
}

#[test]
fn live_retry_control_reaches_the_ui_through_state_ui_and_is_unbound_on_finish() {
    struct RetryUi {
        control: Option<llm::provider::RetryControl>,
        requests: usize,
        cancel: bool,
        interrupt: Arc<AtomicBool>,
    }
    impl Ui for RetryUi {
        fn provider_retry_control(&mut self, control: Option<llm::provider::RetryControl>) {
            self.control = control;
        }
        fn request_phase(&mut self, phase: RequestPhase) {
            if phase == RequestPhase::Backoff {
                let control = self
                    .control
                    .as_ref()
                    .expect("active request must bind its control");
                self.requests += 1;
                if self.cancel {
                    // Cancellation travels from the UI to the worker on its heartbeat. Do not
                    // race it with a separate command that releases the wait first.
                    self.interrupt.store(true, Ordering::SeqCst);
                } else {
                    assert!(control.request_now());
                    assert!(!control.request_now());
                }
            }
        }
    }
    for cancel in [false, true] {
        let answer = json!({"choices":[{"delta":{"content":"recovered"},"finish_reason":"stop"}]});
        let mut scripts = vec![llm::fake::json(503, &json!({"error":"busy"}))];
        if !cancel {
            scripts.push(llm::fake::Script::Sse(format!(
                "data: {answer}\n\ndata: [DONE]\n\n"
            )));
        }
        let server = llm::fake::serve(scripts);
        let mut backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: server.url.clone(),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        );
        let request = Chat {
            messages: vec![json!({"role":"user","content":"finish the task"})],
            tools: vec![],
            temperature: 0.0,
            max_tokens: 64,
            seed: None,
        };
        let interrupt = Arc::new(AtomicBool::new(false));
        let mut ui = RetryUi {
            control: None,
            requests: 0,
            cancel,
            interrupt: interrupt.clone(),
        };
        let mut wrapped = crate::agent::ui::state_ui::StateUi::new(
            &mut ui,
            crate::runtime::Tree::default(),
            None,
        );
        let result = chat_with_ui(
            &mut backend,
            None,
            &request,
            ChatOptions {
                timeout: 5,
                render_text: true,
                context_tokens: None,
                policy: None,
                recovery: None,
            },
            interrupt,
            &mut wrapped,
            &mut |_| {},
        );
        if cancel {
            assert!(
                result
                    .unwrap_err()
                    .downcast_ref::<llm::Interrupted>()
                    .is_some()
            );
        } else {
            assert_eq!(result.unwrap().text, "recovered");
        }
        assert_eq!(ui.requests, 1);
        assert!(
            ui.control.is_none(),
            "finished request must unbind its control"
        );
        assert!(!backend.retry_control().request_now());
        assert_eq!(server.requests().len(), if cancel { 1 } else { 2 });
    }
}

#[test]
fn telemetry_requires_advancing_phase_or_token_counts() {
    let mut watch = TelemetryProgress::default();
    let mut value = llm::ModelProgress::new(llm::InferencePhase::Generating);
    value.output_tokens = Some(3);
    assert!(watch.observe(&value));
    assert!(!watch.observe(&value));
    value.tokens_per_second = Some(0.5);
    value.eta_secs = Some(12.0);
    assert!(!watch.observe(&value));
    value.output_tokens = None;
    assert!(!watch.observe(&value));
    value.output_tokens = Some(2);
    assert!(!watch.observe(&value));
    value.output_tokens = Some(4);
    assert!(watch.observe(&value));
    value.phase = llm::InferencePhase::Processing;
    assert!(!watch.observe(&value));
    value.phase = llm::InferencePhase::Generating;
    assert!(!watch.observe(&value));
    // A new attempt owns fresh marks, so its first report is progress again.
    assert!(TelemetryProgress::default().observe(&value));
}

fn retry_preserves_request_controls(automatic: bool) {
    struct RecoveryUi {
        prompts: usize,
    }
    impl Ui for RecoveryUi {
        fn interactive_input(&self) -> bool {
            true
        }
        fn ask_user(&mut self, _: &str, _: &str, options: &[String]) -> Option<String> {
            self.prompts += 1;
            assert_eq!(self.prompts, 1, "the selected retry should recover");
            Some(options[0].clone())
        }
    }
    let progress = json!({"dial_progress":{"phase":"generating","output_tokens":1}});
    let answer = json!({"choices":[{"delta":{"content":"recovered"},"finish_reason":"stop"}]});
    let server = llm::fake::serve(vec![
        llm::fake::Script::StallAfter {
            first: format!("data: {progress}\n\n"),
            silence: Duration::from_millis(1500),
        },
        llm::fake::Script::Sse(format!("data: {answer}\n\ndata: [DONE]\n\n")),
        llm::fake::Script::Sse(format!("data: {answer}\n\ndata: [DONE]\n\n")),
    ]);
    let baseline = llm::profiles::ModelProfile {
        provider: "endpoint".into(),
        model: "test".into(),
        capabilities: llm::profiles::ModelCapabilities {
            temperature: true,
            top_p: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut adaptive = baseline.clone();
    adaptive.temperature = Some(0.17);
    adaptive.top_p = Some(0.83);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: format!("{}/v1", server.url),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    )
    .with_profile(baseline);
    backend.next_request_profile(Some(adaptive));
    let request = Chat {
        messages: vec![json!({"role":"user","content":"finish the existing task"})],
        tools: vec![],
        temperature: 0.5,
        max_tokens: 64,
        seed: None,
    };
    let recovery = crate::config::types::RequestRecoveryConfig {
        silence_secs: 1,
        retry_once: automatic,
    };
    let options = ChatOptions {
        timeout: 5,
        render_text: true,
        context_tokens: None,
        policy: None,
        recovery: Some(&recovery),
    };
    let mut ui = RecoveryUi { prompts: 0 };
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        options,
        Arc::new(AtomicBool::new(false)),
        &mut ui,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(reply.text, "recovered");
    assert_eq!(ui.prompts, usize::from(!automatic));
    // A later independent request must still use baseline controls, not the retry's override.
    backend
        .chat(None, &request, 5, Some(&mut |_| true))
        .unwrap();
    let sent = server.requests();
    assert_eq!(sent.len(), 3);
    let first = llm::fake::body(&sent[0]);
    let retried = llm::fake::body(&sent[1]);
    assert_eq!(
        retried, first,
        "retry must retain the logical request's wire controls"
    );
    let next = llm::fake::body(&sent[2]);
    assert_eq!(next["temperature"], request.temperature);
    assert!(next.get("top_p").is_none());
}

#[test]
fn automatic_retry_preserves_request_controls() {
    retry_preserves_request_controls(true);
}

#[test]
fn popup_retry_preserves_request_controls() {
    retry_preserves_request_controls(false);
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::{BufRead, Read};
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = std::io::BufReader::new(stream);
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        headers.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let length = llm::fake::header(headers.as_bytes(), "content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut request = headers.into_bytes();
    let offset = request.len();
    request.resize(offset + length, 0);
    reader.read_exact(&mut request[offset..]).unwrap();
    request
}

fn codex_watchdog_retry(cancel: bool) {
    use std::io::{Read, Write};
    struct Retry;
    impl Ui for Retry {
        fn interactive_input(&self) -> bool {
            true
        }
        fn ask_user(&mut self, _: &str, _: &str, options: &[String]) -> Option<String> {
            Some(options[0].clone())
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let original = read_http_request(&mut first);
        started_tx.send(()).unwrap();
        // A silent upgrade, not a rejected upgrade: only the agent watchdog can recover it.
        let mut byte = [0];
        assert_eq!(first.read(&mut byte).unwrap(), 0);
        if cancel {
            return (original, None);
        }
        let (mut second, _) = listener.accept().unwrap();
        let retried = read_http_request(&mut second);
        let body = format!(
            "data: {}\n\n",
            json!({"type":"response.completed","response":{
                "status":"completed","output":[{"type":"message","id":"recovered",
                "role":"assistant","content":[{"type":"output_text","text":"recovered"}]}]
            }})
        );
        write!(second, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        (original, Some(retried))
    });
    let dir = tempfile::tempdir().unwrap();
    llm::chatgpt::test_account(dir.path(), false);
    let mut backend = Backend::new(
        ProviderKind::ChatGpt {
            user_dir: dir.path().into(),
            model: "test".into(),
        },
        PathBuf::new(),
        5,
    )
    .with_account("oaiapp_test".into())
    .with_codex_websocket(true);
    backend.chatgpt_url = Some(format!("http://{address}/responses"));
    backend.retry_backoff = Duration::ZERO;
    let request = Chat {
        messages: vec![json!({"role":"user","content":"resume this task"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let recovery = crate::config::types::RequestRecoveryConfig {
        silence_secs: 1,
        retry_once: false,
    };
    let interrupt = Arc::new(AtomicBool::new(false));
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: true,
            context_tokens: None,
            policy: None,
            recovery: Some(&recovery),
        },
        interrupt.clone(),
        &mut Retry,
        &mut |_| {
            if cancel && started_rx.try_recv().is_ok() {
                interrupt.store(true, Ordering::SeqCst);
            }
        },
    );
    let (original, retried) = server.join().unwrap();
    assert!(llm::fake::request_line(&original).starts_with("GET "));
    if cancel {
        assert!(
            reply
                .unwrap_err()
                .downcast_ref::<llm::Interrupted>()
                .is_some()
        );
        assert!(retried.is_none(), "user cancellation must not retry");
        assert!(
            backend.live_steering_key().is_some(),
            "user cancellation must not disable WebSocket"
        );
        return;
    }
    let retried = retried.unwrap();
    assert!(
        llm::fake::request_line(&retried).starts_with("POST "),
        "a watchdog retry must not repeat the stalled WebSocket upgrade"
    );
    assert_eq!(
        llm::fake::body(&retried)["input"],
        json!([{"role":"user","content":"resume this task"}])
    );
    assert_eq!(reply.unwrap().text, "recovered");
}

#[test]
fn popup_retry_after_watchdog_uses_http_instead_of_another_silent_upgrade() {
    codex_watchdog_retry(false);
}

#[test]
fn user_cancellation_does_not_retry_or_disable_codex_websocket() {
    codex_watchdog_retry(true);
}

#[test]
fn anthropic_signed_thinking_activity_prevents_false_silence_retries() {
    struct QuietUi;
    impl Ui for QuietUi {}
    let events = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":4,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"planning","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed-part-1"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed-part-2"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed-part-3"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"done"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}),
        json!({"type":"message_stop"}),
    ];
    let server = llm::fake::serve(vec![llm::fake::Script::SseSlow {
        pieces: events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect(),
        pause: Duration::from_millis(300),
    }]);
    let mut backend = Backend::new(
        ProviderKind::AnthropicEndpoint {
            base_url: server.url.clone(),
            model: "claude-opus-5-5".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let request = Chat {
        messages: vec![json!({"role":"user","content":"finish this task"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let recovery = crate::config::types::RequestRecoveryConfig {
        silence_secs: 1,
        retry_once: false,
    };
    let reply = chat_with_ui(
        &mut backend,
        None,
        &request,
        ChatOptions {
            timeout: 5,
            render_text: false,
            context_tokens: None,
            policy: None,
            recovery: Some(&recovery),
        },
        Arc::new(AtomicBool::new(false)),
        &mut QuietUi,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(reply.text, "done");
    assert_eq!(
        server.requests().len(),
        1,
        "active inference must not be replayed"
    );
}
