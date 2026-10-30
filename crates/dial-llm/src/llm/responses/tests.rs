use super::super::fake::{self, Script};
use super::*;
#[test]
fn tool_free_requests_send_an_empty_tools_array() {
    let mut chat = chat();
    chat.tools.clear();
    for subscription in [false, true] {
        let body = request_flat("gpt", &chat, None, subscription).unwrap();
        assert_eq!(body.get("tools"), Some(&json!([])));
    }
}

fn chat() -> Chat {
    Chat {
        messages: vec![
            json!({"role":"system","content":"system guidance"}),
            json!({"role":"user","content":"write marker"}),
        ],
        tools: vec![
            json!({"type":"function","function":{"name":"write","description":"write a file","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}}}),
        ],
        temperature: 0.2,
        max_tokens: 2048,
        seed: Some(42),
    }
}
fn call() -> Value {
    json!({"type":"function_call","id":"item_write","status":"completed","namespace":"dial","call_id":"call_write","name":"write","arguments":"{\"path\":\"marker.txt\",\"content\":\"ok\"}"})
}
fn response(output: Vec<Value>) -> Value {
    json!({"status":"completed","output":output,"usage":{"input_tokens":50,"output_tokens":12,"input_tokens_details":{"cached_tokens":20},"output_tokens_details":{"reasoning_tokens":3}}})
}
fn sse(events: Vec<Value>) -> String {
    events
        .into_iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect()
}
#[test]
fn responses_send_only_advertised_max_and_ultra_reasoning_levels() {
    use super::super::profiles::{ModelCapabilities, ModelProfile, ReasoningApi, ReasoningEffort};
    for effort in [
        ReasoningEffort::High,
        ReasoningEffort::Max,
        ReasoningEffort::Ultra,
    ] {
        let mut profile = ModelProfile {
            provider: "chatgpt".into(),
            model: "account-model".into(),
            reasoning_effort: Some(effort),
            capabilities: ModelCapabilities {
                reasoning: ReasoningApi::OpenaiResponses,
                reasoning_efforts: vec![effort],
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            request("account-model", &chat(), Some(&profile)).unwrap()["reasoning"]["effort"],
            effort.as_str()
        );
        profile.capabilities.reasoning_efforts.clear();
        assert!(request("account-model", &chat(), Some(&profile)).is_err());
    }
}
#[test]
fn reasoning_effort_wire_reaches_responses_reasoning_effort() {
    use super::super::profiles::{ModelCapabilities, ModelProfile, ReasoningApi, ReasoningEffort};
    use std::collections::BTreeMap;
    let mut profile = ModelProfile {
        provider: "chatgpt".into(),
        model: "account-model".into(),
        reasoning_effort: Some(ReasoningEffort::High),
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::OpenaiResponses,
            reasoning_efforts: vec![ReasoningEffort::High],
            reasoning_effort_wire: BTreeMap::from([(ReasoningEffort::High, "max".to_string())]),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        request("account-model", &chat(), Some(&profile)).unwrap()["reasoning"]["effort"],
        "max"
    );
    profile.capabilities.reasoning_effort_wire.clear();
    assert_eq!(
        request("account-model", &chat(), Some(&profile)).unwrap()["reasoning"]["effort"],
        "high"
    );
}
#[test]
fn requests_use_namespaces_full_history_and_only_supported_parameters() {
    let body = request("account-model", &chat(), None).unwrap();
    assert_eq!(body["model"], "account-model");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(body["tools"][0]["type"], "namespace");
    assert_eq!(body["tools"][0]["name"], "dial");
    assert_eq!(body["tools"][0]["tools"][0]["name"], "write");
    assert_eq!(body["tools"][0]["tools"][0]["strict"], false);
    for field in [
        "temperature",
        "max_output_tokens",
        "max_tokens",
        "seed",
        "previous_response_id",
        "metadata",
        "conversation",
        "background",
    ] {
        assert!(body.get(field).is_none(), "unexpected {field}");
    }
}
#[test]
fn sse_codex_requests_carry_session_affinity_headers() {
    let done = json!({"type":"response.completed","response":response(vec![])});
    let server = fake::serve(vec![
        Script::Sse(sse(vec![done.clone()])),
        Script::Sse(sse(vec![done])),
    ]);
    for session in [Some("session-1"), None] {
        exchange_codex(
            Endpoint {
                headers: &[],
                url: &server.url,
                token: "fixture-token",
                account_id: Some("acct"),
                model: "account-model",
            },
            &chat(),
            Limits {
                timeout_secs: 5,
                interrupt: Arc::new(AtomicBool::new(false)),
            },
            &mut None,
            None,
            &mut |_| true,
            CacheKeys {
                prompt_cache_key: Some("cache-key"),
                session_key: session,
            },
        )
        .unwrap();
    }
    let requests = server.requests();
    let head = |raw: &[u8]| String::from_utf8_lossy(raw).to_ascii_lowercase();
    let (keyed, unkeyed) = (head(&requests[0]), head(&requests[1]));
    assert!(keyed.contains("\r\nsession_id: session-1\r\n"), "{keyed}");
    assert!(keyed.contains("\r\nconversation_id: session-1\r\n"));
    assert!(keyed.contains("\"prompt_cache_key\":\"cache-key\""));
    assert!(!unkeyed.contains("session_id:") && !unkeyed.contains("conversation_id:"));
}
/// The bytes the backend tokenizes, in order: instructions, tools, then each input item.
fn prompt_stream(body: &Value) -> String {
    let mut stream = format!("{}{}", body["instructions"], body["tools"]);
    for item in body["input"].as_array().unwrap() {
        stream.push_str(&item.to_string());
    }
    stream
}
#[test]
fn consecutive_codex_requests_extend_the_previous_prompt_byte_for_byte() {
    let mut c = chat();
    // Schemas whose keys are not in sorted order, as tool registries produce them.
    c.tools.push(json!({"type":"function","function":{"name":"bash","description":"run","parameters":{"type":"object","required":["command"],"properties":{"timeout":{"type":"integer"},"command":{"type":"string"}}}}}));
    c.tools.push(json!({"type":"function","function":{"name":"read","description":"read","parameters":{"type":"object","properties":{"path":{"type":"string"},"lines":{"type":"array","items":{"type":"integer"}}}}}}));
    let first = request_codex("account-model", &c, None).unwrap();
    let reasoning =
        json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque"});
    let reply = completed(&response(vec![reasoning, call()])).unwrap();
    c.messages.push(super::super::assistant_message(&reply));
    c.messages
        .push(json!({"role":"tool","tool_call_id":"call_write","content":"wrote marker"}));
    let second = request_codex("account-model", &c, None).unwrap();
    let text = json!({"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"done","annotations":[]}]});
    c.messages.push(super::super::assistant_message(
        &completed(&response(vec![text])).unwrap(),
    ));
    c.messages
        .push(json!({"role":"user","content":"now check it"}));
    let third = request_codex("account-model", &c, None).unwrap();
    for (earlier, later) in [(&first, &second), (&second, &third)] {
        let (earlier, later) = (prompt_stream(earlier), prompt_stream(later));
        assert!(
            later.len() > earlier.len() && later.starts_with(&earlier),
            "request is not a byte-identical prefix of the next:\n{earlier}\n---\n{later}"
        );
    }
    // Everything else that selects the cache entry is unchanged across the turn.
    for key in [
        "model",
        "reasoning",
        "text",
        "tool_choice",
        "parallel_tool_calls",
        "store",
    ] {
        assert_eq!(first.get(key), third.get(key), "{key} changed");
    }
    // A resumed session serializes the same bytes as the live one (JSON round trip stands in
    // for session persistence, which storage covers).
    let mut resumed = chat();
    resumed
        .messages
        .extend(c.messages[chat().messages.len()..].iter().map(persisted));
    resumed.tools = c.tools.clone();
    assert_eq!(
        prompt_stream(&request_codex("account-model", &resumed, None).unwrap()),
        prompt_stream(&third)
    );
}
#[test]
fn tool_results_and_encrypted_reasoning_roundtrip_after_session_resume() {
    let reasoning = json!({"type":"reasoning","id":"reasoning_id","summary":[],"encrypted_content":"opaque-encrypted-evidence"});
    let reply = completed(&response(vec![reasoning.clone(), call()])).unwrap();
    let mut c = chat();
    c.messages.extend(
        [
            super::super::assistant_message(&reply),
            json!({"role":"tool","tool_call_id":"call_write","content":"wrote marker"}),
        ]
        .iter()
        .map(persisted),
    );
    let body = request("account-model", &c, None).unwrap();
    assert_eq!(body["input"][2], reasoning);
    assert_eq!(body["input"][3], call());
    assert_eq!(
        body["input"][4],
        json!({"type":"function_call_output","call_id":"call_write","output":"wrote marker"})
    );
    assert_eq!(reply.tool_calls[0].name, "write");
    assert_eq!(reply.prompt_tokens, Some(50));
    assert_eq!(reply.cached_prompt_tokens, Some(20));
    assert_eq!(reply.reasoning_tokens, Some(3));
}
#[test]
fn images_convert_to_responses_input_without_sending_bookkeeping() {
    let mut c = chat();
    c.messages[1] = json!({"role":"user","content":[{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA==","detail":"low"}}],"dial_evidence":{"secret":"never sent"}});
    let body = request("account-model", &c, None).unwrap();
    assert_eq!(body["input"][1]["content"][1]["type"], "input_image");
    assert_eq!(body["input"][1]["content"][1]["detail"], "low");
    assert!(!body.to_string().contains("never sent"));
}
#[test]
fn empty_terminal_output_replays_finished_items_only_after_successful_completion() {
    let message = json!({"type":"message","id":"message_id","role":"assistant","status":"completed","content":[{"type":"output_text","text":"OK","annotations":[]}]});
    let events = vec![
        json!({"type":"response.output_item.added","output_index":0,"item":message}),
        json!({"type":"response.output_item.done","output_index":0,"item":message}),
        json!({"type":"response.output_item.added","output_index":1,"item":call()}),
        json!({"type":"response.output_item.done","output_index":1,"item":call()}),
    ];
    let mut accumulator = Accumulator::default();
    for event in &events {
        accumulator
            .feed(&event.to_string(), &mut |_| true, &mut |_| true)
            .unwrap();
    }
    assert!(
        accumulator.finish().is_err(),
        "done items alone cannot release tools"
    );
    let mut full = events.clone();
    full.push(json!({"type":"response.completed","response":response(vec![])}));
    let server = fake::serve(vec![Script::SseNoContentType(sse(full))]);
    let reply = exchange(
        Endpoint {
            headers: &[],
            url: &server.url,
            token: "fixture-token",
            account_id: None,
            model: "account-model",
        },
        &chat(),
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        &mut |_| true,
    )
    .unwrap();
    assert_eq!(reply.text, "OK");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.provider_content.len(), 2);
    server.requests();
    let mut failed = events;
    failed.push(json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_unavailable"}}}));
    let server = fake::serve(vec![Script::SseNoContentType(sse(failed))]);
    assert!(
        exchange(
            Endpoint {
                headers: &[],
                url: &server.url,
                token: "fixture-token",
                account_id: None,
                model: "account-model"
            },
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true
        )
        .is_err()
    );
    server.requests();
    let server = fake::serve(vec![Script::Sse(sse(vec![
        json!({"type":"response.output_item.added","output_index":0,"item":call()}),
        json!({"type":"response.completed","response":response(vec![])}),
    ]))]);
    assert!(
        exchange(
            Endpoint {
                headers: &[],
                url: &server.url,
                token: "fixture-token",
                account_id: None,
                model: "account-model"
            },
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true
        )
        .is_err()
    );
    server.requests();
}
#[test]
fn valid_responses_streams_can_omit_content_type_but_json_cannot_release_tools() {
    let payload = sse(vec![
        json!({"type":"response.completed","response":response(vec![call()])}),
    ]);
    let server = fake::serve(vec![Script::SseNoContentType(payload)]);
    let reply = exchange(
        Endpoint {
            headers: &[],
            url: &server.url,
            token: "fixture-token",
            account_id: None,
            model: "account-model",
        },
        &chat(),
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        &mut |_| true,
    )
    .unwrap();
    assert_eq!(reply.tool_calls.len(), 1);
    server.requests();
    let server = fake::serve(vec![Script::SseNoContentType(
        response(vec![call()]).to_string(),
    )]);
    assert!(
        exchange(
            Endpoint {
                headers: &[],
                url: &server.url,
                token: "fixture-token",
                account_id: None,
                model: "account-model"
            },
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true
        )
        .is_err()
    );
    server.requests();
}
#[test]
fn completed_stream_yields_tools_and_partial_stream_never_does() {
    let events = vec![
        json!({"type":"response.output_item.added","output_index":0,"item":call()}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"}),
    ];
    let mut accumulator = Accumulator::default();
    let mut seen = Vec::new();
    for event in &events {
        accumulator
            .feed(&event.to_string(), &mut |_| true, &mut |e| {
                seen.push(e.clone());
                true
            })
            .unwrap();
    }
    assert!(accumulator.finish().is_err());
    // The added item is lifecycle progress for the agent watchdog; its arguments follow.
    assert_eq!(seen[0], StreamEvent::Activity);
    assert!(matches!(&seen[1],StreamEvent::ToolArguments { name,.. } if name=="write"));
    let mut events = events;
    events.push(json!({"type":"response.completed","response":response(vec![call()])}));
    let server = fake::serve(vec![Script::Sse(sse(events))]);
    let reply = exchange(
        Endpoint {
            headers: &[],
            url: &server.url,
            token: "test-access",
            account_id: None,
            model: "account-model",
        },
        &chat(),
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        &mut |_| true,
    )
    .unwrap();
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].arguments["path"], "marker.txt");
    let requests = server.requests();
    assert_eq!(
        fake::header(&requests[0], "authorization").as_deref(),
        Some("Bearer test-access")
    );
    let body = fake::body(&requests[0]);
    assert_eq!(body["store"], false);
}
#[test]
fn late_usage_limit_failures_and_incomplete_responses_do_not_release_calls() {
    for terminal in [
        json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"secret prompt"}}}),
        json!({"type":"response.incomplete","response":{"status":"incomplete","output":[call()]}}),
    ] {
        let server = fake::serve(vec![Script::Sse(sse(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":call()}),
            terminal,
        ]))]);
        let err = exchange(
            Endpoint {
                headers: &[],
                url: &server.url,
                token: "test-access",
                account_id: None,
                model: "account-model",
            },
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true,
        )
        .unwrap_err();
        assert!(!err.to_string().contains("secret prompt"));
        server.requests();
    }
}
#[test]
fn failed_events_expose_only_safe_diagnostic_identifiers() {
    for (terminal, expected) in [
        (
            json!({"type":"response.failed","response":{"error":{"code":"model_not_found","message":"private prompt"}}}),
            "code: model_not_found",
        ),
        (
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"error":{"message":"private prompt"}}}),
            "reason: content_filter",
        ),
    ] {
        let mut accumulator = Accumulator::default();
        let err = accumulator
            .feed(&terminal.to_string(), &mut |_| true, &mut |_| true)
            .unwrap_err()
            .to_string();
        assert!(err.contains(expected), "{err}");
        assert!(!err.contains("private prompt"), "{err}");
    }
}

#[test]
fn nested_stream_errors_preserve_classification_without_releasing_calls() {
    use super::super::errors::{ErrorClass, classify};
    for (code, expected) in [
        ("server_error", ErrorClass::Transient),
        (
            "rate_limit_exceeded",
            ErrorClass::RateLimited { retry_after: None },
        ),
        ("invalid_api_key", ErrorClass::Auth),
        (
            "subscription_sharing_usage_limit_exceeded",
            ErrorClass::QuotaExhausted { reset_at: None },
        ),
        // Unrecognized provider codes are retried within the bounded budget, never surfaced raw.
        ("private prompt", ErrorClass::Transient),
    ] {
        let mut accumulator = Accumulator::default();
        accumulator
            .feed(
                &json!({"type":"response.output_item.added","output_index":0,"item":call()})
                    .to_string(),
                &mut |_| true,
                &mut |_| true,
            )
            .unwrap();
        let err = accumulator
            .feed(
                &json!({"type":"error","code":null,"error":{"code":code,"message":"private prompt"}}).to_string(),
                &mut |_| true,
                &mut |_| true,
            )
            .unwrap_err();
        assert_eq!(classify(&err), expected, "{err}");
        assert!(!err.to_string().contains("private prompt"));
        assert!(accumulator.finish().is_err());
    }
}

#[test]
fn unclassified_incomplete_responses_are_not_retried() {
    use super::super::errors::{ErrorClass, classify};
    let mut accumulator = Accumulator::default();
    let err = accumulator
        .feed(
            &json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"other"}}}).to_string(),
            &mut |_| true,
            &mut |_| true,
        )
        .unwrap_err();
    assert!(err.to_string().contains("unclassified_failure"), "{err}");
    assert_eq!(classify(&err), ErrorClass::Fatal);
}

