//! Small, public-API-backed handlers for common documentation and discussion sites.
//! All requests use the guarded client; failure falls back to ordinary page fetching.
use super::fetch::Client;
use serde_json::Value;

const JSON: &str = "application/json";

/// Return a readable API representation for supported URLs, or `None` when the URL is not handled.
pub fn handle(url: &str, client: &Client) -> Option<String> {
    let parsed = super::fetch::parse_url(url).ok()?;
    let host = parsed.host()?.to_ascii_lowercase();
    let path = parsed.path();
    let api = if host == "github.com" {
        github_api(path)?
    } else if host == "www.npmjs.com" || host == "npmjs.com" {
        let name = path.strip_prefix("/package/")?.trim_matches('/');
        if name.is_empty() {
            return None;
        }
        format!("https://registry.npmjs.org/{}", encode_path(name))
    } else if host == "crates.io" || host == "www.crates.io" {
        let name = path
            .strip_prefix("/crates/")?
            .trim_matches('/')
            .split('/')
            .next()?;
        format!("https://crates.io/api/v1/crates/{}", encode_path(name))
    } else if host == "pypi.org" || host == "www.pypi.org" {
        let name = path
            .strip_prefix("/project/")?
            .trim_matches('/')
            .split('/')
            .next()?;
        format!("https://pypi.org/pypi/{}/json", encode_path(name))
    } else if host == "stackoverflow.com" || host == "www.stackoverflow.com" {
        let id = path.split('/').nth(2)?;
        if !path.starts_with("/questions/") || !id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        format!(
            "https://api.stackexchange.com/2.3/questions/{id}?site=stackoverflow&filter=withbody"
        )
    } else {
        return None;
    };
    let response = client.get(&api, JSON, None).ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    let value: Value = serde_json::from_slice(&response.body).ok()?;
    Some(format!(
        "Site API data ({})\n{}",
        response.url,
        summarize(&value)
    ))
}

fn github_api(path: &str) -> Option<String> {
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return None;
    }
    let owner = encode_path(parts[0]);
    let repo = encode_path(parts[1]);
    let endpoint = match parts.get(2).copied() {
        None | Some("") => format!("repos/{owner}/{repo}"),
        Some("issues") if parts.len() >= 4 => {
            format!("repos/{owner}/{repo}/issues/{}", encode_path(parts[3]))
        }
        Some("pull") if parts.len() >= 4 => {
            format!("repos/{owner}/{repo}/pulls/{}", encode_path(parts[3]))
        }
        Some("blob") if parts.len() >= 4 => {
            let branch = encode_path(parts[3]);
            let file = parts[4..]
                .iter()
                .map(|p| encode_path(p))
                .collect::<Vec<_>>()
                .join("/");
            if file.is_empty() {
                return None;
            }
            return Some(format!(
                "https://api.github.com/repos/{owner}/{repo}/contents/{file}?ref={branch}"
            ));
        }
        _ => return None,
    };
    Some(format!("https://api.github.com/{endpoint}"))
}

fn encode_path(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn summarize(value: &Value) -> String {
    match value {
        Value::Object(map) => map
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "body" | "body_html" | "description" | "readme"
                )
            })
            .map(|(key, value)| format!("{key}: {}", compact(value)))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => compact(value),
    }
}
fn compact(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(a) => a.iter().map(compact).collect::<Vec<_>>().join("\n"),
        _ => value.to_string(),
    }
}
