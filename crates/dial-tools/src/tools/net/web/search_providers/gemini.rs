//! Google Gemini with `google_search` grounding: the model's answer plus the sources in its grounding
//! metadata. Grounding reports sources as redirect links carrying the page title, which are kept as
//! given. A reply without grounding sources is refused.

use super::super::fetch::Client;
use super::super::search::SearchResult;
use super::ACCEPT;
use super::grounded::{add_source, prompt};
use super::json_apis::{optional_array, result, text};
use super::{HttpRequest, Parsed, Query};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const GEMINI_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent";

/// Grounding sources come as redirect links on this host; each is resolved to the page it names.
const GROUNDING_REDIRECTS: &str = "https://vertexaisearch.cloud.google.com/grounding-api-redirect/";
/// Per-link budget: a slow link keeps its redirect URL rather than delaying the answer.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Redirect links resolved at once.
const RESOLVE_CONCURRENCY: usize = 4;

pub fn build(query: &Query, key: &str) -> HttpRequest {
    HttpRequest::post(
        GEMINI_URL,
        vec![("x-goog-api-key", key.to_owned())],
        json!({
            "contents": [{"role": "user", "parts": [{"text": prompt(query)}]}],
            "tools": [{"google_search": {}}],
        }),
    )
}

/// The answer concatenates the text parts of the first candidate. Sources are its grounding chunks.
pub fn parse(value: &Value) -> Result<Parsed, String> {
    let candidate = value
        .pointer("/candidates/0")
        .ok_or("the Gemini response has no candidates")?;
    let answer: String = optional_array(candidate, "/content/parts")
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect();
    let mut results = Vec::new();
    for chunk in optional_array(candidate, "/groundingMetadata/groundingChunks") {
        add_source(
            &mut results,
            result(
                text(chunk, "/web/title"),
                text(chunk, "/web/uri"),
                String::new(),
            ),
        );
    }
    if results.is_empty() {
        return Err(
            "Gemini returned no grounded sources, so the reply is not a search result".into(),
        );
    }
    let answer = answer.trim().to_owned();
    Ok(Parsed {
        answer: (!answer.is_empty()).then_some(answer),
        results,
    })
}

/// Resolves grounding redirect links to the page each names, through the guarded client, at most
/// [`RESOLVE_CONCURRENCY`] at a time. A link that fails keeps its redirect URL.
pub(super) fn resolve_redirects(client: &Client, results: &mut Vec<SearchResult>) {
    resolve_with(client, results, GROUNDING_REDIRECTS);
}

fn resolve_with(client: &Client, results: &mut Vec<SearchResult>, prefix: &str) {
    let mut finals: Vec<String> = results.iter().map(|result| result.url.clone()).collect();
    let targets: Vec<usize> = (0..finals.len())
        .filter(|&index| finals[index].starts_with(prefix))
        .collect();
    for batch in targets.chunks(RESOLVE_CONCURRENCY) {
        let resolved: Vec<(usize, Option<String>)> = thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|&index| {
                    let url = finals[index].clone();
                    scope.spawn(move || (index, resolve_one(client, &url)))
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect()
        });
        for (index, url) in resolved {
            if let Some(url) = url {
                finals[index] = url;
            }
        }
    }
    let mut out = Vec::with_capacity(results.len());
    for (mut result, url) in results.drain(..).zip(finals) {
        result.url = url;
        add_source(&mut out, Some(result));
    }
    *results = out;
}

/// The page a redirect link finally answers with, or `None` when it fails, ends without a 2xx, or
/// runs past [`RESOLVE_TIMEOUT`] (or the dial session is interrupted).
fn resolve_one(client: &Client, url: &str) -> Option<String> {
    let cancel = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            let started = Instant::now();
            while !finished.load(Ordering::Acquire) {
                if started.elapsed() >= RESOLVE_TIMEOUT || client.is_interrupted() {
                    cancel.store(true, Ordering::Release);
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let response = client.send_limited_until(url, ACCEPT, &[], None, 1, &cancel);
        finished.store(true, Ordering::Release);
        response
            .ok()
            .filter(|response| (200..300).contains(&response.status))
            .map(|response| response.url)
    })
}

#[cfg(test)]
mod tests;
