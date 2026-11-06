//! End-to-end tests of the web tools against an in-process HTTP server on 127.0.0.1.
//! Loopback is only reachable through [`Client::allowing_loopback`]; the shipping client refuses it.

use super::*;
use crate::config::types::SandboxMode;
use crate::tools::testkit;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn ok(content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type", content_type.into())],
            body: body.into(),
        }
    }

    fn status(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: b"nope".to_vec(),
        }
    }

    fn redirect(to: impl Into<String>) -> Self {
        Self {
            status: 302,
            headers: vec![("Location", to.into())],
            body: vec![],
        }
    }
}

struct Server {
    base: String,
    hits: Arc<AtomicUsize>,
}

/// Serve `handler(path, request_headers)` on a free loopback port until the process exits.
fn serve(handler: impl Fn(&str, &[(String, String)]) -> Reply + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (hits, handler) = (Arc::new(AtomicUsize::new(0)), Arc::new(handler));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            let handler = handler.clone();
            std::thread::spawn(move || respond(stream, &*handler));
        }
    });
    Server { base, hits }
}

type RequestHandler = dyn Fn(&str, &[(String, String)]) -> Reply;

fn respond(mut stream: std::net::TcpStream, handler: &RequestHandler) {
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&request);
    let mut lines = text.lines();
    let path = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    let headers: Vec<(String, String)> = lines
        .map_while(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let reply = handler(&path, &headers);
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    // The client may stop reading once it has hit its size cap; a failed write is expected then.
    let _ = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(&reply.body));
}

fn fetch_tool() -> WebFetch {
    WebFetch {
        client: Arc::new(Client::allowing_loopback()),
    }
}

fn search_tool() -> WebSearch {
    WebSearch {
        client: Arc::new(Client::allowing_loopback()),
    }
}

fn run(tool: &dyn Tool, config: &Config, args: Value) -> ToolOutput {
    let (_dir, workspace) = testkit::workspace();
    tool.call(
        &ToolContext {
            workspace: &workspace,
            config,
            mutation_authorizer: None,
            confirm: None,
        },
        &args,
    )
}

fn fetch(url: &str, extra: Value) -> ToolOutput {
    let mut args = json!({ "url": url });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    run(&fetch_tool(), &Config::default(), args)
}

#[test]
fn converts_html_and_marks_it_untrusted() {
    let page = r#"<html><head><title>Docs</title><script>steal()</script></head><body>
        <h1>Guide</h1><p>See <a href="/more">more</a>.</p><pre>let x = 1;</pre></body></html>"#;
    let server = serve(move |_, _| Reply::ok("text/html; charset=utf-8", page));
    let out = fetch(&format!("{}/guide", server.base), json!({}));
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.starts_with("[Untrusted web content from "),
        "{}",
        out.text
    );
    assert!(out.text.contains("HTTP 200, text/html"), "{}", out.text);
    assert!(
        out.text
            .contains("Title: Docs\n\n# Guide\n\nSee more (http://127.0.0.1:"),
        "{}",
        out.text
    );
    assert!(out.text.contains("```\nlet x = 1;\n```"), "{}", out.text);
    assert!(!out.text.contains("steal"), "{}", out.text);
    assert!(
        out.text.ends_with("[End of untrusted web content]"),
        "{}",
        out.text
    );
}

