use super::*;

fn query(recency: Option<Recency>) -> Query<'static> {
    Query {
        text: "rust & async",
        limit: 3,
        recency,
        today: 19_723, // 2024-01-01
    }
}

fn body(request: &HttpRequest) -> Value {
    serde_json::from_str(request.body.as_deref().expect("POST body")).unwrap()
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn brave_builds_a_get_with_token_header_and_freshness() {
    let request = brave(&query(Some(Recency::Week)), "tok");
    assert_eq!(request.method(), "GET");
    assert!(request.url.starts_with(BRAVE_URL));
    assert!(request.url.contains("q=rust%20%26%20async"));
    assert!(request.url.contains("count=3"));
    assert!(request.url.ends_with("&freshness=pw"));
    assert_eq!(header(&request, "X-Subscription-Token"), Some("tok"));
    assert_eq!(header(&request, "Authorization"), None);
}

#[test]
fn brave_parses_web_results_stripping_markup() {
    let value = json!({"web": {"results": [{
        "title": "<strong>Rust</strong> &amp; co",
        "url": "https://rust-lang.org/",
        "description": "A <strong>fast</strong> language.",
        "extra_snippets": ["More detail"]
    }, {"title": "bad", "url": "ftp://x"}]}});
    let parsed = parse_brave(&value).unwrap();
    assert_eq!(parsed.answer, None);
    assert_eq!(parsed.results.len(), 1);
    assert_eq!(parsed.results[0].title, "Rust & co");
    assert_eq!(parsed.results[0].snippet, "A fast language. More detail");
    assert!(parse_brave(&json!({})).unwrap().results.is_empty());
}

#[test]
fn tavily_posts_json_with_bearer_and_time_range() {
    let request = tavily(&query(Some(Recency::Month)), "tk");
    assert_eq!(request.method(), "POST");
    assert_eq!(request.url, TAVILY_URL);
    assert_eq!(header(&request, "Authorization"), Some("Bearer tk"));
    let sent = body(&request);
    assert_eq!(sent["query"], "rust & async");
    assert_eq!(sent["max_results"], 3);
    assert_eq!(sent["time_range"], "month");
    assert_eq!(sent["search_depth"], "basic");
}

#[test]
fn tavily_parses_results_and_answer() {
    let value = json!({
        "answer": "Rust is fast.",
        "results": [{"title": "T", "url": "https://a.example/", "content": "body"}]
    });
    let parsed = parse_tavily(&value).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Rust is fast."));
    assert_eq!(parsed.results[0].snippet, "body");
    assert!(parse_tavily(&json!({})).is_err());
}

#[test]
fn exa_posts_camel_case_body_and_dates_the_window() {
    let request = exa(&query(Some(Recency::Day)), "ek");
    assert_eq!(request.url, EXA_URL);
    assert_eq!(header(&request, "x-api-key"), Some("ek"));
    let sent = body(&request);
    assert_eq!(sent["numResults"], 3);
    assert_eq!(sent["contents"]["summary"]["query"], "rust & async");
    assert_eq!(sent["startPublishedDate"], "2023-12-31T00:00:00.000Z");
}

#[test]
fn exa_parses_results_and_synthesizes_answer_from_summaries() {
    let value = json!({"results": [
        {"title": "One", "url": "https://1.example/", "summary": "First summary"},
        {"title": "Two", "url": "https://2.example/", "text": "plain text"},
        {"url": "https://3.example/", "highlights": ["h1", "h2"]}
    ]});
    let parsed = parse_exa(&value).unwrap();
    assert_eq!(parsed.results.len(), 3);
    assert_eq!(parsed.results[1].snippet, "plain text");
    assert_eq!(parsed.results[2].title, "https://3.example/");
    assert_eq!(parsed.results[2].snippet, "h1 h2");
    assert_eq!(parsed.answer.as_deref(), Some("**One**: First summary"));
}

#[test]
fn serper_posts_api_key_header_and_reads_answer_box() {
    let request = serper(&query(Some(Recency::Year)), "sk");
    assert_eq!(request.url, SERPER_URL);
    assert_eq!(header(&request, "X-API-KEY"), Some("sk"));
    assert_eq!(body(&request)["tbs"], "qdr:y");

    let value = json!({
        "answerBox": {"snippet": "Quick fact"},
        "organic": [{"title": "Org", "link": "https://o.example/", "snippet": "s"}]
    });
    let parsed = parse_serper(&value).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Quick fact"));
    assert_eq!(parsed.results[0].url, "https://o.example/");
}

#[test]
fn kagi_posts_bearer_body_with_after_filter() {
    let request = kagi(&query(Some(Recency::Week)), "kk");
    assert_eq!(request.url, KAGI_URL);
    assert_eq!(header(&request, "Authorization"), Some("Bearer kk"));
    let sent = body(&request);
    assert_eq!(sent["workflow"], "search");
    assert_eq!(sent["filters"]["after"], "2023-12-25");
}

#[test]
fn kagi_parses_search_news_and_direct_answer() {
    let value = json!({
        "data": {
            "search": [{"url": "https://s.example/", "title": "S", "snippet": "sn"}],
            "news": [{"url": "https://n.example/", "title": "N"}],
            "direct_answer": [{"snippet": "42"}]
        }
    });
    let parsed = parse_kagi(&value).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("42"));
    assert_eq!(parsed.results[0].title, "S");
    assert_eq!(parsed.results[1].title, "[News] N");
    assert_eq!(parsed.results[1].url, "https://n.example/");
}

