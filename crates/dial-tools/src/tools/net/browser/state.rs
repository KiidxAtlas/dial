//! Cookies, web storage, clipboard and named saved state for the browser tool.
//!
//! Chromium work happens in the driver (`site.cjs`). This module validates model input
//! before any browser work, redacts cookie values unless requested, bounds outputs, and owns
//! the named save/load state files. Saved states live under the per-project state directory,
//! never in the workspace.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::profile;

#[cfg(test)]
mod tests;

const MAX_COOKIE_BATCH: usize = 32;
const MAX_URLS: usize = 8;
const MAX_COOKIES_RETURNED: usize = 200;
const MAX_COOKIE_NAME_CHARS: usize = 256;
const MAX_COOKIE_VALUE_CHARS: usize = 4096;
const MAX_COOKIE_PATH_CHARS: usize = 512;
const MAX_DOMAIN_CHARS: usize = 253;
const MAX_STORAGE_KEY_CHARS: usize = 256;
const MAX_STORAGE_VALUE_CHARS: usize = 65_536;
const MAX_OUTPUT_CHARS: usize = 20_000;
const MAX_CLIPBOARD_TEXT_CHARS: usize = 65_536;
const MAX_LOADED_COOKIES: usize = 1_000;
const MAX_LOADED_ORIGINS: usize = 256;
const MAX_LOCAL_STORAGE_ENTRIES: usize = 1_000;

/// Validates the action's arguments and returns extra driver request fields, if any.
/// Errors here fail before a browser operation starts.
pub(super) fn prepare(
    action: &str,
    args: &Value,
    state_dir: &Path,
) -> Result<Option<Value>, String> {
    match action {
        "cookies" => {
            let query = parse_cookie_query(args)?;
            Ok(Some(json!({"urls": query.urls})))
        }
        "set_cookies" => Ok(Some(json!({"cookies": parse_set_cookies(args)?}))),
        "clear_cookies" => Ok(Some(json!({"names": parse_clear_names(args)?}))),
        "storage" => Ok(Some(json!({"storage": parse_storage(args)?}))),
        "clipboard_write" => {
            validate_clipboard_text(args)?;
            Ok(None)
        }
        "save_state" => {
            state_path(state_dir, required_name(args)?)?;
            Ok(None)
        }
        "load_state" => {
            let name = required_name(args)?;
            let path = state_path(state_dir, name)?;
            let stored = profile::load(&path)?
                .ok_or_else(|| format!("no saved browser state named {name:?}"))?;
            Ok(Some(json!({"loaded_state": parse_loaded_state(&stored)?})))
        }
        _ => Ok(None),
    }
}

/// Shapes the driver result: redacts and filters cookies, bounds storage and clipboard
/// output, and writes saved state. Values leave the tool only when explicitly requested.
pub(super) fn finish(
    action: &str,
    args: &Value,
    state_dir: &Path,
    result: &mut Value,
) -> Result<(), String> {
    match action {
        "cookies" => finish_cookies(args, result),
        "storage" => {
            if let Some(entries) = result.get("entries")
                && entries.to_string().chars().count() > MAX_OUTPUT_CHARS
            {
                return Err(format!(
                    "storage entries exceed {MAX_OUTPUT_CHARS} characters; request one key with key"
                ));
            }
            Ok(())
        }
        "clipboard_read" => {
            if let Some(text) = result["text"].as_str().map(str::to_owned)
                && text.chars().count() > MAX_OUTPUT_CHARS
            {
                result["text"] = json!(text.chars().take(MAX_OUTPUT_CHARS).collect::<String>());
                result["truncated"] = json!(true);
            }
            Ok(())
        }
        "save_state" => {
            let name = required_name(args)?;
            let path = state_path(state_dir, name)?;
            let storage_state = result
                .get("storage_state")
                .cloned()
                .ok_or("browser returned no storage state")?;
            let cookies = storage_state["cookies"].as_array().map_or(0, Vec::len);
            let origins = storage_state["origins"].as_array().map_or(0, Vec::len);
            profile::save(&path, &storage_state)?;
            *result = json!({"name": name, "cookies": cookies, "origins": origins});
            Ok(())
        }
        _ => Ok(()),
    }
}

