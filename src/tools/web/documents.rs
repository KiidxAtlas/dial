//! Binary documents (PDF, Word, PowerPoint, Excel, EPUB) fetched and converted to markdown.
//!
//! The page fetch keeps the ordinary 2 MiB cap. A document found by media type, extension, or magic
//! bytes is downloaded again with [`MAX_DOCUMENT_BYTES`]. A document that still does not fit is
//! refused, never converted from a cut-off prefix.

use super::fetch::{Client, Response, parse_url};
use crate::tools::Workspace;

/// Largest document downloaded for conversion.
pub const MAX_DOCUMENT_BYTES: usize = 20 * 1024 * 1024;
const DOCUMENT_ACCEPT: &str = "application/pdf,application/vnd.openxmlformats-officedocument.*,application/epub+zip,*/*;q=0.1";

/// Media type and file extension of each convertible document kind.
const KINDS: &[(&str, &str)] = &[
    ("application/pdf", "pdf"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    ),
    ("application/epub+zip", "epub"),
];

/// The document kind (`pdf`, `docx`, `pptx`, `xlsx`, `epub`) of a response, if it is one.
pub fn kind_of(url: &str, media: Option<&str>, body: &[u8]) -> Option<&'static str> {
    if body.starts_with(b"%PDF-") {
        return Some("pdf");
    }
    if let Some(media) = media
        && let Some((_, kind)) = KINDS.iter().find(|(mime, _)| *mime == media)
    {
        return Some(kind);
    }
    let extension = url_extension(url)?;
    KINDS
        .iter()
        .find(|(_, kind)| *kind == extension)
        .map(|(_, kind)| *kind)
}

/// Lowercased extension of the last path segment of `url`.
fn url_extension(url: &str) -> Option<String> {
    let path = parse_url(url).ok()?.path().to_owned();
    let name = path.rsplit('/').next()?;
    let (_, extension) = name.rsplit_once('.')?;
    Some(extension.to_ascii_lowercase())
}

/// Convert the document `first` (a response to a request for `url`) to markdown. `kind` comes from
/// [`kind_of`]. Re-downloads with [`MAX_DOCUMENT_BYTES`] when the first response was cut short.
pub fn convert(
    ws: &Workspace,
    client: &Client,
    first: Response,
    kind: &str,
) -> Result<String, String> {
    if !(200..300).contains(&first.status) {
        return Err(format!("HTTP {} from {}", first.status, first.url));
    }
    let body = if first.truncated {
        let full =
            client.send_limited(&first.url, DOCUMENT_ACCEPT, &[], None, MAX_DOCUMENT_BYTES)?;
        if full.truncated {
            return Err(format!(
                "the {kind} document is larger than {} MiB and was not converted",
                MAX_DOCUMENT_BYTES / (1024 * 1024)
            ));
        }
        if !(200..300).contains(&full.status) {
            return Err(format!("HTTP {} from {}", full.status, full.url));
        }
        full.body
    } else {
        first.body
    };
    let name = format!("document.{kind}");
    crate::tools::documents::convert_bytes(ws, &body, &name)
        .map(|text| text.trim().to_owned())
        .map_err(|error| format!("could not convert the {kind} document: {error:#}"))
}

#[cfg(test)]
mod tests;
