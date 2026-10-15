//! Turning a fetched response body into text the model can read.

use super::fetch::Response;
use super::html;
use encoding_rs::{Encoding, UTF_8};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    Html,
    Json,
    Text,
}

/// How to treat a body, from its declared media type (sniffing the bytes when there is none).
/// `Err` carries the user-facing reason a body is refused as binary.
pub fn classify(media_type: Option<&str>, body: &[u8]) -> Result<Kind, String> {
    match media_type {
        Some("text/html" | "application/xhtml+xml") => Ok(Kind::Html),
        Some(media)
            if media == "application/json" || media == "text/json" || media.ends_with("+json") =>
        {
            Ok(Kind::Json)
        }
        Some(media) if is_text_media(media) => Ok(Kind::Text),
        None | Some("application/octet-stream") if looks_like_text(body) => {
            let head = String::from_utf8_lossy(&body[..body.len().min(512)])
                .trim_start()
                .to_ascii_lowercase();
            Ok(
                if head.starts_with("<!doctype html") || head.starts_with("<html") {
                    Kind::Html
                } else {
                    Kind::Text
                },
            )
        }
        other => Err(format!(
            "binary content ({}, {} bytes) is not shown; fetch a text representation instead",
            other.unwrap_or("unknown type"),
            body.len()
        )),
    }
}

fn is_text_media(media: &str) -> bool {
    media.starts_with("text/")
        || media.ends_with("+xml")
        || matches!(
            media,
            "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/x-sh"
                | "application/sql"
        )
}

fn looks_like_text(body: &[u8]) -> bool {
    let head = &body[..body.len().min(8192)];
    !head.contains(&0) && (std::str::from_utf8(head).is_ok() || head.len() == 8192)
}

/// Decode `body` to text. A byte-order mark wins, then the declared charset (header, else an HTML
/// `<meta>`), else UTF-8. Labels follow the WHATWG Encoding Standard; undecodable bytes become U+FFFD.
pub fn decode(body: &[u8], charset: Option<&str>, kind: Kind) -> String {
    // An unknown header label is treated as absent, so an HTML `<meta>` declaration still applies.
    // A `<meta>` declaration cannot select a UTF-16 encoding; WHATWG ignores it.
    let encoding = charset
        .and_then(|label| Encoding::for_label(label.trim().as_bytes()))
        .or_else(|| match kind {
            Kind::Html => meta_charset(&body[..body.len().min(2048)])
                .and_then(|label| Encoding::for_label(label.as_bytes()))
                .filter(|encoding| encoding.is_ascii_compatible()),
            _ => None,
        });
    // `decode` sniffs a BOM first, so UTF-16 bodies decode even when the declared charset is wrong.
    encoding.unwrap_or(UTF_8).decode(body).0.into_owned()
}

fn meta_charset(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let after = &text[text.find("charset=")? + "charset=".len()..];
    let name: String = after
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The response body as readable text. `raw` skips HTML conversion and JSON formatting.
pub fn render(response: &Response, raw: bool) -> Result<String, String> {
    let kind = classify(response.media_type.as_deref(), &response.body)?;
    let text = decode(&response.body, response.charset.as_deref(), kind);
    if raw {
        return Ok(text);
    }
    Ok(match kind {
        Kind::Html => html::to_text(&text, &response.url),
        // A body cut at the size cap is not valid JSON; show it as it is.
        Kind::Json => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or(text),
        Kind::Text => text,
    })
}

/// Cut `text` to at most `max` characters, appending a marker that says how much was dropped.
pub fn truncate_chars(text: String, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text,
        Some((cut, _)) => {
            let total = text.chars().count();
            format!(
                "{}\n[truncated: showing {max} of {total} characters; pass a larger max_chars or fetch a more specific URL]",
                &text[..cut]
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_media_type_then_by_content() {
        assert_eq!(classify(Some("text/html"), b""), Ok(Kind::Html));
        assert_eq!(
            classify(Some("application/vnd.api+json"), b""),
            Ok(Kind::Json)
        );
        assert_eq!(classify(Some("text/csv"), b""), Ok(Kind::Text));
        assert_eq!(classify(None, b"  <!DOCTYPE html><p>x"), Ok(Kind::Html));
        assert_eq!(
            classify(Some("application/octet-stream"), b"plain"),
            Ok(Kind::Text)
        );
        let image = classify(Some("image/png"), &[0x89, b'P', b'N', b'G']).unwrap_err();
        assert!(
            image.contains("image/png") && image.contains("4 bytes"),
            "{image}"
        );
        assert!(classify(None, &[1, 0, 2, 0]).is_err());
    }

    #[test]
    fn decodes_declared_charsets() {
        assert_eq!(
            decode(&[b'c', b'a', b'f', 0xE9], Some("iso-8859-1"), Kind::Text),
            "café"
        );
        assert_eq!(
            decode(b"<meta charset=\"ISO-8859-1\"><p>\xE9", None, Kind::Html),
            "<meta charset=\"ISO-8859-1\"><p>é"
        );
        assert_eq!(decode("café".as_bytes(), None, Kind::Text), "café");
    }

    #[test]
    fn truncation_marks_what_was_dropped() {
        assert_eq!(truncate_chars("héllo".into(), 5), "héllo");
        let cut = truncate_chars("héllo wörld".into(), 5);
        assert!(
            cut.starts_with("héllo\n[truncated: showing 5 of 11 characters"),
            "{cut}"
        );
    }
}
