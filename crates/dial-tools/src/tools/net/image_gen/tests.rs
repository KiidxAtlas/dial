use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::thread::JoinHandle;

use base64::Engine as _;
use serde_json::{Value, json};

use super::{ImageGen, Request, TOOL, Wire, auto_tools, request_body, resolve};
use crate::config::types::{Config, EndpointConfig};
use crate::tools::integrations::ext::{Registry, Tool, ToolContext};
use crate::tools::testkit;

fn image_bytes(format: image::ImageFormat) -> Vec<u8> {
    let image = image::DynamicImage::new_rgb8(2, 2);
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}
const KEY: &str = "sk-test-key";

/// Answers exactly one HTTP request on loopback and returns the raw request text.
fn serve_once(status: u16, body: String) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            request.push_str(&line);
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let mut payload = vec![0u8; length];
        reader.read_exact(&mut payload).unwrap();
        request.push_str(&String::from_utf8_lossy(&payload));
        let mut stream = reader.into_inner();
        let response = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        request
    });
    (base, handle)
}

fn ok_body(bytes: &[u8]) -> String {
    json!({"data": [{"b64_json": base64::engine::general_purpose::STANDARD.encode(bytes)}]})
        .to_string()
}

/// A user directory holding one saved key for `base`.
fn user_dir_with_key(base: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    std::fs::write(&path, json!({ base: KEY }).to_string()).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    dir
}

fn config_for(base: &str, user_dir: &std::path::Path) -> Config {
    let mut config = Config {
        user_dir: user_dir.to_path_buf(),
        ..Config::default()
    };
    config.image.base_url = base.to_owned();
    config
}

fn run(ws: &crate::tools::Workspace, config: &Config, args: Value) -> crate::tools::ToolOutput {
    let authorize = testkit::authorize_in_workspace(ws, "image_gen");
    ImageGen.call(
        &ToolContext {
            workspace: ws,
            config,
            mutation_authorizer: Some(&authorize),
            confirm: None,
        },
        &args,
    )
}

#[test]
fn writes_decoded_png_and_sends_model_prompt_and_key() {
    let (_dir, ws) = testkit::workspace();
    let png = image_bytes(image::ImageFormat::Png);
    let (base, server) = serve_once(200, ok_body(&png));
    let user_dir = user_dir_with_key(&base);
    let config = config_for(&base, user_dir.path());

    let out = run(
        &ws,
        &config,
        json!({"prompt": "a red fox", "output": "fox.png", "size": "1536x1024"}),
    );

    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("from gpt-image-1"), "{}", out.text);
    assert_eq!(std::fs::read(ws.root().join("fox.png")).unwrap(), png);
    let request = server.join().unwrap();
    let lower = request.to_ascii_lowercase();
    assert!(
        request.starts_with("POST /v1/images/generations "),
        "{request}"
    );
    assert!(
        lower.contains(&format!("authorization: bearer {KEY}")),
        "{request}"
    );
    let (_, payload) = request.split_once("\r\n\r\n").expect("request body");
    let body: Value = serde_json::from_str(payload).unwrap();
    assert_eq!(body["prompt"], "a red fox", "{request}");
    assert_eq!(body["size"], "1536x1024", "{request}");
    assert_eq!(body["model"], "gpt-image-1", "{request}");
    assert!(body.get("response_format").is_none(), "{request}");
}

#[test]
fn rejects_extension_that_does_not_match_returned_format() {
    let (_dir, ws) = testkit::workspace();
    let (base, server) = serve_once(200, ok_body(&image_bytes(image::ImageFormat::Jpeg)));
    let user_dir = user_dir_with_key(&base);
    let config = config_for(&base, user_dir.path());

    let out = run(
        &ws,
        &config,
        json!({"prompt": "a lake", "output": "lake.png"}),
    );

    assert!(out.is_error);
    assert!(out.text.contains(".jpg"), "{}", out.text);
    assert!(!ws.root().join("lake.png").exists());
    server.join().unwrap();
}

#[test]
fn refuses_to_overwrite_existing_file_before_calling_the_api() {
    let (_dir, ws) = testkit::workspace();
    std::fs::write(ws.root().join("keep.png"), b"original").unwrap();
    let config = config_for("http://127.0.0.1:9/v1", ws.root());

    let out = run(
        &ws,
        &config,
        json!({"prompt": "a tree", "output": "keep.png"}),
    );

    assert!(out.is_error);
    assert!(out.text.contains("already exists"), "{}", out.text);
    assert_eq!(
        std::fs::read(ws.root().join("keep.png")).unwrap(),
        b"original"
    );
}

