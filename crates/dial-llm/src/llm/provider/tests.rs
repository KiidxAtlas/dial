use crate::config;

#[test]
fn steering_bypass_announces_changes_and_retains_status() {
    let mut backend = super::Backend::new(
        super::ProviderKind::Local {
            model_dir: "test".into(),
        },
        "python".into(),
        10,
    );
    assert!(
        super::Backend::steering_bypass(
            &mut backend.steering_runtime,
            "missing coding vector".into()
        )
        .is_some()
    );
    assert!(
        super::Backend::steering_bypass(
            &mut backend.steering_runtime,
            "missing coding vector".into()
        )
        .is_none()
    );
    assert_eq!(
        backend.steering_runtime()["reason"],
        "missing coding vector"
    );
    assert!(
        super::Backend::steering_bypass(&mut backend.steering_runtime, "invalid evaluation".into())
            .is_some()
    );
    backend.steering_runtime = serde_json::json!({"state":"used","uses":1});
    assert!(
        super::Backend::steering_bypass(&mut backend.steering_runtime, "invalid evaluation".into())
            .is_some()
    );
}
#[cfg(unix)]
#[test]
fn lora_runtime_distinguishes_loading_use_failure_and_release() {
    let dir = tempfile::tempdir().unwrap();
    let python = super::server::fixtures::fake_python(dir.path());
    std::thread::scope(|scope| {
        let (python, adapter_path) = (&python, dir.path().join("adapter"));
        let broken_check = scope.spawn(move || {
            let mut broken = Backend::new(
                ProviderKind::Local {
                    model_dir: "bad-model".into(),
                },
                python.clone(),
                3,
            );
            assert!(
                broken
                    .chat(Some(&adapter_path), &request(), 5, None)
                    .is_err()
            );
            assert_eq!(
                broken.adapter_monitor().lock().phase,
                super::AdapterPhase::Failed
            );
        });
        let marker = dir.path().join("server.json");
        let adapter = dir.path().join("adapter");
        let mut local = Backend::new(
            ProviderKind::Local {
                model_dir: marker.to_string_lossy().into_owned(),
            },
            python.clone(),
            10,
        );
        let monitor = local.adapter_monitor();
        assert_eq!(monitor.lock().phase, super::AdapterPhase::Idle);
        let mut observed_loaded = false;
        let mut text = |_: &str| {
            observed_loaded = monitor.lock().phase == super::AdapterPhase::Loaded;
            true
        };
        local
            .chat(Some(&adapter), &request(), 5, Some(&mut text))
            .unwrap();
        assert!(
            observed_loaded,
            "loading alone must not claim a completed response"
        );
        assert_eq!(monitor.lock().phase, super::AdapterPhase::Used);
        assert_eq!(monitor.lock().adapter.as_deref(), Some(adapter.as_path()));
        let argv: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        assert!(argv["argv"].as_array().unwrap().iter().any(|arg| {
            arg.as_str()
                .is_some_and(|arg| arg == format!("--adapter-path={}", adapter.display()))
        }));
        assert!(
            local
                .chat(Some(&adapter), &request(), 5, Some(&mut |_| false))
                .is_err()
        );
        assert_eq!(monitor.lock().phase, super::AdapterPhase::Failed);
        local.release();
        assert_eq!(monitor.lock().phase, super::AdapterPhase::Idle);
        assert!(monitor.lock().adapter.is_none());
        broken_check.join().unwrap();
    });
}
use super::*;
use crate::llm::cancel::Patience;
use crate::llm::fake::{self, Script, header, request_line};
use serde_json::json;

fn endpoint(base_url: &str) -> ProviderKind {
    ProviderKind::Endpoint {
        base_url: base_url.into(),
        model: "gpt-x".into(),
        api_key: Some("sk-secret".into()),
    }
}

fn backend(kind: ProviderKind) -> Backend {
    let mut backend = Backend::new(kind, PathBuf::from("/nonexistent/python"), 5);
    backend.retry_backoff = Duration::from_millis(1);
    backend
}

fn anthropic_endpoint(base_url: &str) -> ProviderKind {
    ProviderKind::AnthropicEndpoint {
        base_url: base_url.into(),
        model: "claude-test".into(),
        api_key: Some("anth-secret".into()),
    }
}