struct CookieQuery {
    urls: Vec<String>,
    domain: Option<String>,
    include_values: bool,
}

fn parse_cookie_query(args: &Value) -> Result<CookieQuery, String> {
    let urls = match present(args, "urls") {
        None => Vec::new(),
        Some(value) => {
            let items = value.as_array().ok_or("urls must be an array of URLs")?;
            if items.len() > MAX_URLS {
                return Err(format!("urls accepts at most {MAX_URLS} entries"));
            }
            items
                .iter()
                .map(|item| {
                    let url = item.as_str().ok_or("urls entries must be strings")?;
                    super::parse_url(url)?;
                    Ok(url.to_owned())
                })
                .collect::<Result<Vec<_>, String>>()?
        }
    };
    let domain = match present(args, "domain") {
        None => None,
        Some(value) => Some(normalize_domain(
            value.as_str().ok_or("domain must be a string")?,
        )?),
    };
    let include_values = match present(args, "include_values") {
        None => false,
        Some(value) => value.as_bool().ok_or("include_values must be a boolean")?,
    };
    Ok(CookieQuery {
        urls,
        domain,
        include_values,
    })
}

fn finish_cookies(args: &Value, result: &mut Value) -> Result<(), String> {
    let query = parse_cookie_query(args)?;
    let cookies = result
        .get_mut("cookies")
        .and_then(Value::as_array_mut)
        .ok_or("browser returned no cookie list")?;
    if let Some(domain) = &query.domain {
        cookies.retain(|cookie| cookie_reaches_domain(cookie, domain));
    }
    let total = cookies.len();
    cookies.truncate(MAX_COOKIES_RETURNED);
    if !query.include_values {
        for cookie in cookies.iter_mut() {
            if let Some(object) = cookie.as_object_mut() {
                object.remove("value");
            }
        }
    }
    result["count"] = json!(total);
    result["truncated"] = json!(total > MAX_COOKIES_RETURNED);
    Ok(())
}

/// True when a request to `host` would carry this cookie: the cookie domain equals the host or
/// is a parent domain of it.
fn cookie_reaches_domain(cookie: &Value, host: &str) -> bool {
    let Some(domain) = cookie.get("domain").and_then(Value::as_str) else {
        return false;
    };
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn parse_set_cookies(args: &Value) -> Result<Vec<Value>, String> {
    let items = args
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or("set_cookies requires a cookies array")?;
    if items.is_empty() || items.len() > MAX_COOKIE_BATCH {
        return Err(format!(
            "set_cookies accepts 1 to {MAX_COOKIE_BATCH} cookies"
        ));
    }
    items.iter().map(normalize_cookie).collect()
}

const COOKIE_FIELDS: &[&str] = &[
    "name", "value", "url", "domain", "path", "expires", "httpOnly", "secure", "sameSite",
];

/// Normalizes one cookie into the shape Playwright accepts: either `url`, or `domain` + `path`.
fn normalize_cookie(value: &Value) -> Result<Value, String> {
    let object = value.as_object().ok_or("each cookie must be an object")?;
    reject_unknown(object, COOKIE_FIELDS, "cookie")?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or("cookie name must be a string")?;
    validate_cookie_name(name)?;
    let cookie_value = object
        .get("value")
        .and_then(Value::as_str)
        .ok_or("cookie value must be a string")?;
    if cookie_value.chars().count() > MAX_COOKIE_VALUE_CHARS
        || cookie_value.chars().any(|c| c.is_control() || c == ';')
    {
        return Err(format!(
            "cookie value must be at most {MAX_COOKIE_VALUE_CHARS} characters without control characters or ';'"
        ));
    }
    let mut out = Map::new();
    out.insert("name".into(), json!(name));
    out.insert("value".into(), json!(cookie_value));
    match (field(object, "url"), field(object, "domain")) {
        (Some(url), None) => {
            let url = url.as_str().ok_or("cookie url must be a string")?;
            super::parse_url(url)?;
            out.insert("url".into(), json!(url));
        }
        (None, Some(domain)) => {
            let domain =
                normalize_domain(domain.as_str().ok_or("cookie domain must be a string")?)?;
            let path = match field(object, "path") {
                None => "/".to_owned(),
                Some(path) => {
                    let path = path.as_str().ok_or("cookie path must be a string")?;
                    if !path.starts_with('/')
                        || path.chars().count() > MAX_COOKIE_PATH_CHARS
                        || path.chars().any(char::is_control)
                    {
                        return Err(
                            "cookie path must start with '/' and contain no control characters"
                                .into(),
                        );
                    }
                    path.to_owned()
                }
            };
            out.insert("domain".into(), json!(domain));
            out.insert("path".into(), json!(path));
        }
        _ => return Err("each cookie requires exactly one of url or domain".into()),
    }
    if let Some(expires) = field(object, "expires") {
        let seconds = expires
            .as_f64()
            .filter(|n| n.is_finite() && *n >= -1.0)
            .ok_or("cookie expires must be a Unix time in seconds, or -1 for a session cookie")?;
        out.insert("expires".into(), json!(seconds));
    }
    for flag in ["httpOnly", "secure"] {
        if let Some(value) = field(object, flag) {
            let flag_value = value
                .as_bool()
                .ok_or_else(|| format!("cookie {flag} must be a boolean"))?;
            out.insert(flag.into(), json!(flag_value));
        }
    }
    if let Some(same_site) = field(object, "sameSite") {
        let canonical = match same_site.as_str().map(str::to_ascii_lowercase).as_deref() {
            Some("strict") => "Strict",
            Some("lax") => "Lax",
            Some("none") => "None",
            _ => return Err("cookie sameSite must be Strict, Lax or None".into()),
        };
        out.insert("sameSite".into(), json!(canonical));
    }
    Ok(Value::Object(out))
}

/// RFC 6265 token characters: printable ASCII excluding separators.
fn validate_cookie_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.chars().count() <= MAX_COOKIE_NAME_CHARS
        && name
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?={}".contains(&b));
    if valid {
        Ok(())
    } else {
        Err("cookie names must be 1-256 RFC 6265 token characters".into())
    }
}

