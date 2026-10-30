use super::*;
use crate::config::image::ImageModelSettings;
use crate::config::types::Config;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

fn target(wire: Wire, model: &str, settings: ImageModelSettings) -> Target {
    Target {
        base_url: "https://example.invalid".to_owned(),
        model: model.to_owned(),
        wire,
        key: "test-key".to_owned(),
        account_id: None,
        headers: Vec::new(),
        settings,
    }
}

fn request(size: &'static str) -> Request<'static> {
    Request {
        prompt: "a lighthouse at dusk",
        size,
        output: "out.png",
        source: None,
    }
}

#[test]
fn bfl_model_names_are_single_lowercase_segments() {
    assert!(bfl_model("flux-2-pro").is_ok());
    assert!(bfl_model("flux-pro-1.1").is_ok());
    assert!(bfl_model("Flux/Pro").is_err());
    assert!(bfl_model("-flux").is_err());
    assert!(bfl_model("").is_err());
}

#[test]
fn fal_model_accepts_two_to_five_segments() {
    assert!(fal_model("fal-ai/flux/dev").is_ok());
    assert!(fal_model("fal-ai/flux-pro/v1.1").is_ok());
    assert!(fal_model("fal-ai").is_err());
    assert!(fal_model("fal-ai/../etc").is_err());
    assert!(fal_model("a/b/c/d/e/f").is_err());
}

#[test]
fn replicate_route_distinguishes_model_and_version_forms() {
    let hash = "a".repeat(64);
    assert_eq!(
        replicate_route("black-forest-labs/flux-schnell").unwrap(),
        ReplicateRoute::Model {
            owner: "black-forest-labs",
            name: "flux-schnell"
        }
    );
    assert_eq!(
        replicate_route(&format!("owner/model:{hash}")).unwrap(),
        ReplicateRoute::Version(&hash)
    );
    assert_eq!(
        replicate_route(&hash).unwrap(),
        ReplicateRoute::Version(&hash)
    );
    assert!(replicate_route("owner/model:ABC").is_err());
    assert!(replicate_route("short").is_err());
}

#[test]
fn only_flux2_models_support_explicit_dimensions() {
    assert!(model_supports_size(&target(
        Wire::BlackForestLabs,
        "flux-2-pro",
        ImageModelSettings::default()
    )));
    assert!(!model_supports_size(&target(
        Wire::BlackForestLabs,
        "flux-kontext-pro",
        ImageModelSettings::default()
    )));
    assert!(!model_supports_size(&target(
        Wire::Fal,
        "fal-ai/flux-2/dev",
        ImageModelSettings::default()
    )));
}

#[test]
fn bfl_flux2_sends_dimensions_and_seed() {
    let settings = ImageModelSettings {
        seed: Some(7),
        ..Default::default()
    };
    let body = bfl_body(
        &target(Wire::BlackForestLabs, "flux-2-pro", settings),
        &request("1536x1024"),
    )
    .unwrap();
    assert_eq!(body["width"], 1536);
    assert_eq!(body["height"], 1024);
    assert_eq!(body["seed"], 7);
    assert_eq!(body["prompt"], "a lighthouse at dusk");
}

#[test]
fn bfl_other_models_omit_dimensions_on_default_size_and_reject_explicit_size() {
    let other = target(
        Wire::BlackForestLabs,
        "flux-kontext-pro",
        ImageModelSettings::default(),
    );
    let body = bfl_body(&other, &request("auto")).unwrap();
    assert!(body.get("width").is_none());
    assert!(body.get("height").is_none());

    let err = bfl_body(&other, &request("1536x1024")).unwrap_err();
    assert!(format!("{err:#}").contains("no size field"), "{err:#}");
}

#[test]
fn fal_body_requests_one_image_and_drops_adapter_owned_input() {
    let mut settings = ImageModelSettings::default();
    settings.input.insert("steps".to_owned(), json!(28));
    settings.input.insert("n".to_owned(), json!(4));
    settings.input.insert("size".to_owned(), json!("512x512"));
    let body = fal_body(
        &target(Wire::Fal, "fal-ai/flux/dev", settings),
        &request("auto"),
    )
    .unwrap();
    assert_eq!(body["num_images"], 1);
    assert_eq!(body["steps"], 28);
    assert_eq!(body["prompt"], "a lighthouse at dusk");
    assert!(body.get("n").is_none());
    assert!(body.get("size").is_none());
}

#[test]
fn explicit_size_fails_on_fal_but_auto_is_accepted() {
    let fal = target(Wire::Fal, "fal-ai/flux/dev", ImageModelSettings::default());
    assert!(fal_body(&fal, &request("1024x1024")).is_err());
    assert!(fal_body(&fal, &request("auto")).is_ok());
}

#[test]
fn replicate_version_form_carries_version_and_model_form_does_not() {
    let hash = "b".repeat(64);
    let t = target(Wire::Replicate, &hash, ImageModelSettings::default());
    let route = ReplicateRoute::Version(&hash);
    let body = replicate_body(&t, &request("auto"), &route).unwrap();
    assert_eq!(body["version"], hash.as_str());
    assert_eq!(body["input"]["num_outputs"], 1);

    let route = ReplicateRoute::Model {
        owner: "owner",
        name: "model",
    };
    let body = replicate_body(&t, &request("auto"), &route).unwrap();
    assert!(body.get("version").is_none());
}

#[test]
fn fal_result_requires_exactly_one_image() {
    let one = json!({"images": [{"url": "https://v3.fal.media/a.png"}]});
    assert_eq!(fal_image_url(&one).unwrap(), "https://v3.fal.media/a.png");
    let two = json!({"images": [{"url": "https://v3.fal.media/a.png"}, {"url": "https://v3.fal.media/b.png"}]});
    assert!(fal_image_url(&two).is_err());
    assert!(fal_image_url(&json!({})).is_err());
}

#[test]
fn replicate_output_accepts_one_string_or_one_element_array() {
    assert_eq!(
        replicate_output(&json!("https://replicate.delivery/a.png")).unwrap(),
        "https://replicate.delivery/a.png"
    );
    assert_eq!(
        replicate_output(&json!(["https://replicate.delivery/a.png"])).unwrap(),
        "https://replicate.delivery/a.png"
    );
    assert!(replicate_output(&json!(["a", "b"])).is_err());
    assert!(replicate_output(&json!("")).is_err());
    assert!(replicate_output(&Value::Null).is_err());
}

#[test]
fn submission_errors_name_missing_handles_and_possible_remote_jobs() {
    let err = required(&json!({}), "id", "Replicate").unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("may have been accepted"), "{text}");
}