#[test]
fn pretty_prints_json_and_honors_raw() {
    let server = serve(|_, _| Reply::ok("application/json", r#"{"b":[1,2],"a":{"k":"v"}}"#));
    let pretty = fetch(&server.base, json!({}));
    assert!(
        pretty
            .text
            .contains("{\n  \"a\": {\n    \"k\": \"v\"\n  },\n  \"b\": [\n    1,\n    2\n  ]\n}"),
        "{}",
        pretty.text
    );
    let raw = fetch(&server.base, json!({"raw": true}));
    assert!(
        raw.text.contains(r#"{"b":[1,2],"a":{"k":"v"}}"#),
        "{}",
        raw.text
    );
}

#[test]
fn plain_text_is_passed_through_and_truncated_with_a_marker() {
    let server = serve(|_, _| Reply::ok("text/plain", "abcdefghij".repeat(10)));
    let out = fetch(&server.base, json!({"max_chars": 25}));
    assert!(
        out.text
            .contains("abcdefghijabcdefghijabcde\n[truncated: showing 25 of 100 characters"),
        "{}",
        out.text
    );
}

#[test]
fn refuses_binary_content() {
    let server = serve(|_, _| Reply::ok("image/png", vec![0x89, b'P', b'N', b'G', 0, 1, 2]));
    let out = fetch(&server.base, json!({}));
    assert!(out.is_error);
    assert!(
        out.text.contains("binary content (image/png, 7 bytes)"),
        "{}",
        out.text
    );
}

#[test]
fn reports_http_errors() {
    let server = serve(|_, _| Reply::status(404));
    let out = fetch(&format!("{}/missing", server.base), json!({}));
    assert!(out.is_error);
    assert!(out.text.contains("HTTP 404 from"), "{}", out.text);
}

#[test]
fn follows_redirects_and_reports_the_final_url() {
    let server = serve(|path, _| match path {
        "/old" => Reply::redirect("/mid"),
        "/mid" => Reply::redirect("final?x=1#frag"),
        "/final?x=1" => Reply::ok("text/plain", "arrived"),
        other => Reply::ok("text/plain", format!("unexpected {other}")),
    });
    let out = fetch(&format!("{}/old", server.base), json!({}));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("arrived"), "{}", out.text);
    assert!(
        out.text
            .contains("/final?x=1 (HTTP 200, text/plain), requested URL"),
        "{}",
        out.text
    );
}

#[test]
fn stops_after_too_many_redirects() {
    let server = serve(|path, _| {
        let n: usize = path.trim_start_matches("/r").parse().unwrap_or(0);
        Reply::redirect(format!("/r{}", n + 1))
    });
    let out = fetch(&format!("{}/r0", server.base), json!({}));
    assert!(out.is_error);
    assert!(
        out.text.contains("too many redirects (more than 5)"),
        "{}",
        out.text
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 6);
}

#[test]
fn caps_the_response_size() {
    let big = vec![b'x'; fetch::MAX_BODY_BYTES + 100_000];
    let server = serve(move |_, _| Reply::ok("text/plain", big.clone()));
    let out = fetch(&server.base, json!({"max_chars": 100_000}));
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text
            .contains("[truncated: showing 100000 of 2097152 characters"),
        "{}",
        out.text
    );
    assert!(
        out.text
            .contains("[response download stopped at the per-response or shared discovery byte limit; remaining content was not downloaded]"),
        "{}",
        out.text
    );
    assert!(
        out.text.len() < 101_000 + 400,
        "output is bounded: {}",
        out.text.len()
    );

    let response = Client::allowing_loopback()
        .get(&server.base, "*/*", None)
        .unwrap();
    assert!(response.truncated);
    assert_eq!(response.body.len(), fetch::MAX_BODY_BYTES);
}

#[test]
fn the_shipping_client_refuses_local_and_metadata_addresses() {
    let server = serve(|_, _| Reply::ok("text/plain", "secret"));
    let client = Client::new();
    for url in [
        server.base.clone(),
        server.base.replace("127.0.0.1", "localhost"),
        "http://169.254.169.254/latest/meta-data/".into(),
        "http://10.0.0.5/".into(),
        "http://192.168.1.1/".into(),
        "http://[::1]/".into(),
        "http://[::ffff:127.0.0.1]/".into(),
        "http://0.0.0.0/".into(),
    ] {
        let error = client
            .get(&url, "*/*", None)
            .err()
            .unwrap_or_else(|| panic!("{url} was fetched"));
        assert!(error.contains("refusing to connect"), "{url}: {error}");
    }
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        0,
        "the guard must refuse before connecting"
    );
}

#[test]
fn redirects_into_private_space_are_refused() {
    let server = serve(|_, _| Reply::redirect("http://169.254.169.254/latest/meta-data/"));
    let out = fetch(&server.base, json!({}));
    assert!(out.is_error);
    assert!(
        out.text.contains("link-local") && out.text.contains("169.254.169.254"),
        "{}",
        out.text
    );
}

