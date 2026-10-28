//! Browser request log: pure parsing and shaping for `requests`, `request`, `har_start` and
//! `har_stop`. The driver owns the live Playwright objects and the bounded per-tab ring; this module
//! validates model arguments and shapes what the driver returns. Page-supplied headers, URLs and bodies
//! are untrusted: sensitive headers are redacted and text is sanitized and bounded before it reaches
//! the model or a HAR file on disk.

use serde_json::{Map, Value, json};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// Records returned by one `requests` call when `limit` is omitted.
pub const REQUESTS_DEFAULT_LIMIT: u64 = 50;
/// Largest `limit` accepted by `requests`; matches the driver's per-tab ring.
pub const REQUESTS_MAX_LIMIT: u64 = 200;
/// Response-body bytes returned inline by `request`.
pub const REQUEST_BODY_MAX_BYTES: usize = 32 * 1024;
/// Response-body bytes embedded per HAR entry.
pub const HAR_BODY_MAX_BYTES: usize = 64 * 1024;
/// Response-body bytes embedded across one HAR file.
pub const HAR_TOTAL_BODY_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Longest URL shown by `requests` and `request`, in bytes.
const URL_SHOWN_MAX_BYTES: usize = 2048;
/// Longest URL shown by `requests` in list form, in bytes.
const URL_LIST_MAX_BYTES: usize = 512;
/// Longest header value kept, in bytes.
const HEADER_VALUE_MAX_BYTES: usize = 1024;
/// Replacement for a sensitive header value.
const REDACTED: &str = "[redacted]";
static NULL: Value = Value::Null;

const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
    "x-csrf-token",
    "x-xsrf-token",
];
const SENSITIVE_HEADER_FRAGMENTS: &[&str] = &["token", "secret", "password", "api-key", "apikey"];
/// Query-parameter names whose values are redacted. A name matches when one of its words (split on
/// non-alphanumeric characters) is listed, or when its letters alone equal a listed name with
/// underscores removed (`accessToken` matches `access_token`).
const SENSITIVE_QUERY_NAMES: &[&str] = &[
    "token",
    "key",
    "secret",
    "password",
    "auth",
    "sig",
    "signature",
    "code",
    "session",
    "credential",
    "access_token",
    "api_key",
];
/// Response headers that carry a URL whose query may hold credentials.
const URL_HEADERS: &[&str] = &["location", "referer"];

/// Parse and validate the arguments of a request-log action into the driver's request fields.
pub fn parse(action: &str, args: &Value) -> Result<Value, String> {
    match action {
        "requests" => {
            let limit = match args.get("limit") {
                None => REQUESTS_DEFAULT_LIMIT,
                Some(value) => value
                    .as_u64()
                    .filter(|limit| (1..=REQUESTS_MAX_LIMIT).contains(limit))
                    .ok_or("requests limit must be an integer from 1 through 200")?,
            };
            Ok(json!({
                "limit": limit,
                "filter": optional_text(args, "filter", URL_SHOWN_MAX_BYTES)?,
                "resource_type": optional_text(args, "resource_type", 32)?,
            }))
        }
        "request" => {
            let id = args
                .get("request_id")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0)
                .ok_or("request requires request_id, a positive id from requests")?;
            Ok(json!({"request_id": id}))
        }
        "har_start" => {
            let content = match args.get("content") {
                None => "text",
                Some(value) => match value.as_str() {
                    Some(policy @ ("text" | "all" | "none")) => policy,
                    _ => return Err("har_start content must be text, all, or none".into()),
                },
            };
            Ok(json!({"content": content}))
        }
        "har_stop" => Ok(json!({})),
        _ => Err(format!("unsupported request log action {action:?}")),
    }
}

fn optional_text(args: &Value, key: &str, max_bytes: usize) -> Result<Value, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::String(text)) if !text.is_empty() && text.len() <= max_bytes => Ok(json!(text)),
        Some(_) => Err(format!(
            "{key} must be a non-empty string up to {max_bytes} bytes"
        )),
    }
}

/// Shape the driver's `requests` reply for the model.
pub fn shape_requests(result: &Value) -> Value {
    let records: Vec<Value> = result["records"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|raw| Value::Object(shape_record(raw, URL_LIST_MAX_BYTES)))
        .collect();
    json!({"count": records.len(), "requests": records, "untrusted": true})
}

