//! DeepSeek search. Its filters are query operators: `site:` for included hosts, `-site:` for excluded
//! hosts, and `after:` for the recency window. The query is capped at 128 characters.

use super::json_apis::{array, count, result, text};
use super::{HttpRequest, Parsed, Query, iso_date};
use serde_json::{Value, json};

const DEEPSEEK_URL: &str = "https://api.deepseek.com/v1/tools/search";
const MAX_QUERY_CHARS: usize = 128;
const MAX_RESULTS: usize = 50;

pub fn build(query: &Query, key: &str) -> Result<HttpRequest, String> {
    let mut text = query.text.to_owned();
    if let Some(hosts) = query.filter.native_include() {
        let sites: Vec<String> = hosts.iter().map(|host| format!("site:{host}")).collect();
        text.push(' ');
        text.push_str(&sites.join(" OR "));
    }
    for host in query.filter.native_exclude() {
        text.push_str(&format!(" -site:{host}"));
    }
    if let Some(recency) = query.recency {
        let start = iso_date(query.today - recency.days());
        text.push_str(&format!(" after:{start}"));
    }
    let length = text.chars().count();
    if length > MAX_QUERY_CHARS {
        return Err(format!(
            "DeepSeek accepts at most {MAX_QUERY_CHARS} query characters, and the query with its filters has {length}"
        ));
    }
    Ok(HttpRequest::post(
        DEEPSEEK_URL,
        vec![("Authorization", format!("Bearer {key}"))],
        json!({"query": text, "count": count(query.limit, MAX_RESULTS)}),
    ))
}

pub fn parse(value: &Value) -> Result<Parsed, String> {
    let results = array(value, "/results", "DeepSeek")?
        .iter()
        .filter_map(|item| {
            let snippet = text(item, "summary")
                .or_else(|| text(item, "snippet"))
                .unwrap_or_default();
            result(text(item, "title"), text(item, "url"), snippet)
        })
        .collect();
    Ok(Parsed {
        answer: None,
        results,
    })
}

#[cfg(test)]
mod tests;
