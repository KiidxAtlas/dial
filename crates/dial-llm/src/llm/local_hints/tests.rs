use super::*;
use serde_json::json;

fn all() -> LocalHints {
    LocalHints {
        thinking: Some(false),
        keep_alive_secs: Some(1800),
        cache_slot: Some(3),
        prompt_progress: true,
        presence_penalty: Some(0.5),
        repetition_penalty: Some(1.2),
    }
}

#[test]
fn llama_cpp_wire_uses_completion_fields_and_merges_template_kwargs() {
    let mut body = json!({"model":"m","chat_template_kwargs":{"other":1}});
    all().apply_openai(LocalServer::LlamaCpp, &mut body);
    assert_eq!(body["cache_prompt"], true);
    assert_eq!(body["id_slot"], 3);
    assert_eq!(body["return_progress"], true);
    assert_eq!(
        body["chat_template_kwargs"],
        json!({"other":1,"enable_thinking":false})
    );
    assert_eq!(body["presence_penalty"], 0.5);
    assert_eq!(body["repeat_penalty"], 1.2);
    assert!(body.get("ttl").is_none() && body.get("think").is_none());
}

#[test]
fn mlx_runtime_wire_uses_template_kwargs_and_mlx_penalty_names_only() {
    let mut body = json!({"model":"m"});
    all().apply_openai(LocalServer::MlxRuntime, &mut body);
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(body["repetition_penalty"], 1.2);
    assert_eq!(body["presence_penalty"], 0.5);
    assert!(body.get("repeat_penalty").is_none());
    assert!(body.get("cache_prompt").is_none() && body.get("ttl").is_none());
}

#[test]
fn lm_studio_wire_uses_documented_ttl_and_penalties_without_thinking() {
    let mut body = json!({"model":"m"});
    all().apply_openai(LocalServer::LmStudio, &mut body);
    assert_eq!(body["ttl"], 1800);
    assert_eq!(body["repeat_penalty"], 1.2);
    assert_eq!(body["presence_penalty"], 0.5);
    assert!(body.get("chat_template_kwargs").is_none());
    assert!(body.get("think").is_none() && body.get("cache_prompt").is_none());
}

#[test]
fn ollama_wire_puts_keep_alive_and_think_on_body_and_penalties_in_options() {
    let mut body = json!({"model":"m","options":{}});
    let mut options = body["options"].take();
    all().apply_ollama(&mut body, &mut options);
    body["options"] = options;
    assert_eq!(body["keep_alive"], "1800s");
    assert_eq!(body["think"], false);
    assert_eq!(body["options"]["repeat_penalty"], 1.2);
    assert_eq!(body["options"]["presence_penalty"], 0.5);
    assert!(body.get("ttl").is_none() && body.get("chat_template_kwargs").is_none());
}

#[test]
fn existing_body_fields_win_over_hints_on_every_server() {
    let mut body = json!({"ttl":60,"repeat_penalty":1.0,"presence_penalty":0.1,"cache_prompt":false,
        "chat_template_kwargs":{"enable_thinking":true}});
    all().apply_openai(LocalServer::LlamaCpp, &mut body);
    assert_eq!(body["cache_prompt"], false);
    assert_eq!(body["repeat_penalty"], 1.0);
    assert_eq!(body["presence_penalty"], 0.1);
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);

    let mut body = json!({"keep_alive":"5m","think":true,"options":{"repeat_penalty":1.05}});
    let mut options = body["options"].take();
    all().apply_ollama(&mut body, &mut options);
    body["options"] = options;
    assert_eq!(body["keep_alive"], "5m");
    assert_eq!(body["think"], true);
    assert_eq!(body["options"]["repeat_penalty"], 1.05);
    assert_eq!(body["options"]["presence_penalty"], 0.5);
}

#[test]
fn ollama_and_other_servers_send_nothing_through_the_openai_path() {
    for server in [LocalServer::Ollama, LocalServer::Other] {
        let mut body = json!({"model":"m"});
        all().apply_openai(server, &mut body);
        assert_eq!(body, json!({"model":"m"}));
    }
}

#[test]
fn next_hints_override_base_per_field_and_explicit_profile_fields_drop_hints() {
    let base = LocalHints {
        keep_alive_secs: Some(60),
        cache_slot: Some(1),
        ..Default::default()
    };
    let next = LocalHints {
        cache_slot: Some(2),
        thinking: Some(true),
        prompt_progress: true,
        ..Default::default()
    };
    let merged = next.over(&base);
    assert_eq!(merged.keep_alive_secs, Some(60));
    assert_eq!(merged.cache_slot, Some(2));
    assert_eq!(merged.thinking, Some(true));
    assert!(merged.prompt_progress);
    let kept = merged.clone().without_explicit(true, true);
    assert_eq!(kept.thinking, None);
    assert_eq!(kept.keep_alive_secs, None);
    assert_eq!(kept.cache_slot, Some(2));
    assert!(LocalHints::default().is_empty() && !merged.is_empty());
}

#[test]
fn detect_never_treats_non_local_routes_as_local_servers() {
    for provider in ["lm-studio", "ollama", "llama-cpp", "mlx", "openai"] {
        assert_eq!(
            LocalServer::detect(provider, "m", Some("https://api.example.com/v1"), None, &[]),
            LocalServer::Other,
            "{provider}"
        );
    }
    assert_eq!(
        LocalServer::detect(
            "lm-studio",
            "m",
            Some("http://127.0.0.1:1234/v1"),
            None,
            &[]
        ),
        LocalServer::LmStudio
    );
    assert_eq!(
        LocalServer::detect("ollama", "m", Some("http://localhost:11434"), None, &[]),
        LocalServer::Ollama
    );
    assert_eq!(
        LocalServer::detect(
            "llama-cpp",
            "m",
            Some("http://127.0.0.1:8080/v1"),
            None,
            &[]
        ),
        LocalServer::LlamaCpp
    );
    assert_eq!(
        LocalServer::detect("mlx", "/nonexistent/model-dir", None, None, &[]),
        LocalServer::MlxRuntime
    );
}
