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