#[test]
fn interrupted_errors_name_the_remote_task_without_claiming_cancellation() {
    let text = format!("{:#}", interrupted("task-1", anyhow!("poll failed")));
    assert!(text.contains("task-1"), "{text}");
    assert!(text.contains("may still be running"), "{text}");
    assert!(text.contains("not cancelled"), "{text}");
}

#[test]
fn remote_detail_prefers_error_fields_and_is_clipped() {
    assert_eq!(remote_detail(&json!({"error": "bad prompt"})), "bad prompt");
    assert_eq!(remote_detail(&json!({})), "no detail was returned");
    let long = "x".repeat(MAX_ERROR_CHARS + 50);
    let detail = remote_detail(&json!({ "error": long }));
    assert_eq!(detail.chars().count(), MAX_ERROR_CHARS);
}

#[test]
fn output_format_follows_each_routes_documented_extensions() {
    let bfl = target(
        Wire::BlackForestLabs,
        "flux-2-pro",
        ImageModelSettings::default(),
    );
    let fal = target(Wire::Fal, "fal-ai/flux/dev", ImageModelSettings::default());
    let replicate = target(
        Wire::Replicate,
        &"c".repeat(64),
        ImageModelSettings::default(),
    );
    let with_output = |output: &'static str| Request {
        prompt: "a lighthouse at dusk",
        size: "auto",
        output,
        source: None,
    };
    for (output, expected) in [("a.png", "png"), ("a.jpeg", "jpeg"), ("a.webp", "webp")] {
        let body = bfl_body(&bfl, &with_output(output)).unwrap();
        assert_eq!(body["output_format"], expected, "BFL {output}");
    }
    for (output, expected) in [("a.png", "png"), ("a.jpg", "jpeg")] {
        let body = fal_body(&fal, &with_output(output)).unwrap();
        assert_eq!(body["output_format"], expected, "fal {output}");
    }
    let route = ReplicateRoute::Version(&"c".repeat(64));
    for (output, expected) in [("a.png", "png"), ("a.jpeg", "jpg"), ("a.webp", "webp")] {
        let body = replicate_body(&replicate, &with_output(output), &route).unwrap();
        assert_eq!(
            body["input"]["output_format"], expected,
            "Replicate {output}"
        );
    }
}