#[test]
fn rejects_output_outside_the_workspace_before_calling_the_api() {
    let (_dir, ws) = testkit::workspace();
    let config = config_for("http://127.0.0.1:9/v1", ws.root());

    let out = run(
        &ws,
        &config,
        json!({"prompt": "a tree", "output": "../escape.png"}),
    );

    assert!(out.is_error);
    assert!(!ws.root().parent().unwrap().join("escape.png").exists());
}

#[test]
fn rejects_unknown_size_and_empty_prompt_without_a_request() {
    let (_dir, ws) = testkit::workspace();
    let config = config_for("http://127.0.0.1:9/v1", ws.root());

    let bad_size = run(
        &ws,
        &config,
        json!({"prompt": "a tree", "output": "a.png", "size": "512x512"}),
    );
    assert!(bad_size.is_error);
    assert!(
        bad_size.text.contains("size must be one of"),
        "{}",
        bad_size.text
    );

    let empty = run(&ws, &config, json!({"prompt": "   ", "output": "a.png"}));
    assert!(empty.is_error);
    assert!(!ws.root().join("a.png").exists());
}

#[test]
fn reports_api_error_message_and_writes_nothing() {
    let (_dir, ws) = testkit::workspace();
    let body = json!({"error": {"message": "prompt rejected by policy"}}).to_string();
    let (base, server) = serve_once(400, body);
    let user_dir = user_dir_with_key(&base);
    let config = config_for(&base, user_dir.path());

    let out = run(&ws, &config, json!({"prompt": "x", "output": "x.png"}));

    assert!(out.is_error);
    assert!(out.text.contains("HTTP 400"), "{}", out.text);
    assert!(
        out.text.contains("prompt rejected by policy"),
        "{}",
        out.text
    );
    assert!(!ws.root().join("x.png").exists());
    server.join().unwrap();
}

#[test]
fn rejects_success_response_without_image_data() {
    let (_dir, ws) = testkit::workspace();
    let (base, server) = serve_once(200, json!({"data": [{}]}).to_string());
    let user_dir = user_dir_with_key(&base);
    let config = config_for(&base, user_dir.path());

    let out = run(&ws, &config, json!({"prompt": "x", "output": "x.png"}));

    assert!(out.is_error);
    assert!(out.text.contains("no b64_json"), "{}", out.text);
    server.join().unwrap();
}

#[test]
fn missing_key_fails_before_any_request() {
    let (_dir, ws) = testkit::workspace();
    let user_dir = tempfile::tempdir().unwrap();
    let mut config = config_for("http://127.0.0.1:9/v1", user_dir.path());
    config.image.api_key_env = Some("DIAL_IMAGE_GEN_TEST_UNSET_KEY_7F3A".into());

    let out = run(&ws, &config, json!({"prompt": "x", "output": "x.png"}));

    assert!(out.is_error);
    assert!(!ws.root().join("x.png").exists());
}

#[test]
fn always_requires_approval() {
    assert!(ImageGen.needs_approval(&json!({"prompt": "x", "output": "x.png"})));
}

const XAI_BASE: &str = "https://api.x.ai/v1";

fn xai_session(user_dir: &std::path::Path) -> Config {
    let mut config = Config {
        user_dir: user_dir.to_path_buf(),
        ..Config::default()
    };
    config.endpoint = Some(EndpointConfig {
        base_url: XAI_BASE.into(),
        api_key_env: None,
        allow_insecure: false,
        protocol: None,
        headers: None,
        account_id: None,
    });
    config
}

#[test]
fn auto_offers_the_tool_only_when_its_endpoint_has_a_key() {
    let base = "http://127.0.0.1:9/v1";
    let keyed = user_dir_with_key(base);
    let mut config = config_for(base, keyed.path());
    assert_eq!(auto_tools(&config).unwrap().len(), 1);

    config.image.auto = false;
    assert!(auto_tools(&config).unwrap().is_empty());

    let empty = tempfile::tempdir().unwrap();
    let unkeyed = config_for(base, empty.path());
    assert!(auto_tools(&unkeyed).is_err());
}

#[test]
fn registry_offers_image_gen_without_an_integrations_entry_when_keyed() {
    let base = "http://127.0.0.1:9/v1";
    let dir = tempfile::tempdir().unwrap();
    let user_dir = user_dir_with_key(base);
    let mut config = Config {
        project_root: dir.path().to_path_buf(),
        user_dir: user_dir.path().to_path_buf(),
        state_dir: dir.path().join("state"),
        integrations: Vec::new(),
        ..Config::default()
    };
    config.extensions.enabled = false;
    config.image.base_url = base.into();
    assert!(Registry::standard(&config, Vec::new()).contains("image_gen"));

    config.image.auto = false;
    assert!(!Registry::standard(&config, Vec::new()).contains("image_gen"));
}

