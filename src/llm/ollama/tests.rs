use super::*;
use crate::llm::profiles::{ModelProfile, OutputLimit};

#[test]
fn repair_format_replaces_tools_only_on_the_repair_request() {
    let endpoint = Endpoint {
        url: "http://localhost",
        auth: None,
        model: "m",
        headers: &[],
    };
    let req = Chat {
        messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
        tools: vec![serde_json::json!({"type": "function", "function": {
            "name": "read",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
        }})],
        temperature: 0.0,
        max_tokens: 64,
        seed: None,
    };
    let mut profile = ModelProfile::minimal("ollama", "m");
    profile.capabilities.output_limit = OutputLimit::MaxTokens;
    let plain = request(&endpoint, &req, Some(&profile), None, "main").unwrap();
    assert!(plain.get("format").is_none());
    assert!(plain.get("tools").is_some());

    profile.tool_repair = true;
    let repair = request(&endpoint, &req, Some(&profile), None, "main").unwrap();
    assert_eq!(repair["format"]["oneOf"].as_array().map(Vec::len), Some(1));
    assert!(repair.get("tools").is_none());
}

fn chat() -> Chat {
    Chat {
        messages: vec![serde_json::json!({"role": "user", "content": "hi"})],
        tools: vec![],
        temperature: 0.2,
        max_tokens: 64,
        seed: None,
    }
}

const ENDPOINT: Endpoint<'static> = Endpoint {
    url: "http://localhost",
    auth: None,
    model: "m",
    headers: &[],
};

#[test]
fn the_planned_window_is_requested_unless_a_profile_sets_its_own() {
    let unprofiled = request(&ENDPOINT, &chat(), None, Some(32768), "main").unwrap();
    assert_eq!(unprofiled["options"]["num_ctx"], 32768);
    let unknown = request(&ENDPOINT, &chat(), None, None, "main").unwrap();
    assert!(unknown["options"].get("num_ctx").is_none());
    let mut profile = ModelProfile::minimal("ollama", "m");
    profile.capabilities.output_limit = OutputLimit::MaxTokens;
    profile.context_window = Some(8192);
    let pinned = request(&ENDPOINT, &chat(), Some(&profile), Some(32768), "main").unwrap();
    assert_eq!(pinned["options"]["num_ctx"], 8192);
}

#[test]
fn ollama_sampling_and_keep_alive_reach_options_with_native_names() {
    let mut profile = ModelProfile::minimal("ollama", "m");
    profile.capabilities.output_limit = OutputLimit::MaxTokens;
    profile.keep_alive = Some("30m".into());
    profile.local_sampling = Some(crate::llm::local_sampling::LocalSampling {
        top_k: Some(20),
        min_p: Some(0.05),
        repetition_penalty: Some(1.05),
        repetition_context_size: Some(128),
        ..Default::default()
    });
    profile.validate().unwrap();
    let body = request(&ENDPOINT, &chat(), Some(&profile), None, "main").unwrap();
    assert_eq!(body["keep_alive"], "30m");
    let options = &body["options"];
    assert_eq!(options["top_k"], 20);
    assert_eq!(options["min_p"], 0.05);
    assert_eq!(options["repeat_penalty"], 1.05);
    assert_eq!(options["repeat_last_n"], 128);
    assert!(options.get("repetition_penalty").is_none());
    assert!(
        body.get("top_k").is_none(),
        "Ollama reads sampling from options"
    );

    profile.local_sampling.as_mut().unwrap().xtc_probability = Some(0.1);
    assert!(profile.validate().is_err());
    assert!(request(&ENDPOINT, &chat(), Some(&profile), None, "main").is_err());
}

fn ndjson(lines: &[Value]) -> Vec<u8> {
    lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>()
        .into_bytes()
}

/// One native `/api/chat` exchange against a scripted server; returns the reply and the
/// visible text pieces delivered to the caller.
fn native_exchange(
    url: &str,
    context_window: Option<u32>,
    pieces: &mut Vec<String>,
) -> Result<Reply> {
    let mut on_text_fn = |piece: &str| {
        pieces.push(piece.to_owned());
        true
    };
    let mut on_text: Option<&mut dyn FnMut(&str) -> bool> = Some(&mut on_text_fn);
    exchange(
        Endpoint {
            url,
            auth: None,
            model: "m",
            headers: &[],
        },
        &chat(),
        10,
        &mut on_text,
        Arc::new(AtomicBool::new(false)),
        None,
        context_window,
        &mut |_| true,
    )
}