#[test]
fn fal_refuses_webp_and_unknown_extensions_before_submission() {
    let fal = target(Wire::Fal, "fal-ai/flux/dev", ImageModelSettings::default());
    for output in ["a.webp", "a.gif", "a"] {
        let req = Request {
            prompt: "p",
            size: "auto",
            output,
            source: None,
        };
        assert!(fal_body(&fal, &req).is_err(), "{output} should be refused");
    }
}

// ---- Loopback lifecycle fixtures: a bounded local server and a real `Client`. ----

/// One scripted HTTP reply.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

fn json_reply(value: Value) -> Reply {
    Reply {
        status: 200,
        content_type: "application/json",
        body: value.to_string().into_bytes(),
    }
}

fn image_reply(bytes: &[u8]) -> Reply {
    Reply {
        status: 200,
        content_type: "image/png",
        body: bytes.to_vec(),
    }
}

fn failure_reply(status: u16, text: &str) -> Reply {
    Reply {
        status,
        content_type: "text/plain",
        body: text.as_bytes().to_vec(),
    }
}

/// Reads one request head and its body from a loopback connection.
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

/// The JSON body of a recorded request, or `Value::Null` when it has none.
fn body_of(raw: &str) -> Value {
    raw.split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str(body).ok())
        .unwrap_or(Value::Null)
}