/// Shape the driver's `request` reply: one record plus its response body when textual.
pub fn shape_request(result: &Value) -> Value {
    let mut out = shape_record(&result["record"], URL_SHOWN_MAX_BYTES);
    let content_type = result["content_type"].as_str().unwrap_or("");
    if !content_type.is_empty() {
        out.insert(
            "content_type".into(),
            json!(bound_text(content_type, 256).0),
        );
    }
    out.insert("body_bytes".into(), result["body_bytes"].clone());
    match result["body"].as_str() {
        Some(body) if is_textual_content_type(content_type) => {
            let (text, truncated) = bound_text(body, REQUEST_BODY_MAX_BYTES);
            out.insert("body".into(), json!(text));
            out.insert("body_truncated".into(), json!(truncated));
        }
        Some(_) => {
            out.insert("body_omitted".into(), json!("binary"));
        }
        None => {
            out.insert("body_omitted".into(), json!("unavailable"));
        }
    }
    out.insert("untrusted".into(), json!(true));
    Value::Object(out)
}

/// Build a HAR 1.2 document from the driver's window of records, write it under `artifacts`, and
/// return the file path with counts. Sensitive headers are redacted; embedded text bodies are bounded
/// per entry and across the file.
pub fn write_har(result: &Value, artifacts: &Path) -> Result<Value, String> {
    let mut bodies_left = HAR_TOTAL_BODY_MAX_BYTES;
    let mut bodies_omitted = result["bodies_omitted"].as_u64().unwrap_or(0);
    let entries: Vec<Value> = result["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|raw| har_entry(raw, &mut bodies_left, &mut bodies_omitted))
        .collect();
    let count = entries.len();
    let document = json!({"log": {
        "version": "1.2",
        "creator": {"name": "dial-browser", "version": "1"},
        "pages": [],
        "entries": entries,
    }});
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("clock before epoch: {e}"))?
        .as_nanos();
    let path = artifacts.join(format!("har-{stamp}.har"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("creating HAR file: {e}"))?;
    file.write_all(document.to_string().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("writing HAR file: {e}"))?;
    Ok(json!({
        "path": path.display().to_string(),
        "entries": count,
        "dropped": result["dropped"].clone(),
        "bodies_omitted": bodies_omitted,
        "content": result["content"].clone(),
    }))
}

/// Whether a Content-Type carries text (text/*, JSON, XML, JavaScript, form data, SVG).
pub fn is_textual_content_type(content_type: &str) -> bool {
    let kind = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    kind.starts_with("text/")
        || kind.ends_with("+json")
        || kind.ends_with("+xml")
        || matches!(
            kind.as_str(),
            "application/json"
                | "application/javascript"
                | "application/xml"
                | "application/x-www-form-urlencoded"
                | "image/svg+xml"
        )
}

/// Drop control characters except tab, newline and carriage return, then cut at a character
/// boundary no later than `max_bytes`. Returns the text and whether it was cut.
pub fn bound_text(raw: &str, max_bytes: usize) -> (String, bool) {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
        .collect();
    if cleaned.len() <= max_bytes {
        return (cleaned, false);
    }
    let mut end = max_bytes;
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    (cleaned[..end].to_owned(), true)
}

/// Whether a header name carries credentials whose value must never reach the model or disk.
pub fn is_sensitive_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_HEADERS.contains(&lower.as_str())
        || SENSITIVE_HEADER_FRAGMENTS
            .iter()
            .any(|fragment| lower.contains(fragment))
}

fn redact_headers(raw: &Value) -> Value {
    let mut out = Map::new();
    for (name, value) in raw.as_object().into_iter().flatten() {
        let Some(text) = value.as_str() else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let shown = if is_sensitive_header(name) {
            REDACTED.to_owned()
        } else if URL_HEADERS.contains(&lower.as_str()) {
            bound_text(&redact_url_query(text), HEADER_VALUE_MAX_BYTES).0
        } else {
            bound_text(text, HEADER_VALUE_MAX_BYTES).0
        };
        out.insert(name.clone(), json!(shown));
    }
    Value::Object(out)
}

fn shape_record(raw: &Value, url_max_bytes: usize) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("id".into(), raw["id"].clone());
    out.insert(
        "method".into(),
        json!(bound_text(raw["method"].as_str().unwrap_or(""), 16).0),
    );
    out.insert(
        "url".into(),
        json!(
            bound_text(
                &redact_url_query(raw["url"].as_str().unwrap_or("")),
                url_max_bytes
            )
            .0
        ),
    );
    out.insert(
        "resource_type".into(),
        json!(bound_text(raw["resource_type"].as_str().unwrap_or(""), 32).0),
    );
    out.insert("status".into(), raw["status"].clone());
    out.insert("duration_ms".into(), raw["duration_ms"].clone());
    out.insert(
        "failure".into(),
        raw["failure"]
            .as_str()
            .map_or(Value::Null, |text| json!(bound_text(text, 512).0)),
    );
    out.insert(
        "started".into(),
        json!(bound_text(raw["started"].as_str().unwrap_or(""), 64).0),
    );
    out.insert(
        "request_headers".into(),
        redact_headers(&raw["request_headers"]),
    );
    out.insert(
        "response_headers".into(),
        if raw["response_headers"].is_object() {
            redact_headers(&raw["response_headers"])
        } else {
            Value::Null
        },
    );
    out.insert("response_bytes".into(), raw["response_bytes"].clone());
    out
}

