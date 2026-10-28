use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;

fn question<'a>() -> Question<'a> {
    Question {
        question: "How much reasoning?",
        choices: &["off", "low", "high"],
        criteria: &["Trivial lookup.", ""],
        context: "fix the typo",
    }
}

fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn request_body_is_system_one_choice_question_with_null_for_missing_rubrics() {
    let body = request_body("jev-latest", &question());
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["state"], "fix the typo");
    let choice = &body["questions"]["choice"];
    assert_eq!(choice["type"], "choice");
    assert_eq!(choice["instructions"], "How much reasoning?");
    assert_eq!(choice["criteria"]["off"], "Trivial lookup.");
    assert!(choice["criteria"]["low"].is_null());
    assert!(choice["criteria"]["high"].is_null());
}

#[test]
fn decode_reply_reads_choice_and_non_negative_cost() {
    let (choice, cost) = decode_reply(
        r#"{"answers":{"choice":{"type":"choice","choice":"low"}},"usage":{"cost":0.002}}"#,
    )
    .unwrap();
    assert_eq!(choice, "low");
    assert_eq!(cost, Some(0.002));
}

#[test]
fn decode_reply_rejects_missing_choice_and_drops_negative_cost() {
    assert!(decode_reply(r#"{"answers":{}}"#).is_err());
    let (choice, cost) =
        decode_reply(r#"{"answers":{"choice":{"choice":"high"}},"usage":{"cost":-1}}"#).unwrap();
    assert_eq!(choice, "high");
    assert_eq!(cost, None);
}

#[test]
fn new_requires_a_key_and_an_http_base_url() {
    assert!(TypesafeBackend::new("  ", None, "jev-latest", 5, cancel()).is_err());
    assert!(TypesafeBackend::new("key", Some("ftp://example"), "jev-latest", 5, cancel()).is_err());
    let backend =
        TypesafeBackend::new("key", Some("http://127.0.0.1:9/"), "", 5, cancel()).unwrap();
    assert_eq!(backend.model(), DEFAULT_MODEL);
    assert_eq!(backend.base_url, "http://127.0.0.1:9");
}

/// One-shot loopback server: records the request line, bearer header and JSON body, then replies.
fn serve_once(reply: &'static str) -> (String, std::thread::JoinHandle<(String, String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut bearer = String::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("authorization:") {
                bearer = value.trim().to_owned();
            }
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        let mut stream = reader.into_inner();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
        (
            request_line.trim().to_owned(),
            bearer,
            serde_json::from_slice(&body).unwrap(),
        )
    });
    (base, handle)
}

#[test]
fn answer_posts_system_one_body_with_bearer_and_returns_choice_text() {
    let (base, server) = serve_once(
        r#"{"answers":{"choice":{"type":"choice","choice":"medium"}},"usage":{"input_tokens":3,"output_tokens":1,"cost":0.01}}"#,
    );
    let mut backend =
        TypesafeBackend::new("secret-key", Some(&base), "jev-latest", 10, cancel()).unwrap();
    let answer = backend.answer(&question(), false).unwrap();
    let (request_line, bearer, body) = server.join().unwrap();

    assert_eq!(request_line, "POST /v1/systemone HTTP/1.1");
    assert_eq!(bearer, "bearer secret-key");
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["questions"]["choice"]["criteria"]["low"], Value::Null);
    let text: Value = serde_json::from_str(&answer.text).unwrap();
    assert_eq!(text["choice"], "medium");
    assert_eq!(answer.cost_usd, Some(0.01));
}
