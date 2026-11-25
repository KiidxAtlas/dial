//! Run with `cargo test --lib provider::real_server -- --ignored --nocapture`.
use super::*;
use serde_json::json;
use std::time::Instant;

fn python() -> PathBuf {
    std::env::var_os("DIAL_TEST_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/.venv/bin/python")
        })
}

fn snapshot(repo_dir: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let snapshots = Path::new(&home)
        .join(".cache/huggingface/hub")
        .join(repo_dir)
        .join("snapshots");
    let entry = std::fs::read_dir(snapshots).ok()?.next()?.ok()?;
    entry.path().to_str().map(str::to_string)
}

fn ask(content: &str, tools: Vec<serde_json::Value>, max_tokens: u32) -> Chat {
    Chat {
        messages: vec![json!({"role": "user", "content": content})],
        tools,
        temperature: 0.0,
        max_tokens,
        seed: Some(1),
    }
}

#[test]
#[ignore = "requires a real single-file chat GGUF and llama-server on PATH or DIAL_LLAMA_SERVER"]
fn prepared_gguf_generates_on_the_same_resident_server() {
    let model_dir = std::env::var("DIAL_TEST_GGUF").expect("path to local GGUF");
    let mut backend = Backend::new(ProviderKind::Local { model_dir }, python(), 120);
    backend
        .prepare_local_model(Arc::new(AtomicBool::new(false)))
        .unwrap();
    let origin = backend.server.as_ref().unwrap().base_url().to_owned();
    let reply = backend
        .chat(None, &ask("Tell me a story.", vec![], 32), 60, None)
        .unwrap();
    assert!(
        !reply.text.trim().is_empty(),
        "no generated text: {reply:?}"
    );
    assert_eq!(backend.server.as_ref().unwrap().base_url(), origin);
}

#[test]
#[ignore = "loads a downloaded vision model; set DIAL_TEST_VISION_MODEL and DIAL_TEST_VISION_IMAGE (a red square)"]
fn sees_images_and_streams_tools_from_the_real_vlm_server() {
    let model = std::env::var("DIAL_TEST_VISION_MODEL").expect("downloaded vision model path");
    let image =
        PathBuf::from(std::env::var("DIAL_TEST_VISION_IMAGE").expect("red-square PNG path"));
    let root = tempfile::tempdir().unwrap();
    let config = crate::config::types::Config {
        user_dir: root.path().join("user"),
        state_dir: root.path().join("state"),
        project_root: root.path().to_path_buf(),
        ..Default::default()
    };
    let mut backend = Backend::new(ProviderKind::Local { model_dir: model }, python(), 180)
        .with_steering(&config);
    assert!(backend.accepts_image_input());
    let mut req = ask(
        "Describe the shape and color in this image in one short sentence.",
        vec![],
        64,
    );
    req.messages[0]["dial_images"] =
        json!([llm::images::user_attachment(root.path(), &image).unwrap()]);
    let reply = backend.chat(None, &req, 90, None).unwrap();
    println!("VISION: {}", reply.text);
    assert!(
        reply.text.to_lowercase().contains("red") && reply.text.to_lowercase().contains("square")
    );
    req.messages[0]["content"] = json!(
        "Inspect the attached image and call describe_image with its color and shape. Use the tool instead of writing an answer."
    );
    req.tools = vec![json!({"type":"function","function":{
        "name":"describe_image", "description":"Record the color and shape seen in the image.",
        "parameters":{"type":"object","properties":{"color":{"type":"string"},"shape":{"type":"string"}},"required":["color","shape"]}
    }})];
    req.max_tokens = 256;
    let reply = backend.chat(None, &req, 90, Some(&mut |_| true)).unwrap();
    println!("VISION TOOL CALLS: {:?}", reply.tool_calls);
    let call = reply
        .tool_calls
        .iter()
        .find(|call| call.name == "describe_image")
        .expect("visual tool call");
    assert_eq!(call.arguments["color"], "red");
    assert_eq!(call.arguments["shape"], "square");
    backend.release();
}

