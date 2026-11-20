//! URL validation and component encoding shared by Dial's clients and tools.
use std::fmt::Write as _;
use ureq::http::Uri;

/// Longest model- or config-supplied URL accepted, in bytes.
pub const MAX_URL_LEN: usize = 2048;

/// Validate a model- or config-supplied URL: http(s), a host, no embedded credentials.
pub fn parse_url(url: &str) -> Result<Uri, String> {
    let url = url.trim();
    if url.len() > MAX_URL_LEN {
        return Err(format!("URL is longer than {MAX_URL_LEN} characters"));
    }
    if has_scheme(url) {
        let scheme = url
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(format!(
                "unsupported URL scheme {scheme:?}; only http and https are allowed"
            ));
        }
    }
    let uri: Uri = url.parse().map_err(|_| "not a valid URL".to_string())?;
    if uri.scheme_str().is_none() {
        return Err("URL must be absolute and start with http:// or https://".into());
    }
    let authority = uri.authority().ok_or("URL has no host")?;
    if authority.as_str().contains('@') {
        return Err("URLs with embedded credentials are not allowed".into());
    }
    Ok(uri)
}

pub fn has_scheme(reference: &str) -> bool {
    let Some((scheme, _)) = reference.split_once(':') else {
        return false;
    };
    scheme
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Encode UTF-8 bytes outside the RFC 3986 unreserved set (a path segment or query value).
pub fn percent_encode(text: &str) -> String {
    encode(text, b"-._~")
}

/// JavaScript `encodeURIComponent` also leaves `!`, `'`, `(`, `)` and `*` untouched.
pub fn encode_uri_component(text: &str) -> String {
    encode(text, b"-._~!'()*")
}

fn encode(text: &str, punctuation: &[u8]) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || punctuation.contains(&byte) {
            out.push(char::from(byte));
        } else {
            write!(out, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    out
}

/// Decode dashboard and coverage-report URL components. A literal `+` stays literal;
/// unparseable escapes pass through, and non-UTF-8 octets use replacement characters.
pub fn decode_uri_component_lossy(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
