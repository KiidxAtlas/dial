//! Loopback fixtures for ComfyUI: the configured workflow is submitted with only the prompt
//! replaced, polled through history for the returned id, and its single output downloaded from
//! `/view` without credentials. Refusals happen before any submission. No live service is used.
use super::super::{Format, Source, Wire};
use super::*;
use crate::config::image::{ComfyWorkflow, ImageModelSettings};
use crate::config::types::Config;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const KEY: &str = "sk-comfy-key";
const PROMPT_NODE: &str = "6";
const OUTPUT_NODE: &str = "9";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\ncomfy-fixture";

fn template() -> Value {
    json!({
        "4": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "model.safetensors"}},
        "6": {"class_type": "CLIPTextEncode", "inputs": {"text": "placeholder", "clip": ["4", 1]}},
        "8": {"class_type": "KSampler", "inputs": {"seed": 7, "steps": 20}},
        "9": {"class_type": "SaveImage", "inputs": {"filename_prefix": "dial", "images": ["8", 0]}}
    })
}

fn comfy_target(base: &str, workflow: Value) -> Target {
    Target {
        base_url: base.into(),
        model: "comfy-model".into(),
        wire: Wire::ComfyUi,
        key: KEY.into(),
        account_id: None,
        headers: Vec::new(),
        settings: ImageModelSettings {
            comfyui: Some(ComfyWorkflow {
                workflow,
                prompt_node: PROMPT_NODE.into(),
                output_node: OUTPUT_NODE.into(),
            }),
            ..Default::default()
        },
    }
}

fn prompt_request(prompt: &str) -> Request<'_> {
    Request {
        prompt,
        size: "auto",
        output: "out.png",
        source: None,
    }
}

fn config(timeout_secs: u64) -> Config {
    let mut config = Config::default();
    config.image.timeout_secs = timeout_secs;
    config
}

/// Runs the ComfyUI adapter once against `target`, returning the outcome and the requests seen.
fn run(target: &Target, config: &Config, cancel: Arc<AtomicBool>, prompt: &str) -> Result<Vec<u8>> {
    let client = Client::new(target, config, cancel);
    super::request(&client, target, &prompt_request(prompt))
}

fn idle_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn read_request(stream: &TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut raw = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        let end = line == "\r\n" || line.is_empty();
        raw.push_str(&line);
        if end {
            break;
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    raw.push_str(&String::from_utf8_lossy(&body));
    raw
}

fn respond(mut stream: TcpStream, status: u16, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    // A client may stop reading early when a limit trips; that is not a server failure.
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

/// Answers the scripted `(status, body)` replies in order on loopback and returns every request
/// received. A request beyond the script gets HTTP 404, so an unexpected call is visible.
fn serve(replies: Vec<(u16, Vec<u8>)>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        let mut seen = Vec::new();
        let mut last = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    seen.push(read_request(&stream));
                    let (status, body) = replies.next().unwrap_or((404, b"{}".to_vec()));
                    respond(stream, status, &body);
                    last = Instant::now();
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if last.elapsed() > Duration::from_millis(1500) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        seen
    });
    (base, handle)
}

/// A loopback address with no server; any connection attempt is recorded by `assert_no_connection`.
fn silent_listener() -> (String, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    (
        format!("http://{}", listener.local_addr().unwrap()),
        listener,
    )
}

fn assert_no_connection(listener: &TcpListener) {
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        other => panic!("unexpected connection: {:?}", other.map(|(_, peer)| peer)),
    }
}

fn json_reply(value: &Value) -> (u16, Vec<u8>) {
    (200, value.to_string().into_bytes())
}