/// Answers scripted replies in order. `build` receives the server's base URL so replies can point
/// back at it. The thread stops once the script is spent and the client has been quiet, or at a
/// fixed deadline, so an extra request cannot hang the test. Returns every request received.
fn serve(build: impl FnOnce(&str) -> Vec<Reply>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let replies = build(&base);
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let total = replies.len();
        let mut replies = replies.into_iter();
        let mut seen = Vec::new();
        let mut answered = 0usize;
        let mut last = Instant::now();
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    seen.push(read_request(&stream));
                    last = Instant::now();
                    let Some(reply) = replies.next() else { break };
                    answered += 1;
                    let head = format!(
                        "HTTP/1.1 {} Test\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        reply.status,
                        reply.content_type,
                        reply.body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&reply.body);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    // Once the script is spent and the client has been quiet, stop early.
                    if answered == total && last.elapsed() > Duration::from_millis(300) {
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

fn loopback_target(base: &str, wire: Wire, model: &str) -> Target {
    Target {
        base_url: base.to_owned(),
        model: model.to_owned(),
        wire,
        key: "test-key".to_owned(),
        account_id: None,
        headers: Vec::new(),
        settings: ImageModelSettings::default(),
    }
}

/// Runs one queued request through a real `Client` against the loopback target.
fn submit_with(target: &Target, config: &Config, output: &str) -> Result<Vec<u8>> {
    let client = Client::new(target, config, Arc::new(AtomicBool::new(false)));
    super::request(
        &client,
        target,
        &Request {
            prompt: "a lighthouse at dusk",
            size: "auto",
            output,
            source: None,
        },
    )
}

#[test]
fn bfl_moderated_task_fails_on_its_first_terminal_poll() {
    let (base, server) = serve(|base| {
        vec![
            json_reply(json!({"id": "task-1", "polling_url": format!("{base}/poll")})),
            json_reply(
                json!({"status": "Content Moderated", "details": {"Moderation Reasons": ["unsafe"]}}),
            ),
        ]
    });
    let target = loopback_target(&base, Wire::BlackForestLabs, "flux-pro-1.1");
    let err = submit_with(&target, &Config::default(), "out.png").unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("Content Moderated"), "{text}");
    assert!(text.contains("task-1"), "{text}");
    assert!(text.contains("not cancelled"), "{text}");

    let seen = server.join().unwrap();
    assert_eq!(
        seen.len(),
        2,
        "one submit and one poll, no retries: {seen:?}"
    );
    assert!(seen[0].starts_with("POST "), "{}", seen[0]);
    assert!(
        seen[0].to_ascii_lowercase().contains("x-key: test-key"),
        "{}",
        seen[0]
    );
    assert!(seen[1].starts_with("GET /poll"), "{}", seen[1]);
}

#[test]
fn provider_text_is_withheld_when_custom_headers_are_attached() {
    use crate::llm::model_config::{HeaderValue, RequestHeader};
    let echoed = "echoed-custom-header-value";
    let (base, server) = serve(|base| {
        vec![
            json_reply(json!({"id": "task-h", "polling_url": format!("{base}/poll")})),
            json_reply(json!({"status": "Error", "error": format!("bad header {echoed}")})),
        ]
    });
    let mut target = loopback_target(&base, Wire::BlackForestLabs, "flux-pro-1.1");
    target.headers = vec![RequestHeader {
        name: "x-image-routing".into(),
        value: HeaderValue::Literal(echoed.into()),
    }];
    let err = submit_with(&target, &Config::default(), "out.png").unwrap_err();
    let text = format!("{err:#}");
    assert!(!text.contains(echoed), "{text}");
    assert!(text.contains("details withheld"), "{text}");
    assert!(text.contains("task-h"), "{text}");
    assert!(text.contains("not cancelled"), "{text}");
    assert!(server.join().unwrap()[0].contains(echoed));
}

#[test]
fn bfl_unknown_status_fails_instead_of_polling_forever() {
    let (base, server) = serve(|base| {
        vec![
            json_reply(json!({"id": "task-2", "polling_url": format!("{base}/poll")})),
            json_reply(json!({"status": "Surprise"})),
        ]
    });
    let target = loopback_target(&base, Wire::BlackForestLabs, "flux-pro-1.1");
    let err = submit_with(&target, &Config::default(), "out.png").unwrap_err();
    assert!(format!("{err:#}").contains("unexpected status"), "{err:#}");
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn bfl_pending_then_ready_downloads_the_sample_without_service_auth() {
    for (output, format) in [
        ("out.png", "png"),
        ("out.jpeg", "jpeg"),
        ("out.webp", "webp"),
    ] {
        let image = b"\x89PNG-bfl".to_vec();
        let (base, server) = serve(|base| {
            vec![
                json_reply(json!({"id": "bfl-1", "polling_url": format!("{base}/poll")})),
                json_reply(json!({"status": "Pending"})),
                json_reply(
                    json!({"status": "Ready", "result": {"sample": format!("{base}/sample")}}),
                ),
                image_reply(&image),
            ]
        });
        let target = loopback_target(&base, Wire::BlackForestLabs, "flux-pro-1.1");
        let bytes = submit_with(&target, &Config::default(), output)
            .unwrap_or_else(|err| panic!("{output}: {err:#}"));
        assert_eq!(bytes, image, "{output}");

        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert_eq!(body_of(&seen[0])["output_format"], format, "{output}");
        assert!(
            seen[0].to_ascii_lowercase().contains("x-key: test-key"),
            "{}",
            seen[0]
        );
        assert!(seen[1].starts_with("GET /poll") && seen[2].starts_with("GET /poll"));
        assert!(
            seen[1].to_ascii_lowercase().contains("x-key: test-key"),
            "{}",
            seen[1]
        );
        assert!(seen[3].starts_with("GET /sample "), "{}", seen[3]);
        let download = seen[3].to_ascii_lowercase();
        assert!(
            !download.contains("x-key:") && !download.contains("authorization:"),
            "{}",
            seen[3]
        );
    }
}

#[test]
fn fal_queue_completes_and_downloads_the_single_image() {
    for (output, format) in [("out.png", "png"), ("out.jpg", "jpeg")] {
        let image = b"fal-image".to_vec();
        let (base, server) = serve(|base| {
            vec![
                json_reply(json!({
                    "request_id": "fal-1",
                    "status_url": format!("{base}/status"),
                    "response_url": format!("{base}/response"),
                })),
                json_reply(json!({"status": "IN_QUEUE"})),
                json_reply(json!({"status": "IN_PROGRESS"})),
                json_reply(json!({"status": "COMPLETED"})),
                json_reply(json!({"images": [{"url": format!("{base}/image.png")}]})),
                image_reply(&image),
            ]
        });
        let target = loopback_target(&base, Wire::Fal, "fal-ai/flux/dev");
        let bytes = submit_with(&target, &Config::default(), output)
            .unwrap_or_else(|err| panic!("{output}: {err:#}"));
        assert_eq!(bytes, image, "{output}");

        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 6, "{seen:?}");
        assert!(seen[0].starts_with("POST /fal-ai/flux/dev "), "{}", seen[0]);
        assert_eq!(body_of(&seen[0])["output_format"], format, "{output}");
        assert!(
            seen[0]
                .to_ascii_lowercase()
                .contains("authorization: key test-key"),
            "{}",
            seen[0]
        );
        for status in &seen[1..4] {
            assert!(status.starts_with("GET /status "), "{status}");
            assert!(
                status
                    .to_ascii_lowercase()
                    .contains("authorization: key test-key"),
                "{status}"
            );
        }
        assert!(seen[4].starts_with("GET /response "), "{}", seen[4]);
        assert!(seen[5].starts_with("GET /image.png "), "{}", seen[5]);
        let download = seen[5].to_ascii_lowercase();
        assert!(!download.contains("authorization:"), "{}", seen[5]);
    }
}

#[test]
fn fal_refuses_webp_before_any_submission() {
    let (base, server) = serve(|_| Vec::new());
    let target = loopback_target(&base, Wire::Fal, "fal-ai/flux/dev");
    let err = submit_with(&target, &Config::default(), "out.webp").unwrap_err();
    assert!(format!("{err:#}").contains("does not support"), "{err:#}");
    assert!(
        server.join().unwrap().is_empty(),
        "no request may reach the service"
    );
}

#[test]
fn replicate_polls_until_succeeded_and_downloads_the_output() {
    let hash = "d".repeat(64);
    for (output, format) in [
        ("out.png", "png"),
        ("out.jpeg", "jpg"),
        ("out.webp", "webp"),
    ] {
        let image = b"replicate-image".to_vec();
        let (base, server) = serve(|base| {
            vec![
                json_reply(
                    json!({"id": "rep-1", "urls": {"get": format!("{base}/predictions/rep-1")}}),
                ),
                json_reply(json!({"status": "starting"})),
                json_reply(json!({"status": "processing"})),
                json_reply(json!({"status": "succeeded", "output": [format!("{base}/image")]})),
                image_reply(&image),
            ]
        });
        let target = loopback_target(&base, Wire::Replicate, &hash);
        let bytes = submit_with(&target, &Config::default(), output)
            .unwrap_or_else(|err| panic!("{output}: {err:#}"));
        assert_eq!(bytes, image, "{output}");

        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 5, "{seen:?}");
        assert!(seen[0].starts_with("POST /predictions "), "{}", seen[0]);
        assert_eq!(body_of(&seen[0])["version"], hash.as_str());
        assert_eq!(
            body_of(&seen[0])["input"]["output_format"],
            format,
            "{output}"
        );
        for poll in &seen[1..4] {
            assert!(poll.starts_with("GET /predictions/rep-1 "), "{poll}");
            assert!(
                poll.to_ascii_lowercase()
                    .contains("authorization: bearer test-key"),
                "{poll}"
            );
        }
        assert!(seen[4].starts_with("GET /image "), "{}", seen[4]);
        assert!(
            !seen[4].to_ascii_lowercase().contains("authorization:"),
            "{}",
            seen[4]
        );
    }
}