#[test]
fn image_capable_session_provider_supplies_endpoint_model_and_key() {
    let user_dir = user_dir_with_key(XAI_BASE);
    let config = xai_session(user_dir.path());

    let target = resolve(&config).unwrap();
    assert_eq!(target.base_url, XAI_BASE);
    assert_eq!(target.model, "grok-imagine-image-2.0");
    assert_eq!(target.wire, Wire::Xai);
    assert_eq!(target.key, KEY);
    assert_eq!(auto_tools(&config).unwrap().len(), 1);

    let body = request_body(
        &target,
        &Request {
            prompt: "a fox",
            size: "1536x1024",
            output: "fox.png",
        },
    );
    assert_eq!(body["aspect_ratio"], "3:2");
    assert_eq!(body["response_format"], "b64_json");
    assert!(body.get("size").is_none(), "{body}");
}

#[test]
fn customized_image_settings_win_over_the_session_provider() {
    let base = "http://127.0.0.1:9/v1";
    let user_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        user_dir.path().join("credentials.json"),
        json!({ base: KEY, XAI_BASE: "xai-key" }).to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        user_dir.path().join("credentials.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut config = xai_session(user_dir.path());
    config.image.base_url = base.into();

    let target = resolve(&config).unwrap();
    assert_eq!(target.base_url, base);
    assert_eq!(target.model, "gpt-image-1");
    assert_eq!(target.wire, Wire::OpenAi);
    assert_eq!(target.key, KEY);
}

#[test]
fn explicit_image_credentials_prevent_session_inheritance() {
    let base = "https://api.openai.com/v1";
    let user_dir = user_dir_with_key(base);
    std::fs::write(
        user_dir.path().join("credentials.json"),
        json!({base: KEY, XAI_BASE: "session-key"}).to_string(),
    )
    .unwrap();
    let mut config = xai_session(user_dir.path());
    config.image.api_key_env = Some("DIAL_IMAGE_GEN_TEST_UNSET_KEY_7F3A".into());
    let target = resolve(&config).unwrap();
    assert_eq!(target.base_url, base);
    assert_eq!(target.key, KEY);
}

#[test]
fn rejects_invalid_destinations_before_network() {
    let (_dir, ws) = testkit::workspace();
    let user_dir = user_dir_with_key("http://127.0.0.1:9/v1");
    let config = config_for("http://127.0.0.1:9/v1", user_dir.path());
    for (output, diagnostic) in [("dog.txt", "extension"), ("missing/dog.png", "destination")] {
        let out = run(&ws, &config, json!({"prompt": "dog", "output": output}));
        assert!(out.is_error);
        assert!(out.text.contains(diagnostic), "{}", out.text);
        assert!(!ws.root().join(output).exists());
    }
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 0);
}

#[test]
fn rejects_corrupt_images_without_leaving_files() {
    for bytes in [
        b"\x89PNG\r\n\x1a\n\x01\x02\x03\x04".as_slice(),
        b"\xff\xd8\xff\xe0\x09\x09\x09".as_slice(),
    ] {
        let (_dir, ws) = testkit::workspace();
        let (base, server) = serve_once(200, ok_body(bytes));
        let user_dir = user_dir_with_key(&base);
        let config = config_for(&base, user_dir.path());
        let out = run(&ws, &config, json!({"prompt": "dog", "output": "dog.png"}));
        server.join().unwrap();
        assert!(out.is_error, "{}", out.text);
        assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 0);
    }
}

#[test]
fn registry_explains_why_auto_registration_failed() {
    let (_dir, ws) = testkit::workspace();
    let user_dir = tempfile::tempdir().unwrap();
    let mut config = config_for("http://127.0.0.1:9/v1", user_dir.path());
    config.project_root = ws.root().to_path_buf();
    config.state_dir = ws.root().join("state");
    config.integrations.clear();
    config.extensions.enabled = false;
    let mut registry = Registry::standard(&config, Vec::new());
    assert!(!registry.contains("image_gen"));
    assert!(
        registry
            .take_notices()
            .iter()
            .any(|notice| notice.contains("image_gen"))
    );
}