#[test]
#[ignore = "loads the downloaded Inkling-2-Mini-4bit checkpoint on Apple Silicon"]
fn inkling_mini_serves_text_and_images_with_mlx_lm() {
    let model_dir =
        snapshot("models--mlx-community--Inkling-2-Mini-4bit").expect("Inkling snapshot");
    let mut backend = Backend::new(ProviderKind::Local { model_dir }, python(), 180);
    assert!(backend.accepts_image_input());
    let reply = backend
        .chat(None, &ask("Say hello.", vec![], 256), 120, None)
        .unwrap();
    assert!(!reply.text.trim().is_empty(), "no text response: {reply:?}");

    let root = tempfile::tempdir().unwrap();
    let image = root.path().join("sample.png");
    image::RgbImage::from_pixel(64, 64, image::Rgb([0, 0, 255]))
        .save(&image)
        .unwrap();
    let mut req = ask(
        "What color is this image? Answer with one word.",
        vec![],
        256,
    );
    req.messages[0]["dial_images"] =
        json!([llm::images::user_attachment(root.path(), &image).unwrap()]);
    let reply = backend.chat(None, &req, 120, Some(&mut |_| true)).unwrap();
    assert!(reply.text.to_lowercase().contains("blue"), "{reply:?}");
}

#[test]
#[ignore = "loads the downloaded Inkling-2-Mini-4bit checkpoint on Apple Silicon"]
fn inkling_mini_returns_an_offered_tool_call() {
    let model_dir =
        snapshot("models--mlx-community--Inkling-2-Mini-4bit").expect("Inkling snapshot");
    let mut backend = Backend::new(ProviderKind::Local { model_dir }, python(), 180);
    let mut req = ask(
        "Record the code word now.",
        vec![
            json!({"type":"function","function":{"name":"record_word","description":"Record one word.","parameters":{"type":"object","properties":{"word":{"type":"string"}},"required":["word"]}}}),
        ],
        1024,
    );
    req.messages.insert(0, json!({"role":"system","content":"The code word is juniper. Use record_word to record the code word when asked."}));
    let reply = backend.chat(None, &req, 120, Some(&mut |_| true)).unwrap();
    assert_eq!(
        reply.tool_calls.len(),
        1,
        "expected one call instead of an answer"
    );
    assert_eq!(reply.tool_calls[0].name, "record_word");
    assert_eq!(reply.tool_calls[0].arguments, json!({"word":"juniper"}));
    assert!(
        !reply.text_calls_lifted,
        "a native tool frame must not require recovered-text approval"
    );
    assert!(!reply.tool_calls[0].malformed_arguments);
}

#[test]
#[ignore = "starts the real mlx_lm.server; needs the SmolLM2 snapshot"]
fn streams_text_from_the_real_server() {
    let dir =
        snapshot("models--mlx-community--SmolLM2-135M-Instruct-8bit").expect("SmolLM2 snapshot");
    let mut backend = Backend::new(ProviderKind::Local { model_dir: dir }, python(), 180);
    let request = ask("Say hello in five words.", vec![], 24);

    let plain = backend.chat(None, &request, 120, None).unwrap();
    let started = Instant::now();
    let mut first = None;
    let mut pieces = Vec::new();
    let mut on_text = |piece: &str| {
        first.get_or_insert_with(|| started.elapsed());
        pieces.push(piece.to_string());
        true
    };
    let streamed = backend
        .chat(None, &request, 120, Some(&mut on_text))
        .unwrap();
    println!(
        "STREAM: {} deltas, first after {:?}, total {:?}, usage {:?}/{:?}, finish {:?}, text {:?}",
        pieces.len(),
        first,
        started.elapsed(),
        streamed.prompt_tokens,
        streamed.completion_tokens,
        streamed.finish_reason,
        streamed.text
    );
    println!(
        "PLAIN:  usage {:?}/{:?}, text {:?}",
        plain.prompt_tokens, plain.completion_tokens, plain.text
    );
    assert!(!pieces.is_empty());
    assert_eq!(pieces.concat(), streamed.text);
    assert_eq!(
        streamed.text, plain.text,
        "temperature 0 + seed: stream and plain must agree"
    );
    assert!(streamed.prompt_tokens.is_some() && streamed.completion_tokens.is_some());

    // Abandoning mid-stream must leave the server usable.
    let mut stop = |_: &str| false;
    let error = backend
        .chat(
            None,
            &ask("Count to fifty.", vec![], 200),
            120,
            Some(&mut stop),
        )
        .unwrap_err();
    assert!(
        error.downcast_ref::<llm::Interrupted>().is_some(),
        "{error:#}"
    );
    let again = backend.chat(None, &request, 120, None).unwrap();
    assert_eq!(again.text, plain.text);
    backend.release();
}

