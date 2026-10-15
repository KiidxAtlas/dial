//! REST search APIs that answer with JSON: Brave, Tavily, Exa, Serper, Kagi, Jina, Bing and Google CSE.
//! Each builder is pure and each parser maps a fixed response shape to [`Parsed`].

use super::super::fetch::percent_encode;
use super::super::html::decode_entities;
use super::super::search::SearchResult;
use super::{HttpRequest, Parsed, Query, Recency, iso_date};
use serde_json::{Value, json};

const BRAVE_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const TAVILY_URL: &str = "https://api.tavily.com/search";
const EXA_URL: &str = "https://api.exa.ai/search";
const SERPER_URL: &str = "https://google.serper.dev/search";
const KAGI_URL: &str = "https://kagi.com/api/v1/search";
const JINA_URL: &str = "https://s.jina.ai";
pub const BING_URL: &str = "https://api.bing.microsoft.com/v7.0/search";
const GOOGLE_URL: &str = "https://www.googleapis.com/customsearch/v1";

/// Result count within a provider's accepted range.
fn count(limit: usize, max: usize) -> usize {
    limit.clamp(1, max)
}

pub fn brave(query: &Query, key: &str) -> HttpRequest {
    let mut url = format!(
        "{BRAVE_URL}?q={}&count={}&extra_snippets=true&text_decorations=false&safesearch=moderate",
        percent_encode(query.text),
        count(query.limit, 20)
    );
    if let Some(recency) = query.recency {
        url.push_str(match recency {
            Recency::Day => "&freshness=pd",
            Recency::Week => "&freshness=pw",
            Recency::Month => "&freshness=pm",
            Recency::Year => "&freshness=py",
        });
    }
    HttpRequest::get(
        url,
        vec![
            ("Accept", "application/json".into()),
            ("X-Subscription-Token", key.to_owned()),
        ],
    )
}

pub fn parse_brave(value: &Value) -> Result<Parsed, String> {
    let results = optional_array(value, "/web/results")
        .iter()
        .filter_map(|item| {
            let mut parts = vec![text(item, "description").unwrap_or_default()];
            parts.extend(strings(item, "extra_snippets"));
            result(
                text(item, "title").map(|title| plain(&title)),
                text(item, "url"),
                plain(&join_parts(parts)),
            )
        })
        .collect();
    Ok(Parsed {
        answer: None,
        results,
    })
}

pub fn tavily(query: &Query, key: &str) -> HttpRequest {
    let mut body = json!({
        "query": query.text,
        "search_depth": "basic",
        "max_results": count(query.limit, 20),
        "include_answer": "advanced",
        "include_raw_content": false,
    });
    if let Some(recency) = query.recency {
        body["time_range"] = json!(recency.name());
    }
    HttpRequest::post(
        TAVILY_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        body,
    )
}

pub fn parse_tavily(value: &Value) -> Result<Parsed, String> {
    let results = array(value, "/results", "Tavily")?
        .iter()
        .filter_map(|item| {
            result(
                text(item, "title"),
                text(item, "url"),
                text(item, "content").unwrap_or_default(),
            )
        })
        .collect();
    Ok(Parsed {
        answer: text(value, "/answer"),
        results,
    })
}

pub fn exa(query: &Query, key: &str) -> HttpRequest {
    let mut body = json!({
        "query": query.text,
        "numResults": count(query.limit, 20),
        "type": "auto",
        "contents": {"summary": {"query": query.text}},
    });
    if let Some(recency) = query.recency {
        let start = iso_date(query.today - recency.days());
        body["startPublishedDate"] = json!(format!("{start}T00:00:00.000Z"));
    }
    HttpRequest::post(EXA_URL, vec![("x-api-key", key.to_owned())], body)
}

