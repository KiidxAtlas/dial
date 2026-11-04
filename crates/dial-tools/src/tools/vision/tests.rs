use super::*;
use crate::config::types::EndpointConfig;

fn endpoint(value: serde_json::Value) -> Option<EndpointConfig> {
    Some(serde_json::from_value(value).unwrap())
}

#[test]
fn local_mlx_and_openai_compatible_endpoints_are_routes() {
    let local = Config {
        provider: Some("mlx".into()),
        endpoint: None,
        ..Config::default()
    };
    assert_eq!(route("grounding", &local).unwrap(), Route::Local);
    let served = Config {
        provider: Some("lmstudio".into()),
        endpoint: endpoint(serde_json::json!({
            "base_url": "http://127.0.0.1:1234/v1",
            "protocol": "chat-completions",
        })),
        ..Config::default()
    };
    assert_eq!(
        route("grounding", &served).unwrap(),
        Route::Endpoint {
            base_url: "http://127.0.0.1:1234/v1".into(),
            api_key_env: None,
            allow_insecure: false,
        }
    );
}

#[test]
fn other_protocols_and_local_providers_are_refused() {
    let apple = Config {
        provider: Some("apple".into()),
        endpoint: None,
        ..Config::default()
    };
    assert!(route("grounding", &apple).is_err());
    let anthropic = Config {
        provider: Some("anthropic".into()),
        endpoint: endpoint(serde_json::json!({"base_url": "https://api.anthropic.com"})),
        ..Config::default()
    };
    let error = route("image_embedding", &anthropic)
        .unwrap_err()
        .to_string();
    assert!(error.contains("OpenAI-compatible"), "{error}");
}

#[test]
fn image_mime_comes_from_magic_bytes() {
    let mut png = Vec::new();
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    assert_eq!(image_mime(&png).unwrap(), "image/png");
    assert!(image_mime(b"not an image").is_err());
}

#[test]
fn explicit_features_resolve_without_activating_unconfigured_tools() {
    let mut config = Config {
        model: "main-model".into(),
        provider: Some("openai".into()),
        endpoint: endpoint(serde_json::json!({"base_url": "https://main.example/v1"})),
        ..Config::default()
    };
    assert!(!grounding::configured(&config));
    assert!(!embedding::configured(&config));
    assert!(config.vision_selection("unknown").is_none());
    config.vision.grounding = Some(crate::llm::model_config::ModelSelection::new(
        "mlx",
        "local/grounding",
    ));
    let resolved = config.for_vision(grounding::FEATURE);
    assert_eq!(resolved.model, "local/grounding");
    assert_eq!(route(grounding::FEATURE, &resolved).unwrap(), Route::Local);
    assert!(!embedding::configured(&config));
    let mut selected = crate::llm::model_config::ModelSelection::new("custom", "image-vectors");
    selected.base_url = Some("https://images.example/v1".into());
    selected.protocol = Some(crate::llm::model_config::ApiProtocol::ChatCompletions);
    config.vision.image_embedding = Some(selected);
    let resolved = config.for_vision(embedding::FEATURE);
    assert_eq!(resolved.model, "image-vectors");
    assert!(
        matches!(route(embedding::FEATURE, &resolved).unwrap(), Route::Endpoint { base_url, .. } if base_url == "https://images.example/v1")
    );
    assert_eq!(config.model, "main-model");
    assert!(toml::from_str::<Config>("[vision]\nunknown = true").is_err());
}