fn request() -> Chat {
    Chat {
        messages: vec![json!({"role": "user", "content": "SECRET-PROMPT"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 16,
        seed: None,
    }
}

#[test]
fn non_chat_model_kinds_are_rejected_before_any_request() {
    use llm::catalog::ModelKind;
    for kind in [
        ModelKind::Embedding,
        ModelKind::Rerank,
        ModelKind::Image,
        ModelKind::Audio,
    ] {
        let mut remote = backend(endpoint("https://must-not-connect.invalid")).with_metadata(
            llm::catalog::ModelMetadata {
                kind,
                ..Default::default()
            },
        );
        let error = remote
            .chat(None, &request(), 1, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a chat model"), "{error}");
        assert!(!error.contains("SECRET-PROMPT"));
    }
}

fn ok(text: &str) -> Script {
    fake::json(
        200,
        &json!({"choices": [{"finish_reason": "stop", "message": {"content": text}}]}),
    )
}

/// A loopback llama-server is recognised by its `/props` once and the answer is reused; hosted
/// hosts are never probed; a server without the native API declares no mechanism.
#[test]
fn endpoint_structured_output_is_detected_on_local_llama_servers_only() {
    let props = fake::json(
        200,
        &json!({"default_generation_settings": {"n_ctx": 4096}}),
    );
    let llama = fake::serve(vec![props]);
    let remote = backend(endpoint(&format!("{}/v1", llama.url)));
    assert_eq!(
        remote.endpoint_structured_output(),
        Some(llm::profiles::StructuredOutput::JsonSchema)
    );
    // Only one connection was scripted: a second probe would fail, so this proves the cache.
    assert_eq!(
        remote.endpoint_structured_output(),
        Some(llm::profiles::StructuredOutput::JsonSchema)
    );
    let other = fake::serve(vec![fake::json(404, &json!({}))]);
    assert_eq!(
        backend(endpoint(&format!("{}/v1", other.url))).endpoint_structured_output(),
        None
    );
    let hosted = backend(endpoint("https://api.example.invalid/v1"));
    assert_eq!(hosted.endpoint_structured_output(), None);
}

/// Local hints reach a detected local llama-server only: the request carries them, a hosted
/// route is never probed or hinted, and a loop retry needs a penalty configured on a local route.
#[test]
fn local_hints_and_loop_penalty_retries_stay_on_local_routes() {
    use llm::local_hints::{LocalHints, LocalServer};
    let props = fake::json(
        200,
        &json!({"default_generation_settings": {"n_ctx": 4096}}),
    );
    // One connection for the /props probe, then one per chat (the retry is the third).
    let llama = fake::serve(vec![props, ok("{}"), ok("{}")]);
    let mut local = backend(endpoint(&format!("{}/v1", llama.url)));
    local.set_local_hints(LocalHints {
        cache_slot: Some(1),
        prompt_progress: true,
        ..Default::default()
    });
    local.set_loop_penalty(Some(LocalHints {
        thinking: Some(false),
        presence_penalty: Some(0.6),
        repetition_penalty: Some(1.3),
        ..Default::default()
    }));
    assert_eq!(local.local_server(), Some(LocalServer::LlamaCpp));
    local.chat(None, &request(), 5, None).unwrap();
    assert!(local.retry_with_loop_penalty());
    local.chat(None, &request(), 5, None).unwrap();
    assert_eq!(local.take_loop_penalty_retries(), 1);
    assert_eq!(local.take_loop_penalty_retries(), 0);
    let requests = llama.requests();
    let first = fake::body(&requests[1]);
    assert_eq!(first["cache_prompt"], true);
    assert_eq!(first["id_slot"], 1);
    assert_eq!(first["return_progress"], true);
    assert!(first.get("presence_penalty").is_none());
    assert!(first.get("chat_template_kwargs").is_none());
    let retry = fake::body(&requests[2]);
    assert_eq!(retry["presence_penalty"], 0.6);
    assert_eq!(retry["repeat_penalty"], 1.3);
    assert_eq!(retry["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(retry["id_slot"], 1);

    let mut hosted = backend(endpoint("https://api.example.invalid/v1"));
    hosted.set_local_hints(LocalHints {
        cache_slot: Some(1),
        ..Default::default()
    });
    hosted.set_loop_penalty(Some(LocalHints {
        presence_penalty: Some(0.6),
        ..Default::default()
    }));
    assert_eq!(hosted.local_server(), None);
    assert!(!hosted.retry_with_loop_penalty());
    assert_eq!(hosted.take_loop_penalty_retries(), 0);
}

/// The repair retry on a detected llama-server carries `json_schema`; without `/props` the same
/// retry is unchanged (no `response_format`, tools kept).
#[test]
fn repair_retry_carries_json_schema_only_on_a_detected_llama_server() {
    let read = json!({"type": "function", "function": {"name": "read_file", "parameters": {
        "type": "object", "properties": {"path": {"type": "string"}}}}});
    let mut req = request();
    req.tools = vec![read];
    let mut repair = llm::profiles::ModelProfile::minimal("openai-compatible", "gpt-x");
    repair.tool_repair = true;

    let llama = fake::serve(vec![
        fake::json(
            200,
            &json!({"default_generation_settings": {"n_ctx": 4096}}),
        ),
        ok("{}"),
    ]);
    let mut remote = backend(endpoint(&format!("{}/v1", llama.url))).with_profile(repair.clone());
    remote.chat(None, &req, 5, None).unwrap();
    let body = fake::body(&llama.requests()[1]);
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert!(body.get("tools").is_none());

    let plain = fake::serve(vec![fake::json(404, &json!({})), ok("{}")]);
    let mut remote = backend(endpoint(&format!("{}/v1", plain.url))).with_profile(repair);
    remote.chat(None, &req, 5, None).unwrap();
    let body = fake::body(&plain.requests()[1]);
    assert!(body.get("response_format").is_none(), "{body}");
    assert!(body.get("tools").is_some());
}

/// A first attempt on a detected llama-server sends `tools` with no `response_format`, `grammar`,
/// `json_schema` or forced `tool_choice`: llama.cpp then applies its lazy tool-call grammar and
/// leaves plain answers free. A template whose `/props` caps lack native tools gets one notice per
/// conversation; a template with them gets none.
#[test]
fn first_attempt_on_llama_server_keeps_answers_free_and_flags_tool_less_templates() {
    use llm::local_hints::LocalHints;
    let read = json!({"type": "function", "function": {"name": "read_file", "parameters": {
        "type": "object", "properties": {"path": {"type": "string"}}}}});
    let mut req = request();
    req.tools = vec![read];
    let props = |native: bool| {
        fake::json(
            200,
            &json!({"default_generation_settings": {"n_ctx": 4096},
                "chat_template_caps": {"supports_tools": native, "supports_tool_calls": native}}),
        )
    };
    let run = |native: bool| {
        // One connection for the /props probe (made by local detection), then one per chat.
        let llama = fake::serve(vec![props(native), ok("{}"), ok("{}")]);
        let mut local = backend(endpoint(&format!("{}/v1", llama.url))).with_session_id("one");
        local.set_local_hints(LocalHints {
            prompt_progress: true,
            ..Default::default()
        });
        let mut notices = Vec::new();
        for _ in 0..2 {
            local
                .chat_with_control(
                    None,
                    &req,
                    5,
                    None,
                    &mut |n| notices.push(n.to_owned()),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
        }
        let body = fake::body(&llama.requests()[1]);
        assert!(body["tools"].as_array().is_some_and(|t| t.len() == 1));
        for field in ["response_format", "grammar", "json_schema", "tool_choice"] {
            assert!(body.get(field).is_none(), "{field}: {body}");
        }
        notices
    };
    let notices = run(false);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("--chat-template-file"), "{notices:?}");
    assert!(run(true).is_empty());
}

/// llama-server without Jinja refuses tools with HTTP 500; Dial stops at once with the flag to add
/// instead of retrying the same request.
#[test]
fn llama_server_without_jinja_fails_once_with_the_flag_to_add() {
    let mut req = request();
    req.tools = vec![json!({"type": "function", "function": {"name": "read_file",
        "parameters": {"type": "object", "properties": {}}}})];
    let server = fake::serve(vec![fake::json(
        500,
        &json!({"error": {"code": 500, "message": "tools param requires --jinja flag",
            "type": "server_error"}}),
    )]);
    let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
    let error = remote.chat(None, &req, 5, None).unwrap_err();
    assert_eq!(
        llm::errors::classify(&error),
        llm::errors::ErrorClass::Fatal
    );
    assert!(
        format!("{error:#}").contains("restart it with --jinja"),
        "{error:#}"
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn url_normalization() {
    for (base, want) in [
        (
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "https://api.openai.com/v1/",
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "https://openrouter.ai/api/v1",
            "https://openrouter.ai/api/v1/chat/completions",
        ),
        (
            "http://localhost:11434",
            "http://localhost:11434/v1/chat/completions",
        ),
        (
            "http://127.0.0.1:8000/",
            "http://127.0.0.1:8000/v1/chat/completions",
        ),
        (
            "https://example.com",
            "https://example.com/v1/chat/completions",
        ),
        (
            "https://g.example/v1beta/openai",
            "https://g.example/v1beta/openai/chat/completions",
        ),
        (
            "http://[::1]:9000/v1",
            "http://[::1]:9000/v1/chat/completions",
        ),
    ] {
        assert_eq!(endpoint_url(base, false).unwrap(), want, "{base}");
    }
}

#[test]
fn url_validation() {
    for bad in [
        "ftp://example.com/v1",
        "example.com/v1",
        "file:///etc/passwd",
        "https://user:pw@example.com/v1",
        "https://example.com/v1?key=abc",
        "",
        "https://",
    ] {
        let error = endpoint_url(bad, false).unwrap_err().to_string();
        assert!(!error.contains("pw@") && !error.contains("abc"), "{error}");
        assert!(
            endpoint_url(bad, true).is_err(),
            "{bad} must be rejected even when insecure is allowed"
        );
    }
}

#[test]
fn plain_http_only_for_local_hosts_unless_flagged() {
    for local in [
        "http://localhost:1/v1",
        "http://127.0.0.1:1",
        "http://192.168.1.20:8000/v1",
        "http://10.0.0.5/v1",
        "http://172.16.3.4/v1",
        "http://[::1]:1",
        "http://studio.local:1234/v1",
    ] {
        assert!(endpoint_url(local, false).is_ok(), "{local}");
    }
    for remote in [
        "http://api.example.com/v1",
        "http://8.8.8.8/v1",
        "http://172.32.0.1/v1",
    ] {
        let error = endpoint_url(remote, false).unwrap_err().to_string();
        assert!(error.contains("plain http"), "{error}");
        assert!(endpoint_url(remote, true).is_ok(), "{remote}");
    }
    assert!(endpoint_url("https://api.example.com/v1", false).is_ok());
}

#[test]
fn validate_checks_model_and_key() {
    assert!(validate(&endpoint("https://e.com/v1"), false).is_ok());
    let blank = ProviderKind::Endpoint {
        base_url: "https://e.com".into(),
        model: " ".into(),
        api_key: None,
    };
    assert!(validate(&blank, false).is_err());
    let bad_key = ProviderKind::Endpoint {
        base_url: "https://e.com".into(),
        model: "m".into(),
        api_key: Some("a b".into()),
    };
    let error = validate(&bad_key, false).unwrap_err().to_string();
    assert!(!error.contains("a b"));
    assert!(
        validate(
            &ProviderKind::Local {
                model_dir: String::new()
            },
            false
        )
        .is_err()
    );
}

#[test]
fn anthropic_url_uses_messages_path_and_endpoint_safety() {
    assert_eq!(
        anthropic_url("https://api.anthropic.com/v1", false).unwrap(),
        "https://api.anthropic.com/v1/messages"
    );
    assert_eq!(
        anthropic_url("https://api.anthropic.com", false).unwrap(),
        "https://api.anthropic.com/v1/messages"
    );
    assert!(anthropic_url("https://user:secret@example.com/v1", false).is_err());
    assert!(anthropic_url("http://api.example.com/v1", false).is_err());
}

#[test]
fn anthropic_request_maps_system_tools_and_tool_results() {
    let request = Chat {
        messages: vec![
            json!({"role":"system","content":"Follow rules"}),
            json!({"role":"user","content":"Read a file"}),
            json!({"role":"assistant","content":null,"tool_calls":[{
                "id":"call-1","type":"function",
                "function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}
            }]}),
            json!({"role":"tool","tool_call_id":"call-1","content":"file contents"}),
        ],
        tools: vec![json!({"type":"function","function":{
            "name":"read_file","description":"Read a file",
            "parameters":{"type":"object","properties":{"path":{"type":"string"}}}
        }})],
        temperature: 0.0,
        max_tokens: 128,
        seed: None,
    };
    let body = anthropic_request("claude-test", &request).unwrap();
    assert_eq!(body["system"], "Follow rules");
    assert_eq!(body["max_tokens"], 128);
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(body["messages"][1]["content"][0]["input"]["path"], "a.txt");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call-1");
}

#[test]
fn anthropic_request_returns_a_parallel_batch_in_one_user_message() {
    let call =
        |id: &str| json!({"id":id,"type":"function","function":{"name":"read","arguments":"{}"}});
    let result = |id: &str| json!({"role":"tool","tool_call_id":id,"content":id});
    let request = Chat {
        messages: vec![
            json!({"role":"user","content":"Read both"}),
            json!({"role":"assistant","content":null,"tool_calls":[call("a"), call("b")]}),
            result("a"),
            result("b"),
            json!({"role":"user","content":"steer"}),
            json!({"role":"assistant","content":null,"tool_calls":[call("c")]}),
            result("c"),
        ],
        tools: Vec::new(),
        temperature: 0.0,
        max_tokens: 128,
        seed: None,
    };
    let body = anthropic_request("claude-test", &request).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let ids = |m: &serde_json::Value| -> Vec<String> {
        m["content"].as_array().map_or_else(Vec::new, |blocks| {
            blocks
                .iter()
                .filter_map(|b| b["tool_use_id"].as_str().map(str::to_owned))
                .collect()
        })
    };
    assert_eq!(messages.len(), 6);
    assert_eq!(ids(&messages[2]), ["a", "b"]);
    assert_eq!(messages[3]["content"], "steer");
    assert_eq!(ids(&messages[5]), ["c"]);
}

#[test]
fn provider_switch_replays_responses_history_as_claude_text_and_tools() {
    let items = json!([
        {"type":"reasoning","id":"r1","encrypted_content":"responses-only","summary":[]},
        {"type":"message","id":"m1","role":"assistant","content":[{"type":"output_text","text":"Reading now."}]},
        {"type":"function_call","id":"f1","call_id":"call-1","namespace":"dial","name":"read","arguments":"{\"path\":\"a.rs\"}"}
    ]);
    let server = fake::serve(vec![Script::Sse(format!(
        "data: {}\n\n",
        json!({"type":"response.completed","response":{"status":"completed","output":items}})
    ))]);
    let reply = llm::responses::exchange(
        llm::responses::Endpoint {
            headers: &[],
            url: &server.url,
            token: "fixture-token",
            account_id: None,
            model: "fixture-model",
        },
        &request(),
        5,
        &mut None,
        Arc::new(AtomicBool::new(false)),
        None,
        &mut |_| true,
    )
    .unwrap();
    server.requests();
    // Old session files have the untagged native wrapper too; test a saved-history roundtrip.
    let saved = serde_json::to_string(&llm::assistant_message(&reply)).unwrap();
    let restored: serde_json::Value = serde_json::from_str(&saved).unwrap();
    let claude_blocks = json!([
        {"type":"thinking","thinking":"checked","signature":"claude-signed"},
        {"type":"redacted_thinking","data":"claude-redacted"},
        {"type":"text","text":"File inspected."}
    ]);
    let claude_reply =
        anthropic_reply(&json!({"content":claude_blocks,"stop_reason":"end_turn"})).unwrap();
    let mut req = request();
    req.messages.extend([
        restored,
        llm::tool_message("call-1", "contents"),
        llm::assistant_message(&claude_reply),
    ]);
    for oauth in [false, true] {
        let body = anthropic_request_variant("claude-test", &req, oauth).unwrap();
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({"type":"text","text":"Reading now."})
        );
        assert_eq!(
            body["messages"][1]["content"][1],
            json!({"type":"tool_use","id":"call-1","name":if oauth {"_read"} else {"read"},"input":{"path":"a.rs"}})
        );
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call-1");
        assert_eq!(body["messages"][3]["content"], claude_blocks);
        assert!(!body.to_string().contains("dial_response_item"));
        assert!(!body.to_string().contains("responses-only"));
    }
    // Switching back still preserves Responses IDs/encrypted reasoning, without leaking
    // Claude's signed thinking into the Responses protocol or changing saved history.
    let body = llm::responses::request("fixture-model", &req, None).unwrap();
    assert_eq!(
        &body["input"].as_array().unwrap()[1..4],
        items.as_array().unwrap()
    );
    assert_eq!(body["input"][4]["call_id"], "call-1");
    assert_eq!(body["input"][5]["content"], "File inspected.");
    assert!(!body.to_string().contains("claude-signed"));
}
#[test]
fn claude_subscription_request_matches_claude_code_message_conventions() {
    let request = Chat {
        messages: vec![
            json!({"role":"system","content":"Follow rules"}),
            json!({"role":"user","content":"012345678901234567890"}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{}"}}]}),
        ],
        tools: vec![
            json!({"type":"function","function":{"name":"read_file","parameters":{"type":"object","properties":{}}}}),
        ],
        temperature: 0.0,
        max_tokens: 128,
        seed: None,
    };
    let mut body = anthropic_request_variant("claude-test", &request, true).unwrap();
    assert_eq!(
        body["system"][0]["text"].as_str().unwrap(),
        claude_billing_header("012345678901234567890")
    );
    assert!(
        body["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header: cc_version=2.1.280.")
    );
    assert_eq!(
        body["system"][1]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_eq!(body["system"][2]["text"], "Follow rules");
    assert_eq!(body["tools"][0]["name"], "_read_file");
    assert_eq!(body["messages"][1]["content"][0]["name"], "_read_file");
    body["stream"] = serde_json::json!(true);
    attest_claude_code_body(&mut body).unwrap();
    let encoded = serde_json::to_vec(&body).unwrap();
    let text = std::str::from_utf8(&encoded).unwrap();
    let cch = text
        .split("cch=")
        .nth(1)
        .unwrap()
        .split(';')
        .next()
        .unwrap();
    assert_eq!(cch.len(), 5);
    let mut unattested = encoded.clone();
    let cch_start = unattested
        .windows(4)
        .position(|part| part == b"cch=")
        .unwrap()
        + 4;
    unattested[cch_start..cch_start + 5].copy_from_slice(b"00000");
    assert_eq!(
        cch,
        format!("{:05x}", xxhash64(&unattested, CCH_SEED) & 0xfffff)
    );
}

#[test]
fn claude_subscription_exchange_reaches_transport_after_finalizing_body() {
    use llm::profiles::*;
    let profile = ModelProfile {
        provider: "anthropic".into(),
        model: "claude-test".into(),
        capabilities: ModelCapabilities {
            temperature: true,
            prompt_cache: true,
            ..Default::default()
        },
        temperature: Some(0.3),
        prompt_cache: Some(true),
        ..Default::default()
    };
    for streaming in [false, true] {
        let mut accept_text = |_: &str| true;
        let mut on_text: Option<&mut dyn FnMut(&str) -> bool> =
            streaming.then_some(&mut accept_text);
        // Exercise subscription preparation in the real exchange path, then cancel
        // at the transport boundary before DNS or any credential is sent.
        let error = anthropic_exchange(
            llm::Endpoint {
                headers: &[],
                url: "https://api.anthropic.com/v1/messages",
                auth: Some("sk-ant-oat01-test"),
                model: "claude-test",
            },
            &request(),
            5,
            &mut on_text,
            Arc::new(AtomicBool::new(true)),
            Some(&profile),
            &mut |_| true,
        )
        .unwrap_err();
        assert!(
            error.downcast_ref::<llm::TransportError>().is_some()
                && error.to_string().contains("interrupted"),
            "{error:#}"
        );
    }
}

#[test]
fn claude_fingerprint_hash_matches_xxhash64_reference_vector() {
    assert_eq!(
        xxhash64(b"The quick brown fox jumps over the lazy dog", 0),
        0x0b242d361fda71bc
    );
    assert_eq!(xxhash64(b"", 0), 0xef46db3751d8e999);
    assert_ne!(
        claude_billing_header("0123A5678901234567890"),
        claude_billing_header("012345678901234567890")
    );
}

#[test]
fn anthropic_reply_preserves_text_and_tool_calls() {
    let reply = anthropic_reply(&json!({
        "content":[
            {"type":"text","text":"Reading"},
            {"type":"tool_use","id":"tool-9","name":"read_file","input":{"path":"a.txt"}}
        ],
        "stop_reason":"tool_use",
        "usage":{"input_tokens":12,"output_tokens":4}
    }))
    .unwrap();
    assert_eq!(reply.text, "Reading");
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(reply.prompt_tokens, Some(12));
    assert_eq!(reply.completion_tokens, Some(4));
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].id, "tool-9");
    assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.txt"}));
}

#[test]
fn anthropic_chat_uses_key_header_and_messages_wire_format() {
    let server = fake::serve(vec![fake::json(
        200,
        &json!({
            "content":[{"type":"text","text":"hello"}],
            "stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}
        }),
    )]);
    let mut remote = backend(anthropic_endpoint(&format!("{}/v1", server.url)));
    let reply = remote.chat(None, &request(), 5, None).unwrap();
    assert_eq!(reply.text, "hello");
    let seen = server.requests().remove(0);
    assert_eq!(request_line(&seen), "POST /v1/messages HTTP/1.1");
    assert_eq!(header(&seen, "x-api-key").as_deref(), Some("anth-secret"));
    assert_eq!(
        header(&seen, "anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    let body = fake::body(&seen);
    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["max_tokens"], 16);
    assert_eq!(body["messages"][0]["content"][0]["text"], "SECRET-PROMPT");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
}

#[test]
fn anthropic_tool_free_compaction_wire_never_contains_null_tools() {
    for streaming in [false, true] {
        for cache in [false, true] {
            let server = fake::serve(vec![fake::json(
                200,
                &json!({
                    "content":[{"type":"text","text":"summary"}], "stop_reason":"end_turn"
                }),
            )]);
            let mut remote = backend(anthropic_endpoint(&format!("{}/v1", server.url)));
            remote.profile = Some(llm::profiles::ModelProfile {
                prompt_cache: Some(cache),
                ..llm::profiles::ModelProfile::minimal("anthropic", "claude-test")
            });
            remote.profile.as_mut().unwrap().capabilities.prompt_cache = true;
            let mut request = request();
            request.messages = vec![
                json!({"role":"system","content":"Summarize the archived conversation."}),
                json!({"role":"user","content":"<conversation>history</conversation>"}),
            ];
            let mut visible = String::new();
            let mut callback = |text: &str| {
                visible.push_str(text);
                true
            };
            let reply = remote
                .chat(None, &request, 5, streaming.then_some(&mut callback))
                .unwrap();
            assert_eq!(reply.text, "summary");
            let sent = fake::body(&server.requests()[0]);
            assert!(
                sent.get("tools").is_none_or(serde_json::Value::is_array),
                "invalid tools on final wire: {sent}"
            );
            assert_eq!(sent["stream"], streaming);
        }
    }
}

#[test]
fn anthropic_auth_errors_do_not_expose_key_or_prompt() {
    let server = fake::serve(vec![fake::json(
        401,
        &json!({"error":"invalid anth-secret SECRET-PROMPT"}),
    )]);
    let mut remote = backend(anthropic_endpoint(&format!("{}/v1", server.url)));
    let error = remote
        .chat(None, &request(), 5, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("HTTP 401"), "{error}");
    assert!(
        !error.contains("anth-secret") && !error.contains("SECRET-PROMPT"),
        "{error}"
    );
}

#[test]
fn anthropic_callback_receives_buffered_text_once() {
    let server = fake::serve(vec![fake::json(
        200,
        &json!({
            "content":[{"type":"text","text":"one"},{"type":"text","text":" answer"}],
            "stop_reason":"end_turn"
        }),
    )]);
    let mut remote = backend(anthropic_endpoint(&format!("{}/v1", server.url)));
    let mut pieces = Vec::new();
    let mut callback = |text: &str| {
        pieces.push(text.to_owned());
        true
    };
    let reply = remote
        .chat(None, &request(), 5, Some(&mut callback))
        .unwrap();
    assert_eq!(reply.text, "one answer");
    assert_eq!(pieces, ["one answer"]);
}

#[test]
fn describe_and_debug_never_show_the_key_or_userinfo() {
    let kind = ProviderKind::Endpoint {
        base_url: "https://user:hunter2@example.com/v1?token=abc".into(),
        model: "m".into(),
        api_key: Some("sk-secret".into()),
    };
    let shown = format!("{kind:?} {}", backend(kind.clone()).describe());
    for secret in ["sk-secret", "hunter2", "token=abc"] {
        assert!(!shown.contains(secret), "{shown}");
    }
    assert!(shown.contains("example.com"));

    let anthropic = ProviderKind::AnthropicEndpoint {
        base_url: "https://api.anthropic.com/v1".into(),
        model: "claude-test".into(),
        api_key: Some("anth-secret".into()),
    };
    assert!(!format!("{anthropic:?}").contains("anth-secret"));
}

#[test]
fn capabilities_and_labels() {
    let local = backend(ProviderKind::Local {
        model_dir: "/models/SmolLM2".into(),
    });
    assert!(local.supports_adapters());
    assert_eq!(local.model_label(), SERVER_MODEL);
    assert!(local.describe().contains("SmolLM2"));
    let remote = backend(endpoint("https://e.com/v1"));
    assert!(!remote.supports_adapters());
    assert_eq!(remote.model_label(), "gpt-x");
}

#[test]
fn endpoint_rejects_adapters_and_bad_urls_without_network() {
    let mut remote = backend(endpoint("https://e.com/v1"));
    let error = remote
        .chat(Some(Path::new("/adapter")), &request(), 5, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("adapter"), "{error}");
    let mut insecure = backend(endpoint("http://api.example.com/v1"));
    let error = insecure
        .chat(None, &request(), 5, None)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("plain http") && !error.contains("sk-secret"),
        "{error}"
    );
}

#[test]
fn endpoint_chat_sends_key_model_and_path() {
    let server = fake::serve(vec![ok("pong")]);
    let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
    let reply = remote.chat(None, &request(), 5, None).unwrap();
    assert_eq!(reply.text, "pong");
    let seen = server.requests().remove(0);
    assert_eq!(request_line(&seen), "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(
        header(&seen, "authorization").as_deref(),
        Some("Bearer sk-secret")
    );
    assert_eq!(fake::body(&seen)["model"], "gpt-x");
}

/// Writes a synthetic, unexpired OAuth grant for `id` in the store's on-disk layout.
fn write_grant(user_dir: &Path, id: &str, extra: serde_json::Value) {
    let grant = json!({
        "access": "tok",
        "refresh": "",
        "expires_at_ms": 4_102_444_800_000u64,
        "extra": extra,
    });
    crate::config::settings_store::write_private(
        &user_dir.join(format!("oauth-{id}.json")),
        grant.to_string().as_bytes(),
    )
    .unwrap();
}

#[test]
fn oauth_grant_is_never_attached_to_a_non_canonical_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    std::fs::create_dir_all(&user).unwrap();
    write_grant(&user, "kimi-code", json!({}));
    let binding = llm::oauth::Binding::for_provider(&user, "kimi-code").unwrap();
    // The configured route is a different server than the grant's canonical origin. The refusal
    // happens before dispatch, so this server is never sent a request or the bearer token.
    let server = fake::serve(vec![ok("pong")]);
    let mut remote = backend(endpoint(&format!("{}/v1", server.url))).with_oauth(Some(binding));
    let error = remote
        .chat(None, &request(), 5, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("bound to its own endpoint"), "{error}");
    assert!(!error.contains("tok"), "{error}");
}

#[test]
fn invalid_grant_identifiers_fail_before_any_request_and_not_as_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    std::fs::create_dir_all(&user).unwrap();
    write_grant(
        &user,
        "cloudflare-ai-gateway",
        json!({"accountId": "bad/account", "gatewayId": "gw"}),
    );
    let binding = llm::oauth::Binding::for_provider(&user, "cloudflare-ai-gateway").unwrap();
    let mut remote = backend(ProviderKind::AnthropicEndpoint {
        base_url: "https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/anthropic".into(),
        model: "claude-x".into(),
        api_key: None,
    })
    .with_oauth(Some(binding));
    let error = remote
        .chat(None, &request(), 5, None)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("Cloudflare AI Gateway account or gateway ID is invalid"),
        "{error}"
    );
    assert!(!error.contains("placeholder"), "{error}");
}

#[test]
fn endpoint_key_is_read_at_each_dispatch_not_at_construction() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    std::fs::create_dir_all(&user).unwrap();
    let base = "https://key-rotation.example/v1";
    llm::catalog::save_credential(&user, base, "sk-first").unwrap();
    let remote = backend(endpoint(base)).with_credential_source(&user, base, None);
    let source = remote
        .credential
        .as_ref()
        .expect("key-authenticated endpoints read their key at dispatch");
    assert_eq!(source.resolve().unwrap().as_deref(), Some("sk-first"));
    llm::catalog::save_credential(&user, base, "sk-second").unwrap();
    assert_eq!(source.resolve().unwrap().as_deref(), Some("sk-second"));
    llm::catalog::remove_credential(&user, base).unwrap();
    assert_eq!(source.resolve().unwrap(), None);
}

#[test]
fn endpoint_retry_resolves_rotated_or_removed_key_without_stale_fallback() {
    for anthropic in [false, true] {
        for replacement in [Some("sk-replacement"), None] {
            let dir = tempfile::tempdir().unwrap();
            let server = fake::serve(vec![
                fake::json(
                    503,
                    &json!({"error": {"message": "temporarily unavailable"}}),
                ),
                if anthropic {
                    fake::json(
                        200,
                        &json!({
                            "id": "msg_test", "type": "message", "role": "assistant",
                            "content": [{"type": "text", "text": "retried"}],
                            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
                        }),
                    )
                } else {
                    ok("retried")
                },
            ]);
            let base = format!("{}/v1", server.url);
            llm::catalog::save_credential(dir.path(), &base, "sk-original").unwrap();
            let kind = if anthropic {
                anthropic_endpoint(&base)
            } else {
                endpoint(&base)
            };
            let mut remote = backend(kind).with_credential_source(dir.path(), &base, None);
            let mut changed = false;
            let mut notices = Vec::new();
            let reply = remote
                .chat_with_control_events(
                    None,
                    &request(),
                    llm::Limits {
                        timeout_secs: 5,
                        interrupt: Arc::new(AtomicBool::new(false)),
                    },
                    None,
                    &mut |notice| notices.push(notice.to_owned()),
                    &mut |event| {
                        if matches!(event, llm::StreamEvent::Retry { waiting: true }) {
                            match replacement {
                                Some(key) => {
                                    llm::catalog::save_credential(dir.path(), &base, key).unwrap()
                                }
                                None => llm::catalog::remove_credential(dir.path(), &base).unwrap(),
                            }
                            changed = true;
                        }
                        true
                    },
                )
                .unwrap();
            assert!(changed);
            assert_eq!(reply.text, "retried");
            let requests = server.requests();
            assert_eq!(requests.len(), 2);
            let name = if anthropic {
                "x-api-key"
            } else {
                "authorization"
            };
            let expected = |key: &str| {
                if anthropic {
                    key.to_owned()
                } else {
                    format!("Bearer {key}")
                }
            };
            assert!(
                header(&requests[0], name).as_deref() == Some(expected("sk-original").as_str())
            );
            assert!(header(&requests[1], name) == replacement.map(expected));
            for secret in ["sk-original", "sk-replacement", "sk-secret", "anth-secret"] {
                assert!(!notices.iter().any(|notice| notice.contains(secret)));
            }
        }
    }
}

// All account/transport fixtures are local; these tests never refresh a token or contact ChatGPT.
fn rotation_backend(dir: &Path, url: &str) -> Backend {
    llm::chatgpt::test_account(dir, false);
    let path = dir.join("chatgpt-accounts.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut other = state["accounts"]["oaiapp_test"].clone();
    other["subject"] = json!("oaiapp_other");
    other["tokens"]["access_token"] = json!(llm::chatgpt::test_access("oaiapp_other"));
    state["accounts"]["oaiapp_other"] = other;
    crate::config::settings_store::write_private(&path, state.to_string().as_bytes()).unwrap();
    let mut remote = backend(ProviderKind::ChatGpt {
        user_dir: dir.into(),
        model: "account-model".into(),
    })
    .with_account("oaiapp_test".into());
    remote.chatgpt_url = Some(url.into());
    remote
}

fn codex_rotation_success() -> Script {
    Script::Sse(format!(
        "data: {}\n\n",
        json!({"type":"response.completed","response":{"status":"completed","output":[
            {"type":"message","id":"msg_rotated","role":"assistant","content":[{"type":"output_text","text":"rotated"}]}
        ]}})
    ))
}

#[test]
fn chatgpt_portable_failover_pins_backend_and_replays_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = fake::serve(vec![
        fake::json(401, &json!({"error":{"message":"rejected"}})),
        codex_rotation_success(),
        codex_rotation_success(),
    ]);
    let mut remote = rotation_backend(dir.path(), &server.url);
    // Constructors generate transport affinity IDs even without an owning session.
    // Such an ID must not make a portable request look persistently bound.
    assert!(remote.session_id().is_some());
    assert!(!remote.session_bound);
    let mut chat = request();
    let reply = remote.chat(None, &chat, 5, None).unwrap();
    assert_eq!(reply.text, "rotated");
    assert!(!reply.provider_content.is_empty());
    assert_eq!(remote.account_id(), Some("oaiapp_other"));
    chat.messages.push(llm::assistant_message(&reply));
    chat.messages
        .push(json!({"role":"user","content":"continue"}));
    remote.chat(None, &chat, 5, None).unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    for (raw, expected) in requests
        .iter()
        .zip(["oaiapp_test", "oaiapp_other", "oaiapp_other"])
    {
        assert_eq!(header(raw, "chatgpt-account-id").as_deref(), Some(expected));
        assert_eq!(
            header(raw, "authorization"),
            Some(format!("Bearer {}", llm::chatgpt::test_access(expected)))
        );
    }
}

#[test]
fn chatgpt_failover_refuses_bound_cache_and_existing_native_history() {
    for binding in ["session", "cache", "native"] {
        let dir = tempfile::tempdir().unwrap();
        let server = fake::serve(vec![fake::json(
            401,
            &json!({"error":{"message":"rejected"}}),
        )]);
        let mut remote = rotation_backend(dir.path(), &server.url);
        let mut chat = request();
        match binding {
            "session" => remote = remote.with_session_id("existing-session"),
            "cache" => remote.prompt_cache_key = Some("existing-namespace".into()),
            "native" => {
                let scope = llm::model_config::native_replay_scope(
                    "responses",
                    &server.url,
                    None,
                    Some("oaiapp_test"),
                    None,
                    &[],
                );
                chat.messages.push(json!({"role":"assistant","content":"prior reply",
                    "dial_provider_content":[{"type":"dial_response_item","scope":scope,
                        "item":{"type":"reasoning","id":"rs_A","summary":[],"encrypted_content":"opaque-A"}}]}));
            }
            _ => unreachable!(),
        }
        let error = remote.chat(None, &chat, 5, None).unwrap_err();
        assert!(
            error.to_string().contains("account failover refused"),
            "{error:#}"
        );
        assert_eq!(remote.account_id(), Some("oaiapp_test"));
        assert_eq!(
            server.requests().len(),
            1,
            "must not dispatch as B: {binding}"
        );
    }
}

#[test]
fn chatgpt_failover_preserves_account_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let server = fake::serve(vec![fake::json(
        401,
        &json!({"error":{"message":"rejected"}}),
    )]);
    let mut remote = rotation_backend(dir.path(), &server.url);
    let mut config = crate::config::types::Config::default();
    config.chatgpt_allowed_accounts = Some(["oaiapp_test".to_owned()].into());
    remote = remote.with_steering(&config);
    assert!(remote.chat(None, &request(), 5, None).is_err());
    assert_eq!(remote.account_id(), Some("oaiapp_test"));
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn endpoint_streams_when_a_callback_is_given() {
    let chunk = |c: &str| {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": c}}]})
        )
    };
    let body = format!(
        "{}{}data: {}\n\ndata: [DONE]\n\n",
        chunk("a"),
        chunk("b"),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
    );
    let server = fake::serve(vec![Script::Sse(body)]);
    let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
    let mut seen = String::new();
    let mut on_text = |piece: &str| {
        seen.push_str(piece);
        true
    };
    let reply = remote
        .chat(None, &request(), 5, Some(&mut on_text))
        .unwrap();
    assert_eq!(reply.text, "ab");
    assert_eq!(seen, "ab");
    assert_eq!(fake::body(&server.requests()[0])["stream"], true);
}

#[test]
fn tool_free_auxiliary_requests_keep_an_array_tools_field_on_the_wire() {
    // Semantic find and other ranking calls offer no tools. Strict OpenAI-compatible servers reject a
    // missing or null `tools`, so the empty array must survive profile transforms. Constrained repair
    // is skipped when there is nothing to repair; a toolful request keeps its tools.
    let mut repair = llm::profiles::ModelProfile::minimal("lm-studio", "gpt-x");
    repair.tool_repair = true;
    let read =
        json!({"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}});
    for (profile, tools) in [(None, vec![]), (Some(repair), vec![]), (None, vec![read])] {
        let toolful = !tools.is_empty();
        let server = fake::serve(vec![ok("ranked")]);
        let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
        if let Some(profile) = profile {
            remote = remote.with_profile(profile);
        }
        let mut req = request();
        req.tools = tools;
        assert_eq!(remote.chat(None, &req, 5, None).unwrap().text, "ranked");
        let body = fake::body(&server.requests()[0]);
        if toolful {
            assert_eq!(body["tools"][0]["function"]["name"], "read_file", "{body}");
        } else {
            assert_eq!(body.get("tools"), Some(&json!([])), "{body}");
            assert!(body.get("response_format").is_none(), "{body}");
        }
    }
}

#[test]
fn image_references_reach_both_provider_wire_formats() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let root = tempfile::tempdir().unwrap();
    let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=").unwrap();
    std::fs::write(root.path().join("tiny.png"), &png).unwrap();
    let message = json!({
        "role": "user",
        "content": "Describe @tiny.png",
        "dial_images": [llm::images::user_attachment(root.path(), std::path::Path::new("tiny.png")).unwrap()],
    });
    assert!(!message.to_string().contains("base64"));
    for (anthropic, vision) in [
        (false, Some(true)),
        (true, Some(true)),
        (false, None),
        (true, None),
    ] {
        let response = if anthropic {
            fake::json(
                200,
                &json!({"content":[{"type":"text","text":"pixel"}],"stop_reason":"end_turn"}),
            )
        } else {
            ok("pixel")
        };
        let server = fake::serve(vec![response]);
        let kind = if anthropic {
            anthropic_endpoint(&server.url)
        } else {
            endpoint(&server.url)
        };
        let mut remote = backend(kind).with_metadata(llm::catalog::ModelMetadata {
            vision,
            ..Default::default()
        });
        let mut req = request();
        req.messages = vec![message.clone()];
        assert_eq!(remote.chat(None, &req, 5, None).unwrap().text, "pixel");
        let body = fake::body(&server.requests()[0]);
        let image = &body["messages"][0]["content"][1];
        let observation = remote.wire_observation().unwrap();
        assert_eq!(observation.boundary, "final_provider_wire");
        assert!(observation.payload_bytes > png.len());
        assert_eq!(
            observation.payload_sha256,
            format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(serde_json::to_vec(&body).unwrap())
            )
        );
        if anthropic {
            assert_eq!(image["type"], "image");
            assert_eq!(image["source"]["media_type"], "image/png");
            assert_eq!(image["source"]["data"], STANDARD.encode(&png));
        } else {
            assert_eq!(image["type"], "image_url");
            assert_eq!(
                image["image_url"]["url"],
                format!("data:image/png;base64,{}", STANDARD.encode(&png))
            );
        }
        assert!(!body.to_string().contains("dial_images"));
    }
    let server = fake::serve(vec![ok("text only")]);
    let mut remote = backend(endpoint(&server.url)).with_metadata(llm::catalog::ModelMetadata {
        vision: Some(false),
        ..Default::default()
    });
    let mut req = request();
    req.messages = vec![message];
    let mut notices = Vec::new();
    remote
        .chat_with_control(
            None,
            &req,
            5,
            None,
            &mut |text| notices.push(text.to_owned()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains("cannot view images"))
    );
    let content = fake::body(&server.requests()[0])["messages"][0]["content"].clone();
    assert!(content.as_str().unwrap().starts_with("Describe @tiny.png"));
    assert!(content.as_str().unwrap().contains("Image unavailable"));
    assert!(!content.to_string().contains("image_url"));
}

#[test]
fn stale_image_notice_is_once_per_conversation_with_stable_wire_history() {
    let root = tempfile::tempdir().unwrap();
    let image = root.path().join("shot.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\noriginal").unwrap();
    let reference = llm::images::attachment(root.path(), Path::new("shot.png")).unwrap();
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nreplacement").unwrap();
    let server = fake::serve(vec![ok("first"), ok("second"), ok("third")]);
    let mut remote = backend(endpoint(&server.url)).with_session_id("one");
    let mut req = request();
    req.messages = vec![json!({"role":"user","content":"inspect","dial_images":[reference]})];
    let mut notices = Vec::new();
    // Compaction and ordinary requests share notice state.
    remote.prepare_compaction_request(&req, &mut |n| notices.push(n.to_owned()));
    for _ in 0..2 {
        remote
            .chat_with_control(
                None,
                &req,
                5,
                None,
                &mut |n| notices.push(n.to_owned()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
    }
    assert_eq!(notices.len(), 1);
    remote = remote.with_session_id("two");
    remote
        .chat_with_control(
            None,
            &req,
            5,
            None,
            &mut |n| notices.push(n.to_owned()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert_eq!(notices.len(), 2, "a new conversation gets its own warning");
    let requests = server.requests();
    assert_eq!(
        fake::body(&requests[0])["messages"],
        fake::body(&requests[1])["messages"]
    );
    let content = fake::body(&requests[0])["messages"][0]["content"].clone();
    assert_eq!(
        content,
        "inspect\n\n[Image unavailable: shot.png; changed; reattach]"
    );
    assert!(!fake::body(&requests[0]).to_string().contains("image_url"));
}

#[cfg(unix)]
#[test]
fn local_chat_projects_system_messages_without_rewriting_the_request() {
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let mut local = Backend::new(
        ProviderKind::Local {
            model_dir: model.to_string_lossy().into_owned(),
        },
        server::fixtures::fake_python(root.path()),
        30,
    );
    let mut req = request();
    req.messages = vec![
        json!({"role":"system","content":"policy"}),
        json!({"role":"system","content":"workspace"}),
        json!({"role":"user","content":"hello"}),
        json!({"role":"system","content":"guidance","dial_transient":true}),
    ];
    let original = req.messages.clone();
    for streaming in [false, true] {
        let mut on_text = |_: &str| true;
        local
            .chat(None, &req, 5, streaming.then_some(&mut on_text))
            .unwrap();
    }
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(model.join("info.json")).unwrap()).unwrap();
    let requests = info["requests"].as_array().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "warmup plus plain and streaming requests"
    );
    for wire in &requests[1..] {
        assert_eq!(
            wire["messages"],
            json!([
                {"role":"system","content":"policy\n\nworkspace"},
                {"role":"user","content":"hello"},
                {"role":"user","content":"guidance"},
            ])
        );
    }
    assert_eq!(req.messages, original);
}

#[cfg(unix)]
#[test]
fn a_rebuilt_backend_adopts_the_resident_server_without_reloading_the_model() {
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let python = server::fixtures::fake_python(root.path());
    let kind = || ProviderKind::Local {
        model_dir: model.to_string_lossy().into_owned(),
    };
    let info = || -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(model.join("info.json")).unwrap()).unwrap()
    };
    let mut before = Backend::new(kind(), python.clone(), 30);
    before.chat(None, &request(), 5, None).unwrap();
    let launched = info()["pid"].clone();
    let server = before.take_resident_server().expect("a resident server");
    drop(before);

    let mut after = Backend::new(kind(), python, 30);
    after.adopt_resident_server(server);
    after.chat(None, &request(), 5, None).unwrap();
    let info = info();
    assert_eq!(info["pid"], launched, "the same server process answered");
    assert_eq!(
        info["requests"].as_array().unwrap().len(),
        3,
        "one warmup, then one request from each backend"
    );
    after.release();
}

#[cfg(unix)]
#[test]
fn failed_local_inference_preserves_diagnostics_and_restarts_before_reuse() {
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let mut local = Backend::new(
        ProviderKind::Local {
            model_dir: model.to_string_lossy().into_owned(),
        },
        server::fixtures::fake_python(root.path()),
        30,
    );
    let mut failing = request();
    failing.messages = vec![json!({"role":"user","content":"dial-test-fail"})];
    let error = local.chat(None, &failing, 5, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("test inference allocation failed"),
        "{error:#}"
    );
    assert!(
        error.downcast_ref::<llm::HttpError>().is_some(),
        "classification must survive diagnostics"
    );
    assert!(!local.server.as_ref().unwrap().is_running());
    let old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(model.join("info.json")).unwrap()).unwrap();
    for key in ["pid", "child"] {
        let pid = old[key].as_i64().unwrap() as libc::pid_t;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "{key} must be stopped before retry"
        );
    }
    local.chat(None, &request(), 5, None).unwrap();
    let new: serde_json::Value =
        serde_json::from_slice(&std::fs::read(model.join("info.json")).unwrap()).unwrap();
    assert_ne!(old["pid"], new["pid"]);
}