#[test]
fn jina_gets_encoded_path_with_headers() {
    let request = jina(&query(None), "jk");
    assert_eq!(request.url, "https://s.jina.ai/rust%20%26%20async?count=3");
    assert_eq!(header(&request, "Authorization"), Some("Bearer jk"));
    assert_eq!(header(&request, "X-Respond-With"), Some("no-content"));
}

#[test]
fn jina_parses_bare_array_and_envelope() {
    let bare = json!([{"title": "A", "url": "https://a.example/", "description": "d"}]);
    assert_eq!(parse_jina(&bare).unwrap().results[0].snippet, "d");
    let enveloped =
        json!({"code": 200, "data": [{"title": "B", "url": "https://b.example/", "content": "c"}]});
    assert_eq!(parse_jina(&enveloped).unwrap().results[0].snippet, "c");
    assert!(parse_jina(&json!({"code": 500})).is_err());
}

#[test]
fn bing_uses_subscription_header_and_year_date_range() {
    let request = bing(&query(Some(Recency::Year)), "bk", BING_URL);
    assert!(request.url.starts_with(BING_URL));
    assert!(request.url.contains("count=3"));
    assert!(request.url.ends_with("&freshness=2023-01-01..2024-01-01"));
    assert_eq!(header(&request, "Ocp-Apim-Subscription-Key"), Some("bk"));
    assert!(
        bing(&query(Some(Recency::Day)), "bk", BING_URL)
            .url
            .ends_with("&freshness=Day")
    );
}

#[test]
fn bing_parses_web_pages_value() {
    let value = json!({"webPages": {"value": [
        {"name": "N", "url": "https://n.example/", "snippet": "s"}
    ]}});
    let parsed = parse_bing(&value).unwrap();
    assert_eq!(parsed.results[0].title, "N");
    assert!(parse_bing(&json!({})).unwrap().results.is_empty());
}

#[test]
fn google_cse_puts_key_and_cx_in_the_query_and_caps_num_at_ten() {
    let request = google(&query(Some(Recency::Month)), "gk", "cx1");
    assert_eq!(request.method(), "GET");
    assert!(request.url.starts_with(GOOGLE_URL));
    assert!(
        request
            .url
            .contains("key=gk&cx=cx1&q=rust%20%26%20async&num=3")
    );
    assert!(request.url.ends_with("&dateRestrict=m1"));
    assert!(request.headers.is_empty());
    let wide = google(
        &Query {
            limit: 20,
            ..query(None)
        },
        "gk",
        "cx1",
    );
    assert!(wide.url.contains("num=10"));
}

#[test]
fn google_parses_items() {
    let value = json!({"items": [{"title": "G", "link": "https://g.example/", "snippet": "s"}]});
    let parsed = parse_google(&value).unwrap();
    assert_eq!(parsed.results[0].url, "https://g.example/");
    assert!(parse_google(&json!({})).unwrap().results.is_empty());
}
