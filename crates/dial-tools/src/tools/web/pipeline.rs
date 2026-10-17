//! `web_fetch` pipeline: site handlers, documents, markdown discovery, the reader chain, and the
//! built-in conversion as the last resort. Every request goes through [`Client`], so the SSRF guard
//! and the redirect checks apply to each step.

use super::alternates::{self, MARKDOWN_ACCEPT};
use super::content::{self, Kind};
use super::documents;
use super::fetch::{Client, Response};
use super::reader;
use super::sites;
use crate::config::types::WebConfig;
use crate::tools::Workspace;

/// The result of rendering one URL.
pub struct Rendered {
    pub text: String,
    /// The URL the content came from (after redirects).
    pub url: String,
    pub status: u16,
    /// Description of the source, shown next to the URL.
    pub media: String,
    /// The body was cut at the download cap.
    pub truncated: bool,
}

/// Render `url` to readable text. `raw` skips every conversion and site handler.
pub fn render(
    client: &Client,
    config: &WebConfig,
    workspace: &Workspace,
    url: &str,
    raw: bool,
    accept: &str,
) -> Result<Rendered, String> {
    if !raw && let Some(text) = sites::handle(url, client) {
        return Ok(Rendered {
            text,
            url: url.to_owned(),
            status: 200,
            media: "site API".to_owned(),
            truncated: false,
        });
    }
    let page = client.get(url, accept, None)?;
    if !(200..300).contains(&page.status) {
        return Err(format!("HTTP {} from {}", page.status, page.url));
    }
    let media = page
        .media_type
        .clone()
        .unwrap_or_else(|| "unknown type".to_owned());
    let final_url = page.url.clone();
    let status = page.status;
    let truncated = page.truncated;

    if let Some(kind) = documents::kind_of(&page.url, page.media_type.as_deref(), &page.body) {
        let text = documents::convert(workspace, client, page, kind)?;
        return Ok(Rendered {
            text,
            url: final_url,
            status,
            media,
            truncated: false,
        });
    }

    let kind = content::classify(page.media_type.as_deref(), &page.body)?;
    if raw || !matches!(kind, Kind::Html) {
        let text = content::render(&page, raw)?;
        return Ok(Rendered {
            text,
            url: final_url,
            status,
            media,
            truncated,
        });
    }

    let markup = content::decode(&page.body, page.charset.as_deref(), Kind::Html);
    let served_as_markdown = |text: String, truncated: bool| Rendered {
        text,
        url: final_url.clone(),
        status,
        media: format!("{media}, markdown"),
        truncated,
    };

    for link in alternates::markdown_alternates(&markup, &final_url) {
        if let Some((text, cut)) = probe(client, &link) {
            return Ok(served_as_markdown(text, cut));
        }
    }
    if let Some(link) = alternates::md_candidate(&final_url)
        && let Some((text, cut)) = probe(client, &link)
    {
        return Ok(served_as_markdown(text, cut));
    }
    let negotiated = client
        .send(&final_url, MARKDOWN_ACCEPT, &[], None)
        .ok()
        .filter(|response| (200..300).contains(&response.status))
        .and_then(|response| probe_response(&response));
    if let Some((text, cut)) = negotiated {
        return Ok(served_as_markdown(text, cut));
    }

    if let Some(text) = reader::convert(client, config, workspace.root(), &final_url, &markup)? {
        return Ok(Rendered {
            text,
            url: final_url,
            status,
            media: format!("{media}, converted by reader"),
            truncated,
        });
    }

    for link in alternates::llms_candidates(&final_url) {
        if let Some((text, cut)) = probe(client, &link) {
            return Ok(Rendered {
                text,
                url: link,
                status: 200,
                media: "llms.txt".to_owned(),
                truncated: cut,
            });
        }
    }

    let text = content::render(&page, false)?;
    Ok(Rendered {
        text,
        url: final_url,
        status,
        media,
        truncated,
    })
}

/// Fetch a discovery candidate. `None` on any failure or unusable body; discovery never fails the
/// fetch.
fn probe(client: &Client, url: &str) -> Option<(String, bool)> {
    let response = client.get(url, MARKDOWN_ACCEPT, None).ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    probe_response(&response)
}

/// The markdown body of `response`, when it is one (see [`alternates::accepts`]).
fn probe_response(response: &Response) -> Option<(String, bool)> {
    let text = content::decode(&response.body, response.charset.as_deref(), Kind::Text);
    let text = text.trim().to_owned();
    alternates::accepts(response.media_type.as_deref(), &text).then_some((text, response.truncated))
}