#[cfg(unix)]
#[test]
fn cancelled_local_prefill_stops_engine_that_ignores_disconnect_before_retry() {
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("model");
    std::fs::create_dir(&model).unwrap();
    let mut local = Backend::new(
        ProviderKind::Local {
            model_dir: model.to_string_lossy().into_owned(),
        },
        server::fixtures::fake_python(root.path()),
        30,
    );
    let mut stalled = request();
    stalled.messages = vec![json!({"role":"user","content":"dial-test-stall"})];
    let mut observed = false;
    let started = std::time::Instant::now();
    let error = local
        .chat_with_control_events(
            None,
            &stalled,
            llm::Limits {
                timeout_secs: 5,
                interrupt: Arc::new(AtomicBool::new(false)),
            },
            Some(&mut |_| true),
            &mut |_| {},
            &mut |event| {
                if matches!(event, llm::StreamEvent::Progress(p) if p.prompt_tokens == Some(42)) {
                    observed = true;
                    false
                } else {
                    true
                }
            },
        )
        .unwrap_err();
    assert!(observed, "cancel only after the engine has begun prefill");
    assert!(
        error.downcast_ref::<llm::Interrupted>().is_some(),
        "{error:#}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!local.server.as_ref().unwrap().is_running());
    local.chat(None, &request(), 5, None).unwrap();
}

#[cfg(unix)]
#[test]
fn local_vision_model_receives_pixels_and_uses_one_supervised_vlm_server() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("vision-model");
    std::fs::create_dir(&model).unwrap();
    std::fs::write(
        model.join("config.json"),
        r#"{"vision_config":{},"text_config":{}}"#,
    )
    .unwrap();
    // A real 1x1 PNG: image attachments are decoded for dimensions before they reach a model.
    let png = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=",
    )
    .unwrap();
    std::fs::write(root.path().join("image.png"), &png).unwrap();
    let mut local = Backend::new(
        ProviderKind::Local {
            model_dir: model.to_string_lossy().into_owned(),
        },
        server::fixtures::fake_python(root.path()),
        30,
    );
    assert!(local.accepts_image_input());
    let mut req = request();
    req.messages[0]["dial_images"] =
        json!([llm::images::attachment(root.path(), Path::new("image.png")).unwrap()]);
    let mut notices = Vec::new();
    for _ in 0..2 {
        assert_eq!(
            local
                .chat_with_control(
                    None,
                    &req,
                    5,
                    None,
                    &mut |notice| notices.push(notice.to_owned()),
                    Arc::new(AtomicBool::new(false))
                )
                .unwrap()
                .text,
            "pong"
        );
    }
    assert!(notices.is_empty(), "{notices:?}");
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(model.join("info.json")).unwrap()).unwrap();
    assert_eq!(info["engine"], "mlx-vlm");
    assert_eq!(info["apc"], "1");
    assert_eq!(
        info["apc_config"],
        json!({
            "APC_DISK_ENABLED": "0", "APC_NUM_BLOCKS": "256", "APC_MEMORY_MAX_GB": "1",
            "APC_MEMORY_RESERVE_GB": "0.25", "APC_CHECKPOINT_ENTRIES": "1",
        })
    );
    assert!(
        info["argv"]
            .as_array()
            .unwrap()
            .contains(&json!("--vision-cache-size=1"))
    );
    assert!(!info["argv"].as_array().unwrap().iter().any(|a| {
        a.as_str()
            .is_some_and(|s| s.starts_with("--prompt-cache-bytes="))
    }));
    let requests = info["requests"].as_array().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "one warmup and two requests reuse the same server"
    );
    for wire in &requests[1..] {
        assert_eq!(wire["model"], model.to_string_lossy().as_ref());
        assert_eq!(
            wire["messages"][0]["content"][1]["image_url"]["url"],
            format!("data:image/png;base64,{}", STANDARD.encode(&png))
        );
        assert!(!wire.to_string().contains("dial_images"));
    }
    let disabled = llm::profiles::ModelProfile {
        capabilities: llm::profiles::ModelCapabilities {
            vision: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(!local.image_input_for(Some(&disabled)));
    drop(local);
    let text = backend(ProviderKind::Local {
        model_dir: root.path().to_string_lossy().into_owned(),
    });
    assert!(!text.accepts_image_input());
}

#[cfg(unix)]
#[test]
fn vision_steering_status_and_notice_respect_manual_automatic_and_disabled_settings() {
    let shared = tempfile::tempdir().unwrap();
    let python = server::fixtures::fake_python(shared.path());
    let python = &python;
    std::thread::scope(|scope| {
        for (enabled, automatic, strength, state, notice_count) in [
            (true, false, 0.5, "bypassed", 1),
            (false, true, 0.5, "disabled", 0),
            (true, true, 0.0, "disabled", 0),
            (false, false, 0.5, "disabled", 0),
        ] {
            scope.spawn(move || {
                let root = tempfile::tempdir().unwrap();
                let model = root.path().join("vision-model");
                std::fs::create_dir(&model).unwrap();
                std::fs::write(model.join("config.json"), r#"{"vision_config":{}}"#).unwrap();
                let config = crate::config::types::Config {
                    model: model.to_string_lossy().into_owned(),
                    user_dir: root.path().join("user"),
                    state_dir: root.path().join("state"),
                    steering: config::steering::Config {
                        enabled,
                        automatic,
                        strength,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut local = Backend::new(
                    ProviderKind::Local {
                        model_dir: config.model.clone(),
                    },
                    python.clone(),
                    30,
                )
                .with_steering(&config);
                let mut notices = Vec::new();
                for _ in 0..2 {
                    assert_eq!(
                        local
                            .chat_with_control(
                                None,
                                &request(),
                                5,
                                None,
                                &mut |notice| notices.push(notice.to_owned()),
                                Arc::new(AtomicBool::new(false)),
                            )
                            .unwrap()
                            .text,
                        "pong"
                    );
                }
                assert_eq!(local.steering_runtime()["state"], state);
                assert_eq!(local.steering_runtime()["uses"], 0);
                assert_eq!(notices.len(), notice_count, "{notices:?}");
                assert!(!crate::llm::steering::vector_path(&config).exists());
            });
        }
    });
}

#[test]
fn costs_require_complete_usage_supported_prices_and_remote_provider() {
    let metadata = llm::catalog::ModelMetadata {
        vision: None,
        pricing: Some(llm::catalog::TokenPricing {
            prompt: 0.000001,
            completion: 0.000003,
            cache_read: None,
            cache_write: None,
        }),
        ..Default::default()
    };
    let remote = backend(endpoint("https://api.example.com")).with_metadata(metadata);
    let reply = Reply {
        text: String::new(),
        tool_calls: vec![],
        finish_reason: "stop".into(),
        prompt_tokens: Some(1000),
        completion_tokens: Some(500),
        ..Default::default()
    };
    assert!((remote.reply_cost(&reply).unwrap() - 0.0025).abs() < 1e-12);
    let local = backend(endpoint("http://localhost:1234")).with_metadata(metadata);
    assert_eq!(local.reply_cost(&reply), None);
    let mut missing = reply.clone();
    missing.prompt_tokens = None;
    assert_eq!(remote.reply_cost(&missing), None);
    assert_eq!(
        backend(endpoint("https://api.example.com")).reply_cost(&reply),
        None
    );
}

#[test]
fn bad_record_mac_alerts_retry_on_fresh_tls_connections_and_stop_at_the_bound() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut connections = 0;
        while connections < 10 && Instant::now() < deadline {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("TLS fixture accept: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            // Consume the ClientHello, then send a real TLS fatal BadRecordMac alert.
            // No certificate is accepted and no HTTP prompt or credentials reach the peer.
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).unwrap();
            assert_eq!(header[0], 0x16);
            let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
            stream.read_exact(&mut vec![0; length]).unwrap();
            stream
                .write_all(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x14])
                .unwrap();
            connections += 1;
        }
        connections
    });
    let mut remote = backend(endpoint(&url));
    let mut notices = Vec::new();
    let error = remote
        .chat_with_control(
            None,
            &request(),
            5,
            None,
            &mut |notice| notices.push(notice.to_owned()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
    assert_eq!(peer.join().unwrap(), 10, "{error:#}");
    assert_eq!(notices.len(), 9);
    assert!(format!("{error:#}").contains("received fatal alert: BadRecordMac"));
    assert!(error.to_string().contains("failed after 10 attempts"));
}

#[test]
fn connection_failures_retry_but_healthy_silence_keeps_the_same_stream() {
    let server = fake::serve(vec![Script::Hangup, ok("reconnected")]);
    let mut remote = backend(endpoint(&server.url));
    assert_eq!(
        remote.chat(None, &request(), 5, None).unwrap().text,
        "reconnected"
    );
    assert_eq!(server.requests().len(), 2);

    let pieces = vec![
        ": thinking\n\n".into(),
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":"after silence"},"finish_reason":"stop"}]})
        ),
    ];
    let server = fake::serve(vec![Script::SseSlow {
        pieces,
        pause: Duration::from_millis(350),
    }]);
    let mut remote = backend(endpoint(&server.url));
    let mut text = String::new();
    let mut request = request();
    request.messages[0]["content"] = json!("long prompt ".repeat(2000));
    let reply = remote
        .chat(
            None,
            &request,
            3600,
            Some(&mut |piece| {
                text.push_str(piece);
                true
            }),
        )
        .unwrap();
    assert_eq!(reply.text, "after silence");
    assert_eq!(text, "after silence");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn transient_failures_retry_with_visible_notices_and_stop_at_the_bound() {
    for status in [408, 409, 425, 429, 500, 502, 503, 504, 529] {
        let first = Script::HttpHeaders {
            status,
            headers: vec![("Retry-After", "0".into())],
            body: b"{\"error\":\"busy\"}".to_vec(),
        };
        let server = fake::serve(vec![first, ok("recovered")]);
        let mut remote = backend(endpoint(&server.url));
        let mut notices = Vec::new();
        let mut notify = |text: &str| notices.push(text.to_owned());
        assert_eq!(
            remote
                .chat_with_control(
                    None,
                    &request(),
                    5,
                    None,
                    &mut notify,
                    Arc::new(AtomicBool::new(false))
                )
                .unwrap()
                .text,
            "recovered"
        );
        assert_eq!(notices.len(), 1);
        assert_eq!(server.requests().len(), 2);
    }
    let server = fake::serve(
        (0..10)
            .map(|_| fake::json(503, &json!({"error":"busy"})))
            .collect(),
    );
    let mut remote = backend(endpoint(&server.url));
    let error = remote.chat(None, &request(), 5, None).unwrap_err();
    assert!(
        error.to_string().contains("failed after 10 attempts")
            && error.to_string().contains("Say \"continue\" to retry"),
        "{error:#}"
    );
    assert_eq!(error.downcast_ref::<llm::HttpError>().unwrap().status, 503);
    assert_eq!(server.requests().len(), 10);
}

#[test]
fn backoff_is_interruptible_and_obeys_retry_after() {
    let server = fake::serve(vec![Script::HttpHeaders {
        status: 429,
        headers: vec![("Retry-After", "4".into())],
        body: b"{}".to_vec(),
    }]);
    let mut remote = backend(endpoint(&server.url));
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut notice = String::new();
    let start = Instant::now();
    let error = remote
        .chat_with_control(
            None,
            &request(),
            5,
            None,
            &mut |text| {
                notice = text.into();
                cancelled.store(true, Ordering::SeqCst);
            },
            Arc::clone(&cancelled),
        )
        .unwrap_err();
    assert!(error.downcast_ref::<llm::Interrupted>().is_some());
    assert!(notice.contains("4.0s"));
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn permanent_statuses_never_retry() {
    for status in [400, 401, 403, 404, 501] {
        let server = fake::serve(vec![fake::json(status, &json!({"error":"permanent"}))]);
        let mut remote = backend(endpoint(&server.url));
        assert_eq!(
            remote
                .chat(None, &request(), 5, None)
                .unwrap_err()
                .downcast_ref::<llm::HttpError>()
                .unwrap()
                .status,
            status
        );
        assert_eq!(server.requests().len(), 1);
    }
}

#[test]
fn disconnected_streams_retry_with_fresh_replies_and_identical_requests() {
    for delta in [
        json!({"content":"partial"}),
        json!({"tool_calls":[{"index":0,"id":"call","function":{"name":"read","arguments":"{"}}]}),
    ] {
        for disconnect in [false, true] {
            let partial = format!("data: {}\n\n", json!({"choices":[{"delta":delta}]}));
            let server = fake::serve(vec![
                if disconnect {
                    Script::SseDisconnect(partial)
                } else {
                    Script::Sse(partial)
                },
                Script::Sse(format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[{"delta":{"content":"recovered"},"finish_reason":"stop"}]})
                )),
            ]);
            let mut remote = backend(endpoint(&server.url));
            let mut notices = Vec::new();
            let reply = remote
                .chat_with_control(
                    None,
                    &request(),
                    5,
                    Some(&mut |_| true),
                    &mut |text| notices.push(text.to_owned()),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            assert_eq!(reply.text, "recovered");
            assert!(reply.tool_calls.is_empty());
            assert!(
                notices
                    .iter()
                    .any(|n| n.contains("discarding the partial reply"))
            );
            let requests = server.requests();
            assert_eq!(requests.len(), 2);
            assert_eq!(fake::body(&requests[0]), fake::body(&requests[1]));
        }
    }
}

#[test]
fn disconnected_stream_retries_are_bounded_and_cancellable() {
    let server = fake::serve(
        (0..10)
            .map(|_| Script::SseDisconnect(String::new()))
            .collect(),
    );
    let error = backend(endpoint(&server.url))
        .chat(None, &request(), 5, Some(&mut |_| true))
        .unwrap_err();
    assert!(error.to_string().contains("after 10 attempts"), "{error:#}");
    assert_eq!(server.requests().len(), 10);

    let server = fake::serve(vec![Script::SseDisconnect(String::new())]);
    let cancelled = Arc::new(AtomicBool::new(false));
    let error = backend(endpoint(&server.url))
        .chat_with_control(
            None,
            &request(),
            5,
            Some(&mut |_| true),
            &mut |_| cancelled.store(true, Ordering::SeqCst),
            Arc::clone(&cancelled),
        )
        .unwrap_err();
    assert!(error.downcast_ref::<llm::Interrupted>().is_some());
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn disconnect_after_chat_finish_reason_does_not_regenerate_the_reply() {
    let body = format!(
        "data: {}\n\n",
        json!({"choices":[{"delta":{"content":"complete"},"finish_reason":"stop"}]})
    );
    let server = fake::serve(vec![Script::SseDisconnect(body)]);
    let reply = backend(endpoint(&server.url))
        .chat(None, &request(), 5, Some(&mut |_| true))
        .unwrap();
    assert_eq!(reply.text, "complete");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn client_errors_and_interruptions_are_not_retried() {
    let server = fake::serve(vec![fake::json(401, &json!({"error": "no"}))]);
    let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
    let error = remote.chat(None, &request(), 5, None).unwrap_err();
    assert_eq!(
        error.downcast_ref::<llm::HttpError>().map(|e| e.status),
        Some(401)
    );

    let pieces = (0..20)
        .map(|i| {
            format!(
                "data: {}\n\n",
                json!({"choices": [{"delta": {"content": format!("w{i}")}}]})
            )
        })
        .collect();
    let server = fake::serve(vec![Script::SseSlow {
        pieces,
        pause: Duration::from_millis(50),
    }]);
    let mut remote = backend(endpoint(&format!("{}/v1", server.url)));
    let mut stop = |_: &str| false;
    let error = remote
        .chat(None, &request(), 10, Some(&mut stop))
        .unwrap_err();
    assert!(
        error.downcast_ref::<llm::Interrupted>().is_some(),
        "{error:#}"
    );
}

#[test]
fn context_window_from_local_metadata_not_endpoint_name_guesses() {
    let dir = tempfile::tempdir().unwrap();
    let local = ProviderKind::Local {
        model_dir: dir.path().to_str().unwrap().into(),
    };
    assert_eq!(context_window_hint(&local), None, "missing config.json");
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"max_position_embeddings": 8192}"#,
    )
    .unwrap();
    assert_eq!(context_window_hint(&local), Some(8192));
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"text_config": {"max_position_embeddings": 32768}}"#,
    )
    .unwrap();
    assert_eq!(context_window_hint(&local), Some(32768));
    std::fs::write(dir.path().join("config.json"), "{broken").unwrap();
    assert_eq!(context_window_hint(&local), None);

    assert_eq!(
        context_window_hint(&endpoint("https://e.com")),
        None,
        "unknown model"
    );
    let known = ProviderKind::Endpoint {
        base_url: "https://openrouter.ai/api/v1".into(),
        model: "openai/gpt-4o-mini".into(),
        api_key: None,
    };
    assert_eq!(
        context_window_hint(&known),
        Some(128000),
        "bundled context metadata is grounded for catalogued remote models"
    );
}