#[test]
fn rejects_unusable_urls() {
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/",
        "example.com/page",
        "http://user:pw@example.com/",
    ] {
        let out = fetch(url, json!({}));
        assert!(
            out.is_error && out.text.starts_with("web_fetch failed: "),
            "{url}: {}",
            out.text
        );
    }
    let server = serve(|_, _| Reply::redirect("file:///etc/passwd"));
    let out = fetch(&server.base, json!({}));
    assert!(
        out.text
            .contains("redirect refused: unsupported URL scheme \"file\""),
        "{}",
        out.text
    );
}

#[test]
fn refuses_when_the_sandbox_has_no_network() {
    let server = serve(|_, _| Reply::ok("text/plain", "hi"));
    let mut strict = Config::default();
    strict.sandbox.mode = SandboxMode::Strict;
    let mut offline = Config::default();
    offline.sandbox.allow_network = false;
    for config in [&strict, &offline] {
        let fetched = run(&fetch_tool(), config, json!({"url": server.base}));
        assert!(
            fetched.is_error && fetched.text.contains("network access is disabled"),
            "{}",
            fetched.text
        );
        let searched = run(&search_tool(), config, json!({"query": "rust"}));
        assert!(
            searched.is_error && searched.text.contains("network access is disabled"),
            "{}",
            searched.text
        );
    }
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);

    let mut unsandboxed = Config::default();
    unsandboxed.sandbox.mode = SandboxMode::Off;
    unsandboxed.sandbox.allow_network = false;
    assert!(!run(&fetch_tool(), &unsandboxed, json!({"url": server.base})).is_error);
}

#[test]
fn both_tools_require_approval() {
    let tools = tools(&Config::default());
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names, ["web_fetch", "web_search"]);
    assert!(tools.iter().all(|tool| tool.needs_approval(&json!({}))));
    for tool in &tools {
        assert_eq!(tool.schema()["function"]["name"], tool.name());
    }
}

#[test]
fn bearer_token_goes_only_to_the_original_origin() {
    let other = serve(|_, headers| {
        let seen = headers.iter().any(|(name, _)| name == "authorization");
        Reply::ok("text/plain", format!("other saw auth: {seen}"))
    });
    let other_url = format!("{}/elsewhere", other.base);
    let origin = serve(move |path, headers| {
        let auth = headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, v)| v.clone());
        match path {
            "/direct" => Reply::ok("text/plain", format!("origin saw {auth:?}")),
            _ => Reply::redirect(other_url.clone()),
        }
    });
    let client = Client::allowing_loopback();
    let direct = client
        .get(&format!("{}/direct", origin.base), "*/*", Some("tok"))
        .unwrap();
    assert_eq!(
        String::from_utf8(direct.body).unwrap(),
        r#"origin saw Some("Bearer tok")"#
    );
    let hopped = client
        .get(&format!("{}/hop", origin.base), "*/*", Some("tok"))
        .unwrap();
    assert_eq!(
        String::from_utf8(hopped.body).unwrap(),
        "other saw auth: false"
    );
}

fn search_config(template: String) -> Config {
    let mut config = Config::default();
    config.web.search_url = template;
    config
}