#[test]
fn stream_callbacks_and_transport_are_cancellable() {
    let server = fake::serve(vec![Script::Sse(sse(vec![
        json!({"type":"response.output_text.delta","delta":"hello"}),
        json!({"type":"response.completed","response":response(vec![])}),
    ]))]);
    let mut callback = |_: &str| false;
    assert!(
        exchange(
            Endpoint {
                headers: &[],
                url: &server.url,
                token: "test-access",
                account_id: None,
                model: "account-model"
            },
            &chat(),
            5,
            &mut Some(&mut callback),
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true
        )
        .unwrap_err()
        .downcast_ref::<Interrupted>()
        .is_some()
    );
    server.requests();
    let server = fake::serve(vec![Script::Stall {
        silence: std::time::Duration::from_millis(300),
    }]);
    let interrupt = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupt);
    let start = std::time::Instant::now();
    let job = std::thread::spawn(move || {
        exchange(
            Endpoint {
                headers: &[],
                url: &server.url,
                token: "test-access",
                account_id: None,
                model: "account-model",
            },
            &chat(),
            5,
            &mut None,
            interrupt,
            None,
            &mut |_| true,
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    flag.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(job.join().unwrap().is_err());
    assert!(start.elapsed() < std::time::Duration::from_millis(250));
}
#[test]
fn malformed_arguments_reach_the_existing_repair_path_but_unfinished_calls_fail() {
    let mut invalid = call();
    invalid["arguments"] = json!("{broken");
    assert!(
        completed(&response(vec![invalid.clone()]))
            .unwrap()
            .tool_calls[0]
            .malformed_arguments
    );
    invalid["status"] = json!("in_progress");
    assert!(completed(&response(vec![invalid])).is_err());
    let mut foreign = call();
    foreign["namespace"] = json!("other-app");
    assert!(completed(&response(vec![foreign])).is_err());
    assert!(completed(&response(vec![call(), call()])).is_err());
}
#[test]
fn auth_errors_and_redirects_cannot_leak_tokens_to_a_new_origin() {
    let server = fake::serve(vec![
        fake::json(401, &json!({"detail":"test-access"})),
        Script::HttpHeaders {
            status: 307,
            headers: vec![("Location", "https://evil.example".into())],
            body: vec![],
        },
    ]);
    for _ in 0..2 {
        let err = exchange(
            Endpoint {
                headers: &[],
                url: &server.url,
                token: "test-access",
                account_id: None,
                model: "account-model",
            },
            &chat(),
            5,
            &mut None,
            Arc::new(AtomicBool::new(false)),
            None,
            &mut |_| true,
        )
        .unwrap_err();
        assert!(!err.to_string().contains("test-access"));
    }
    assert_eq!(server.requests().len(), 2);
}

fn cache_profile(
    prompt_cache: Option<bool>,
    cache_retention: super::super::profiles::CacheRetention,
    declared: bool,
) -> super::super::profiles::ModelProfile {
    use super::super::profiles::{ModelCapabilities, ModelProfile};
    ModelProfile {
        provider: "openai".into(),
        model: "gpt-5.6".into(),
        prompt_cache,
        cache_retention,
        capabilities: ModelCapabilities {
            prompt_cache: declared,
            ..Default::default()
        },
        ..Default::default()
    }
}
#[test]
fn explicit_prompt_cache_marks_latest_stable_user_turn_in_multi_turn_history() {
    use super::super::profiles::CacheRetention;
    let mut turns = chat();
    turns.messages = vec![
        json!({"role":"system","content":"system guidance"}),
        json!({"role":"user","content":"write marker"}),
        json!({"role":"assistant","content":"wrote marker.txt"}),
        json!({"role":"user","content":"now verify"}),
    ];
    let profile = cache_profile(Some(true), CacheRetention::Auto, true);
    let mut body = request_api("gpt-5.6", &turns, Some(&profile)).unwrap();
    apply_api_prompt_cache(
        &mut body,
        Some(&profile),
        "https://api.openai.com/v1/responses",
    );
    assert_eq!(
        body["prompt_cache_options"],
        json!({"mode":"explicit","ttl":"30m"})
    );
    assert_eq!(
        body["input"][1]["content"],
        json!([{
            "type": "input_text",
            "text": "write marker",
            "prompt_cache_breakpoint": {"mode": "explicit"}
        }])
    );
    assert_eq!(body["input"][2]["content"], "wrote marker.txt");
    assert_eq!(body["input"][3]["content"], "now verify");
    assert_eq!(
        body.to_string().matches("prompt_cache_breakpoint").count(),
        1
    );
    assert!(body.get("prompt_cache_retention").is_none());
}
#[test]
fn explicit_prompt_cache_wraps_instruction_when_it_is_the_only_stable_item() {
    use super::super::profiles::CacheRetention;
    let profile = cache_profile(Some(true), CacheRetention::Auto, true);
    let mut body = request_api("gpt-5.6", &chat(), Some(&profile)).unwrap();
    apply_api_prompt_cache(
        &mut body,
        Some(&profile),
        "https://api.openai.com/v1/responses",
    );
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(
        body["input"][0]["content"],
        json!([{
            "type": "input_text",
            "text": "system guidance",
            "prompt_cache_breakpoint": {"mode": "explicit"}
        }])
    );
    assert_eq!(body["input"][1]["content"], "write marker");
    assert_eq!(
        body.to_string().matches("prompt_cache_breakpoint").count(),
        1
    );
}

#[test]
fn codex_routes_send_prompt_cache_key_unless_profile_opts_out() {
    use super::super::profiles::CacheRetention;
    assert!(sends_prompt_cache_key(None, true));
    let undeclared = cache_profile(None, CacheRetention::Auto, false);
    assert!(sends_prompt_cache_key(Some(&undeclared), true));
    let opted_out = cache_profile(Some(false), CacheRetention::Auto, true);
    assert!(!sends_prompt_cache_key(Some(&opted_out), true));
    // API-key routes keep the declared-capability gate.
    assert!(!sends_prompt_cache_key(None, false));
    assert!(!sends_prompt_cache_key(Some(&undeclared), false));
    let declared = cache_profile(None, CacheRetention::Auto, true);
    assert!(sends_prompt_cache_key(Some(&declared), false));
}

#[test]
fn prompt_cache_false_adds_no_breakpoints_options_or_retention() {
    use super::super::profiles::CacheRetention;
    let profile = cache_profile(Some(false), CacheRetention::Auto, true);
    let mut body = request_api("gpt-5.6", &chat(), Some(&profile)).unwrap();
    body["prompt_cache_key"] = json!("key");
    apply_api_prompt_cache(
        &mut body,
        Some(&profile),
        "https://api.openai.com/v1/responses",
    );
    assert!(body.get("prompt_cache_options").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
    assert!(!body.to_string().contains("prompt_cache_breakpoint"));
    assert_eq!(body["prompt_cache_key"], "key");
}
#[test]
fn long_retention_sends_24h_only_for_keyed_official_openai() {
    use super::super::profiles::CacheRetention;
    let profile = cache_profile(None, CacheRetention::Long, true);
    let keyed = |url: &str| {
        let mut body = request_api("gpt-5.6", &chat(), Some(&profile)).unwrap();
        body["prompt_cache_key"] = json!("key");
        apply_api_prompt_cache(&mut body, Some(&profile), url);
        body
    };
    assert_eq!(
        keyed("https://api.openai.com/v1/responses")["prompt_cache_retention"],
        "24h"
    );
    assert!(
        keyed("https://proxy.example.com/v1/responses")
            .get("prompt_cache_retention")
            .is_none()
    );
    let mut unkeyed = request_api("gpt-5.6", &chat(), Some(&profile)).unwrap();
    apply_api_prompt_cache(
        &mut unkeyed,
        Some(&profile),
        "https://api.openai.com/v1/responses",
    );
    assert!(unkeyed.get("prompt_cache_retention").is_none());
    assert!(unkeyed.get("prompt_cache_options").is_none());
    let auto = cache_profile(None, CacheRetention::Auto, true);
    let mut auto_body = request_api("gpt-5.6", &chat(), Some(&auto)).unwrap();
    auto_body["prompt_cache_key"] = json!("key");
    apply_api_prompt_cache(
        &mut auto_body,
        Some(&auto),
        "https://api.openai.com/v1/responses",
    );
    assert!(auto_body.get("prompt_cache_retention").is_none());
}
#[test]
fn long_retention_is_rejected_on_subscription_and_without_declared_caching() {
    use super::super::profiles::CacheRetention;
    let declared = cache_profile(None, CacheRetention::Long, true);
    assert!(request_api("gpt-5.6", &chat(), Some(&declared)).is_ok());
    assert!(request("gpt-5.6", &chat(), Some(&declared)).is_err());
    let disabled = cache_profile(Some(false), CacheRetention::Long, true);
    assert!(request_api("gpt-5.6", &chat(), Some(&disabled)).is_err());
    let undeclared = cache_profile(None, CacheRetention::Long, false);
    assert!(request_api("gpt-5.6", &chat(), Some(&undeclared)).is_err());
}

fn persisted(message: &Value) -> Value {
    serde_json::from_str(&message.to_string()).unwrap()
}

#[test]
fn image_turn_releases_only_a_completed_image_after_terminal_success() {
    let item = json!({"type":"image_generation_call", "id":"img", "status":"completed", "result":"aW1hZ2U="});
    let response = response(vec![item.clone()]);
    assert!(
        completed(&response).is_err(),
        "ordinary chat must not accept unrequested image output"
    );
    let mut collector = Accumulator {
        image_generation: true,
        ..Default::default()
    };
    for event in [
        json!({"type":"response.output_item.added", "output_index":0, "item":{"type":"image_generation_call", "id":"img"}}),
        json!({"type":"response.output_item.done", "output_index":0, "item":item}),
    ] {
        assert!(
            !collector
                .feed(&event.to_string(), &mut |_| true, &mut |_| true)
                .unwrap()
        );
    }
    assert!(collector.result.is_none());
    assert!(collector.feed(&json!({"type":"response.completed", "response":{"status":"completed","output":[]}}).to_string(), &mut |_| true, &mut |_| true).unwrap());
    assert_eq!(
        collector.finish().unwrap().provider_content.last().unwrap()["item"]["result"],
        "aW1hZ2U="
    );
    for response in [
        json!({"status":"completed", "output":[]}),
        json!({"status":"incomplete", "output":[{"type":"image_generation_call","status":"completed","result":"abc"}]}),
        json!({"status":"completed", "output":[{"type":"image_generation_call","status":"in_progress","result":"abc"}]}),
    ] {
        assert!(completed_image(&response).is_err(), "{response}");
    }
}

#[test]
fn image_stream_accepts_large_inline_images_but_never_partial_or_failed_turns() {
    let encoded = "a".repeat(1024 * 1024 + 100);
    let image = json!({"type":"image_generation_call","status":"completed","result":encoded});
    let terminal = json!({"type":"response.completed","response":response(vec![image.clone()])});
    let server = fake::serve(vec![Script::Sse(sse(vec![terminal]))]);
    let output = generate_image(
        Endpoint {
            url: &server.url,
            token: "fixture-token",
            account_id: None,
            model: "selected",
            headers: &[],
        },
        &json!({"model":"selected","stream":true}),
        5,
    )
    .unwrap();
    assert_eq!(output, encoded);
    server.requests();
    for terminal in [
        json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[image.clone()]}}),
        json!({"type":"response.failed","response":{"status":"failed","error":{"message":"failed"}}}),
        json!({"type":"response.completed","response":response(vec![image.clone(),image.clone()])}),
    ] {
        let server = fake::serve(vec![Script::Sse(sse(vec![terminal]))]);
        assert!(
            generate_image(
                Endpoint {
                    url: &server.url,
                    token: "fixture-token",
                    account_id: None,
                    model: "selected",
                    headers: &[]
                },
                &json!({"model":"selected","stream":true}),
                5
            )
            .is_err()
        );
        server.requests();
    }
}

#[test]
fn done_only_call_signals_completion_before_response_and_honors_cancellation() {
    for cancel in [false, true] {
        let mut acc = Accumulator::default();
        acc.feed(
            &json!({"type":"response.output_item.added","output_index":0,"item":call()})
                .to_string(),
            &mut |_| true,
            &mut |_| true,
        )
        .unwrap();
        let mut seen = Vec::new();
        let result = acc.feed(
            &json!({"type":"response.output_item.done","output_index":0,"item":call()}).to_string(),
            &mut |_| true,
            &mut |e| {
                seen.push(e.clone());
                !cancel
            },
        );
        assert_eq!(result.is_err(), cancel);
        assert!(
            matches!(&seen[0], StreamEvent::ToolCallComplete { index:0, name, arguments, .. }
            if name == "write" && arguments == call()["arguments"].as_str().unwrap())
        );
        assert!(acc.finish().is_err());
    }
}