#[test]
fn release_without_a_server_is_a_no_op() {
    let mut local = backend(ProviderKind::Local {
        model_dir: "/models/x".into(),
    });
    local.release();
    local.release();
}
fn anthropic_sse(events: &[serde_json::Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

fn thinking_tool_events() -> Vec<serde_json::Value> {
    vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":0,"cache_read_input_tokens":11,"cache_creation_input_tokens":13}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"consider "}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"the file"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed-block"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Reading "}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"now."}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call-a","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":19}}),
        json!({"type":"message_stop"}),
    ]
}

#[test]
fn native_anthropic_streams_text_thinking_tools_usage_and_preserves_signed_replay() {
    let server = fake::serve(vec![Script::Sse(anthropic_sse(&thinking_tool_events()))]);
    let mut remote = backend(anthropic_endpoint(&server.url));
    let mut text = Vec::new();
    let mut events = Vec::new();
    let reply = remote
        .chat_with_control_events(
            None,
            &request(),
            crate::llm::Limits {
                timeout_secs: 5,
                interrupt: Arc::new(AtomicBool::new(false)),
            },
            Some(&mut |piece| {
                text.push(piece.to_owned());
                true
            }),
            &mut |_| {},
            &mut |event| {
                events.push(event.clone());
                true
            },
        )
        .unwrap();
    assert_eq!(text, ["Reading ", "now."]);
    assert_eq!(reply.text, "Reading now.");
    assert_eq!(reply.reasoning, "consider the file");
    assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(
        (reply.prompt_tokens, reply.completion_tokens),
        (Some(31), Some(19))
    );
    assert_eq!(
        (reply.cached_prompt_tokens, reply.cache_creation_tokens),
        (Some(11), Some(13))
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, llm::StreamEvent::Reasoning(_)))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, llm::StreamEvent::ToolArguments { .. }))
            .count(),
        2
    );
    let body = fake::body(&server.requests()[0]);
    assert_eq!(body["stream"], true);
    let mut req = request();
    req.messages = vec![
        llm::assistant_message(&reply),
        llm::tool_message("call-a", "contents"),
    ];
    let replay = anthropic_request("claude-test", &req).unwrap();
    assert_eq!(
        replay["messages"][0]["content"][0]["signature"],
        "signed-block"
    );
    assert_eq!(replay["messages"][1]["content"][0]["tool_use_id"], "call-a");
}