#[test]
fn writes_valid_images_in_each_supported_format() {
    for (format, output) in [
        (image::ImageFormat::Png, "dog.png"),
        (image::ImageFormat::Jpeg, "dog.jpg"),
        (image::ImageFormat::WebP, "dog.webp"),
    ] {
        let (_dir, ws) = testkit::workspace();
        let bytes = image_bytes(format);
        let (base, server) = serve_once(200, ok_body(&bytes));
        let user_dir = user_dir_with_key(&base);
        let config = config_for(&base, user_dir.path());
        let out = run(&ws, &config, json!({"prompt": "dog", "output": output}));
        server.join().unwrap();
        assert!(!out.is_error, "{}", out.text);
        let saved = std::fs::read(ws.root().join(output)).unwrap();
        assert_eq!(saved, bytes);
        assert!(image::load_from_memory(&saved).is_ok());
        assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 1);
    }
}

#[test]
fn rejects_images_exceeding_decode_limits() {
    let (_dir, ws) = testkit::workspace();
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(16_385, 1)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let (base, server) = serve_once(200, ok_body(bytes.get_ref()));
    let user_dir = user_dir_with_key(&base);
    let config = config_for(&base, user_dir.path());
    let out = run(&ws, &config, json!({"prompt": "dog", "output": "dog.png"}));
    server.join().unwrap();
    assert!(out.is_error, "{}", out.text);
    assert!(out.text.contains("decoding"), "{}", out.text);
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 0);
}

fn selected_config(
    base: &str,
    dir: &std::path::Path,
    api: crate::llm::profiles::ImageGenerationApi,
) -> Config {
    let mut config = xai_session(dir);
    config.endpoint.as_mut().unwrap().base_url = base.into();
    config.endpoint.as_mut().unwrap().protocol =
        Some(crate::llm::model_config::ApiProtocol::ChatCompletions);
    config.model = "selected-image-model".into();
    let selection = config.selection();
    let mut profile =
        crate::llm::profiles::ModelProfile::minimal(&selection.provider, &selection.model);
    profile.capabilities.image_generation = Some(api);
    config.model_profiles.insert("selected".into(), profile);
    config.model_profile = Some("selected".into());
    config
}