#[test]
fn foreign_poll_urls_are_rejected_before_any_connection() {
    let hash = "e".repeat(64);
    let cases = [
        (
            Wire::BlackForestLabs,
            "flux-pro-1.1",
            json!({"id": "t", "polling_url": "https://attacker.example/poll"}),
        ),
        (
            Wire::Fal,
            "fal-ai/flux/dev",
            json!({"request_id": "t", "status_url": "https://attacker.example/s", "response_url": "https://attacker.example/r"}),
        ),
        (
            Wire::Replicate,
            hash.as_str(),
            json!({"id": "t", "urls": {"get": "https://attacker.example/get"}}),
        ),
    ];
    for (wire, model, reply) in cases {
        let (base, server) = serve(|_| vec![json_reply(reply)]);
        let target = loopback_target(&base, wire, model);
        let err = submit_with(&target, &Config::default(), "out.png").unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("outside its authenticated origin"),
            "{wire:?}: {text}"
        );
        assert_eq!(
            server.join().unwrap().len(),
            1,
            "{wire:?} must not poll a foreign URL"
        );
    }
}

#[test]
fn submissions_missing_tracking_handles_fail_without_polling() {
    let hash = "f".repeat(64);
    type MissingHandleCase<'a> = (Wire, &'a str, fn(&str) -> Value, &'static str);
    let cases: [MissingHandleCase<'_>; 6] = [
        (
            Wire::BlackForestLabs,
            "flux-pro-1.1",
            |base| json!({"polling_url": format!("{base}/poll")}),
            "id",
        ),
        (
            Wire::BlackForestLabs,
            "flux-pro-1.1",
            |_| json!({"id": "t"}),
            "polling_url",
        ),
        (
            Wire::Fal,
            "fal-ai/flux/dev",
            |base| json!({"request_id": "t", "response_url": format!("{base}/r")}),
            "status_url",
        ),
        (
            Wire::Fal,
            "fal-ai/flux/dev",
            |base| json!({"request_id": "t", "status_url": format!("{base}/s")}),
            "response_url",
        ),
        (
            Wire::Replicate,
            hash.as_str(),
            |base| json!({"urls": {"get": format!("{base}/g")}}),
            "id",
        ),
        (
            Wire::Replicate,
            hash.as_str(),
            |_| json!({"id": "t"}),
            "get",
        ),
    ];
    for (wire, model, reply, missing) in cases {
        let (base, server) = serve(|base| vec![json_reply(reply(base))]);
        let target = loopback_target(&base, wire, model);
        let err = submit_with(&target, &Config::default(), "out.png").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("may have been accepted"), "{wire:?}: {text}");
        assert!(
            text.contains(missing),
            "{wire:?} should name {missing}: {text}"
        );
        assert_eq!(
            server.join().unwrap().len(),
            1,
            "{wire:?} must not poll without a handle"
        );
    }
}

