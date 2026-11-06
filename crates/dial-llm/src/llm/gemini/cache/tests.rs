use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    thread,
};

type Recorded = (String, Value);
fn server(replies: Vec<(u16, Value)>) -> (String, thread::JoinHandle<Vec<Recorded>>) {
    server_with_cancel(replies, None)
}
fn server_with_cancel(
    replies: Vec<(u16, Value)>,
    cancel: Option<(usize, Arc<AtomicBool>)>,
) -> (String, thread::JoinHandle<Vec<Recorded>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1beta", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let mut records = Vec::new();
        for (index, (status, body)) in replies.into_iter().enumerate() {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "expected cache HTTP request");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            // Accepted sockets inherit the nonblocking listener flag on macOS. Reading the
            // request must block until bytes arrive, rather than race the client serializer.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut length = 0;
            let mut authenticated = false;
            loop {
                let mut line = String::new();
                assert!(
                    reader.read_line(&mut line).unwrap() > 0,
                    "request headers ended early"
                );
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                    if key.eq_ignore_ascii_case("x-goog-api-key") {
                        authenticated = value.trim() == "test-key";
                    }
                }
            }
            assert!(
                authenticated,
                "cache requests preserve header authentication"
            );
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            let request = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            records.push((first.trim().to_owned(), request));
            let body = body.to_string();
            if let Some((at, flag)) = &cancel
                && *at == index
            {
                flag.store(true, Ordering::SeqCst);
            }
            let sent = write!(
                stream,
                "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            if !cancel.as_ref().is_some_and(|(at, _)| *at == index) {
                sent.unwrap();
            }
        }
        records
    });
    (base, worker)
}
fn body() -> Value {
    json!({"systemInstruction":{"parts":[{"text":"stable system"}]},
        "tools":[{"functionDeclarations":[{"name":"read","parametersJsonSchema":{"type":"object"}}]}],
        "contents":[{"role":"user","parts":[{"text":"immutable task"}]},
            {"role":"model","parts":[{"text":"prior answer"}]},
            {"role":"user","parts":[{"text":"transient tail"}]}],
        "generationConfig":{"temperature":0.3}})
}
fn endpoint(url: &str) -> Endpoint<'_> {
    Endpoint {
        url,
        auth: Some("test-key"),
        model: "gemini-2.5-pro",
        headers: &[],
    }
}
fn prepare_body(endpoint: &Endpoint<'_>, scope: &str, body: &mut Value) -> Result<Option<Used>> {
    prepare(
        endpoint,
        body,
        Some(&GeminiCache::default()),
        scope,
        &Arc::new(AtomicBool::new(false)),
        3,
    )
}
fn created(name: &str) -> Value {
    json!({"name":name,"expireTime":"2099-01-01T00:00:00Z"})
}