/// Lowercased request head (request line and headers), without the body.
fn head(raw: &str) -> String {
    raw.split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn request_line(raw: &str) -> &str {
    raw.lines().next().unwrap_or_default()
}

fn request_path(raw: &str) -> &str {
    request_line(raw)
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
}

fn body_json(raw: &str) -> Value {
    let (_, payload) = raw.split_once("\r\n\r\n").expect("request body");
    serde_json::from_str(payload).expect("JSON request body")
}

fn query_pairs(raw: &str) -> Vec<(String, String)> {
    url::Url::parse(&format!("http://127.0.0.1{}", request_path(raw)))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

fn is_view(raw: &str) -> bool {
    request_line(raw).starts_with("GET /view?")
}

#[test]
fn submits_the_substituted_template_polls_history_and_downloads_the_single_output() {
    let (base, server) = serve(vec![
        json_reply(&json!({"prompt_id": "abc-123"})),
        json_reply(&json!({})),
        json_reply(&json!({
            "abc-123": {
                "status": {"status_str": "success"},
                "outputs": {"9": {"images": [{
                    "filename": "dial 0 & x.png",
                    "subfolder": "my dir",
                    "type": "output"
                }]}}
            }
        })),
        (200, PNG.to_vec()),
    ]);
    let target = comfy_target(&base, template());
    let config = config(30);
    let bytes = run(&target, &config, idle_flag(), "a fox in snow").unwrap();
    assert_eq!(bytes, PNG, "the downloaded bytes are returned unchanged");

    let seen = server.join().unwrap();
    assert_eq!(seen.len(), 4, "{seen:?}");

    // Submission: only the prompt node's text changes; every other node is the template.
    assert!(
        request_line(&seen[0]).starts_with("POST /prompt "),
        "{}",
        seen[0]
    );
    let mut expected = template();
    expected["6"]["inputs"]["text"] = json!("a fox in snow");
    assert_eq!(body_json(&seen[0])["prompt"], expected);
    assert!(
        head(&seen[0]).contains("authorization: bearer sk-comfy-key"),
        "{}",
        seen[0]
    );

    // Polling uses the returned id, with the same service credential.
    for poll in [&seen[1], &seen[2]] {
        assert_eq!(
            request_line(poll),
            "GET /history/abc-123 HTTP/1.1",
            "{poll}"
        );
        assert!(
            head(poll).contains("authorization: bearer sk-comfy-key"),
            "{poll}"
        );
    }

    // The output is fetched from /view with URL-encoded filename, subfolder and type, and no
    // service credential or custom header.
    assert!(is_view(&seen[3]), "{}", seen[3]);
    assert_eq!(
        query_pairs(&seen[3]),
        vec![
            ("filename".to_owned(), "dial 0 & x.png".to_owned()),
            ("subfolder".to_owned(), "my dir".to_owned()),
            ("type".to_owned(), "output".to_owned()),
        ]
    );
    assert!(!head(&seen[3]).contains("authorization:"), "{}", seen[3]);

    // The configured template is not modified by a request.
    let configured = target.settings.comfyui.as_ref().unwrap();
    assert_eq!(configured.workflow, template());
}

#[test]
fn temp_outputs_are_downloaded_from_the_temp_folder() {
    let (base, server) = serve(vec![
        json_reply(&json!({"prompt_id": "tmp-1"})),
        json_reply(&json!({"tmp-1": {
            "status": {"status_str": "success"},
            "outputs": {"9": {"images": [{"filename": "preview.png", "type": "temp"}]}}
        }})),
        (200, PNG.to_vec()),
    ]);
    let target = comfy_target(&base, template());
    let config = config(30);
    run(&target, &config, idle_flag(), "a lake").unwrap();
    let seen = server.join().unwrap();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(
        query_pairs(&seen[2]),
        vec![
            ("filename".to_owned(), "preview.png".to_owned()),
            ("subfolder".to_owned(), String::new()),
            ("type".to_owned(), "temp".to_owned()),
        ]
    );
}

#[test]
fn empty_node_errors_do_not_block_the_run() {
    let (base, server) = serve(vec![
        json_reply(&json!({"prompt_id": "ok-1", "node_errors": {}})),
        json_reply(&json!({"ok-1": {
            "status": {"status_str": "success"},
            "outputs": {"9": {"images": [{"filename": "a.png", "subfolder": "", "type": "output"}]}}
        }})),
        (200, PNG.to_vec()),
    ]);
    let target = comfy_target(&base, template());
    let config = config(30);
    assert_eq!(run(&target, &config, idle_flag(), "a cat").unwrap(), PNG);
    server.join().unwrap();
}

#[test]
fn invalid_workflows_are_refused_before_any_submission() {
    let mut missing_prompt = template();
    missing_prompt.as_object_mut().unwrap().remove("6");
    let mut prompt_without_text = template();
    prompt_without_text["6"]["inputs"] = json!({"clip": ["4", 1]});
    let mut non_string_text = template();
    non_string_text["6"]["inputs"]["text"] = json!(5);
    let cases: Vec<(&str, Value, &str, &str)> = vec![
        (
            "missing prompt node",
            missing_prompt,
            "6",
            "prompt_node must identify",
        ),
        (
            "prompt node without inputs.text",
            prompt_without_text,
            "6",
            "prompt_node must identify",
        ),
        (
            "non-string prompt text",
            non_string_text,
            "6",
            "must be a string",
        ),
        (
            "workflow is not an object",
            json!([]),
            "6",
            "prompt_node must identify",
        ),
    ];
    for (name, workflow, _, expected) in cases {
        let (base, listener) = silent_listener();
        let target = comfy_target(&base, workflow);
        let error = run(&target, &config(5), idle_flag(), "a fox").unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{name}: {error:#}");
        assert_no_connection(&listener);
    }

    // The output node must exist in the workflow.
    let (base, listener) = silent_listener();
    let mut target = comfy_target(&base, template());
    target.settings.comfyui.as_mut().unwrap().output_node = "missing".into();
    let error = run(&target, &config(5), idle_flag(), "a fox").unwrap_err();
    assert!(
        format!("{error:#}").contains("output_node is not in the workflow"),
        "{error:#}"
    );
    assert_no_connection(&listener);
}

#[test]
fn unconfigured_comfy_settings_are_refused_before_any_submission() {
    let (base, listener) = silent_listener();
    let mut target = comfy_target(&base, template());
    target.settings.comfyui = None;
    let error = run(&target, &config(5), idle_flag(), "a fox").unwrap_err();
    assert!(
        format!("{error:#}").contains("requires image.models"),
        "{error:#}"
    );
    assert_no_connection(&listener);
}

#[test]
fn explicit_sizes_and_image_edits_are_refused_before_any_submission() {
    let (base, listener) = silent_listener();
    let target = comfy_target(&base, template());
    let config = config(5);
    let client = Client::new(&target, &config, idle_flag());

    let sized = Request {
        size: "1024x1024",
        ..prompt_request("a fox")
    };
    let error = super::request(&client, &target, &sized).unwrap_err();
    assert!(
        format!("{error:#}").contains("dimensions are controlled by its workflow"),
        "{error:#}"
    );

    let edit = Request {
        source: Some(Source {
            relative: "fox.png".into(),
            bytes: PNG.to_vec(),
            format: Format::Png,
        }),
        ..prompt_request("a fox")
    };
    let error = super::request(&client, &target, &edit).unwrap_err();
    assert!(
        format!("{error:#}").contains("image-upload workflow"),
        "{error:#}"
    );

    assert_no_connection(&listener);
}

#[test]
fn service_rejected_nodes_stop_before_polling() {
    let (base, server) = serve(vec![json_reply(&json!({
        "prompt_id": "rej-1",
        "node_errors": {"6": {"errors": [{"type": "unknown_node"}]}}
    }))]);
    let target = comfy_target(&base, template());
    let config = config(30);
    let error = run(&target, &config, idle_flag(), "a fox").unwrap_err();
    assert!(
        format!("{error:#}").contains("rejected workflow nodes"),
        "{error:#}"
    );
    let seen = server.join().unwrap();
    assert_eq!(
        seen.len(),
        1,
        "no history poll after a rejected submission: {seen:?}"
    );
}

#[test]
fn invalid_prompt_ids_are_refused_before_polling() {
    let cases = [
        json!({}),
        json!({"prompt_id": ""}),
        json!({"prompt_id": "../history"}),
        json!({"prompt_id": "a b"}),
    ];
    for submitted in cases {
        let (base, server) = serve(vec![json_reply(&submitted)]);
        let target = comfy_target(&base, template());
        let config = config(30);
        assert!(
            run(&target, &config, idle_flag(), "a fox").is_err(),
            "{submitted}"
        );
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 1, "{submitted}: {seen:?}");
    }
}

#[test]
fn history_errors_are_reported_and_nothing_is_downloaded() {
    let (base, server) = serve(vec![
        json_reply(&json!({"prompt_id": "err-1"})),
        json_reply(&json!({"err-1": {
            "status": {"status_str": "error"},
            "outputs": {"9": {"images": [{"filename": "a.png", "type": "output"}]}}
        }})),
    ]);
    let target = comfy_target(&base, template());
    let config = config(30);
    let error = run(&target, &config, idle_flag(), "a fox").unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("ComfyUI workflow failed"), "{text}");
    assert!(
        text.contains("stopping the wait does not cancel the remote workflow"),
        "{text}"
    );
    let seen = server.join().unwrap();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(
        !seen.iter().any(|raw| is_view(raw)),
        "a failed run must not download: {seen:?}"
    );
}

#[test]
fn output_node_must_produce_exactly_one_well_formed_image() {
    let cases: Vec<(&str, Value, &str)> = vec![
        ("no outputs for the node", json!({}), "produced no images"),
        (
            "no images array",
            json!({"images": "a.png"}),
            "produced no images",
        ),
        ("zero images", json!({"images": []}), "exactly one image"),
        (
            "two images",
            json!({"images": [
                {"filename": "a.png", "type": "output"},
                {"filename": "b.png", "type": "output"}
            ]}),
            "exactly one image",
        ),
        (
            "missing filename",
            json!({"images": [{"type": "output"}]}),
            "no filename",
        ),
        (
            "empty filename",
            json!({"images": [{"filename": "", "type": "output"}]}),
            "invalid image location",
        ),
        (
            "input folder",
            json!({"images": [{"filename": "a.png", "type": "input"}]}),
            "invalid image location",
        ),
        (
            "missing folder type",
            json!({"images": [{"filename": "a.png"}]}),
            "no folder type",
        ),
    ];
    for (name, outputs, expected) in cases {
        let (base, server) = serve(vec![
            json_reply(&json!({"prompt_id": "out-1"})),
            json_reply(&json!({"out-1": {
                "status": {"status_str": "success"},
                "outputs": {"9": outputs}
            }})),
            (200, PNG.to_vec()),
        ]);
        let target = comfy_target(&base, template());
        let config = config(30);
        let error = run(&target, &config, idle_flag(), "a fox").unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{name}: {error:#}");
        let seen = server.join().unwrap();
        assert!(
            !seen.iter().any(|raw| is_view(raw)),
            "{name} must not download: {seen:?}"
        );
    }
}

#[test]
fn a_failed_view_download_is_not_returned_as_an_image() {
    let (base, server) = serve(vec![
        json_reply(&json!({"prompt_id": "dl-1"})),
        json_reply(&json!({"dl-1": {
            "status": {"status_str": "success"},
            "outputs": {"9": {"images": [{"filename": "a.png", "type": "output"}]}}
        }})),
        (500, b"server failure".to_vec()),
    ]);
    let target = comfy_target(&base, template());
    let config = config(30);
    let error = run(&target, &config, idle_flag(), "a fox").unwrap_err();
    assert!(
        format!("{error:#}").contains("image download failed"),
        "{error:#}"
    );
    server.join().unwrap();
}

#[test]
fn the_total_deadline_stops_polling_without_downloading() {
    let mut replies = vec![json_reply(&json!({"prompt_id": "slow-1"}))];
    replies.extend((0..40).map(|_| json_reply(&json!({}))));
    let (base, server) = serve(replies);
    let target = comfy_target(&base, template());
    let config = config(1);
    let started = Instant::now();
    let error = run(&target, &config, idle_flag(), "a fox").unwrap_err();
    assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    let seen = server.join().unwrap();
    assert!(
        seen.len() >= 2,
        "polling must have run before the deadline: {seen:?}"
    );
    assert!(
        !seen.iter().any(|raw| is_view(raw)),
        "a timed-out run must not download: {seen:?}"
    );
}

#[test]
fn cancellation_while_polling_publishes_nothing() {
    let mut replies = vec![json_reply(&json!({"prompt_id": "cancel-1"}))];
    replies.extend((0..40).map(|_| json_reply(&json!({}))));
    let (base, server) = serve(replies);
    let target = comfy_target(&base, template());
    let config = config(0);
    let cancel = idle_flag();
    let setter = cancel.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        setter.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let error = run(&target, &config, cancel, "a fox").unwrap_err();
    trigger.join().unwrap();
    assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    let seen = server.join().unwrap();
    assert!(
        seen.iter()
            .any(|raw| request_line(raw) == "GET /history/cancel-1 HTTP/1.1"),
        "cancellation must land while polling: {seen:?}"
    );
    assert!(
        !seen.iter().any(|raw| is_view(raw)),
        "a cancelled run must not download: {seen:?}"
    );
}