#[test]
fn failed_submission_is_reported_once_and_never_retried() {
    let (base, server) = serve(|_| vec![failure_reply(500, "overloaded")]);
    let target = loopback_target(&base, Wire::Fal, "fal-ai/flux/dev");
    let err = submit_with(&target, &Config::default(), "out.png").unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("not retried"), "{text}");
    assert!(text.contains("500"), "{text}");
    assert_eq!(
        server.join().unwrap().len(),
        1,
        "a failed submission must not be retried"
    );
}

#[test]
fn replicate_failed_prediction_names_the_task_and_stops_polling() {
    let hash = "a".repeat(64);
    let (base, server) = serve(|base| {
        vec![
            json_reply(
                json!({"id": "rep-f", "urls": {"get": format!("{base}/predictions/rep-f")}}),
            ),
            json_reply(json!({"status": "failed", "error": "NSFW content detected"})),
        ]
    });
    let target = loopback_target(&base, Wire::Replicate, &hash);
    let err = submit_with(&target, &Config::default(), "out.png").unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("ended as failed"), "{text}");
    assert!(text.contains("NSFW content detected"), "{text}");
    assert!(text.contains("rep-f"), "{text}");
    assert!(text.contains("not cancelled"), "{text}");
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn fal_completed_with_error_fails_without_fetching_a_result() {
    let (base, server) = serve(|base| {
        vec![
            json_reply(json!({
                "request_id": "fal-e",
                "status_url": format!("{base}/status"),
                "response_url": format!("{base}/response"),
            })),
            json_reply(json!({"status": "COMPLETED", "error": "model crashed"})),
        ]
    });
    let target = loopback_target(&base, Wire::Fal, "fal-ai/flux/dev");
    let err = submit_with(&target, &Config::default(), "out.png").unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("completed with an error"), "{text}");
    assert!(text.contains("fal-e"), "{text}");
    assert_eq!(
        server.join().unwrap().len(),
        2,
        "no response fetch after an error"
    );
}