#[test]
fn repeated_prefix_creates_reuses_and_isolates_changes_and_scopes() {
    let (base, worker) = server(vec![
        (200, json!({"totalTokens":5000})),
        (200, created("cachedContents/test")),
        (200, json!({})),
    ]);
    let url = format!("{base}/models/gemini-2.5-pro:streamGenerateContent?alt=sse");
    let endpoint = endpoint(&url);
    let scope = url.clone();
    assert!(
        prepare_body(&endpoint, &scope, &mut body())
            .unwrap()
            .is_none()
    );
    let mut cached = body();
    let used = prepare_body(&endpoint, &scope, &mut cached)
        .unwrap()
        .unwrap();
    assert_eq!(cached["cachedContent"], "cachedContents/test");
    assert!(cached.get("systemInstruction").is_none());
    assert!(cached.get("tools").is_none());
    assert_eq!(cached["contents"].as_array().unwrap().len(), 2);
    assert_eq!(cached["contents"][1]["parts"][0]["text"], "transient tail");
    let mut next = body();
    next["contents"][2]["parts"][0]["text"] = json!("new transient tail");
    assert!(
        prepare_body(&endpoint, &scope, &mut next)
            .unwrap()
            .is_some()
    );
    assert_eq!(next["cachedContent"], "cachedContents/test");
    let mut changed = body();
    changed["tools"][0]["functionDeclarations"][0]["name"] = json!("write");
    assert!(
        prepare_body(&endpoint, &scope, &mut changed)
            .unwrap()
            .is_none()
    );
    assert!(changed.get("cachedContent").is_none());
    assert!(
        prepare_body(
            &endpoint,
            &format!("{scope}-other-credentials"),
            &mut body()
        )
        .unwrap()
        .is_none()
    );
    invalidate(&endpoint, &used);
    let records = worker.join().unwrap();
    assert!(records[0].0.contains(":countTokens"));
    assert_eq!(
        records[0].1["generateContentRequest"]["contents"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(records[1].0, "POST /v1beta/cachedContents HTTP/1.1");
    assert_eq!(records[1].1["ttl"], "300s");
    assert!(!records[1].1.to_string().contains("transient tail"));
    assert_eq!(records[2].0, "DELETE /v1beta/cachedContents/test HTTP/1.1");
}

#[test]
fn expired_server_resource_is_deleted_and_rejected() {
    let (base, worker) = server(vec![
        (200, json!({"totalTokens":5000})),
        (
            200,
            json!({"name":"cachedContents/expired","expireTime":"2000-01-01T00:00:00Z"}),
        ),
        (200, json!({})),
    ]);
    let url = format!("{base}/models/gemini-2.5-pro:streamGenerateContent?alt=sse");
    let endpoint = endpoint(&url);
    prepare_body(&endpoint, &url, &mut body()).unwrap();
    assert!(
        prepare_body(&endpoint, &url, &mut body())
            .err()
            .unwrap()
            .to_string()
            .contains("expired")
    );
    assert!(worker.join().unwrap()[2].0.starts_with("DELETE "));
}

#[test]
fn local_expiry_does_not_reuse_and_below_threshold_is_not_counted_again() {
    let (base, worker) = server(vec![(200, json!({"totalTokens":200}))]);
    let url = format!("{base}/models/gemini-2.5-pro:streamGenerateContent?alt=sse");
    let endpoint = endpoint(&url);
    prepare_body(&endpoint, &url, &mut body()).unwrap();
    prepare_body(&endpoint, &url, &mut body()).unwrap();
    prepare_body(&endpoint, &url, &mut body()).unwrap();
    assert_eq!(worker.join().unwrap().len(), 1);
    for slot in SLOTS.lock().iter_mut().filter(|slot| slot.scope == url) {
        slot.expires = Instant::now();
    }
    // Expiry starts a new observation, never an immediate paid cache recreation.
    assert!(
        prepare_body(&endpoint, &url, &mut body())
            .unwrap()
            .is_none()
    );
}

#[test]
fn rejected_creation_is_not_retried_as_an_ambiguous_billed_mutation() {
    let (base, worker) = server(vec![
        (200, json!({"totalTokens":5000})),
        (400, json!({"error":{"message":"cache unavailable"}})),
    ]);
    let url = format!("{base}/models/gemini-2.5-pro:streamGenerateContent?alt=sse");
    let endpoint = endpoint(&url);
    prepare_body(&endpoint, &url, &mut body()).unwrap();
    assert!(prepare_body(&endpoint, &url, &mut body()).is_err());
    assert!(
        prepare_body(&endpoint, &url, &mut body())
            .unwrap()
            .is_none()
    );
    assert_eq!(worker.join().unwrap().len(), 2);
}

#[test]
fn off_unknown_models_bounds_and_cancel_never_send_a_cache_request() {
    let url = "http://127.0.0.1:1/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse";
    let mut endpoint = endpoint(url);
    let interrupt = Arc::new(AtomicBool::new(false));
    assert!(
        prepare(&endpoint, &mut body(), None, "off", &interrupt, 1)
            .unwrap()
            .is_none()
    );
    for model in [
        "gemini-4-pro",
        "custom-gemini-2.5-pro",
        "gemini-3.99-pro",
        "gemini-2.0-flash",
    ] {
        endpoint.model = model;
        assert!(
            prepare(
                &endpoint,
                &mut body(),
                Some(&GeminiCache::default()),
                model,
                &interrupt,
                1
            )
            .unwrap()
            .is_none()
        );
    }
    endpoint.model = "gemini-2.5-pro";
    let config = GeminiCache {
        max_entries: 1,
        ..Default::default()
    };
    prepare(
        &endpoint,
        &mut body(),
        Some(&config),
        "capacity",
        &interrupt,
        1,
    )
    .unwrap();
    let mut changed = body();
    changed["systemInstruction"] = json!({"parts":[{"text":"changed"}]});
    assert!(
        prepare(
            &endpoint,
            &mut changed,
            Some(&config),
            "capacity",
            &interrupt,
            1
        )
        .unwrap()
        .is_none()
    );
    interrupt.store(true, Ordering::SeqCst);
    assert!(
        prepare(
            &endpoint,
            &mut body(),
            Some(&config),
            "capacity",
            &interrupt,
            1
        )
        .unwrap_err()
        .is::<Interrupted>()
    );
}

#[test]
fn cache_profile_defaults_and_validation_are_typed() {
    let profile: llm::profiles::ModelProfile =
        toml::from_str("provider='gemini'\nmodel='gemini-2.5-pro'\n[gemini_cache]\nttl_secs=60")
            .unwrap();
    profile.validate().unwrap();
    assert_eq!(profile.gemini_cache.as_ref().unwrap().min_tokens, 4096);
    for (field, value) in [
        ("ttl_secs", 0),
        ("ttl_secs", 3601),
        ("min_tokens", 1),
        ("max_entries", 33),
        ("max_prefix_bytes", 0),
    ] {
        let text =
            format!("provider='gemini'\nmodel='gemini-2.5-pro'\n[gemini_cache]\n{field}={value}");
        assert!(
            toml::from_str::<llm::profiles::ModelProfile>(&text)
                .unwrap()
                .validate()
                .is_err(),
            "{field}={value}"
        );
    }
    let mut profile = profile;
    profile.provider = "openai".into();
    assert!(profile.validate().is_err());
}

#[test]
fn cached_generation_rejection_deletes_without_replaying_or_dropping_policy() {
    let rejection = json!({"error":{"message":"cached content expired"}});
    let (base, worker) = server(vec![
        (404, rejection.clone()),
        (200, json!({"totalTokens":5000})),
        (200, created("cachedContents/rejected")),
        (404, rejection),
        (200, json!({})),
    ]);
    let url = format!("{base}/models/gemini-2.5-pro:streamGenerateContent?alt=sse");
    let chat = llm::Chat {
        messages: vec![
            json!({"role":"system","content":"stable policy"}),
            json!({"role":"user","content":"original task"}),
            json!({"role":"assistant","content":"prior answer"}),
            json!({"role":"user","content":"new question"}),
        ],
        tools: vec![
            json!({"type":"function","function":{"name":"read","parameters":{"type":"object"}}}),
        ],
        temperature: 0.3,
        max_tokens: 64,
        seed: None,
    };
    let profile = llm::profiles::ModelProfile {
        provider: "gemini".into(),
        model: "gemini-2.5-pro".into(),
        gemini_cache: Some(GeminiCache::default()),
        ..Default::default()
    };
    for _ in 0..2 {
        let result = super::super::exchange_at(
            endpoint(&url),
            &chat,
            llm::Limits {
                timeout_secs: 3,
                interrupt: Arc::new(AtomicBool::new(false)),
            },
            &mut None,
            Some(&profile),
            &mut |_| true,
        );
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("cached content expired"),
            "unexpected generation failure: {error:#}"
        );
    }
    let records = worker.join().unwrap();
    assert_eq!(
        records.len(),
        5,
        "no generation replay after a cache rejection"
    );
    assert_eq!(
        records[0].1["tools"][0]["functionDeclarations"][0]["name"],
        "read"
    );
    assert_eq!(records[2].1["tools"], records[0].1["tools"]);
    assert_eq!(
        records[2].1["systemInstruction"],
        records[0].1["systemInstruction"]
    );
    assert_eq!(records[3].1["cachedContent"], "cachedContents/rejected");
    assert!(records[3].1.get("tools").is_none());
    assert_eq!(
        records[4].0,
        "DELETE /v1beta/cachedContents/rejected HTTP/1.1"
    );
}

#[test]
fn cancellation_during_cache_creation_is_interrupted_and_ttl_bounded() {
    let interrupt = Arc::new(AtomicBool::new(false));
    let (base, worker) = server_with_cancel(
        vec![
            (200, json!({"totalTokens":5000})),
            (200, created("cachedContents/cancelled")),
        ],
        Some((1, Arc::clone(&interrupt))),
    );
    let url = format!("{base}/models/gemini-2.5-pro:streamGenerateContent?alt=sse");
    let endpoint = endpoint(&url);
    let config = GeminiCache::default();
    prepare(&endpoint, &mut body(), Some(&config), &url, &interrupt, 3).unwrap();
    assert!(
        prepare(&endpoint, &mut body(), Some(&config), &url, &interrupt, 3)
            .unwrap_err()
            .is::<Interrupted>()
    );
    assert_eq!(worker.join().unwrap().len(), 2);
    interrupt.store(false, Ordering::SeqCst);
    assert!(
        prepare(&endpoint, &mut body(), Some(&config), &url, &interrupt, 3)
            .unwrap()
            .is_none()
    );
}