#[test]
fn selected_image_model_uses_its_own_model_and_endpoint() {
    use crate::llm::profiles::ImageGenerationApi;
    let (_dir, ws) = testkit::workspace();
    let png = image_bytes(image::ImageFormat::Png);
    let (base, server) = serve_once(200, ok_body(&png));
    let user = user_dir_with_key(&base);
    let config = selected_config(&base, user.path(), ImageGenerationApi::Images);
    assert_eq!(auto_tools(&config).unwrap().len(), 1);
    let out = run(&ws, &config, json!({"prompt":"dog", "output":"dog.png"}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(std::fs::read(ws.root().join("dog.png")).unwrap(), png);
    let request = server.join().unwrap();
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["model"], "selected-image-model");
}

#[test]
fn selected_chat_model_writes_inline_image_without_fetching_urls() {
    use crate::llm::profiles::ImageGenerationApi;
    let (_dir, ws) = testkit::workspace();
    let png = image_bytes(image::ImageFormat::Png);
    let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
    let body = json!({"choices":[{"finish_reason":"stop", "message":{"images":[{"image_url":{"url":format!("data:image/png;base64,{encoded}")}}]}}]}).to_string();
    let (base, server) = serve_once(200, body);
    let user = user_dir_with_key(&base);
    let config = selected_config(&base, user.path(), ImageGenerationApi::ChatCompletions);
    let out = run(&ws, &config, json!({"prompt":"dog", "output":"dog.png"}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(std::fs::read(ws.root().join("dog.png")).unwrap(), png);
    let request = server.join().unwrap();
    assert!(
        request.starts_with("POST /v1/chat/completions "),
        "{request}"
    );
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["model"], "selected-image-model");
    assert_eq!(body["modalities"], json!(["image", "text"]));
    assert!(super::selected::chat_image(&json!({"choices":[{"finish_reason":"stop", "message":{"images":[{"image_url":{"url":"https://example.com/dog.png"}}]}}]})).is_err());
}

#[test]
fn image_overrides_win_and_vision_alone_does_not_select_the_chat_model() {
    use crate::llm::profiles::ImageGenerationApi;
    let user = user_dir_with_key(XAI_BASE);
    let mut config = selected_config(XAI_BASE, user.path(), ImageGenerationApi::Responses);
    assert_eq!(resolve(&config).unwrap().model, "selected-image-model");
    config
        .model_profiles
        .get_mut("selected")
        .unwrap()
        .capabilities
        .image_generation = None;
    config
        .model_profiles
        .get_mut("selected")
        .unwrap()
        .capabilities
        .vision = Some(true);
    assert_eq!(resolve(&config).unwrap().model, "grok-imagine-image-2.0");
    config
        .model_profiles
        .get_mut("selected")
        .unwrap()
        .capabilities
        .image_generation = Some(ImageGenerationApi::Responses);
    config.image.model = "explicit-image-model".into();
    config.image.base_url = XAI_BASE.into();
    assert_eq!(resolve(&config).unwrap().model, "explicit-image-model");
}

#[test]
fn selected_model_switch_is_resolved_for_each_generation() {
    use crate::llm::profiles::ImageGenerationApi;
    let user = user_dir_with_key(XAI_BASE);
    let mut config = selected_config(XAI_BASE, user.path(), ImageGenerationApi::Responses);
    assert_eq!(resolve(&config).unwrap().model, "selected-image-model");
    config.model = "different-model".into();
    assert_eq!(resolve(&config).unwrap().model, "grok-imagine-image-2.0");
}

#[test]
fn responses_body_selects_the_user_model_and_forces_image_generation() {
    use crate::llm::profiles::ImageGenerationApi;
    let user = user_dir_with_key(XAI_BASE);
    let config = selected_config(XAI_BASE, user.path(), ImageGenerationApi::Responses);
    let target = resolve(&config).unwrap();
    let body = request_body(
        &target,
        &Request {
            prompt: "dog",
            size: "1024x1536",
            output: "dog.png",
        },
    );
    assert_eq!(body["model"], "selected-image-model");
    assert_eq!(body["tools"][0]["type"], "image_generation");
    assert_eq!(body["tools"][0]["size"], "1024x1536");
    assert_eq!(body["tool_choice"]["type"], "image_generation");
    assert_eq!(body["store"], false);
}

#[test]
fn hosted_image_format_matches_destination_before_spending_quota() {
    use crate::llm::profiles::ImageGenerationApi;
    let user = user_dir_with_key(XAI_BASE);
    let config = selected_config(XAI_BASE, user.path(), ImageGenerationApi::Responses);
    let target = resolve(&config).unwrap();
    for (output, format) in [
        ("dog.png", "png"),
        ("dog.jpg", "jpeg"),
        ("dog.jpeg", "jpeg"),
        ("dog.webp", "webp"),
    ] {
        let body = request_body(
            &target,
            &Request {
                prompt: "dog",
                output,
                size: "auto",
            },
        );
        assert_eq!(body["tools"][0]["output_format"], format);
    }
}

#[test]
fn registry_refreshes_image_availability_after_model_switches() {
    use crate::llm::profiles::ImageGenerationApi;
    let user = user_dir_with_key("http://127.0.0.1:9/v1");
    let mut config = selected_config(
        "http://127.0.0.1:9/v1",
        user.path(),
        ImageGenerationApi::Responses,
    );
    let mut registry = crate::tools::integrations::ext::Registry::default();
    assert!(registry.sync_image_generation(&config).unwrap());
    assert!(registry.contains(TOOL));
    assert!(!registry.sync_image_generation(&config).unwrap());
    config.model = "vision-only".into();
    config.image.api_key_env = Some("DIAL_IMAGE_GEN_TEST_UNSET_KEY_7F3A".into());
    config.image.auto = false;
    assert!(registry.sync_image_generation(&config).unwrap());
    assert!(!registry.contains(TOOL));
    config.integrations.push(TOOL.into());
    assert!(registry.sync_image_generation(&config).unwrap());
    assert!(registry.contains(TOOL));
}

#[test]
fn selected_responses_transport_publishes_only_terminal_success() {
    use crate::llm::{
        fake::{self, Script},
        profiles::ImageGenerationApi,
    };
    let png = image_bytes(image::ImageFormat::Png);
    let image = json!({"type":"image_generation_call", "status":"completed", "result":base64::engine::general_purpose::STANDARD.encode(&png)});
    for success in [true, false] {
        let (_dir, ws) = testkit::workspace();
        let terminal = if success {
            json!({"type":"response.completed","response":{"status":"completed","output":[image.clone()]}})
        } else {
            json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[image.clone()]}})
        };
        let server = fake::serve(vec![Script::Sse(format!("data: {terminal}\n\n"))]);
        let user = user_dir_with_key(&server.url);
        let config = selected_config(&server.url, user.path(), ImageGenerationApi::Responses);
        let out = run(&ws, &config, json!({"prompt":"dog", "output":"dog.png"}));
        assert_eq!(!out.is_error, success, "{}", out.text);
        if success {
            assert_eq!(std::fs::read(ws.root().join("dog.png")).unwrap(), png);
        } else {
            assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 0);
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "no retry or billing fallback");
        let request = String::from_utf8(requests[0].clone()).unwrap();
        assert!(request.starts_with("POST /v1/responses "), "{request}");
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["model"], "selected-image-model");
    }
}