#[test]
fn cancel_during_polling_names_the_known_remote_task() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (base, server) = serve(|base| {
        let mut replies = vec![json_reply(
            json!({"id": "task-cancel", "polling_url": format!("{base}/poll")}),
        )];
        replies.extend((0..20).map(|_| json_reply(json!({"status": "Pending"}))));
        replies
    });
    let target = loopback_target(&base, Wire::BlackForestLabs, "flux-pro-1.1");
    let config = Config::default();
    let client = Client::new(&target, &config, cancel.clone());
    let flag = cancel.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        flag.store(true, Ordering::SeqCst);
    });
    let err = super::request(&client, &target, &request("auto")).unwrap_err();
    trigger.join().unwrap();
    let text = format!("{err:#}");
    assert!(text.contains("interrupted"), "{text}");
    assert!(text.contains("task-cancel"), "{text}");
    assert!(text.contains("not cancelled"), "{text}");

    let seen = server.join().unwrap();
    assert!(seen[0].starts_with("POST "), "{}", seen[0]);
    assert!(
        seen.len() < 20,
        "polling must stop after cancellation: {}",
        seen.len()
    );
}

#[test]
fn deadline_during_polling_names_the_known_remote_task() {
    let (base, server) = serve(|base| {
        let mut replies = vec![json_reply(
            json!({"id": "task-slow", "polling_url": format!("{base}/poll")}),
        )];
        replies.extend((0..12).map(|_| json_reply(json!({"status": "Pending"}))));
        replies
    });
    let target = loopback_target(&base, Wire::BlackForestLabs, "flux-pro-1.1");
    let mut config = Config::default();
    config.image.timeout_secs = 1;
    let err = submit_with(&target, &config, "out.png").unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("timed out") || text.contains("timeout"),
        "{text}"
    );
    assert!(text.contains("task-slow"), "{text}");
    assert!(text.contains("not cancelled"), "{text}");
    assert!(
        server.join().unwrap().len() < 12,
        "polling must stop at the deadline"
    );
}