#[test]
#[ignore = "starts the real mlx_lm.server; needs the Qwen2.5-0.5B snapshot"]
fn streams_tool_calls_from_the_real_server() {
    let Some(dir) = snapshot("models--mlx-community--Qwen2.5-0.5B-Instruct-4bit") else {
        println!("Qwen2.5-0.5B snapshot not present; skipping");
        return;
    };
    let mut backend = Backend::new(ProviderKind::Local { model_dir: dir }, python(), 180);
    let tool = json!({"type": "function", "function": {
        "name": "read_file",
        "description": "Read a text file from the workspace",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
    }});
    let request = ask("Please read the file notes.txt", vec![tool], 128);
    let mut pieces = Vec::new();
    let mut on_text = |piece: &str| {
        pieces.push(piece.to_string());
        true
    };
    let started = Instant::now();
    let streamed = backend
        .chat(None, &request, 120, Some(&mut on_text))
        .unwrap();
    println!(
        "TOOL STREAM ({:?}): text deltas {pieces:?}\n  reply {streamed:?}",
        started.elapsed()
    );
    let plain = backend.chat(None, &request, 120, None).unwrap();
    println!("TOOL PLAIN: {plain:?}");
    assert_eq!(streamed.tool_calls.len(), plain.tool_calls.len());
    assert!(
        !streamed.tool_calls.is_empty(),
        "no tool call: {streamed:?}"
    );
    assert_eq!(streamed.tool_calls[0].name, "read_file");
    assert!(!streamed.tool_calls[0].malformed_arguments);
    backend.release();
}

#[test]
#[ignore = "requires running Osaurus; loads DIAL_TEST_OSAURUS_MODEL for streaming and cancellation"]
fn osaurus_live_streams_and_cancels_through_dial_backend() {
    let model = std::env::var("DIAL_TEST_OSAURUS_MODEL").expect("installed Osaurus model id");
    let base_url =
        std::env::var("OSAURUS_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:1337/v1".into());
    let profile = llm::profiles::ModelProfile::minimal("osaurus", &model);
    let mut backend = Backend::new(
        ProviderKind::Endpoint {
            base_url,
            model,
            api_key: None,
        },
        PathBuf::new(),
        90,
    )
    .with_profile(profile);
    let request = ask("Say hello in one short sentence.", vec![], 32);
    let mut text = String::new();
    let reply = backend
        .chat(
            None,
            &request,
            90,
            Some(&mut |piece| {
                text.push_str(piece);
                true
            }),
        )
        .unwrap();
    assert!(!text.trim().is_empty(), "no streamed text");
    assert_eq!(
        reply.text, text,
        "streamed deltas match the accumulated reply"
    );
    let error = backend
        .chat(None, &request, 90, Some(&mut |_| false))
        .unwrap_err();
    assert!(error.is::<llm::Interrupted>(), "{error:#}");
}