#[test]
fn selected_image_header_values_are_not_exposed_by_provider_errors() {
    use crate::llm::{
        model_config::{HeaderValue, RequestHeader},
        profiles::ImageGenerationApi,
    };
    let (_dir, ws) = testkit::workspace();
    let (base, server) = serve_once(
        400,
        json!({"error":{"message":"echoed-custom-header-value"}}).to_string(),
    );
    let user = user_dir_with_key(&base);
    let mut config = selected_config(&base, user.path(), ImageGenerationApi::Images);
    config.endpoint.as_mut().unwrap().headers = Some(vec![RequestHeader {
        name: "x-image-routing".into(),
        value: HeaderValue::Literal("echoed-custom-header-value".into()),
    }]);
    let out = run(&ws, &config, json!({"prompt":"dog", "output":"dog.png"}));
    assert!(out.is_error);
    assert!(
        !out.text.contains("echoed-custom-header-value"),
        "{}",
        out.text
    );
    assert!(
        server
            .join()
            .unwrap()
            .contains("echoed-custom-header-value")
    );
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 0);
}

#[test]
fn assigned_image_role_uses_its_model_not_main() {
    use crate::llm::{
        model_config::ModelSelection,
        profiles::{ImageGenerationApi, ModelCapabilities, ModelProfile},
    };
    let base = "https://images.example/v1";
    let user = user_dir_with_key(base);
    let mut config = Config {
        user_dir: user.path().into(),
        model: "main-chat".into(),
        ..Default::default()
    };
    config.model_roles.insert(
        "image".into(),
        ModelSelection {
            provider: "image-provider".into(),
            model: "image-model".into(),
            base_url: Some(base.into()),
            ..Default::default()
        },
    );
    config.model_profiles.insert(
        "images".into(),
        ModelProfile {
            provider: "image-provider".into(),
            model: "image-model".into(),
            capabilities: ModelCapabilities {
                image_generation: Some(ImageGenerationApi::Images),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let target = resolve(&config).unwrap();
    assert_eq!(target.model, "image-model");
    assert_eq!(target.base_url, base);
    assert_eq!(config.model, "main-chat");
}

#[test]
fn assigned_unsupported_image_model_never_falls_back_to_billed_provider() {
    let user = user_dir_with_key("https://api.openai.com/v1");
    let mut config = Config {
        user_dir: user.path().into(),
        ..Default::default()
    };
    config.model_roles.insert(
        "image".into(),
        crate::llm::model_config::ModelSelection::new("mlx", "not-an-image"),
    );
    let error = resolve(&config).err().unwrap().to_string();
    assert!(error.contains("assigned image model"), "{error}");
}

#[test]
fn native_mlx_serve_generates_png_without_a_key() {
    use crate::llm::profiles::ImageGenerationApi;
    let (_dir, ws) = testkit::workspace();
    let png = image_bytes(image::ImageFormat::Png);
    let (base, server) = serve_once(200, ok_body(&png));
    let user = tempfile::tempdir().unwrap();
    let config = selected_config(&base, user.path(), ImageGenerationApi::MlxServe);
    let out = run(&ws, &config, json!({"prompt":"dog", "output":"dog.png"}));
    let request = server.join().unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert!(request.starts_with("POST /v1/images/generations "));
    assert_eq!(std::fs::read(ws.root().join("dog.png")).unwrap(), png);
}

#[test]
fn native_mlx_serve_rejects_non_png_before_request() {
    use crate::llm::profiles::ImageGenerationApi;
    let (_dir, ws) = testkit::workspace();
    let user = tempfile::tempdir().unwrap();
    let config = selected_config(
        "http://127.0.0.1:9/v1",
        user.path(),
        ImageGenerationApi::MlxServe,
    );
    let out = run(&ws, &config, json!({"prompt":"dog", "output":"dog.webp"}));
    assert!(out.is_error);
    assert!(out.text.contains("PNG"), "{}", out.text);
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 0);
}

fn chatgpt_session(user_dir: &std::path::Path) -> Config {
    use crate::llm::{chatgpt, model_config::ModelSelection};
    let config = Config {
        user_dir: user_dir.into(),
        ..Default::default()
    };
    config.with_selection(&ModelSelection::new(chatgpt::PROVIDER, "chat-model"))
}

#[test]
fn chatgpt_unknown_metadata_offers_hosted_images_without_an_api_key() {
    use crate::llm::{chatgpt, profiles::ImageGenerationApi};
    let user = tempfile::tempdir().unwrap();
    chatgpt::test_account(user.path(), false);
    let mut config = chatgpt_session(user.path());
    assert_eq!(auto_tools(&config).unwrap().len(), 1);
    let target = resolve(&config).unwrap();
    assert_eq!(target.model, "chat-model");
    assert_eq!(target.base_url, chatgpt::BASE_URL);
    assert_eq!(target.wire, Wire::Responses);
    assert_eq!(target.account_id.as_deref(), Some("oaiapp_test"));
    assert!(target.key.is_empty());

    let mut profile = crate::llm::profiles::ModelProfile::minimal(chatgpt::PROVIDER, "chat-model");
    profile.capabilities.image_generation = Some(ImageGenerationApi::Disabled);
    config.model_profiles.insert("disabled".into(), profile);
    config
        .model_roles
        .insert("image".into(), config.selection());
    assert!(
        auto_tools(&config).is_err(),
        "explicitly disabled image roles must not fall back"
    );
}

#[test]
fn any_chat_model_can_automatically_use_a_configured_image_profile() {
    use crate::llm::profiles::{ImageGenerationApi, ModelProfile};
    let user = tempfile::tempdir().unwrap();
    let mut config = Config {
        user_dir: user.path().into(),
        model: "text-only".into(),
        ..Default::default()
    };
    let base = "http://127.0.0.1:9/v1";
    config.providers.insert(
        "pictures".into(),
        crate::llm::model_config::ProviderSpec {
            base_url: base.into(),
            ..Default::default()
        },
    );
    let mut profile = ModelProfile::minimal("pictures", "image-model");
    profile.capabilities.image_generation = Some(ImageGenerationApi::Images);
    config.model_profiles.insert("pictures".into(), profile);
    assert_eq!(auto_tools(&config).unwrap().len(), 1);
    let target = resolve(&config).unwrap();
    assert_eq!(target.model, "image-model");
    assert_eq!(target.base_url, base);
    assert_eq!(config.model, "text-only");

    config.providers.get_mut("pictures").unwrap().enabled = false;
    assert!(
        resolve(&config).is_err(),
        "disabled providers are not image candidates"
    );
}

#[test]
fn assigned_image_errors_are_reported_even_with_default_image_settings() {
    let user = tempfile::tempdir().unwrap();
    let mut config = Config {
        user_dir: user.path().into(),
        ..Default::default()
    };
    config.model_roles.insert(
        "image".into(),
        crate::llm::model_config::ModelSelection::new("mlx", "unsupported"),
    );
    assert!(auto_tools(&config).is_err());
}

#[test]
fn shared_image_profile_generates_without_changing_main() {
    use crate::llm::{
        model_config::ProviderSpec,
        profiles::{ImageGenerationApi, ModelProfile},
    };
    let (_dir, ws) = testkit::workspace();
    let png = image_bytes(image::ImageFormat::Png);
    let (base, server) = serve_once(200, ok_body(&png));
    let user = user_dir_with_key(&base);
    let mut config = Config {
        user_dir: user.path().into(),
        model: "text-only".into(),
        ..Default::default()
    };
    config.providers.insert(
        "pictures".into(),
        ProviderSpec {
            base_url: base,
            ..Default::default()
        },
    );
    let mut profile = ModelProfile::minimal("pictures", "image-model");
    profile.capabilities.image_generation = Some(ImageGenerationApi::Images);
    config.model_profiles.insert("pictures".into(), profile);
    let out = run(&ws, &config, json!({"prompt":"dog", "output":"dog.png"}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(std::fs::read(ws.root().join("dog.png")).unwrap(), png);
    let request = server.join().unwrap();
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["model"], "image-model");
    assert_eq!(config.model, "text-only");
}

#[test]
fn chatgpt_hosted_images_enforce_account_endpoint_and_auth_policy() {
    use crate::llm::{
        chatgpt,
        model_config::{AuthMethod, ProviderSpec},
    };
    let user = tempfile::tempdir().unwrap();
    let mut config = chatgpt_session(user.path());
    assert!(
        super::selected::resolve(&config).is_err(),
        "sign-in required"
    );
    chatgpt::test_account(user.path(), false);
    config.chatgpt_allowed_accounts = Some(["other-account".into()].into());
    assert!(
        super::selected::resolve(&config).is_err(),
        "private account policy enforced"
    );
    config.chatgpt_allowed_accounts = None;
    config.endpoint.as_mut().unwrap().base_url = "https://untrusted.example".into();
    assert!(
        super::selected::resolve(&config).is_err(),
        "OAuth route requires official endpoint"
    );
    config.endpoint.as_mut().unwrap().base_url = chatgpt::BASE_URL.into();
    config.providers.insert(
        chatgpt::PROVIDER.into(),
        ProviderSpec {
            auth: AuthMethod::ApiKey,
            ..Default::default()
        },
    );
    assert!(
        super::selected::resolve(&config).unwrap().is_none(),
        "API-key selection forbids OAuth"
    );
}

#[test]
fn configured_chatgpt_route_is_shared_with_local_main() {
    use crate::llm::{chatgpt, profiles::ModelProfile};
    let user = tempfile::tempdir().unwrap();
    chatgpt::test_account(user.path(), false);
    let mut config = Config {
        user_dir: user.path().into(),
        model: "local-text".into(),
        ..Default::default()
    };
    config.model_profiles.insert(
        "chatgpt".into(),
        ModelProfile::minimal(chatgpt::PROVIDER, "chat-model"),
    );
    let target = resolve(&config).unwrap();
    assert_eq!(target.model, "chat-model");
    assert_eq!(target.wire, Wire::Responses);
    assert_eq!(target.account_id.as_deref(), Some("oaiapp_test"));
    let mut registry = Registry::default();
    assert!(registry.sync_image_generation(&config).unwrap());
    assert!(registry.contains(TOOL));
    config.model = "another-text-model".into();
    assert!(!registry.sync_image_generation(&config).unwrap());
    assert!(registry.contains(TOOL));
}

#[test]
fn keyed_xai_image_default_is_shared_with_other_chat_providers() {
    use crate::llm::model_config::{AuthMethod, ProviderSpec};
    let user = user_dir_with_key(XAI_BASE);
    let mut config = Config {
        user_dir: user.path().into(),
        model: "text-only".into(),
        ..Default::default()
    };
    config.providers.insert(
        "openai".into(),
        ProviderSpec {
            enabled: false,
            ..Default::default()
        },
    );
    let target = resolve(&config).unwrap();
    assert_eq!(target.base_url, XAI_BASE);
    assert_eq!(target.model, "grok-imagine-image-2.0");
    config.providers.insert(
        "xai".into(),
        ProviderSpec {
            auth: AuthMethod::OAuth,
            ..Default::default()
        },
    );
    assert!(
        resolve(&config).is_err(),
        "do not bypass explicit authentication selection"
    );
}

#[test]
fn unavailable_automatic_main_does_not_block_a_shared_route_but_assigned_role_does() {
    use crate::llm::{
        model_config::ProviderSpec,
        profiles::{ImageGenerationApi, ModelProfile},
    };
    let user = tempfile::tempdir().unwrap();
    // No signed-in ChatGPT account: the automatic main candidate is unavailable.
    let mut config = chatgpt_session(user.path());
    config.providers.insert(
        "pictures".into(),
        ProviderSpec {
            base_url: "http://127.0.0.1:9/v1".into(),
            ..Default::default()
        },
    );
    let mut profile = ModelProfile::minimal("pictures", "image-model");
    profile.capabilities.image_generation = Some(ImageGenerationApi::Images);
    config.model_profiles.insert("pictures".into(), profile);
    assert_eq!(resolve(&config).unwrap().model, "image-model");
    config
        .model_roles
        .insert("image".into(), config.selection());
    assert!(
        resolve(&config).is_err(),
        "assigned route must not switch provider"
    );
    assert!(
        auto_tools(&config).is_err(),
        "assigned route errors remain visible"
    );
}

#[test]
fn automatic_chatgpt_profile_preserves_the_sessions_pinned_account() {
    use crate::llm::{chatgpt, profiles::ModelProfile};
    let user = tempfile::tempdir().unwrap();
    chatgpt::test_account(user.path(), false);
    let mut config = chatgpt_session(user.path());
    config.endpoint.as_mut().unwrap().account_id = Some("missing-pinned-account".into());
    config.model_profiles.insert(
        "chatgpt".into(),
        ModelProfile::minimal(chatgpt::PROVIDER, "other-model"),
    );
    assert!(
        super::configured_target(&config).is_none(),
        "must not switch to the globally active account"
    );
}
