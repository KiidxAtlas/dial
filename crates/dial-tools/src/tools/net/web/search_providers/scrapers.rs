//! Keyless HTML scrapers: Startpage, Mojeek and Ecosia. They read pages meant for people, so layouts
//! drift and bot walls appear without notice. A wall (challenge, captcha, firewall) or an unrecognised
//! layout is an error, never an empty result list, so `auto` falls through to the next provider.

use super::super::fetch::percent_encode;
use super::super::html::{self, Token};
use super::super::search::SearchResult;
use super::{HttpRequest, Provider, Query, Recency};

pub fn startpage(query: &Query) -> HttpRequest {
    let mut url = format!(
        "https://www.startpage.com/sp/search?query={}&cat=web",
        percent_encode(query.text)
    );
    if let Some(recency) = query.recency {
        url.push_str(match recency {
            Recency::Day => "&with_date=d",
            Recency::Week => "&with_date=w",
            Recency::Month => "&with_date=m",
            Recency::Year => "&with_date=y",
        });
    }
    HttpRequest::get(url, Vec::new())
}

pub fn mojeek(query: &Query) -> HttpRequest {
    HttpRequest::get(
        format!(
            "https://www.mojeek.com/search?q={}",
            percent_encode(query.text)
        ),
        Vec::new(),
    )
}

pub fn ecosia(query: &Query) -> HttpRequest {
    HttpRequest::get(
        format!(
            "https://www.ecosia.org/search?q={}",
            percent_encode(query.text)
        ),
        Vec::new(),
    )
}

/// Parse a decoded scraper page for `provider`.
pub fn parse(provider: Provider, page: &str) -> Result<Vec<SearchResult>, String> {
    let (name, wall, results) = match provider {
        Provider::Startpage => ("Startpage", startpage_wall(page), parse_startpage(page)),
        Provider::Mojeek => ("Mojeek", mojeek_wall(page), parse_mojeek(page)),
        Provider::Ecosia => ("Ecosia", ecosia_wall(page), parse_ecosia(page)),
        _ => return Err(format!("{} is not a scraper provider", provider.name())),
    };
    if let Some(wall) = wall {
        return Err(format!(
            "{name} refused the request with {wall}; try again later or use another search provider"
        ));
    }
    if !results.is_empty() {
        return Ok(results);
    }
    if provider == Provider::Mojeek && page.contains("No pages found") {
        return Ok(results);
    }
    Err(format!(
        "no {name} results could be found in the response: the page layout may have changed or the request was blocked"
    ))
}

fn startpage_wall(page: &str) -> Option<&'static str> {
    (page.contains("id=\"anubis_challenge\"") || page.contains("anubis-main"))
        .then_some("an Anubis proof-of-work challenge")
}

fn mojeek_wall(page: &str) -> Option<&'static str> {
    (page.contains("captcha-wrap") || page.contains("<title>Captcha</title>"))
        .then_some("a captcha")
}

fn ecosia_wall(page: &str) -> Option<&'static str> {
    (page.contains("<title>Ecosia Firewall</title>") || page.contains("challenge-error-text"))
        .then_some("a firewall challenge")
}

fn parse_startpage(page: &str) -> Vec<SearchResult> {
    let tokens = html::tokenize(page);
    let mut results: Vec<SearchResult> = Vec::new();
    let mut url = String::new();
    for (index, token) in tokens.iter().enumerate() {
        let Token::Open { name, attrs, .. } = token else {
            continue;
        };
        let classes = html::attr(attrs, "class").unwrap_or("");
        if name == "a" && has_class(classes, "result-title") {
            url = html::attr(attrs, "href").unwrap_or("").to_owned();
        } else if name == "h2" && has_class(classes, "wgl-title") {
            push(
                &mut results,
                capture(&tokens, index),
                std::mem::take(&mut url),
            );
        } else if name == "p" && has_class(classes, "description") {
            set_snippet(&mut results, capture(&tokens, index));
        }
    }
    results
}

fn parse_mojeek(page: &str) -> Vec<SearchResult> {
    let tokens = html::tokenize(page);
    let mut results: Vec<SearchResult> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let Token::Open { name, attrs, .. } = token else {
            continue;
        };
        let classes = html::attr(attrs, "class").unwrap_or("");
        if name == "a" && has_class(classes, "title") {
            let url = html::attr(attrs, "href").unwrap_or("").to_owned();
            push(&mut results, capture(&tokens, index), url);
        } else if name == "p" && has_class(classes, "s") {
            set_snippet(&mut results, capture(&tokens, index));
        }
    }
    results
}

fn parse_ecosia(page: &str) -> Vec<SearchResult> {
    let tokens = html::tokenize(page);
    let mut results: Vec<SearchResult> = Vec::new();
    let (mut organic, mut url) = (false, String::new());
    for (index, token) in tokens.iter().enumerate() {
        let Token::Open { name, attrs, .. } = token else {
            continue;
        };
        let marker = html::attr(attrs, "data-test-id").unwrap_or("");
        match (name.as_str(), marker) {
            ("article", marker) => organic = marker == "organic-result",
            ("a", "result-link") if organic => {
                url = html::attr(attrs, "href").unwrap_or("").to_owned();
            }
            ("h2", "result-title") if organic => {
                push(
                    &mut results,
                    capture(&tokens, index),
                    std::mem::take(&mut url),
                );
            }
            ("p", "web-result-description") if organic => {
                set_snippet(&mut results, capture(&tokens, index));
            }
            _ => {}
        }
    }
    results
}

fn has_class(classes: &str, wanted: &str) -> bool {
    classes.split_whitespace().any(|class| class == wanted)
}

/// Whitespace-normalised, entity-decoded text of the element opened at `start`, up to its matching
/// close tag (or the end of the page for malformed markup).
fn capture(tokens: &[Token], start: usize) -> String {
    let Token::Open { name: tag, .. } = &tokens[start] else {
        return String::new();
    };
    let (mut depth, mut text) = (1usize, String::new());
    for token in &tokens[start + 1..] {
        match token {
            Token::Text(part) => text.push_str(&html::decode_entities(part)),
            Token::Open {
                name,
                self_closing: false,
                ..
            } if name == tag && !html::is_void(name) => depth += 1,
            Token::Close(name) if name == tag => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Keep only absolute http(s) results with a title.
fn push(results: &mut Vec<SearchResult>, title: String, url: String) {
    if title.is_empty() || !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    results.push(SearchResult {
        title,
        url,
        snippet: String::new(),
    });
}

fn set_snippet(results: &mut [SearchResult], snippet: String) {
    if let Some(last) = results.last_mut().filter(|last| last.snippet.is_empty()) {
        last.snippet = snippet;
    }
}

#[cfg(test)]
mod tests;
