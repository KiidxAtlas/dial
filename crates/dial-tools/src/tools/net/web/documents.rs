//! Binary documents and data files fetched and converted to text: PDF, Word, PowerPoint, Excel
//! and EPUB become markdown; Parquet/Arrow, OpenDocument, legacy spreadsheets, SQLite, mail and
//! the other formats `read` understands locally reuse those same readers.
//!
//! The page fetch keeps the ordinary 2 MiB cap. A document found by media type, extension, or magic
//! bytes is downloaded again with [`MAX_DOCUMENT_BYTES`]. A document that still does not fit is
//! refused, never converted from a cut-off prefix.

use super::fetch::{Client, Response, parse_url};
use crate::tools::Workspace;
use crate::tools::fs::documents;

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

/// Media types of formats read by their local readers (`documents::IN_MEMORY_READERS`).
const DATA_KINDS: &[(&str, &str)] = &[
    ("application/vnd.apache.parquet", "parquet"),
    ("application/x-parquet", "parquet"),
    ("application/vnd.apache.arrow.file", "arrow"),
    ("application/vnd.apache.arrow.stream", "arrow"),
    (
        "application/vnd.oasis.opendocument.spreadsheet",
        "spreadsheet",
    ),
    ("application/vnd.ms-excel", "spreadsheet"),
    (
        "application/vnd.ms-excel.sheet.binary.macroenabled.12",
        "spreadsheet",
    ),
    (
        "application/vnd.ms-excel.sheet.macroenabled.12",
        "spreadsheet",
    ),
    ("application/vnd.oasis.opendocument.text", "opendocument"),
    (
        "application/vnd.oasis.opendocument.presentation",
        "opendocument",
    ),
    ("message/rfc822", "mail"),
    ("application/mbox", "mbox"),
    ("application/vnd.sqlite3", "sqlite"),
    ("application/x-sqlite3", "sqlite"),
];

/// The document kind of a response, if it is one: a markdown conversion (`pdf`, `docx`, `pptx`,
/// `xlsx`, `epub`) or a format named by [`documents::kind`] that has an in-memory reader.
pub fn kind_of(url: &str, media: Option<&str>, body: &[u8]) -> Option<&'static str> {
    if body.starts_with(b"%PDF-") {
        return Some("pdf");
    }
    if let Some(media) = media
        && let Some((_, kind)) = KINDS
            .iter()
            .chain(DATA_KINDS)
            .find(|(mime, _)| mime.eq_ignore_ascii_case(media))
    {
        return Some(kind);
    }
    let readable = |format: &&str| documents::IN_MEMORY_READERS.contains(format);
    if let Some(format) = crate::tools::fs::binary::identify(body)
        .and_then(|kind| kind.format)
        .filter(readable)
    {
        return Some(format);
    }
    let path = parse_url(url).ok()?.path().to_owned();
    if let Some(format) = documents::kind(&path).filter(readable) {
        return Some(format);
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
    documents::convert_format(ws, &body, kind)
        .map(|text| text.trim().to_owned())
        .map_err(|error| format!("could not convert the {kind} document: {error:#}"))
}

#[cfg(test)]
mod tests;
