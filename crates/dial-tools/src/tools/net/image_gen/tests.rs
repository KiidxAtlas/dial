use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::thread::JoinHandle;

use base64::Engine as _;
use serde_json::{Value, json};

use super::{ImageGen, Request, Wire, auto_tools, request_body, resolve};
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
