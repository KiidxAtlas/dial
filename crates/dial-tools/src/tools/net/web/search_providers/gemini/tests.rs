use super::super::Recency;
use super::super::domains::UNFILTERED;
use super::*;

fn query() -> Query<'static> {
    Query {
        text: "latest rust release",
        limit: 5,
        recency: Some(Recency::Year),
        filter: &UNFILTERED,
        today: 19_723,
    }
}

#[test]
fn builds_a_generate_content_request_with_google_search_grounding() {
    let request = build(&query(), "gk");
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url, GEMINI_URL);
    assert_eq!(request.headers, vec![("x-goog-api-key", "gk".to_owned())]);
    let sent: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
    assert!(sent["tools"][0].get("google_search").is_some());
    assert!(
        sent["contents"][0]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("past year")
    );
}

#[test]
fn parses_the_answer_and_the_grounding_chunks() {
    let value = json!({"candidates": [{
        "content": {"parts": [{"text": "Rust 1.80 "}, {"text": "shipped."}]},
        "groundingMetadata": {"groundingChunks": [
            {"web": {"uri": "https://blog.rust-lang.org/", "title": "blog.rust-lang.org"}},
            {"web": {"uri": "https://blog.rust-lang.org/", "title": "dup"}},
            {"web": {"uri": "ftp://skip.example/"}}
        ]}
    }]});
    let parsed = parse(&value).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Rust 1.80 shipped."));
    assert_eq!(parsed.results.len(), 1);
    assert_eq!(parsed.results[0].title, "blog.rust-lang.org");
}

#[test]
fn refuses_a_reply_without_grounded_sources_or_candidates() {
    let ungrounded = json!({"candidates": [{"content": {"parts": [{"text": "x"}]}}]});
    let error = parse(&ungrounded).unwrap_err();
    assert!(error.contains("no grounded sources"), "{error}");
    assert!(parse(&json!({})).unwrap_err().contains("no candidates"));
}

/// Serve `respond(path)` (a complete HTTP response) on a free loopback port, one request per
/// connection. Returns the base URL; the server runs until the process exits.
fn serve(respond: impl Fn(&str) -> String + Send + 'static) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match std::io::Read::read(&mut stream, &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                }
            }
            let head = String::from_utf8_lossy(&request);
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let _ = std::io::Write::write_all(&mut stream, respond(&path).as_bytes());
        }
    });
    base
}

fn redirect(location: &str) -> String {
    format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn page(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn not_found() -> String {
    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
}

fn result(title: &str, url: String) -> SearchResult {
    SearchResult {
        title: title.to_owned(),
        url,
        snippet: String::new(),
    }
}

#[test]
fn resolves_grounding_links_to_their_final_pages_and_keeps_failed_ones() {
    let base = serve(|path| match path {
        "/grounding-api-redirect/ok1" | "/grounding-api-redirect/ok2" => redirect("/pages/rust"),
        "/grounding-api-redirect/gone" => not_found(),
        "/pages/rust" => page("<p>rust</p>"),
        _ => not_found(),
    });
    let prefix = format!("{base}/grounding-api-redirect/");
    let mut results = vec![
        result("A", format!("{prefix}ok1")),
        result("B", format!("{prefix}ok2")),
        result("C", format!("{prefix}gone")),
        result("D", "https://example.com/direct".to_owned()),
    ];
    resolve_with(
        &Client::allowing_loopback(),
        &mut results,
        &format!("{base}/grounding-api-redirect/"),
    );
    let urls: Vec<String> = results.iter().map(|result| result.url.clone()).collect();
    assert_eq!(
        urls,
        vec![
            format!("{base}/pages/rust"),
            format!("{prefix}gone"),
            "https://example.com/direct".to_owned(),
        ]
    );
    assert_eq!(
        results[0].title, "A",
        "the first resolved link keeps its title"
    );
}