#[test]
fn searches_a_json_backend_with_the_query_encoded() {
    let server = serve(|path, _| {
        let body = json!({"results": [
            {"title": "Async/await", "url": "https://a.example/1", "content": "x ".repeat(400)},
            {"title": "Second", "url": "https://b.example/2", "content": path},
            {"title": "Third", "url": "https://c.example/3"},
        ]});
        Reply::ok("application/json", body.to_string())
    });
    let config = search_config(format!("{}/search?q={{query}}&format=json", server.base));
    let out = run(
        &search_tool(),
        &config,
        json!({"query": "rust async/await & más", "limit": 2}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text
            .contains("2 results for \"rust async/await & más\":"),
        "{}",
        out.text
    );
    assert!(
        out.text
            .contains("1. Async/await\n   https://a.example/1\n   x x x"),
        "{}",
        out.text
    );
    assert!(
        out.text
            .contains("/search?q=rust%20async%2Fawait%20%26%20m%C3%A1s&format=json"),
        "{}",
        out.text
    );
    assert!(!out.text.contains("Third"), "limit applies: {}", out.text);
    assert!(
        out.text.contains('…') || !out.text.contains(&"x ".repeat(300)),
        "snippets are bounded"
    );
    assert!(
        out.text.starts_with("[Untrusted web content"),
        "{}",
        out.text
    );
}

#[test]
fn searches_a_duckduckgo_style_html_backend() {
    let server = serve(|_, _| Reply::ok("text/html; charset=UTF-8", search::DDG_SAMPLE));
    let out = run(
        &search_tool(),
        &search_config(format!("{}/html/?q={{query}}", server.base)),
        json!({"query": "rust"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("4 results for \"rust\":"), "{}", out.text);
    assert!(
        out.text.contains(
            "1. Rust Programming Language\n   https://rust-lang.org/\n   Rust is a fast, reliable"
        ),
        "{}",
        out.text
    );
    assert!(
        !out.text.contains("Sponsored") && !out.text.contains("y.js"),
        "ads are dropped: {}",
        out.text
    );
}

#[test]
fn search_says_when_a_custom_backend_ignores_recency() {
    let server = serve(|_, _| Reply::ok("text/html; charset=UTF-8", search::DDG_SAMPLE));
    let config = search_config(format!("{}/html/?q={{query}}", server.base));
    let note = "Note: the recency filter is not applied to a custom web.search_url endpoint.";
    let windowed = run(
        &search_tool(),
        &config,
        json!({"query": "rust", "recency": "day"}),
    );
    assert!(!windowed.is_error, "{}", windowed.text);
    assert!(windowed.text.contains(note), "{}", windowed.text);
    let plain = run(&search_tool(), &config, json!({"query": "rust"}));
    assert!(!plain.text.contains("Note:"), "{}", plain.text);
    let invalid = run(
        &search_tool(),
        &config,
        json!({"query": "rust", "recency": "hour"}),
    );
    assert!(
        invalid.is_error && invalid.text.contains("recency must be"),
        "{}",
        invalid.text
    );
}

#[test]
fn search_failures_are_honest() {
    let server = serve(|path, _| match path {
        p if p.starts_with("/blocked") => Reply::ok(
            "text/html",
            "<html><body>Please verify you are human</body></html>",
        ),
        p if p.starts_with("/empty") => {
            Reply::ok("text/html", r#"<div class="no-results">No results.</div>"#)
        }
        p if p.starts_with("/down") => Reply::status(503),
        _ => Reply::ok("application/json", r#"{"unexpected": true}"#),
    });
    let query = |route: &str| {
        run(
            &search_tool(),
            &search_config(format!("{}/{route}?q={{query}}", server.base)),
            json!({"query": "x"}),
        )
    };
    let blocked = query("blocked");
    assert!(
        blocked.is_error && blocked.text.contains("layout may have changed"),
        "{}",
        blocked.text
    );
    let empty = query("empty");
    assert!(
        !empty.is_error && empty.text.contains("No results for \"x\"."),
        "{}",
        empty.text
    );
    let down = query("down");
    assert!(
        down.is_error && down.text.contains("HTTP 503"),
        "{}",
        down.text
    );
    let shape = query("other");
    assert!(
        shape.is_error && shape.text.contains("no recognizable results array"),
        "{}",
        shape.text
    );
}

#[test]
fn search_validates_its_inputs_and_configuration() {
    let config = Config::default();
    for args in [
        json!({"query": "  "}),
        json!({"query": "x", "limit": 0}),
        json!({"query": "x".repeat(501)}),
        json!({}),
    ] {
        let out = run(&search_tool(), &config, args.clone());
        assert!(out.is_error, "{args}: {}", out.text);
    }
    let missing = run(
        &search_tool(),
        &search_config("https://example.com/search".into()),
        json!({"query": "x"}),
    );
    assert!(
        missing.text.contains("{query} placeholder"),
        "{}",
        missing.text
    );

    // PATH is always set; its value must neither be sent over http nor echoed in the error.
    let mut keyed = search_config("http://example.com/s?q={query}".into());
    keyed.web.search_api_key_env = Some("PATH".into());
    let out = run(&search_tool(), &keyed, json!({"query": "x"}));
    assert!(out.text.contains("plain http"), "{}", out.text);
    assert!(
        !out.text.contains(&std::env::var("PATH").unwrap()),
        "secret leaked"
    );
    keyed.web.search_api_key_env = Some("DIAL_TEST_SURELY_UNSET_KEY".into());
    let out = run(&search_tool(), &keyed, json!({"query": "x"}));
    assert!(
        out.text
            .contains("DIAL_TEST_SURELY_UNSET_KEY, which is not set"),
        "{}",
        out.text
    );
}

#[test]
fn markdown_alternate_reports_its_redirected_source_and_original_relationship() {
    for (mode, relationship) in [
        ("alternate", "alternate"),
        ("sibling", "markdown sibling"),
        ("negotiated", "negotiated markdown"),
        ("llms", "llms.txt"),
    ] {
        let target = serve(|_, _| {
            let mut reply = Reply::ok("text/markdown", "Actual alternate content. ".repeat(12));
            reply.status = 203;
            reply
        });
        let target_url = format!("{}/actual.md", target.base);
        let redirected = target_url.clone();
        let origin = serve(move |path, headers| {
            if (mode == "alternate" && path == "/alternate")
                || (mode == "sibling" && path == "/guide.md")
                || (mode == "llms" && path == "/guide/llms.txt")
                || (mode == "negotiated"
                    && path == "/guide"
                    && headers.iter().any(|(name, value)| {
                        name == "accept" && value.starts_with("text/markdown")
                    }))
            {
                Reply::redirect(redirected.clone())
            } else if path == "/guide" {
                let link = if mode == "alternate" {
                    "<link rel=\"alternate\" type=\"text/markdown\" href=\"/alternate\">"
                } else {
                    ""
                };
                Reply::ok("text/html", format!("{link}<p>Original</p>"))
            } else {
                Reply::status(404)
            }
        });
        let original = format!("{}/guide", origin.base);
        let mut config = Config::default();
        config.web.reader_chain = vec!["native".into()];
        let out = run(&fetch_tool(), &config, json!({"url": original}));
        assert!(!out.is_error, "{mode}: {}", out.text);
        assert!(
            out.text
                .contains(&format!("{target_url} (HTTP 203, text/markdown")),
            "{mode}: {}",
            out.text
        );
        assert!(
            out.text.contains(&format!("{relationship} for {original}")),
            "{mode}: {}",
            out.text
        );
        assert!(
            !out.text.contains("redirected from"),
            "alternate is not a redirect of original: {}",
            out.text
        );
    }
}

#[test]
fn discovery_budget_covers_every_send_and_redirect_without_limiting_foreground() {
    let server = serve(|path, _| {
        let (prefix, count) = path.rsplit_once('/').unwrap();
        let count: usize = count.parse().unwrap();
        if count < 4 {
            Reply::redirect(format!("{prefix}/{}", count + 1))
        } else {
            Reply::ok("text/plain", "done")
        }
    });
    let foreground = Client::allowing_loopback();
    let discovery = foreground.for_discovery();
    discovery
        .get(&format!("{}/a/0", server.base), "*/*", None)
        .unwrap();
    discovery
        .send(&format!("{}/b/0", server.base), "*/*", &[], None)
        .unwrap();
    assert!(
        discovery
            .send_limited_until(
                &format!("{}/c/0", server.base),
                "*/*",
                &[],
                None,
                100,
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        12,
        "redirect hops share request budget"
    );
    foreground
        .get(&format!("{}/a/4", server.base), "*/*", None)
        .unwrap();
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        13,
        "foreground remains independent"
    );
    discovery.interrupt();
    assert!(
        discovery
            .get(&format!("{}/d/4", server.base), "*/*", None)
            .is_err()
    );
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        13,
        "cancelled discovery never starts another request"
    );
}

#[test]
fn markdown_discovery_deduplicates_candidates_before_probing() {
    let links = "<link rel=\"alternate\" type=\"text/markdown\" href=\"/duplicate\">".repeat(200);
    let server = serve(move |path, _| match path {
        "/guide" => Reply::ok(
            "text/html",
            format!("{links}<link rel=\"alternate\" type=\"text/markdown\" href=\"/good\">"),
        ),
        "/good" => Reply::ok("text/markdown", "Useful alternate prose. ".repeat(12)),
        _ => Reply::status(404),
    });
    let out = fetch(&format!("{}/guide", server.base), json!({}));
    assert!(out.text.contains("Useful alternate prose"), "{}", out.text);
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);
}

#[test]
fn markdown_discovery_shares_one_deadline_for_many_slow_links() {
    let links: String = (0..100)
        .map(|i| format!("<link rel=\"alternate\" type=\"text/markdown\" href=\"/slow/{i}\">"))
        .collect();
    let server = serve(move |path, _| {
        if path == "/guide" {
            Reply::ok("text/html", format!("{links}<p>Original fallback</p>"))
        } else {
            std::thread::sleep(std::time::Duration::from_millis(250));
            Reply::status(404)
        }
    });
    let mut config = Config::default();
    config.web.timeout_secs = 1;
    config.web.reader_chain = vec!["native".into()];
    let started = std::time::Instant::now();
    let out = run(
        &fetch_tool(),
        &config,
        json!({"url": format!("{}/guide", server.base)}),
    );
    assert!(
        !out.is_error && out.text.contains("Original fallback"),
        "{}",
        out.text
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "elapsed {:?}",
        started.elapsed()
    );
    assert!(
        server.hits.load(Ordering::SeqCst) <= 6,
        "hits {}",
        server.hits.load(Ordering::SeqCst)
    );
}

#[test]
fn markdown_discovery_bounds_requests_and_aggregate_bodies() {
    for body in [Vec::new(), vec![b'x'; fetch::MAX_BODY_BYTES]] {
        let large = !body.is_empty();
        let links: String = (0..100)
            .map(|i| {
                format!("<link rel=\"alternate\" type=\"text/markdown\" href=\"/candidate/{i}\">")
            })
            .collect();
        let server = serve(move |path, _| {
            if path == "/guide" {
                Reply::ok("text/html", format!("{links}<p>Original fallback</p>"))
            } else {
                let mut reply = Reply::ok("text/html", body.clone());
                reply.status = 404;
                reply
            }
        });
        let mut config = Config::default();
        config.web.reader_chain = vec!["native".into()];
        let out = run(
            &fetch_tool(),
            &config,
            json!({"url": format!("{}/guide", server.base)}),
        );
        assert!(
            !out.is_error && out.text.contains("Original fallback"),
            "{}",
            out.text
        );
        let max_hits = if large { 3 } else { 13 };
        assert!(
            server.hits.load(Ordering::SeqCst) <= max_hits,
            "large={large}, hits={}",
            server.hits.load(Ordering::SeqCst)
        );
    }
}

#[test]
fn background_cancellation_is_not_reset_or_shared_with_foreground_clients() {
    let cancel = Arc::new(AtomicBool::new(true));
    let client = Client::with_cancel(cancel.clone());
    client.reset_interrupt();
    assert!(client.is_interrupted());
    assert!(client.for_discovery().is_interrupted());
    assert_eq!(
        client
            .send_limited("https://example.com", "text/plain", &[], None, 1024)
            .err()
            .as_deref(),
        Some("web request cancelled")
    );
    let foreground = Client::new();
    assert!(!foreground.is_interrupted());
    cancel.store(false, Ordering::Release);
    assert!(!client.is_interrupted());
    foreground.interrupt();
    assert!(!client.is_interrupted());
}

#[test]
fn search_fetch_top_is_opt_in_ordered_bounded_and_keeps_partial_failures() {
    let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let requested = paths.clone();
    let pages = serve(move |path, headers| {
        requested.lock().unwrap().push(path.to_owned());
        assert!(!headers.iter().any(|(name, _)| name == "authorization"));
        match path {
            "/first" => Reply::ok("text/plain", "é".repeat(7_000)),
            "/second" => Reply::status(503),
            "/third" => Reply::redirect("/final"),
            "/final" => Reply::ok("text/plain", "third page body"),
            _ => panic!("unexpected fetched page {path}"),
        }
    });
    let base = pages.base.clone();
    let search = serve(move |_, _| {
        Reply::ok(
            "application/json",
            json!({"results": [
                {"title": "First", "url": format!("{base}/first")},
                {"title": "Second", "url": format!("{base}/second")},
                {"title": "Third", "url": format!("{base}/third")},
                {"title": "Fourth", "url": format!("{base}/fourth")}
            ]})
            .to_string(),
        )
    });
    let config = search_config(format!("{}/?q={{query}}", search.base));
    let default = run(&search_tool(), &config, json!({"query": "x"}));
    let disabled = run(
        &search_tool(),
        &config,
        json!({"query": "x", "fetch_top": 0}),
    );
    assert!(!default.is_error, "{}", default.text);
    assert_eq!(default.text, disabled.text);
    assert_eq!(pages.hits.load(Ordering::SeqCst), 0);
    for count in 1..=3 {
        paths.lock().unwrap().clear();
        let out = run(
            &search_tool(),
            &config,
            json!({"query": "x", "fetch_top": count}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.starts_with(&default.text),
            "search results retained"
        );
        assert!(
            out.text.contains("showing 6000 of 7000 characters"),
            "{}",
            out.text
        );
        assert!(out.text.chars().count() <= 100_000, "output bounded");
        let expected = match count {
            1 => vec!["/first"],
            2 => vec!["/first", "/second"],
            _ => vec!["/first", "/second", "/third", "/final"],
        };
        assert_eq!(*paths.lock().unwrap(), expected, "fetch_top={count}");
        if count >= 2 {
            assert!(out.text.contains("Fetch failed: HTTP 503"), "{}", out.text);
        }
        if count == 3 {
            assert!(out.text.contains("third page body"), "{}", out.text);
            assert!(out.text.contains("requested URL"), "{}", out.text);
        }
    }
}

#[test]
fn search_fetch_top_rejects_invalid_bounds_before_network() {
    let server = serve(|_, _| panic!("invalid fetch_top must not reach network"));
    let config = search_config(format!("{}/?q={{query}}", server.base));
    for value in [json!(-1), json!(4), json!(1.5), json!("1"), json!(true)] {
        let out = run(
            &search_tool(),
            &config,
            json!({"query": "x", "fetch_top": value}),
        );
        assert!(out.is_error, "{}", out.text);
        assert!(
            out.text
                .contains("fetch_top must be an integer from 0 to 3"),
            "{}",
            out.text
        );
    }
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn search_fetch_results_reuses_shipping_guards_and_does_not_scrape_answer_links() {
    let (_dir, workspace) = testkit::workspace();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &workspace,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let tool = WebSearch {
        client: Arc::new(Client::new()),
    };
    assert!(tool.needs_approval(&json!({"fetch_top": 3})));
    for url in [
        "http://127.0.0.1/private",
        "http://169.254.169.254/latest",
        "https://user:password@example.com/",
        "file:///etc/passwd",
    ] {
        let results = vec![search::SearchResult {
            title: "Kept".into(),
            url: url.into(),
            snippet: String::new(),
        }];
        let out = tool.fetch_results(&ctx, "original search".into(), &results, 1);
        assert!(out.starts_with("original search"), "{out}");
        assert!(out.contains("Fetch failed:"), "{url}: {out}");
        assert!(out.contains(url), "{out}");
    }
    let answer = "Grounded answer with https://example.com/ in prose";
    let out = tool.fetch_results(&ctx, answer.into(), &[], 3);
    assert!(out.starts_with(answer), "{out}");
    assert!(out.contains("No eligible structured result URLs"), "{out}");
}
