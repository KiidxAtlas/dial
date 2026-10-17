//! Markdown discovery for HTML pages: `llms.txt` probes, `.md` siblings, and `<link rel="alternate">`
//! markdown links. Everything here is pure; the pipeline performs the requests.

use super::fetch::{parse_url, resolve_url};
use super::html::{self, Token};

/// Accept header for markdown-first requests; HTML is still acceptable as a last resort.
pub const MARKDOWN_ACCEPT: &str = "text/markdown, text/plain;q=0.9, text/html;q=0.8";

/// `scheme://authority` and the path (at least `/`) of `url`.
fn origin_and_path(url: &str) -> Option<(String, String)> {
    let uri = parse_url(url).ok()?;
    let scheme = uri.scheme_str()?;
    let authority = uri.authority()?;
    let path = if uri.path().is_empty() {
        "/"
    } else {
        uri.path()
    }
    .to_owned();
    Some((format!("{scheme}://{authority}"), path))
}

/// `llms.txt` / `llms.md` probe URLs for `url`, most specific first. The scope walks from the page's
/// directory up to the first path segment, then the site root (`/.well-known/llms.txt` first).
pub fn llms_candidates(url: &str) -> Vec<String> {
    let Some((origin, path)) = origin_and_path(url) else {
        return Vec::new();
    };
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let depth = if path.ends_with('/') || segments.is_empty() {
        segments.len()
    } else {
        (segments.len() - 1).max(1)
    };
    let mut candidates = Vec::new();
    for scope in (1..=depth).rev() {
        let prefix = segments[..scope].join("/");
        candidates.push(format!("{origin}/{prefix}/llms.txt"));
        candidates.push(format!("{origin}/{prefix}/llms.md"));
    }
    candidates.push(format!("{origin}/.well-known/llms.txt"));
    candidates.push(format!("{origin}/llms.txt"));
    candidates.push(format!("{origin}/llms.md"));
    candidates
}

/// The `.md` sibling of `url`: `/guide` becomes `/guide.md`, `/guide/` becomes `/guide/index.html.md`.
pub fn md_candidate(url: &str) -> Option<String> {
    let (origin, path) = origin_and_path(url)?;
    if path.ends_with('/') {
        Some(format!("{origin}{path}index.html.md"))
    } else {
        Some(format!("{origin}{path}.md"))
    }
}

/// Markdown `<link rel="alternate" type="text/markdown">` targets in `page`, resolved against `base`.
pub fn markdown_alternates(page: &str, base: &str) -> Vec<String> {
    let mut found = Vec::new();
    for token in html::tokenize(page) {
        let Token::Open { name, attrs, .. } = token else {
            continue;
        };
        if !name.eq_ignore_ascii_case("link") {
            continue;
        }
        let rel = html::attr(&attrs, "rel").unwrap_or("").to_ascii_lowercase();
        let kind = html::attr(&attrs, "type")
            .unwrap_or("")
            .to_ascii_lowercase();
        if !rel.split_ascii_whitespace().any(|word| word == "alternate")
            || !kind.contains("markdown")
        {
            continue;
        }
        if let Some(href) = html::attr(&attrs, "href")
            && let Some(resolved) = resolve_url(base, href)
        {
            found.push(resolved);
        }
    }
    found
}

/// Whether `text` (a probe response) is a usable markdown or plain-text body rather than an HTML page
/// or a stub.
pub fn accepts(media: Option<&str>, text: &str) -> bool {
    if media.is_some_and(|media| media.contains("html")) {
        return false;
    }
    let head: String = text
        .trim_start()
        .chars()
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase();
    if ["<!doctype", "<html", "<head", "<body"]
        .iter()
        .any(|marker| head.starts_with(marker))
    {
        return false;
    }
    text.chars().filter(|c| !c.is_whitespace()).count() > 100
}

#[cfg(test)]
mod tests;