/// Exa returns a per-result summary; the answer joins the first three of them.
pub fn parse_exa(value: &Value) -> Result<Parsed, String> {
    let items = array(value, "/results", "Exa")?;
    let results = items
        .iter()
        .filter_map(|item| {
            let snippet = text(item, "summary")
                .or_else(|| text(item, "text"))
                .unwrap_or_else(|| join_parts(strings(item, "highlights")));
            result(text(item, "title"), text(item, "url"), snippet)
        })
        .collect();
    let summaries: Vec<String> = items
        .iter()
        .filter(|item| text(item, "url").is_some())
        .filter_map(|item| {
            let summary = text(item, "summary")?;
            let title = text(item, "title")
                .or_else(|| text(item, "url"))
                .unwrap_or_else(|| "Untitled".to_owned());
            Some(format!("**{title}**: {summary}"))
        })
        .take(3)
        .collect();
    Ok(Parsed {
        answer: (!summaries.is_empty()).then(|| summaries.join("\n\n")),
        results,
    })
}

pub fn serper(query: &Query, key: &str) -> HttpRequest {
    let mut body = json!({"q": query.text, "num": count(query.limit, 20)});
    if let Some(recency) = query.recency {
        body["tbs"] = json!(match recency {
            Recency::Day => "qdr:d",
            Recency::Week => "qdr:w",
            Recency::Month => "qdr:m",
            Recency::Year => "qdr:y",
        });
    }
    HttpRequest::post(SERPER_URL, vec![("X-API-KEY", key.to_owned())], body)
}

pub fn parse_serper(value: &Value) -> Result<Parsed, String> {
    let results = optional_array(value, "/organic")
        .iter()
        .filter_map(|item| {
            result(
                text(item, "title"),
                text(item, "link"),
                text(item, "snippet").unwrap_or_default(),
            )
        })
        .collect();
    let answer_box = value.get("answerBox");
    let answer = answer_box.and_then(|note| text(note, "answer").or_else(|| text(note, "snippet")));
    Ok(Parsed { answer, results })
}

pub fn kagi(query: &Query, key: &str) -> HttpRequest {
    let mut body = json!({
        "query": query.text,
        "workflow": "search",
        "limit": count(query.limit, 40),
    });
    if let Some(recency) = query.recency {
        body["filters"] = json!({"after": iso_date(query.today - recency.days())});
    }
    HttpRequest::post(
        KAGI_URL,
        vec![
            ("Authorization", format!("Bearer {key}")),
            ("Accept", "application/json".into()),
        ],
        body,
    )
}

pub fn parse_kagi(value: &Value) -> Result<Parsed, String> {
    let mut results = Vec::new();
    for (bucket, tag) in [("search", ""), ("news", "[News] ")] {
        for item in optional_array(value, &format!("/data/{bucket}")) {
            let title = text(item, "title").map(|title| format!("{tag}{title}"));
            results.extend(result(
                title,
                text(item, "url"),
                text(item, "snippet").unwrap_or_default(),
            ));
        }
    }
    let answer = value
        .pointer("/data/direct_answer")
        .and_then(Value::as_array)
        .and_then(|answers| answers.first())
        .and_then(|first| text(first, "snippet").or_else(|| text(first, "title")));
    Ok(Parsed { answer, results })
}

/// Jina's reader search answers with a bare array, or an envelope `{code, data}` on some plans.
pub fn jina(query: &Query, key: &str) -> HttpRequest {
    HttpRequest::get(
        format!(
            "{JINA_URL}/{}?count={}",
            percent_encode(query.text),
            count(query.limit, 20)
        ),
        vec![
            ("Accept", "application/json".into()),
            ("Authorization", format!("Bearer {key}")),
            ("X-Respond-With", "no-content".into()),
            ("X-Retain-Images", "none".into()),
        ],
    )
}

pub fn parse_jina(value: &Value) -> Result<Parsed, String> {
    let items = match value {
        Value::Array(items) => items,
        Value::Object(_) => {
            if let Some(code) = value.get("code").and_then(Value::as_i64) {
                if code != 200 {
                    return Err(format!("Jina reported failure code {code}"));
                }
            }
            value
                .get("data")
                .and_then(Value::as_array)
                .ok_or("the Jina response has no data array")?
        }
        _ => return Err("the Jina response is neither an array nor an object".into()),
    };
    let results = items
        .iter()
        .filter_map(|item| {
            let snippet = text(item, "description")
                .or_else(|| text(item, "content"))
                .unwrap_or_default();
            result(text(item, "title"), text(item, "url"), snippet)
        })
        .collect();
    Ok(Parsed {
        answer: None,
        results,
    })
}