fn har_entry(raw: &Value, bodies_left: &mut usize, bodies_omitted: &mut u64) -> Value {
    let shaped = shape_record(raw, usize::MAX);
    let field = |key: &str| shaped.get(key).unwrap_or(&NULL);
    let url = field("url").as_str().unwrap_or("");
    let response_headers = match field("response_headers") {
        Value::Object(_) => field("response_headers").clone(),
        _ => json!({}),
    };
    let mime_type = header_value(&response_headers, "content-type").unwrap_or_default();
    let response_bytes = field("response_bytes").as_u64();
    let mut content = json!({
        "size": response_bytes.unwrap_or(0),
        "mimeType": mime_type,
    });
    if let Some(body) = raw["body"]
        .as_str()
        .filter(|_| is_textual_content_type(&mime_type))
    {
        let (text, _) = bound_text(body, HAR_BODY_MAX_BYTES);
        if text.len() <= *bodies_left {
            *bodies_left -= text.len();
            content["text"] = json!(text);
        } else {
            *bodies_omitted += 1;
        }
    } else if let Some(encoded) = raw["body_base64"].as_str() {
        if encoded.len() <= *bodies_left {
            *bodies_left -= encoded.len();
            content["text"] = json!(encoded);
            content["encoding"] = json!("base64");
        } else {
            *bodies_omitted += 1;
        }
    }
    let status = field("status").as_u64().unwrap_or(0);
    json!({
        "startedDateTime": field("started"),
        "time": field("duration_ms").as_f64().unwrap_or(0.0),
        "request": {
            "method": field("method"),
            "url": url,
            "httpVersion": "HTTP/1.1",
            "headers": name_values(field("request_headers")),
            "queryString": query_pairs(url),
            "cookies": [],
            "headersSize": -1,
            "bodySize": -1,
        },
        "response": {
            "status": status,
            "statusText": "",
            "httpVersion": "HTTP/1.1",
            "headers": name_values(&response_headers),
            "cookies": [],
            "content": content,
            "redirectURL": header_value(&response_headers, "location").unwrap_or_default(),
            "headersSize": -1,
            "bodySize": response_bytes.map_or(-1, |bytes| i64::try_from(bytes).unwrap_or(-1)),
        },
        "cache": {},
        "timings": {"send": 0, "wait": field("duration_ms").as_f64().unwrap_or(0.0), "receive": 0},
    })
}

fn header_value(headers: &Value, name: &str) -> Option<String> {
    headers
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str().map(str::to_owned))
}

fn name_values(headers: &Value) -> Value {
    Value::Array(
        headers
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, value)| json!({"name": name, "value": value.as_str().unwrap_or("")}))
            .collect(),
    )
}

fn query_pairs(url: &str) -> Value {
    let pairs: Vec<Value> = url::Url::parse(url)
        .map(|parsed| {
            parsed
                .query_pairs()
                .map(|(name, value)| json!({"name": name, "value": value}))
                .collect()
        })
        .unwrap_or_default();
    Value::Array(pairs)
}

/// Whether a query parameter name carries a credential whose value must not reach the model or disk.
fn is_sensitive_query_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let letters: String = lower.chars().filter(char::is_ascii_alphanumeric).collect();
    SENSITIVE_QUERY_NAMES.iter().any(|word| {
        letters == word.replace('_', "")
            || lower
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|part| part == *word)
    })
}

/// Replaces the value of every sensitive query parameter with `[redacted]`. Parameter names,
/// their order, and every other byte of the URL stay as they were.
pub fn redact_url_query(url: &str) -> String {
    let Some((base, rest)) = url.split_once('?') else {
        return url.to_owned();
    };
    let (query, fragment) = match rest.split_once('#') {
        Some((query, fragment)) => (query, Some(fragment)),
        None => (rest, None),
    };
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| {
            let raw_name = pair.split_once('=').map_or(pair, |(name, _)| name);
            let name = url::form_urlencoded::parse(raw_name.as_bytes())
                .next()
                .map_or_else(|| raw_name.to_owned(), |(decoded, _)| decoded.into_owned());
            if is_sensitive_query_name(&name) {
                format!("{raw_name}={REDACTED}")
            } else {
                pair.to_owned()
            }
        })
        .collect();
    let mut out = format!("{base}?{}", pairs.join("&"));
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

#[cfg(test)]
mod tests;