fn normalize_domain(raw: &str) -> Result<String, String> {
    let domain = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    let valid = !domain.is_empty()
        && domain.chars().count() <= MAX_DOMAIN_CHARS
        && domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    if valid {
        Ok(domain)
    } else {
        Err("domain must be a hostname such as example.com".into())
    }
}

fn parse_clear_names(args: &Value) -> Result<Option<Vec<String>>, String> {
    let Some(value) = present(args, "names") else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .ok_or("names must be an array of cookie names")?;
    if items.is_empty() || items.len() > MAX_COOKIE_BATCH {
        return Err(format!(
            "names accepts 1 to {MAX_COOKIE_BATCH} cookie names; omit it to clear all cookies"
        ));
    }
    items
        .iter()
        .map(|item| {
            let name = item.as_str().ok_or("cookie names must be strings")?;
            validate_cookie_name(name)?;
            Ok(name.to_owned())
        })
        .collect::<Result<Vec<_>, String>>()
        .map(Some)
}

fn parse_storage(args: &Value) -> Result<Value, String> {
    let area = match args.get("area").and_then(Value::as_str) {
        Some("local") => "local",
        Some("session") => "session",
        _ => return Err("storage area must be local or session".into()),
    };
    let operation = match args.get("operation").and_then(Value::as_str) {
        Some(op @ ("get" | "set" | "clear")) => op,
        _ => return Err("storage operation must be get, set or clear".into()),
    };
    let key = match present(args, "key") {
        None => None,
        Some(value) => {
            let key = value.as_str().ok_or("storage key must be a string")?;
            if key.is_empty() || key.chars().count() > MAX_STORAGE_KEY_CHARS {
                return Err(format!(
                    "storage key must be 1-{MAX_STORAGE_KEY_CHARS} characters"
                ));
            }
            Some(key.to_owned())
        }
    };
    let value = present(args, "value");
    let mut out = Map::new();
    out.insert("area".into(), json!(area));
    out.insert("operation".into(), json!(operation));
    match operation {
        "set" => {
            let key = key.ok_or("storage set requires key")?;
            let value = value
                .and_then(Value::as_str)
                .ok_or("storage set requires a string value")?;
            if value.chars().count() > MAX_STORAGE_VALUE_CHARS {
                return Err(format!(
                    "storage value must be at most {MAX_STORAGE_VALUE_CHARS} characters"
                ));
            }
            out.insert("key".into(), json!(key));
            out.insert("value".into(), json!(value));
        }
        "get" => {
            if value.is_some() {
                return Err("storage get does not accept value".into());
            }
            if let Some(key) = key {
                out.insert("key".into(), json!(key));
            }
        }
        _ => {
            if key.is_some() || value.is_some() {
                return Err(
                    "storage clear empties the whole area and takes no key or value".into(),
                );
            }
        }
    }
    Ok(Value::Object(out))
}

