//! URL syntax checks shared by config validation and the web and browser tools.
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
