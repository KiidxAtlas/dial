//! `read` of an `http(s)` URL. The network path is `web_fetch`'s: the guarded [`Client`] and the
//! network gate. Selectors, archive members, and document handlers then work as they do for local
//! files, and the result is framed as untrusted web content.

use super::content;
use super::documents as web_documents;
use super::fetch::{Client, Response};
use super::untrusted;
use crate::tools::fs::{documents, select};
use crate::tools::{Workspace, opt_u64, req_str};
use serde_json::Value;
use std::sync::atomic::AtomicBool;

/// The most a URL read downloads: the same cap as a local document read.
const MAX_URL_BYTES: usize = documents::MAX_INPUT as usize;

const READ_ACCEPT: &str = "text/html, text/plain, text/markdown, application/json, application/pdf, application/zip, application/x-tar, application/octet-stream;q=0.5, */*;q=0.1";

/// Whether a `read` path is an `http://` or `https://` URL (the scheme is case-insensitive).
pub fn is_url(input: &str) -> bool {
    let bytes = input.as_bytes();
    bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://")
        || bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://")
}

/// Read a URL through the shared network path.
pub(in crate::tools) fn read(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<String, String> {
    read_with(&Client::new(), ws, args, cancel)
}

fn read_with(
    client: &Client,
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let input = req_str(args, "path")?;
    if let Some(reason) = ws.web_network_block() {
        return Err(format!(
            "read of a URL is unavailable: network access is disabled ({reason})"
        ));
    }
    let offset = opt_u64(args, "offset")?.unwrap_or(1);
    let limit = opt_u64(args, "limit")?.unwrap_or(select::DEFAULT_READ_LINES as u64);
    if offset == 0 || limit == 0 {
        return Err("offset and limit must be >=1".into());
    }
    let target = split_url(input)?;
    if args.get("offset").is_some_and(|v| !v.is_null()) && target.selector.selects_lines() {
        return Err("use offset or a line selector, not both".into());
    }
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return Err("read of a URL was cancelled".into());
    }
    let response =
        client.send_limited_until(target.url, READ_ACCEPT, &[], None, MAX_URL_BYTES, cancel)?;
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return Err("read of a URL was cancelled".into());
    }
    if !(200..300).contains(&response.status) {
        return Err(format!("HTTP {} from {}", response.status, response.url));
    }
    if response.truncated {
        return Err(format!(
            "{} is larger than {} MiB; download it and read the local copy",
            response.url,
            MAX_URL_BYTES / (1024 * 1024)
        ));
    }
    let source = response.url.clone();
    let text = page_text(
        ws,
        client,
        response,
        target.member,
        target.selector.raw,
        cancel,
    )?;
    let shown = documents::render(
        ws,
        target.url,
        &text,
        &target.selector,
        offset.min(usize::MAX as u64) as usize,
        limit.min(select::MAX_READ_LINES as u64) as usize,
    )?;
    Ok(untrusted(&source, &shown))
}

/// The text of a downloaded page: an archive listing or member, a converted document, or the
/// content's own rendering. Archives and documents are parsed in isolated workers.
fn page_text(
    ws: &Workspace,
    client: &Client,
    response: Response,
    member: Option<&str>,
    raw: bool,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let path = response
        .url
        .split(['?', '#'])
        .next()
        .unwrap_or(response.url.as_str());
    if let Some(format) = documents::kind(path).filter(|format| documents::is_archive(format)) {
        return documents::read_archive_bytes(
            &response.url,
            path,
            format,
            member,
            &response.body,
            cancel,
        );
    }
    if member.is_some() {
        return Err("a :member selector applies only to archives".into());
    }
    if let Some(kind) = web_documents::kind_of(
        &response.url,
        response.media_type.as_deref(),
        &response.body,
    ) {
        return web_documents::convert(ws, client, response, kind);
    }
    content::render(&response, raw)
}

/// A URL argument split into the URL, an archive member, and the read selector.
struct Target<'a> {
    url: &'a str,
    member: Option<&'a str>,
    selector: select::Selector,
}

/// Split `input` into the URL and its selector or archive member. Both are recognised only after
/// the authority, so a port such as `:8080` stays part of the URL.
fn split_url(input: &str) -> Result<Target<'_>, String> {
    let authority = input.find("://").map_or(0, |i| i + 3);
    let path_start = input[authority..]
        .find(['/', '?', '#'])
        .map_or(input.len(), |i| authority + i);
    let (base, selector) = select::split_selector(&input[path_start..])?;
    let url = &input[..path_start + base.len()];
    let archive_end = url[path_start..]
        .char_indices()
        .rev()
        .filter(|(_, c)| *c == ':')
        .find_map(|(i, _)| {
            let end = path_start + i;
            documents::kind(&url[..end])
                .is_some_and(documents::is_archive)
                .then_some(end)
        });
    Ok(match archive_end {
        Some(end) => Target {
            url: &url[..end],
            member: Some(&url[end + 1..]),
            selector,
        },
        None => Target {
            url,
            member: None,
            selector,
        },
    })
}

#[cfg(test)]
mod tests;