fn validate_clipboard_text(args: &Value) -> Result<(), String> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or("clipboard_write requires text")?;
    if text.chars().count() > MAX_CLIPBOARD_TEXT_CHARS {
        return Err(format!(
            "clipboard text must be at most {MAX_CLIPBOARD_TEXT_CHARS} characters"
        ));
    }
    Ok(())
}

/// Validates a loaded state file and returns only the fields the driver restores.
fn parse_loaded_state(stored: &Value) -> Result<Value, String> {
    let object = stored
        .as_object()
        .ok_or("stored browser state is not an object")?;
    let cookies = object
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or("stored browser state has no cookies array")?;
    if cookies.len() > MAX_LOADED_COOKIES {
        return Err(format!(
            "stored browser state has more than {MAX_LOADED_COOKIES} cookies"
        ));
    }
    let cookies = cookies
        .iter()
        .map(normalize_cookie)
        .collect::<Result<Vec<_>, String>>()?;
    let origins = object
        .get("origins")
        .and_then(Value::as_array)
        .ok_or("stored browser state has no origins array")?;
    if origins.len() > MAX_LOADED_ORIGINS {
        return Err(format!(
            "stored browser state has more than {MAX_LOADED_ORIGINS} origins"
        ));
    }
    let origins = origins
        .iter()
        .map(parse_loaded_origin)
        .collect::<Result<Vec<_>, String>>()?;
    Ok(json!({"cookies": cookies, "origins": origins}))
}

fn parse_loaded_origin(value: &Value) -> Result<Value, String> {
    let object = value.as_object().ok_or("each origin must be an object")?;
    let origin = object
        .get("origin")
        .and_then(Value::as_str)
        .ok_or("origin must be a string")?;
    super::parse_url(origin)?;
    let entries = match object.get("localStorage") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            if items.len() > MAX_LOCAL_STORAGE_ENTRIES {
                return Err(format!(
                    "origin {origin} has more than {MAX_LOCAL_STORAGE_ENTRIES} localStorage entries"
                ));
            }
            items
                .iter()
                .map(|item| {
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or("localStorage entries need a string name")?;
                    let value = item
                        .get("value")
                        .and_then(Value::as_str)
                        .ok_or("localStorage entries need a string value")?;
                    if name.chars().count() > MAX_STORAGE_KEY_CHARS
                        || value.chars().count() > MAX_STORAGE_VALUE_CHARS
                    {
                        return Err("localStorage entry exceeds the storage size limit".into());
                    }
                    Ok(json!({"name": name, "value": value}))
                })
                .collect::<Result<Vec<_>, String>>()?
        }
        Some(_) => return Err("localStorage must be an array".into()),
    };
    Ok(json!({"origin": origin, "localStorage": entries}))
}

fn required_name(args: &Value) -> Result<&str, String> {
    args.get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "this action requires name".to_owned())
}

/// `<state>/browser/states/<name>.json`, kept apart from the session profiles.
fn state_path(state_dir: &Path, name: &str) -> Result<PathBuf, String> {
    profile::validate_profile_name(name)?;
    Ok(state_dir
        .join("browser")
        .join("states")
        .join(format!("{name}.json")))
}

fn present<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|value| !value.is_null())
}

fn field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    object.get(key).filter(|value| !value.is_null())
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str], name: &str) -> Result<(), String> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("{name} does not accept {key:?}")),
        None => Ok(()),
    }
}