#[test]
fn native_ndjson_stream_delivers_text_thinking_and_object_arguments_with_counts() {
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Http {
        status: 200,
        content_type: "application/x-ndjson",
        body: ndjson(&[
            json!({"model": "m", "message": {"role": "assistant", "thinking": "plan"}, "done": false}),
            json!({"model": "m", "message": {"role": "assistant", "content": "Hel"}, "done": false}),
            json!({"model": "m", "message": {"role": "assistant", "content": "lo"}, "done": false}),
            json!({"model": "m", "message": {"role": "assistant", "tool_calls": [
                {"function": {"name": "read_file", "arguments": {"path": "a.txt"}}}
            ]}, "done": false}),
            json!({"model": "m", "message": {"role": "assistant", "content": ""}, "done": true,
                "done_reason": "stop", "prompt_eval_count": 12, "eval_count": 7,
                "eval_duration": 1_000_000_000u64}),
        ]),
    }]);
    let mut pieces = Vec::new();
    let reply = native_exchange(&server.url, Some(32768), &mut pieces).unwrap();
    assert_eq!(pieces, ["Hel", "lo"]);
    assert_eq!(reply.text, "Hello");
    assert_eq!(reply.reasoning, "plan");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].name, "read_file");
    assert_eq!(reply.tool_calls[0].arguments, json!({"path": "a.txt"}));
    assert!(!reply.tool_calls[0].malformed_arguments);
    assert_eq!(reply.finish_reason, "tool_calls");
    assert_eq!(
        (reply.prompt_tokens, reply.completion_tokens),
        (Some(12), Some(7))
    );
    let sent = crate::llm::fake::body(&server.requests().remove(0));
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["options"]["num_ctx"], 32768);
}

#[test]
fn local_hints_reach_ollama_body_and_explicit_profile_values_win() {
    use crate::llm::local_hints::{LocalHints, LocalServer};
    let hints = LocalHints {
        thinking: Some(false),
        keep_alive_secs: Some(1800),
        presence_penalty: Some(0.5),
        repetition_penalty: Some(1.2),
        ..Default::default()
    };
    let mut profile = ModelProfile::minimal("ollama", "m");
    profile.local_hints = hints.clone();
    profile.local_server = Some(LocalServer::Ollama);
    let body = request(&ENDPOINT, &chat(), Some(&profile), None, "main").unwrap();
    assert_eq!(body["keep_alive"], "1800s");
    assert_eq!(body["think"], false);
    assert_eq!(body["options"]["presence_penalty"], 0.5);
    assert_eq!(body["options"]["repeat_penalty"], 1.2);

    profile.keep_alive = Some("30m".into());
    let body = request(&ENDPOINT, &chat(), Some(&profile), None, "main").unwrap();
    assert_eq!(body["keep_alive"], "30m");

    // Hosted: no server means no hint fields, even with hints present.
    let mut hosted = ModelProfile::minimal("ollama", "m");
    hosted.local_hints = hints;
    let body = request(&ENDPOINT, &chat(), Some(&hosted), None, "main").unwrap();
    assert!(body.get("keep_alive").is_none() && body.get("think").is_none());
    assert!(body["options"].get("presence_penalty").is_none());
}

#[test]
fn ollama_final_chunk_durations_become_rates_and_load_time() {
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Http {
        status: 200,
        content_type: "application/x-ndjson",
        body: ndjson(&[
            json!({"model": "m", "message": {"role": "assistant", "content": "ok"}, "done": true,
                "done_reason": "stop", "prompt_eval_count": 100, "prompt_eval_duration": 2_000_000_000u64,
                "eval_count": 50, "eval_duration": 1_000_000_000u64, "load_duration": 500_000_000u64}),
        ]),
    }]);
    let mut pieces = Vec::new();
    let reply = native_exchange(&server.url, None, &mut pieces).unwrap();
    assert_eq!(reply.prefill_tokens_per_second, Some(50.0));
    assert_eq!(reply.decode_tokens_per_second, Some(50.0));
    assert_eq!(reply.load_secs, Some(0.5));
}

#[test]
fn native_stream_without_done_fails_and_releases_no_partial_tool_call() {
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Http {
        status: 200,
        content_type: "application/x-ndjson",
        body: ndjson(&[
            json!({"message": {"role": "assistant", "content": "partial"}, "done": false}),
            json!({"message": {"role": "assistant", "tool_calls": [
                {"function": {"name": "write_file", "arguments": {"path": "x"}}}
            ]}, "done": false}),
        ]),
    }]);
    let mut pieces = Vec::new();
    let error = native_exchange(&server.url, None, &mut pieces).unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::llm::stream::IncompleteStream>()
            .is_some(),
        "{error:#}"
    );
}