/// `endpoint` is the public Bing API or an https Azure resource (validated by the caller).
pub fn bing(query: &Query, key: &str, endpoint: &str) -> HttpRequest {
    let mut url = format!(
        "{endpoint}?q={}&count={}",
        percent_encode(query.text),
        count(query.limit, 20)
    );
    if let Some(recency) = query.recency {
        let freshness: String = match recency {
            Recency::Day => "Day".into(),
            Recency::Week => "Week".into(),
            Recency::Month => "Month".into(),
            // Bing has no year bucket; an explicit date range is its equivalent.
            Recency::Year => format!(
                "{}..{}",
                iso_date(query.today - recency.days()),
                iso_date(query.today)
            ),
        };
        url.push_str(&format!("&freshness={freshness}"));
    }
    HttpRequest::get(url, vec![("Ocp-Apim-Subscription-Key", key.to_owned())])
}

pub fn parse_bing(value: &Value) -> Result<Parsed, String> {
    let results = optional_array(value, "/webPages/value")
        .iter()
        .filter_map(|item| {
            result(
                text(item, "name"),
                text(item, "url"),
                text(item, "snippet").unwrap_or_default(),
            )
        })
        .collect();
    Ok(Parsed {
        answer: None,
        results,
    })
}

/// Google Programmable Search (Custom Search JSON API). The key travels in the query string to the
/// googleapis.com origin only; nothing in this module echoes the URL.
pub fn google(query: &Query, key: &str, cx: &str) -> HttpRequest {
    let mut url = format!(
        "{GOOGLE_URL}?key={}&cx={}&q={}&num={}",
        percent_encode(key),
        percent_encode(cx),
        percent_encode(query.text),
        count(query.limit, 10)
    );
    if let Some(recency) = query.recency {
        url.push_str(match recency {
            Recency::Day => "&dateRestrict=d1",
            Recency::Week => "&dateRestrict=w1",
            Recency::Month => "&dateRestrict=m1",
            Recency::Year => "&dateRestrict=y1",
        });
    }
    HttpRequest::get(url, Vec::new())
}

pub fn parse_google(value: &Value) -> Result<Parsed, String> {
    let results = optional_array(value, "/items")
        .iter()
        .filter_map(|item| {
            result(
                text(item, "title"),
                text(item, "link"),
                text(item, "snippet").unwrap_or_default(),
            )
        })
        .collect();
    Ok(Parsed {
        answer: None,
        results,
    })
}

/// A required array; a missing one means the response shape is not what this provider documents.
fn array<'a>(value: &'a Value, pointer: &str, api: &str) -> Result<&'a Vec<Value>, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("the {api} response has no {pointer} array"))
}

/// An array that the API omits when there are no results.
pub(super) fn optional_array<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

pub(super) fn text(value: &Value, key: &str) -> Option<String> {
    let field = if key.starts_with('/') {
        value.pointer(key)
    } else {
        value.get(key)
    };
    field
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn join_parts(parts: Vec<String>) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strip markup (Brave wraps matches in `<strong>`) and decode entities.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for character in text.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(character),
            _ => {}
        }
    }
    decode_entities(&out).trim().to_owned()
}

/// A result needs an http(s) URL; the URL stands in for a missing title.
pub(super) fn result(
    title: Option<String>,
    url: Option<String>,
    snippet: String,
) -> Option<SearchResult> {
    let url = url?;
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    Some(SearchResult {
        title: title.unwrap_or_else(|| url.clone()),
        url,
        snippet,
    })
}

#[cfg(test)]
mod tests {
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
        let enveloped = json!({"code": 200, "data": [{"title": "B", "url": "https://b.example/", "content": "c"}]});
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
        let value =
            json!({"items": [{"title": "G", "link": "https://g.example/", "snippet": "s"}]});
        let parsed = parse_google(&value).unwrap();
        assert_eq!(parsed.results[0].url, "https://g.example/");
        assert!(parse_google(&json!({})).unwrap().results.is_empty());
    }
}
