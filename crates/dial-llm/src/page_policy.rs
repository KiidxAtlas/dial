//! Content Security Policy for Dial's self-contained HTML pages. Every inline `<script>` and
//! `<style>` body is pinned by SHA-256 hash, so the page runs its own code and nothing injected.
//! Shared by the relay's collaboration viewer and the local stats dashboard; both serve pages
//! that fetch data only from their own origin.
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

/// Policy for a page whose only network access is back to its own origin.
pub fn page_policy(html: &str) -> String {
    format!(
        "default-src 'none'; script-src{}; style-src{}; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        inline_hashes(html, "script"),
        inline_hashes(html, "style")
    )
}

/// Response headers that accompany a page served under `policy`: the policy itself and no
/// referrer leaking to whatever the page links to. Each line ends with CRLF.
pub fn page_headers(policy: &str) -> String {
    format!("Content-Security-Policy: {policy}\r\nReferrer-Policy: no-referrer\r\n")
}

/// `'sha256-…'` sources for every inline `<tag …>…</tag>` body in `html`.
fn inline_hashes(html: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
    let mut sources = String::new();
    let mut rest = html;
    while let Some(at) = rest.find(&open) {
        let Some(body) = rest[at..].find('>').map(|end| at + end + 1) else {
            break;
        };
        let Some(end) = rest[body..].find(&close).map(|end| body + end) else {
            break;
        };
        sources.push_str(&format!(
            " 'sha256-{}'",
            STANDARD.encode(Sha256::digest(&rest.as_bytes()[body..end]))
        ));
        rest = &rest[end + close.len()..];
    }
    sources
}

#[cfg(test)]
mod tests;
