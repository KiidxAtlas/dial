//! Perplexity chat completions with web search: the model's answer plus the sources it cites.

use super::json_apis::{optional_array, result, text};
use super::{HttpRequest, Parsed, Query, Recency};
use serde_json::{Value, json};

const PERPLEXITY_URL: &str = "https://api.perplexity.ai/chat/completions";
const MODEL: &str = "sonar-pro";

pub fn build(query: &Query, key: &str) -> HttpRequest {
    let mut body = json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": query.text}],
        "temperature": 0.2,
        "search_mode": "web",
        "num_search_results": query.limit.clamp(1, 20),
        "return_related_questions": false,
    });
    if let Some(recency) = query.recency {
        body["search_recency_filter"] = json!(match recency {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        });
    }
    HttpRequest::post(
        PERPLEXITY_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        body,
    )
}

/// The answer is `choices[0].message.content`. Sources come from `search_results` when present, else
/// from the bare `citations` URL list.
pub fn parse(value: &Value) -> Result<Parsed, String> {
    let answer = text(value, "/choices/0/message/content");
    let mut results: Vec<_> = optional_array(value, "/search_results")
        .iter()
        .filter_map(|item| {
            result(
                text(item, "title"),
                text(item, "url"),
                text(item, "snippet").unwrap_or_default(),
            )
        })
        .collect();
    if results.is_empty() {
        results = optional_array(value, "/citations")
            .iter()
            .filter_map(|item| {
                let url = item.as_str()?.to_owned();
                result(None, Some(url), String::new())
            })
            .collect();
    }
    Ok(Parsed { answer, results })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(recency: Option<Recency>) -> Query<'static> {
        Query {
            text: "latest rust release",
            limit: 5,
            recency,
            today: 19_723,
        }
    }

    #[test]
    fn builds_a_bearer_chat_completion_with_web_search() {
        let request = build(&query(Some(Recency::Week)), "pk");
        assert_eq!(request.method(), "POST");
        assert_eq!(request.url, PERPLEXITY_URL);
        assert_eq!(
            request.headers,
            vec![("Authorization", "Bearer pk".to_owned())]
        );
        let sent: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(sent["model"], MODEL);
        assert_eq!(sent["messages"][0]["role"], "user");
        assert_eq!(sent["messages"][0]["content"], "latest rust release");
        assert_eq!(sent["search_mode"], "web");
        assert_eq!(sent["num_search_results"], 5);
        assert_eq!(sent["search_recency_filter"], "week");
    }

    #[test]
    fn omits_recency_filter_when_not_requested() {
        let request = build(&query(None), "pk");
        let sent: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert!(sent.get("search_recency_filter").is_none());
    }

    #[test]
    fn parses_answer_and_search_results() {
        let value = json!({
            "choices": [{"message": {"content": "Rust 1.80 shipped."}}],
            "search_results": [{"title": "Notes", "url": "https://r.example/", "snippet": "s"}],
            "citations": ["https://ignored.example/"]
        });
        let parsed = parse(&value).unwrap();
        assert_eq!(parsed.answer.as_deref(), Some("Rust 1.80 shipped."));
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].url, "https://r.example/");
    }

    #[test]
    fn falls_back_to_citation_urls() {
        let value = json!({
            "choices": [{"message": {"content": "Answer"}}],
            "citations": ["https://c1.example/", "not a url", "https://c2.example/"]
        });
        let parsed = parse(&value).unwrap();
        let urls: Vec<&str> = parsed.results.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls, ["https://c1.example/", "https://c2.example/"]);
        assert_eq!(parsed.results[0].title, "https://c1.example/");
    }
}