#[test]
fn incomplete_anthropic_stream_retries_without_returning_partial_calls() {
    let events = thinking_tool_events();
    let mut missing_reason = events.clone();
    missing_reason.remove(missing_reason.len() - 2);
    let mut open_call = events[..12].to_vec();
    open_call.push(json!({"type":"message_stop"}));
    for incomplete in [
        events[..events.len() - 1].to_vec(),
        missing_reason,
        open_call,
    ] {
        let server = fake::serve(vec![
            Script::Sse(anthropic_sse(&incomplete)),
            Script::Sse(anthropic_sse(&events)),
        ]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let mut notices = Vec::new();
        let reply = remote
            .chat_with_control(
                None,
                &request(),
                5,
                Some(&mut |_| true),
                &mut |n| notices.push(n.to_owned()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
        assert!(
            notices
                .iter()
                .any(|n| n.contains("discarding the partial reply"))
        );
        assert_eq!(fake::body(&requests[0]), fake::body(&requests[1]));
    }
}

#[test]
fn incomplete_anthropic_stream_stops_at_existing_retry_bound() {
    let events = thinking_tool_events();
    let server = fake::serve(
        (0..10)
            .map(|_| Script::Sse(anthropic_sse(&events[..events.len() - 1])))
            .collect(),
    );
    let mut remote = backend(anthropic_endpoint(&server.url));
    let error = remote
        .chat(None, &request(), 5, Some(&mut |_| true))
        .unwrap_err();
    assert!(error.to_string().contains("after 10 attempts"), "{error:#}");
    assert_eq!(server.requests().len(), 10);
}

#[test]
fn native_anthropic_stalls_and_ping_only_streams_retry() {
    Patience::with_test_idle_limit(Duration::from_millis(200));
    let events = thinking_tool_events();
    let ping = anthropic_sse(&[json!({"type":"ping"})]);
    for first in [None, Some(anthropic_sse(&events[..1]))] {
        let mut pieces = vec![first.unwrap_or_default()];
        // Long enough to outlast the setup budget, so a ping-only stream ends by the idle limit, not EOF.
        pieces.extend(std::iter::repeat_n(ping.clone(), 250));
        let server = fake::serve(vec![
            Script::SseSlow {
                pieces,
                pause: Duration::from_millis(10),
            },
            Script::Sse(anthropic_sse(&events)),
        ]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let mut notices = Vec::new();
        let reply = remote
            .chat_with_control(
                None,
                &request(),
                1,
                Some(&mut |_| true),
                &mut |n| notices.push(n.to_owned()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        assert!(
            notices.iter().any(|n| n.contains("idle limit")),
            "{notices:?}"
        );
        assert_eq!(server.requests().len(), 2);
    }
    for script in [
        Script::Stall {
            // Silence beyond the setup budget: the client's deadline, not end-of-stream, ends this attempt.
            silence: Duration::from_millis(2000),
        },
        Script::StallAfter {
            first: anthropic_sse(&events[..12]),
            silence: Duration::from_millis(320),
        },
    ] {
        let server = fake::serve(vec![script, Script::Sse(anthropic_sse(&events))]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let reply = remote
            .chat(None, &request(), 1, Some(&mut |_| true))
            .unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
        assert_eq!(server.requests().len(), 2);
    }
}

#[test]
fn native_anthropic_noop_events_cannot_keep_a_failed_tool_round_waiting() {
    Patience::with_test_idle_limit(Duration::from_millis(200));
    let events = thinking_tool_events();
    // Stall after an open tool call, with valid but output-free events arriving.
    // The failed browser result is already in the request for this model round.
    for (prefix, noise) in [
        (
            12,
            json!({"type":"content_block_delta","index":2,
            "delta":{"type":"input_json_delta","partial_json":""}}),
        ),
        (
            8,
            json!({"type":"content_block_delta","index":1,
            "delta":{"type":"text_delta","text":""}}),
        ),
        (
            4,
            json!({"type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":""}}),
        ),
        (
            4,
            json!({"type":"content_block_delta","index":0,
            "delta":{"type":"signature_delta","signature":""}}),
        ),
        (
            12,
            json!({"type":"message_delta","delta":{},"usage":{"output_tokens":0}}),
        ),
    ] {
        let mut pieces = vec![anthropic_sse(&events[..prefix])];
        // The noise must outlast the 200ms idle limit. A stream that closes first is an incomplete-stream
        // retry, not the idle path this case asserts, so the two would race on a loaded machine.
        pieces.extend(std::iter::repeat_n(anthropic_sse(&[noise]), 60));
        let server = fake::serve(vec![
            Script::SseSlow {
                pieces,
                pause: Duration::from_millis(10),
            },
            Script::Sse(anthropic_sse(&events)),
        ]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let mut req = request();
        req.messages
            .push(json!({"role":"assistant","content":null,"tool_calls":[
                {"id":"browser-failed","type":"function","function":{
                    "name":"browser","arguments":"{}"}}
            ]}));
        req.messages.push(llm::tool_message(
            "browser-failed",
            "Connection refused (os error 61)",
        ));
        let mut notices = Vec::new();
        let reply = remote
            .chat_with_control(
                None,
                &req,
                1,
                Some(&mut |_| true),
                &mut |n| notices.push(n.to_owned()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert!(
            notices.iter().any(|n| n.contains("idle limit")),
            "{notices:?}"
        );
        assert_eq!(server.requests().len(), 2);
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
    }
}

#[test]
fn native_anthropic_meaningful_deltas_extend_idle_deadline() {
    // Leave scheduling headroom on shared runners while keeping the complete stream
    // longer than the idle budget: meaningful deltas must extend it to finish once.
    Patience::with_test_idle_limit(Duration::from_secs(1));
    let events = thinking_tool_events();
    let mut pieces = vec![anthropic_sse(&events[..12])];
    pieces.extend(std::iter::repeat_n(
        anthropic_sse(&[json!({
            "type":"content_block_delta","index":2,
            "delta":{"type":"input_json_delta","partial_json":" "}
        })]),
        40,
    ));
    pieces.push(anthropic_sse(&events[12..]));
    let server = fake::serve(vec![Script::SseSlow {
        pieces,
        pause: Duration::from_millis(50),
    }]);
    let mut remote = backend(anthropic_endpoint(&server.url));
    let mut notices = Vec::new();
    let reply = remote
        .chat_with_control(
            None,
            &request(),
            1,
            Some(&mut |_| true),
            &mut |n| notices.push(n.to_owned()),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert!(
        !notices.iter().any(|n| n.contains("idle limit")),
        "{notices:?}"
    );
    assert_eq!(server.requests().len(), 1);
    assert_eq!(reply.tool_calls[0].arguments, json!({"path":"a.rs"}));
}

#[test]
fn native_anthropic_transient_stream_errors_retry_and_remain_bounded() {
    let events = thinking_tool_events();
    for kind in ["overloaded_error", "api_error", "rate_limit_error"] {
        let mut partial = events[..12].to_vec();
        partial.push(json!({"type":"error","error":{
            "type":kind,"message":"anth-secret SECRET-PROMPT"
        }}));
        let server = fake::serve(vec![
            Script::Sse(anthropic_sse(&partial)),
            Script::Sse(anthropic_sse(&events)),
        ]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let mut notices = Vec::new();
        let reply = remote
            .chat_with_control(
                None,
                &request(),
                5,
                Some(&mut |_| true),
                &mut |n| notices.push(n.to_owned()),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        assert!(
            notices
                .iter()
                .any(|n| n.contains("discarding the partial reply"))
        );
        assert!(!notices.join(" ").contains("anth-secret"));
        assert!(!notices.join(" ").contains("SECRET-PROMPT"));
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(fake::body(&requests[0]), fake::body(&requests[1]));
    }
    let error_event = anthropic_sse(&[json!({"type":"error","error":{
        "type":"overloaded_error","message":"anth-secret SECRET-PROMPT"
    }})]);
    let server = fake::serve((0..10).map(|_| Script::Sse(error_event.clone())).collect());
    let mut remote = backend(anthropic_endpoint(&server.url));
    let error = remote
        .chat(None, &request(), 5, Some(&mut |_| true))
        .unwrap_err();
    assert!(error.to_string().contains("after 10 attempts"));
    assert!(!error.to_string().contains("anth-secret"));
    assert!(!error.to_string().contains("SECRET-PROMPT"));
    assert_eq!(server.requests().len(), 10);
}

#[test]
fn native_anthropic_post_response_failures_never_replay_and_cancel_is_typed() {
    let events = thinking_tool_events();
    for script in [
        Script::Sse("data: not-json\n\n".into()),
        Script::Sse("data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"anth-secret SECRET-PROMPT\"}}\n\n".into()),
    ] {
        let server = fake::serve(vec![script]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        let error = remote.chat(None, &request(), 5, Some(&mut |_| true)).unwrap_err();
        assert!(!llm::is_transient(&error));
        assert!(!error.to_string().contains("anth-secret") && !error.to_string().contains("SECRET-PROMPT"));
        assert_eq!(server.requests().len(), 1);
    }
    let server = fake::serve(vec![Script::Sse(anthropic_sse(&events))]);
    let mut remote = backend(anthropic_endpoint(&server.url));
    let error = remote
        .chat_with_control_events(
            None,
            &request(),
            crate::llm::Limits {
                timeout_secs: 5,
                interrupt: Arc::new(AtomicBool::new(false)),
            },
            Some(&mut |_| true),
            &mut |_| {},
            &mut |_| false,
        )
        .unwrap_err();
    assert!(error.downcast_ref::<llm::Interrupted>().is_some());
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn request_profile_is_one_shot_and_error_restores_baseline() {
    use llm::profiles::*;
    let baseline = ModelProfile {
        provider: "endpoint".into(),
        model: "test".into(),
        capabilities: ModelCapabilities {
            temperature: true,
            top_p: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut adaptive = baseline.clone();
    adaptive.temperature = Some(0.1);
    adaptive.top_p = Some(0.9);
    let server = fake::serve(vec![
        fake::json(400, &json!({"error":"invalid request"})),
        ok("second"),
    ]);
    let mut remote = backend(endpoint(&server.url)).with_profile(baseline);
    remote.next_request_profile(Some(adaptive));
    let req = request();
    assert!(remote.chat(None, &req, 5, None).is_err());
    remote.chat(None, &req, 5, None).unwrap();
    let sent = server.requests();
    assert_eq!(fake::body(&sent[0])["temperature"], 0.1);
    assert_eq!(fake::body(&sent[0])["top_p"], 0.9);
    assert_eq!(fake::body(&sent[1])["temperature"], req.temperature);
    assert!(fake::body(&sent[1]).get("top_p").is_none());
    assert!(remote.model_profile().unwrap().temperature.is_none());
}

#[cfg(unix)]
#[test]
fn thinking_controls_reach_only_the_owned_mlx_request_and_expire_after_use() {
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("text-model");
    std::fs::create_dir(&model).unwrap();
    let mut local = Backend::new(
        ProviderKind::Local {
            model_dir: model.to_string_lossy().into_owned(),
        },
        server::fixtures::fake_python(root.path()),
        30,
    );
    let guard = crate::config::types::ReasoningGuardConfig {
        enabled: false,
        max_reasoning_bytes: 4096,
        ..Default::default()
    };
    local.next_reasoning_guard(&guard);
    local
        .chat(None, &request(), 5, Some(&mut |_| true))
        .unwrap();
    local.chat(None, &request(), 5, None).unwrap();
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(model.join("info.json")).unwrap()).unwrap();
    let requests = info["requests"].as_array().unwrap();
    // The first request is the supervisor's one-token warmup, outside the agent gate.
    assert!(requests[0].get("dial_reasoning").is_none());
    assert_eq!(
        requests[1]["dial_reasoning"],
        json!({"max_bytes":3072,"max_seconds":0.0})
    );
    assert!(requests[2].get("dial_reasoning").is_none());
    local.release();

    let server = fake::serve(vec![ok("remote")]);
    let mut remote = backend(endpoint(&server.url));
    remote.next_reasoning_guard(&guard);
    remote.chat(None, &request(), 5, None).unwrap();
    assert!(
        fake::body(&server.requests()[0])
            .get("dial_reasoning")
            .is_none()
    );
}

#[test]
fn explicit_openai_reasoning_profile_and_cached_usage_reach_wire_and_reply() {
    use llm::profiles::*;
    let profile = ModelProfile {
        provider: "endpoint".into(),
        model: "gpt-x".into(),
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::OpenaiEffort,
            reasoning_efforts: vec![ReasoningEffort::High],
            output_limit: OutputLimit::MaxCompletionTokens,
            ..Default::default()
        },
        reasoning_effort: Some(ReasoningEffort::High),
        max_tokens: Some(4096),
        ..Default::default()
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"reasoning_content":"reason"},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":80},"completion_tokens_details":{"reasoning_tokens":17}}})
    );
    let server = fake::serve(vec![Script::Sse(body)]);
    let mut remote = backend(endpoint(&server.url)).with_profile(profile);
    let mut req = request();
    req.max_tokens = 4096;
    let reply = remote.chat(None, &req, 5, Some(&mut |_| true)).unwrap();
    assert_eq!(reply.reasoning, "reason");
    assert_eq!(reply.reasoning_tokens, Some(17));
    assert_eq!(reply.cached_prompt_tokens, Some(80));
    let wire = fake::body(&server.requests()[0]);
    assert_eq!(wire["reasoning_effort"], "high");
    assert_eq!(wire["max_completion_tokens"], 4096);
    assert!(wire.get("temperature").is_none() && wire.get("max_tokens").is_none());
}

#[test]
fn native_anthropic_explicit_thinking_effort_and_cache_prefix_are_consumed() {
    use llm::profiles::*;
    let profile = ModelProfile {
        provider: "anthropic".into(),
        model: "claude-test".into(),
        capabilities: ModelCapabilities {
            reasoning: ReasoningApi::AnthropicBudget,
            reasoning_efforts: vec![ReasoningEffort::High],
            prompt_cache: true,
            ..Default::default()
        },
        thinking: Some(true),
        thinking_budget: Some(1024),
        reasoning_effort: Some(ReasoningEffort::High),
        max_tokens: Some(2048),
        prompt_cache: Some(true),
        ..Default::default()
    };
    let server = fake::serve(vec![fake::json(
        200,
        &json!({"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}),
    )]);
    let mut remote = backend(anthropic_endpoint(&server.url)).with_profile(profile);
    let mut req = request();
    req.messages
        .insert(0, json!({"role":"system","content":"stable prefix"}));
    req.max_tokens = 2048;
    remote.chat(None, &req, 5, None).unwrap();
    let wire = fake::body(&server.requests()[0]);
    assert_eq!(
        wire["thinking"],
        json!({"type":"enabled","budget_tokens":1024})
    );
    assert_eq!(wire["output_config"]["effort"], "high");
    assert_eq!(wire["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(wire["max_tokens"], 2048);
    assert!(wire.get("temperature").is_none());
}

#[test]
fn native_anthropic_history_caching_and_transient_guidance_reach_the_wire() {
    use llm::profiles::*;
    for enabled in [None, Some(false)] {
        let response = || {
            fake::json(
                200,
                &json!({"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}),
            )
        };
        let server = fake::serve(vec![response(), response()]);
        let mut remote = backend(anthropic_endpoint(&server.url));
        if enabled.is_some() {
            remote = remote.with_profile(ModelProfile {
                provider: "anthropic".into(),
                model: "claude-test".into(),
                prompt_cache: enabled,
                ..Default::default()
            });
        }
        let mut req = request();
        req.messages
            .insert(0, json!({"role":"system","content":"stable"}));
        req.messages
            .push(json!({"role":"user","content":"contract one","dial_transient":true}));
        remote.chat(None, &req, 5, None).unwrap();
        req.messages.pop();
        req.messages
            .push(json!({"role":"assistant","content":"ok"}));
        req.messages
            .push(json!({"role":"user","content":"contract two","dial_transient":true}));
        remote.chat(None, &req, 5, None).unwrap();
        let seen = server.requests();
        let first = fake::body(&seen[0]);
        let next = fake::body(&seen[1]);
        assert_eq!(first["system"], next["system"]);
        assert_eq!(first["messages"][0], next["messages"][0]);
        assert_eq!(first["messages"][1]["content"], "contract one");
        assert_eq!(next["messages"][2]["content"], "contract two");
        assert!(next["messages"][2].get("dial_transient").is_none());
        if enabled == Some(false) {
            assert_eq!(next["messages"][0]["content"], "SECRET-PROMPT");
            assert_eq!(next["system"], "stable");
        } else {
            assert_eq!(
                next["messages"][0]["content"][0]["cache_control"]["type"],
                "ephemeral"
            );
            assert_eq!(
                next["messages"][1]["content"][0]["cache_control"]["type"],
                "ephemeral"
            );
        }
    }
}

#[test]
fn profiled_provider_honors_increased_output_budget_on_next_request() {
    use llm::profiles::*;
    for anthropic in [false, true] {
        let response = |length: bool| {
            if anthropic {
                fake::json(
                    200,
                    &json!({"content":[{"type":"text","text":"done"}],"stop_reason":if length { "max_tokens" } else { "end_turn" }}),
                )
            } else {
                fake::json(
                    200,
                    &json!({"choices":[{"finish_reason":if length { "length" } else { "stop" },"message":{"content":"done"}}]}),
                )
            }
        };
        let server = fake::serve(vec![response(true), response(false)]);
        let profile = ModelProfile {
            provider: if anthropic { "anthropic" } else { "endpoint" }.into(),
            model: if anthropic { "claude-test" } else { "gpt-x" }.into(),
            max_tokens: Some(16),
            capabilities: ModelCapabilities {
                output_limit: if anthropic {
                    OutputLimit::MaxTokens
                } else {
                    OutputLimit::MaxCompletionTokens
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let mut remote = backend(if anthropic {
            anthropic_endpoint(&server.url)
        } else {
            endpoint(&server.url)
        })
        .with_profile(profile);
        let mut req = request();
        req.max_tokens = 16;
        assert_eq!(
            remote.chat(None, &req, 5, None).unwrap().finish_reason,
            "length"
        );
        req.max_tokens = 64;
        remote.chat(None, &req, 5, None).unwrap();
        let sent = server.requests();
        let field = if anthropic {
            "max_tokens"
        } else {
            "max_completion_tokens"
        };
        assert_eq!(fake::body(&sent[0])[field], 16);
        assert_eq!(fake::body(&sent[1])[field], 64);
    }
}

#[test]
fn cache_cost_never_fabricates_missing_discount_prices() {
    let pricing = llm::catalog::TokenPricing {
        prompt: 1.0,
        completion: 2.0,
        cache_read: None,
        cache_write: None,
    };
    let reply = Reply {
        prompt_tokens: Some(100),
        completion_tokens: Some(5),
        cached_prompt_tokens: Some(80),
        ..Default::default()
    };
    assert_eq!(pricing.reply_cost(&reply), None);
    assert_eq!(
        llm::catalog::TokenPricing {
            cache_read: Some(0.1),
            ..pricing
        }
        .reply_cost(&reply),
        Some(38.0)
    );
}

#[test]
fn native_anthropic_interrupt_abandons_one_live_stream() {
    let server = fake::serve(vec![Script::Drip {
        pause: Duration::from_millis(20),
        count: 100,
    }]);
    let mut remote = backend(anthropic_endpoint(&server.url));
    let interrupt = Arc::new(AtomicBool::new(false));
    let flag = interrupt.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        flag.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let error = remote
        .chat_with_control(
            None,
            &request(),
            3600,
            Some(&mut |_| true),
            &mut |_| {},
            interrupt,
        )
        .unwrap_err();
    trigger.join().unwrap();
    assert!(error.downcast_ref::<llm::Interrupted>().is_some());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn interrupted_openai_usage_chunk_is_retained_but_never_accepted_or_priced() {
    let server = fake::serve(vec![Script::Sse(
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":10}}}\n\n".into(),
    )]);
    let mut remote = backend(endpoint(&server.url));
    remote.set_usage_scope(Some(llm::usage::Scope::default().next_turn(0).unwrap()));
    let error = remote.chat_with_control_events(
        None, &request(), llm::Limits { timeout_secs: 5, interrupt: Arc::new(AtomicBool::new(false)) },
        Some(&mut |_| true), &mut |_| {}, &mut |event| !matches!(event, llm::StreamEvent::Progress(p) if p.output_tokens == Some(7)),
    ).unwrap_err();
    assert!(error.downcast_ref::<llm::Interrupted>().is_some());
    let counts = remote.usage_scope_snapshot().all;
    assert_eq!(
        (counts.input, counts.output, counts.cache_read),
        (42, 7, 10)
    );
    assert_eq!(counts.partial_reported_attempts, 1);
    assert_eq!(counts.failed_requests, 1);
    assert_eq!(counts.reported_responses, 0);
    assert_eq!(counts.unreported_attempts, 1);
    assert_eq!(counts.observed_cost_usd, None);
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn disconnected_anthropic_start_retains_only_reported_usage() {
    let server = fake::serve(vec![Script::SseDisconnect(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"cache_read_input_tokens\":30}}}\n\n".into(),
    )]);
    let mut remote = backend(anthropic_endpoint(&server.url));
    remote.set_usage_scope(Some(llm::usage::Scope::default().next_turn(0).unwrap()));
    assert!(
        remote
            .chat(None, &request(), 5, Some(&mut |_| true))
            .is_err()
    );
    let counts = remote.usage_scope_snapshot().all;
    assert_eq!(
        (counts.input, counts.output, counts.cache_read),
        (50, 0, 30)
    );
    assert_eq!(counts.partial_reported_attempts, 1);
    assert_eq!(counts.reported_responses, 0);
    // Disconnect recovery may retry; every attempt still lacks complete terminal usage.
    assert!(counts.transport_attempts >= 1);
    assert_eq!(counts.unreported_attempts, counts.transport_attempts);
    assert_eq!(counts.failed_requests, 1);
    assert_eq!(counts.observed_cost_usd, None);
    assert_eq!(server.requests().len(), 1);
}
