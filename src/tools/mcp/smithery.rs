//! Smithery registry: search,
//! install planning and API-key login. Slash-command glue lives in `commands/smithery.rs`.

use crate::config::types::McpServerConfig;
use crate::storage::settings_store::{Change, read_private, write_private};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const REGISTRY_URL: &str = "https://registry.smithery.ai";

/// Registry base URL. `SMITHERY_REGISTRY_URL` overrides it, for tests and local fakes.
pub fn registry_url() -> String {
    std::env::var("SMITHERY_REGISTRY_URL").unwrap_or_else(|_| REGISTRY_URL.to_owned())
}
const DEFAULT_AUTH_URL: &str = "https://smithery.ai";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
pub const AUTH_DEADLINE: Duration = Duration::from_secs(300);
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;
const MAX_PAGES: usize = 3;
const KEY_ENV: &str = "SMITHERY_API_KEY";
const KEY_FILE: &str = "smithery.json";

/// Registry or auth failure. `status` is the HTTP status, or 0 for transport failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmitheryError {
    pub message: String,
    pub status: u16,
}

impl std::fmt::Display for SmitheryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

// ---- HTTP -----------------------------------------------------------------------------------

struct Reply {
    status: u16,
    reason: String,
    body: String,
}

fn new_agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build();
    ureq::Agent::new_with_config(config)
}

fn read_reply(mut response: ureq::http::Response<ureq::Body>) -> Result<Reply, ureq::Error> {
    let status = response.status();
    let reason = status.canonical_reason().unwrap_or_default().to_owned();
    let body = response.body_mut().read_to_string()?;
    Ok(Reply {
        status: status.as_u16(),
        reason,
        body,
    })
}

fn get(url: &str, api_key: Option<&str>, timeout: Duration) -> Result<Reply, ureq::Error> {
    let agent = new_agent(timeout);
    let mut request = agent.get(url);
    if let Some(key) = api_key {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    read_reply(request.call()?)
}

fn post_empty(url: &str, timeout: Duration) -> Result<Reply, ureq::Error> {
    let agent = new_agent(timeout);
    read_reply(agent.post(url).send("")?)
}

fn is_timeout(error: &ureq::Error) -> bool {
    matches!(error, ureq::Error::Timeout(_))
}

fn transport_error(error: ureq::Error) -> SmitheryError {
    let message = if is_timeout(&error) {
        "Smithery search timed out after 10s".to_owned()
    } else {
        format!("Smithery search failed: {error}")
    };
    SmitheryError { message, status: 0 }
}

// ---- Registry search ------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub limit: usize,
    pub api_key: Option<String>,
    pub semantic: bool,
}

/// A registry server with a usable connection, mapped into a search result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Qualified name without the leading `@`, e.g. `acme/notes`.
    pub name: String,
    /// Qualified name with a leading `@`, e.g. `@acme/notes`.
    pub qualified_name: String,
    pub display_name: String,
    pub description: String,
    pub verified: bool,
    pub deployed: bool,
    pub use_count: f64,
    /// Connection `type` of the config (`stdio`, `http`, ...), `stdio` by default.
    pub transport: String,
    pub connection_type: String,
    /// `remote` for direct or remote servers, `package` otherwise.
    pub source_type: &'static str,
    /// `YYYY-MM-DD` of the registry `createdAt`, when it is a valid date.
    pub created_at: Option<String>,
    pub homepage: Option<String>,
    pub tools: Vec<ToolInfo>,
    pub warnings: Vec<String>,
    /// Every config input declared by the connection's schema.
    pub inputs: Vec<SchemaInput>,
    /// Connection config: `{"type":"http","url":..}` or `{"type":"stdio","command":"bunx",..}`.
    pub config: Value,
}

/// A tool advertised by a registry server (`display.tools` in the registry response).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub params: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SchemaInput {
    pub key: String,
    /// `number`, `boolean` or `string`.
    pub input_type: &'static str,
    pub required: bool,
    pub default: Option<String>,
    pub enum_values: Vec<String>,
    pub description: Option<String>,
}

/// Searches the registry at `REGISTRY_URL`.
pub fn search(keyword: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SmitheryError> {
    search_at(&registry_url(), keyword, opts)
}

pub fn search_at(
    registry: &str,
    keyword: &str,
    opts: &SearchOptions,
) -> Result<Vec<SearchResult>, SmitheryError> {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        return Ok(Vec::new());
    }
    let limit = opts.limit.clamp(1, MAX_LIMIT);
    let page_size = (limit * 2).max(DEFAULT_LIMIT);
    let key = opts.api_key.as_deref();
    let mut entries: Vec<Value> = Vec::new();
    for page in 1..=MAX_PAGES {
        let url =
            registry_search_url(registry, keyword, page_size, page).map_err(|e| SmitheryError {
                message: format!("Smithery search failed: {e}"),
                status: 0,
            })?;
        let reply = get(&url, key, SEARCH_TIMEOUT).map_err(transport_error)?;
        if !(200..300).contains(&reply.status) {
            return Err(SmitheryError {
                message: format!("Smithery search failed with status {}", reply.status),
                status: reply.status,
            });
        }
        let body: Value = serde_json::from_str(&reply.body).map_err(|e| SmitheryError {
            message: format!("Smithery search returned invalid JSON: {e}"),
            status: reply.status,
        })?;
        let servers = body
            .get("servers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if servers.is_empty() {
            break;
        }
        let page_len = servers.len();
        entries.extend(servers);
        let counted = if opts.semantic {
            entries.len()
        } else {
            entries
                .iter()
                .filter(|e| matches_identity(e, keyword))
                .count()
        };
        if counted >= limit * 2 || page_len < page_size {
            break;
        }
    }

    let mut kept: Vec<Value> = entries
        .into_iter()
        .filter(|e| opts.semantic || matches_identity(e, keyword))
        .collect();
    if !opts.semantic {
        kept.sort_by(|a, b| {
            use_count(b)
                .partial_cmp(&use_count(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let mut seen = HashSet::new();
    let unique: Vec<Value> = kept
        .into_iter()
        .filter(|e| identity_key(e).is_some_and(|k| seen.insert(k)))
        .collect();

    let mut results: Vec<SearchResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = unique
            .iter()
            .map(|entry| scope.spawn(move || resolve_entry(registry, entry, key)))
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok().flatten())
            .collect()
    });
    results.truncate(limit);
    Ok(results)
}

/// One registry page for interactive browsing. Listing metadata is enough to show a server;
/// connection details are resolved only when installing, so unavailable details cannot hide it.
pub fn browse_page_at(
    registry: &str,
    keyword: &str,
    page: usize,
    api_key: Option<&str>,
) -> Result<(Vec<super::catalog::Candidate>, bool), SmitheryError> {
    let url =
        registry_search_url(registry, keyword.trim(), DEFAULT_LIMIT, page.max(1)).map_err(|e| {
            SmitheryError {
                message: e.to_string(),
                status: 0,
            }
        })?;
    let reply = get(&url, api_key, SEARCH_TIMEOUT).map_err(transport_error)?;
    if !(200..300).contains(&reply.status) {
        return Err(SmitheryError {
            message: format!("Smithery catalog returned status {}", reply.status),
            status: reply.status,
        });
    }
    let body: Value = serde_json::from_str(&reply.body).map_err(|e| SmitheryError {
        message: format!("Smithery catalog returned invalid JSON: {e}"),
        status: reply.status,
    })?;
    let entries = body
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| SmitheryError {
            message: "Smithery catalog response has no servers array".into(),
            status: reply.status,
        })?;
    let more = body
        .pointer("/pagination/totalPages")
        .and_then(Value::as_u64)
        .map(|total| (page.max(1) as u64) < total)
        .unwrap_or(entries.len() >= DEFAULT_LIMIT);
    let mut seen = HashSet::new();
    let results = entries
        .iter()
        .filter_map(listing_candidate)
        .filter(|candidate| seen.insert(candidate.name.clone()))
        .collect();
    Ok((results, more))
}

/// Discovery is not install validation. Reuse the common candidate shape without inventing a
/// connection or fetching per-server details; the Smithery install flow resolves those later.
fn listing_candidate(entry: &Value) -> Option<super::catalog::Candidate> {
    use super::catalog::{Candidate, Install, Source};
    let raw_name = safe_metadata(entry.get("qualifiedName")).or_else(|| {
        Some(format!(
            "{}/{}",
            safe_metadata(entry.get("namespace"))?,
            safe_metadata(entry.get("slug"))?
        ))
    })?;
    if raw_name.trim_start_matches('@').is_empty() {
        return None;
    }
    let name = normalize_qualified_name(&raw_name);
    Some(Candidate {
        title: safe_metadata(entry.get("displayName")).unwrap_or_else(|| name.clone()),
        name,
        description: safe_metadata(entry.get("description"))
            .unwrap_or_else(|| "No description".into()),
        sources: vec![Source::Smithery],
        badges: if entry.get("verified").and_then(Value::as_bool) == Some(true) {
            vec!["verified".into()]
        } else {
            Vec::new()
        },
        install: Install::Blocked(
            "Smithery servers install from /extensions, which asks for their settings".into(),
        ),
        inputs: Vec::new(),
    })
}

fn registry_search_url(base: &str, keyword: &str, page_size: usize, page: usize) -> Result<String> {
    let mut url = url::Url::parse(&format!("{}/servers", base.trim_end_matches('/')))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("q", keyword)
            .append_pair("pageSize", &page_size.to_string());
        if page > 1 {
            query.append_pair("page", &page.to_string());
        }
    }
    Ok(url.to_string())
}

pub(crate) fn resolve_entry(
    registry: &str,
    entry: &Value,
    key: Option<&str>,
) -> Option<SearchResult> {
    for candidate in candidate_paths(entry) {
        match fetch_details_at(registry, &candidate, key) {
            Ok(Some(details)) => return to_search_result(entry, &details),
            Ok(None) | Err(_) => {}
        }
    }
    None
}

/// `GET /servers/<path>` at `REGISTRY_URL`. `Ok(None)` for non-2xx, like a `null` response.
pub fn fetch_details(path: &str, api_key: Option<&str>) -> Result<Option<Value>, SmitheryError> {
    fetch_details_at(&registry_url(), path, api_key)
}

pub(crate) fn fetch_details_at(
    registry: &str,
    path: &str,
    api_key: Option<&str>,
) -> Result<Option<Value>, SmitheryError> {
    let url = format!("{}/servers/{path}", registry.trim_end_matches('/'));
    let reply = get(&url, api_key, SEARCH_TIMEOUT).map_err(transport_error)?;
    if !(200..300).contains(&reply.status) {
        return Ok(None);
    }
    serde_json::from_str(&reply.body)
        .map(Some)
        .map_err(|e| SmitheryError {
            message: format!("Smithery server details returned invalid JSON: {e}"),
            status: reply.status,
        })
}

fn trimmed<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn has_text(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn use_count(entry: &Value) -> f64 {
    entry.get("useCount").and_then(Value::as_f64).unwrap_or(0.0)
}

/// Detail lookup paths in order: `namespace/slug`, `slug`, then the qualified name without `@`.
fn candidate_paths(entry: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let (Some(namespace), Some(slug)) = (trimmed(entry, "namespace"), trimmed(entry, "slug")) {
        out.push(format!("{namespace}/{slug}"));
    }
    if let Some(slug) = trimmed(entry, "slug") {
        out.push(slug.to_owned());
    }
    if let Some(qualified) = trimmed(entry, "qualifiedName") {
        let path = qualified.trim_start_matches('@');
        if !path.is_empty() {
            out.push(path.to_owned());
        }
    }
    let mut seen = HashSet::new();
    out.retain(|path| seen.insert(path.clone()));
    out
}

fn identity_key(entry: &Value) -> Option<String> {
    candidate_paths(entry)
        .into_iter()
        .next()
        .or_else(|| trimmed(entry, "id").map(|id| format!("id:{id}")))
}

fn matches_identity(entry: &Value, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let display = entry
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let qualified = entry
        .get("qualifiedName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    display.contains(&query) || qualified.contains(&query)
}

/// Picks the connection to use from a server's connection list.
struct Selected {
    connection: Value,
    direct: bool,
}

fn choose_connection(details: &Value) -> Option<Selected> {
    let connections = details
        .get("connections")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let http = connections.iter().find(|c| {
        c.get("type").and_then(Value::as_str) == Some("http") && has_text(c.get("deploymentUrl"))
    });
    if let Some(connection) = http
        && schema_inputs(connection.get("configSchema")).is_empty()
    {
        return Some(Selected {
            connection: connection.clone(),
            direct: true,
        });
    }
    if let Some(stdio) = connections
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("stdio"))
    {
        return Some(Selected {
            connection: stdio.clone(),
            direct: false,
        });
    }
    http.map(|connection| Selected {
        connection: connection.clone(),
        direct: false,
    })
}

fn create_config(qualified_name: &str, selected: &Selected) -> Value {
    if selected.direct
        && let Some(url) = selected
            .connection
            .get("deploymentUrl")
            .and_then(Value::as_str)
    {
        return json!({"type": "http", "url": url});
    }
    json!({
        "type": "stdio",
        "command": "bunx",
        "args": ["-y", "@smithery/cli", "run", qualified_name, "--config", "{}"],
    })
}

/// Maps a registry entry and its details to a search result, or `None` when it has no usable connection.
pub fn to_search_result(entry: &Value, details: &Value) -> Option<SearchResult> {
    trimmed(entry, "id")?;
    let fallback = format!(
        "{}/{}",
        trimmed(entry, "namespace").unwrap_or(""),
        trimmed(entry, "slug").unwrap_or("")
    );
    let raw_name = trimmed(details, "qualifiedName")
        .or(trimmed(entry, "qualifiedName"))
        .map(str::to_owned)
        .unwrap_or(fallback);
    let qualified_name = normalize_qualified_name(&raw_name);
    let name = qualified_name.trim_start_matches('@').to_owned();
    let display_name = safe_metadata(first_present(
        details.get("displayName"),
        entry.get("displayName"),
    ))
    .unwrap_or_else(|| name.clone());
    let description = safe_metadata(first_present(
        details.get("description"),
        entry.get("description"),
    ))
    .unwrap_or_else(|| "No description".to_owned());
    let selected = choose_connection(details)?;
    let inputs = schema_inputs(selected.connection.get("configSchema"));
    let config = create_config(&qualified_name, &selected);
    let transport = safe_metadata(config.get("type")).unwrap_or_else(|| "stdio".to_owned());
    let connection_type =
        safe_metadata(selected.connection.get("type")).unwrap_or_else(|| "unknown".to_owned());
    let mut warnings = Vec::new();
    if transport == "stdio" {
        warnings.push(
            "Runs through Smithery CLI at runtime (`bunx @smithery/cli run ...`).".to_owned(),
        );
    }
    if !inputs.is_empty() {
        warnings
            .push("Provider requires configuration input defined by Smithery schema.".to_owned());
    }
    let remote = selected.direct || details.get("remote").and_then(Value::as_bool) == Some(true);
    Some(SearchResult {
        name,
        qualified_name,
        display_name,
        description,
        verified: entry.get("verified").and_then(Value::as_bool) == Some(true),
        deployed: entry.get("isDeployed").and_then(Value::as_bool) == Some(true),
        use_count: entry.get("useCount").and_then(Value::as_f64).unwrap_or(0.0),
        transport,
        connection_type,
        source_type: if remote { "remote" } else { "package" },
        created_at: entry
            .get("createdAt")
            .and_then(Value::as_str)
            .and_then(date_label),
        homepage: safe_metadata(entry.get("homepage")),
        tools: tools_list(details.get("tools")),
        warnings,
        inputs,
        config,
    })
}

/// Reduces a timestamp to its `YYYY-MM-DD` prefix, if it is a valid ISO-8601 date.
fn date_label(value: &str) -> Option<String> {
    let date = value.get(..10)?;
    let shape_ok = date.bytes().enumerate().all(|(i, b)| {
        if i == 4 || i == 7 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        }
    });
    let month: u32 = date.get(5..7)?.parse().ok()?;
    let day: u32 = date.get(8..10)?.parse().ok()?;
    (shape_ok && (1..=12).contains(&month) && (1..=31).contains(&day)).then(|| date.to_owned())
}

/// Named tools with their description and input property names.
fn tools_list(tools: Option<&Value>) -> Vec<ToolInfo> {
    let Some(items) = tools.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = safe_metadata(item.get("name"))?;
            let params = item
                .get("inputSchema")
                .and_then(|schema| schema.get("properties"))
                .and_then(Value::as_object)
                .map(|properties| properties.keys().cloned().collect())
                .unwrap_or_default();
            Some(ToolInfo {
                name,
                description: safe_metadata(item.get("description")),
                params,
            })
        })
        .collect()
}

fn first_present<'a>(first: Option<&'a Value>, second: Option<&'a Value>) -> Option<&'a Value> {
    first
        .filter(|v| !v.is_null())
        .or(second.filter(|v| !v.is_null()))
}

/// One entry per `properties` key; `required` is taken from the `required` array.
fn schema_inputs(schema: Option<&Value>) -> Vec<SchemaInput> {
    let Some(properties) = schema
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let required: Vec<&str> = schema
        .and_then(|s| s.get("required"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    properties
        .iter()
        .map(|(key, property)| SchemaInput {
            key: key.clone(),
            input_type: match property.get("type").and_then(Value::as_str) {
                Some("number" | "integer") => "number",
                Some("boolean") => "boolean",
                _ => "string",
            },
            required: required.contains(&key.as_str()),
            default: scalar_string(property.get("default")),
            description: property
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            enum_values: property
                .get("enum")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| scalar_string(Some(v)))
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Strings as-is, other values as JSON, whitespace collapsed; empty is `None`.
pub fn safe_metadata(value: Option<&Value>) -> Option<String> {
    let raw = match value? {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

pub fn normalize_qualified_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.starts_with('@') {
        trimmed.to_owned()
    } else {
        format!("@{trimmed}")
    }
}

/// Lowercase slug of the qualified name, `mcp-server` when empty.
pub fn to_config_name(qualified_name: &str) -> String {
    let lowered = qualified_name.to_lowercase();
    let lowered = lowered.trim_start_matches('@');
    let mut out = String::new();
    let mut previous_dash = false;
    for c in lowered.chars() {
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-');
        let c = if keep { c } else { '-' };
        if c == '-' {
            if previous_dash {
                continue;
            }
            previous_dash = true;
        } else {
            previous_dash = false;
        }
        out.push(c);
    }
    let name = out.trim_matches('-');
    if name.is_empty() {
        "mcp-server".to_owned()
    } else {
        name.to_owned()
    }
}

/// `base`, or `base-2`…`base-999`, or `base-<millis>`, whichever is first free.
pub fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    for index in 2..1000 {
        let candidate = format!("{base}-{index}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{base}-{millis}")
}

// ---- Install --------------------------------------------------------------------------------

/// Builds the MCP server config for `result` from `--set key=value` pairs. Required inputs without a
/// default must be set, unknown keys are rejected, and values are typed by the schema.
pub fn plan_install(
    result: &SearchResult,
    sets: &BTreeMap<String, String>,
) -> Result<McpServerConfig> {
    for key in sets.keys() {
        if !result.inputs.iter().any(|input| &input.key == key) {
            bail!("Unknown Smithery input \"{key}\".");
        }
    }
    let mut values = serde_json::Map::new();
    for input in &result.inputs {
        let raw = match (sets.get(&input.key), &input.default) {
            (Some(value), _) => value.clone(),
            (None, Some(default)) => default.clone(),
            (None, None) if input.required => {
                bail!("Missing required value for \"{}\".", input.key)
            }
            (None, None) => continue,
        };
        values.insert(input.key.clone(), typed_value(input, &raw)?);
    }
    let mut config = result.config.clone();
    if config.get("type").and_then(Value::as_str) == Some("stdio") {
        let args = config
            .get_mut("args")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow!("Smithery stdio config has no args."))?;
        let position = args
            .iter()
            .position(|arg| arg.as_str() == Some("--config"))
            .ok_or_else(|| anyhow!("Smithery stdio config has no --config argument."))?;
        let slot = args
            .get_mut(position + 1)
            .ok_or_else(|| anyhow!("Smithery stdio config has no --config value."))?;
        *slot = Value::String(serde_json::to_string(&Value::Object(values))?);
    }
    to_server(&config)
}

fn typed_value(input: &SchemaInput, raw: &str) -> Result<Value> {
    if !input.enum_values.is_empty() && !input.enum_values.iter().any(|v| v == raw) {
        bail!(
            "Invalid value for \"{}\". Use one of: {}.",
            input.key,
            input.enum_values.join(", ")
        );
    }
    match input.input_type {
        "number" => {
            if let Ok(integer) = raw.parse::<i64>() {
                return Ok(json!(integer));
            }
            let float: f64 = raw
                .parse()
                .map_err(|_| anyhow!("Invalid number for \"{}\".", input.key))?;
            let number = serde_json::Number::from_f64(float)
                .ok_or_else(|| anyhow!("Invalid number for \"{}\".", input.key))?;
            Ok(Value::Number(number))
        }
        "boolean" => match raw {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => bail!("Invalid boolean for \"{}\"; use true or false.", input.key),
        },
        _ => Ok(Value::String(raw.to_owned())),
    }
}

fn to_server(config: &Value) -> Result<McpServerConfig> {
    let field = |name: &str| config.get(name).cloned().unwrap_or(Value::Null);
    let server = match config.get("type").and_then(Value::as_str) {
        Some("http") => json!({ "url": field("url") }),
        Some("stdio") => json!({ "program": field("command"), "args": field("args") }),
        _ => bail!("Unsupported Smithery connection type."),
    };
    serde_json::from_value(server).context("Smithery produced an invalid MCP server config.")
}

pub fn config_change(name: &str, server: &McpServerConfig) -> Result<Change> {
    Ok(Change::Set {
        path: format!("mcp.{name}"),
        value: toml::Value::try_from(server)?,
    })
}

// ---- Login ----------------------------------------------------------------------------------

/// Auth base from `SMITHERY_URL` (trailing `/` removed), default `https://smithery.ai`.
pub fn auth_base() -> String {
    std::env::var("SMITHERY_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_AUTH_URL.to_owned())
}

pub struct AuthSession {
    pub session_id: String,
    pub auth_url: String,
}

pub fn create_auth_session(base: &str) -> Result<AuthSession> {
    let reply = post_empty(&format!("{base}/api/auth/cli/session"), AUTH_TIMEOUT)
        .map_err(|e| anyhow!("Failed to create Smithery auth session: {e}"))?;
    if !(200..300).contains(&reply.status) {
        bail!(
            "Failed to create Smithery auth session: {} {}",
            reply.status,
            reply.reason
        );
    }
    let body: Value = serde_json::from_str(&reply.body)
        .map_err(|_| anyhow!("Failed to create Smithery auth session: invalid response."))?;
    match (trimmed(&body, "sessionId"), trimmed(&body, "authUrl")) {
        (Some(session_id), Some(auth_url)) => Ok(AuthSession {
            session_id: session_id.to_owned(),
            auth_url: auth_url.to_owned(),
        }),
        _ => bail!("Failed to create Smithery auth session: invalid response."),
    }
}

enum PollState {
    Pending,
    Success(Option<String>),
    Failed(Option<String>),
    TimedOut,
}

fn poll_once(base: &str, session_id: &str) -> Result<PollState> {
    let url = format!("{base}/api/auth/cli/poll/{session_id}");
    let reply = match get(&url, None, POLL_TIMEOUT) {
        Ok(reply) => reply,
        Err(error) if is_timeout(&error) => return Ok(PollState::TimedOut),
        Err(error) => bail!("Smithery auth polling failed: {error}"),
    };
    if reply.status == 404 || reply.status == 410 {
        bail!("Smithery login session expired. Please try again.");
    }
    if !(200..300).contains(&reply.status) {
        bail!(
            "Smithery auth polling failed: {} {}",
            reply.status,
            reply.reason
        );
    }
    let body: Value = serde_json::from_str(&reply.body)
        .context("Smithery auth polling returned invalid JSON.")?;
    Ok(match body.get("status").and_then(Value::as_str) {
        Some("success") => PollState::Success(trimmed(&body, "apiKey").map(str::to_owned)),
        Some("error") => PollState::Failed(trimmed(&body, "message").map(str::to_owned)),
        _ => PollState::Pending,
    })
}

/// Polls until the user approves the session, the session fails, `deadline` passes, or `cancel` is set.
pub fn await_api_key(
    base: &str,
    session_id: &str,
    interval: Duration,
    deadline: Duration,
    cancel: &AtomicBool,
) -> Result<String> {
    let start = Instant::now();
    loop {
        if start.elapsed() >= deadline {
            bail!("Smithery authorization timed out after 5 minutes.");
        }
        if cancel.load(Ordering::Relaxed) {
            bail!("Smithery login cancelled.");
        }
        match poll_once(base, session_id)? {
            PollState::Success(Some(key)) => return Ok(key),
            PollState::Failed(message) => {
                bail!(
                    "{}",
                    message.unwrap_or_else(|| "Smithery authorization failed.".to_owned())
                )
            }
            PollState::Pending | PollState::Success(None) | PollState::TimedOut => {}
        }
        sleep_unless_cancelled(interval, cancel);
    }
}

fn sleep_unless_cancelled(duration: Duration, cancel: &AtomicBool) {
    let until = Instant::now() + duration;
    while Instant::now() < until && !cancel.load(Ordering::Relaxed) {
        let remaining = until.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

/// Checks a key with a one-result registry search at `REGISTRY_URL`.
pub fn validate_key(key: &str) -> Result<()> {
    validate_key_at(&registry_url(), key)
}

pub(crate) fn validate_key_at(registry: &str, key: &str) -> Result<()> {
    let opts = SearchOptions {
        limit: 1,
        api_key: Some(key.to_owned()),
        semantic: false,
    };
    search_at(registry, "mcp", &opts)
        .map(|_| ())
        .map_err(|e| anyhow!("Smithery API key validation failed: {}", e.message))
}

// ---- API-key cache --------------------------------------------------------------------------

/// `<user_dir>/smithery.json`. Errors when there is no Dial home directory.
pub fn key_file(user_dir: &Path) -> Result<PathBuf> {
    if user_dir.as_os_str().is_empty() {
        bail!("Smithery API key storage needs a Dial home directory.");
    }
    Ok(user_dir.join(KEY_FILE))
}

pub fn read_key(path: &Path) -> Result<Option<String>> {
    let Some(bytes) = read_private(path)? else {
        return Ok(None);
    };
    let value: Value =
        serde_json::from_slice(&bytes).context("Smithery API key file is not valid JSON.")?;
    Ok(trimmed(&value, "apiKey").map(str::to_owned))
}

pub fn save_key(path: &Path, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("Smithery API key cannot be empty.");
    }
    let mut text = serde_json::to_string_pretty(&json!({ "apiKey": key }))?;
    text.push('\n');
    write_private(path, text.as_bytes())
}

/// Removes the cached key. `false` when no key file exists.
pub fn clear_key(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// `SMITHERY_API_KEY` first, then the cached key.
pub fn resolve_api_key(user_dir: &Path) -> Result<Option<String>> {
    if let Some(key) = std::env::var(KEY_ENV)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
    {
        return Ok(Some(key));
    }
    if user_dir.as_os_str().is_empty() {
        return Ok(None);
    }
    read_key(&key_file(user_dir)?)
}

// ---- Login and re-auth ----------------------------------------------------------------------

/// Terminal side of the pasted-key login.
pub trait KeyPrompt {
    /// Shows the prompt; `None` when the user cancels.
    fn ask(&mut self, question: &str) -> Option<String>;
    /// Reports a rejected entry; the prompt then asks again.
    fn error(&mut self, message: &str);
}

/// Outcome of a login started during an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reauth {
    /// A key was cached; the operation can run again.
    Saved,
    /// A browser login was started and completes in the background.
    Pending,
    /// The user cancelled, or no login could start.
    Declined,
}

/// Reason for a deliberate `/mcp smithery-login`.
pub const LOGIN_REASON: &str = "login";
/// Reason shown after the registry rate-limits a request.
pub const RATE_LIMIT_REASON: &str = "rate limited by Smithery";
/// Reason shown after the registry rejects the cached key.
pub const UNAUTHORIZED_REASON: &str = "forbidden/unauthorized with Smithery";

/// Asks until a non-empty key validates, then caches it.
/// `Ok(false)` means the user cancelled.
pub fn paste_key_login(
    path: &Path,
    ui: &mut dyn KeyPrompt,
    validate: impl Fn(&str) -> Result<()>,
) -> Result<bool> {
    loop {
        let Some(entry) = ui.ask("Smithery API key") else {
            return Ok(false);
        };
        let key = entry.trim();
        if key.is_empty() {
            ui.error("Smithery API key cannot be empty.");
            continue;
        }
        match validate(key) {
            Ok(()) => {
                save_key(path, key)?;
                return Ok(true);
            }
            Err(error) => ui.error(&format!("{error:#}")),
        }
    }
}

/// Runs `op` with the cached key, retrying through a login when needed. With no key cached,
/// `reauth` starts a login first, citing `missing_reason`. A 401, 403 or 429 triggers one re-auth
/// and one retry. `may_reauth == false` runs without re-auth: it is the retry of an operation that
/// was parked behind a login, which is attempted only once.
/// `reauth(reason, may_reauth)` receives the reason and whether the operation resumed after that
/// login may re-auth again. `Ok(None)` means a browser login is still running; the operation resumes
/// when it finishes.
pub fn run_with_reauth<T>(
    user_dir: &Path,
    missing_reason: &str,
    may_reauth: bool,
    mut op: impl FnMut(Option<&str>) -> Result<T, SmitheryError>,
    mut reauth: impl FnMut(&str, bool) -> Result<Reauth>,
) -> Result<Option<T>, SmitheryError> {
    let mut key = resolve_api_key(user_dir).map_err(local_error)?;
    if key.is_none() {
        if !may_reauth {
            return Err(login_cancelled());
        }
        match reauth(missing_reason, true).map_err(local_error)? {
            Reauth::Saved => key = Some(key_after_login(user_dir)?),
            Reauth::Pending => return Ok(None),
            Reauth::Declined => return Err(login_cancelled()),
        }
    }
    let mut retried = false;
    loop {
        match op(key.as_deref()) {
            Ok(value) => return Ok(Some(value)),
            Err(error) if may_reauth && !retried && matches!(error.status, 401 | 403 | 429) => {
                retried = true;
                let reason = if error.status == 429 {
                    RATE_LIMIT_REASON
                } else {
                    UNAUTHORIZED_REASON
                };
                match reauth(reason, false).map_err(local_error)? {
                    Reauth::Saved => key = Some(key_after_login(user_dir)?),
                    Reauth::Pending => return Ok(None),
                    Reauth::Declined => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// The key a login just cached. Finding none afterwards is an error.
fn key_after_login(user_dir: &Path) -> Result<String, SmitheryError> {
    resolve_api_key(user_dir)
        .map_err(local_error)?
        .ok_or_else(|| SmitheryError {
            message: "Smithery API key not found after login.".to_owned(),
            status: 0,
        })
}

fn login_cancelled() -> SmitheryError {
    SmitheryError {
        message: "Smithery login cancelled. Run /mcp smithery-login, then retry.".to_owned(),
        status: 0,
    }
}

fn local_error(error: anyhow::Error) -> SmitheryError {
    SmitheryError {
        message: format!("{error:#}"),
        status: 0,
    }
}

// ---- Argument parsing -----------------------------------------------------------------------

use crate::storage::settings_store::Scope;

/// Default MCP scope for the Smithery commands.
const DEFAULT_SCOPE: Scope = Scope::Project;

#[derive(Debug, Clone, PartialEq)]
pub struct SearchArgs {
    pub keyword: String,
    pub limit: usize,
    pub semantic: bool,
    pub scope: Scope,
}

pub fn parse_search(args: &str) -> Result<SearchArgs> {
    let mut words: Vec<&str> = Vec::new();
    let mut limit = DEFAULT_LIMIT;
    let mut semantic = false;
    let mut scope = DEFAULT_SCOPE;
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        match token {
            "--scope" => scope = check_scope(tokens.next())?,
            "--limit" => {
                let value = tokens
                    .next()
                    .ok_or_else(|| anyhow!("Missing value for --limit."))?;
                limit = value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=MAX_LIMIT).contains(n))
                    .ok_or_else(|| {
                        anyhow!("Invalid --limit value. Use an integer between 1 and 100.")
                    })?;
            }
            "--semantic" => semantic = true,
            option if option.starts_with("--") => bail!("Unknown option: {option}"),
            word => words.push(word),
        }
    }
    if words.is_empty() {
        bail!(
            "Keyword required. Usage: /mcp smithery-search <keyword> [--scope project|user] [--limit <1-100>] [--semantic]"
        );
    }
    Ok(SearchArgs {
        keyword: words.join(" "),
        limit,
        semantic,
        scope,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstallArgs {
    /// `@namespace/server` for a direct install, or a keyword to search and pick from.
    pub name: String,
    pub sets: BTreeMap<String, String>,
    pub scope: Scope,
}

pub fn parse_install(args: &str) -> Result<InstallArgs> {
    let mut name: Option<&str> = None;
    let mut sets = BTreeMap::new();
    let mut scope = DEFAULT_SCOPE;
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        match token {
            "--scope" => scope = check_scope(tokens.next())?,
            "--set" => {
                let pair = tokens
                    .next()
                    .ok_or_else(|| anyhow!("Missing value for --set."))?;
                let (key, value) = pair
                    .split_once('=')
                    .filter(|(key, _)| !key.is_empty())
                    .ok_or_else(|| anyhow!("Invalid --set value. Use key=value."))?;
                sets.insert(key.to_owned(), value.to_owned());
            }
            option if option.starts_with("--") => bail!("Unknown option: {option}"),
            word if name.is_none() => name = Some(word),
            word => bail!("Unexpected argument: {word}"),
        }
    }
    let name = name.ok_or_else(|| {
        anyhow!("Server name required. Usage: /mcp smithery-install <@namespace/server|keyword> [--scope project|user] [--set key=value]...")
    })?;
    Ok(InstallArgs {
        name: name.to_owned(),
        sets,
        scope,
    })
}

/// `Some(key)` for `--api-key <key>`, `None` for the browser flow.
pub fn parse_login(args: &str) -> Result<Option<String>> {
    let mut api_key = None;
    let mut tokens = args.split_whitespace();
    while let Some(token) = tokens.next() {
        match token {
            "--api-key" => {
                let value = tokens
                    .next()
                    .ok_or_else(|| anyhow!("Missing value for --api-key."))?;
                api_key = Some(value.to_owned());
            }
            option if option.starts_with("--") => bail!("Unknown option: {option}"),
            word => bail!("Unexpected argument: {word}"),
        }
    }
    Ok(api_key)
}

fn check_scope(value: Option<&str>) -> Result<Scope> {
    match value {
        None => bail!("Missing value for --scope."),
        Some("user" | "global") => Ok(Scope::Global),
        Some("project") => Ok(Scope::Project),
        Some(_) => bail!("Invalid --scope value. Use project or user."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    type Handler = Arc<dyn Fn(&str) -> (u16, String) + Send + Sync>;

    /// Loopback HTTP fixture: each connection gets one response, `handler(path_and_query)`.
    fn serve(handler: Handler) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let target = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (status, body) = handler(&target);
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn err_text<T>(result: Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn browsing_keeps_nuxt_without_details_and_follows_provider_pagination() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let base = serve(Arc::new(move |target| {
            counter.fetch_add(1, Ordering::SeqCst);
            if !target.starts_with("/servers?") {
                return (404, "{}".into());
            }
            let url = url::Url::parse(&format!("http://localhost{target}")).unwrap();
            let params: BTreeMap<_, _> = url.query_pairs().collect();
            assert_eq!(params.get("q").map(|v| v.as_ref()), Some("nuxt"));
            let second = params.get("page").is_some_and(|v| v == "2");
            (200, json!({
                "servers": if second {
                    json!([{"namespace":"nuxt", "slug":"docs", "displayName":"Nuxt docs"}])
                } else {
                    json!([
                        {"qualifiedName":"@nuxt/mcp", "displayName":"Nuxt", "verified":true},
                        {"qualifiedName":"nuxt/mcp", "displayName":"Duplicate"},
                        {"id":"not-an-install-identity"}
                    ])
                },
                "pagination": {"totalPages":2}
            }).to_string())
        }));
        let (first, more) = browse_page_at(&base, "nuxt", 1, None).expect("first page");
        assert!(more);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "@nuxt/mcp");
        assert_eq!(first[0].badges, ["verified"]);
        assert!(!first[0].installable());
        let (second, more) = browse_page_at(&base, "nuxt", 2, None).expect("second page");
        assert!(!more);
        assert_eq!(second[0].name, "@nuxt/docs");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "no detail requests while browsing");
    }

    #[test]
    fn config_names_follow_slug_rules() {
        assert_eq!(to_config_name("@Acme/My Notes!"), "acme-my-notes");
        assert_eq!(to_config_name("@@"), "mcp-server");
        assert_eq!(normalize_qualified_name("acme/x"), "@acme/x");
        assert_eq!(unique_name("a", |n| n == "a"), "a-2");
    }

    #[test]
    fn search_maps_entries_and_drops_unresolvable_ones() {
        let servers = json!({"servers": [
            {"id": "srv1", "namespace": "acme", "slug": "notes", "qualifiedName": "@acme/notes",
             "displayName": "Notes  Pro", "useCount": 5, "verified": true, "isDeployed": true},
            {"id": "srv2", "namespace": "acme", "slug": "ghost", "qualifiedName": "@acme/ghost",
             "displayName": "Notes Ghost", "useCount": 9}
        ]})
        .to_string();
        let details = json!({
            "qualifiedName": "@acme/notes",
            "displayName": "Notes",
            "description": "  Take\n notes  ",
            "connections": [{"type": "http", "deploymentUrl": "https://notes.example/mcp"}],
        })
        .to_string();
        let handler: Handler = Arc::new(move |target: &str| {
            if target.starts_with("/servers?") {
                (200, servers.clone())
            } else if target == "/servers/acme/notes" {
                (200, details.clone())
            } else {
                (404, "{}".to_owned())
            }
        });
        let base = serve(handler);
        let opts = SearchOptions {
            limit: 20,
            api_key: None,
            semantic: false,
        };
        let results = search_at(&base, "notes", &opts).expect("search");
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.name, "acme/notes");
        assert_eq!(result.qualified_name, "@acme/notes");
        assert_eq!(result.display_name, "Notes");
        assert_eq!(result.description, "Take notes");
        assert!(result.inputs.is_empty());
        assert_eq!(
            result.config,
            json!({"type": "http", "url": "https://notes.example/mcp"})
        );
    }

    #[test]
    fn search_reports_registry_status() {
        let handler: Handler = Arc::new(|_: &str| (401, "{}".to_owned()));
        let base = serve(handler);
        let opts = SearchOptions {
            limit: 1,
            api_key: None,
            semantic: false,
        };
        let error = search_at(&base, "x", &opts).expect_err("401 must fail");
        assert_eq!(error.status, 401);
        assert_eq!(error.message, "Smithery search failed with status 401");
    }

    #[test]
    fn login_session_and_poll_loop() {
        let polls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&polls);
        let handler: Handler = Arc::new(move |target: &str| {
            if target == "/api/auth/cli/session" {
                (
                    200,
                    json!({"sessionId": "s1", "authUrl": "https://smithery.ai/auth/s1"})
                        .to_string(),
                )
            } else if target == "/api/auth/cli/poll/s1" {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    (200, json!({"status": "pending"}).to_string())
                } else {
                    (
                        200,
                        json!({"status": "success", "apiKey": "sk-test"}).to_string(),
                    )
                }
            } else {
                (404, "{}".to_owned())
            }
        });
        let base = serve(handler);
        let session = create_auth_session(&base).expect("session");
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.auth_url, "https://smithery.ai/auth/s1");
        let key = await_api_key(
            &base,
            &session.session_id,
            Duration::from_millis(5),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .expect("key");
        assert_eq!(key, "sk-test");
        assert_eq!(polls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn login_expired_and_timed_out() {
        let expired: Handler = Arc::new(|_: &str| (410, "{}".to_owned()));
        let base = serve(expired);
        let error = await_api_key(
            &base,
            "s",
            Duration::from_millis(5),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .expect_err("expired");
        assert_eq!(
            error.to_string(),
            "Smithery login session expired. Please try again."
        );

        let pending: Handler = Arc::new(|_: &str| (200, json!({"status": "pending"}).to_string()));
        let base = serve(pending);
        let error = await_api_key(
            &base,
            "s",
            Duration::from_millis(5),
            Duration::from_millis(50),
            &AtomicBool::new(false),
        )
        .expect_err("deadline");
        assert_eq!(
            error.to_string(),
            "Smithery authorization timed out after 5 minutes."
        );
    }

    #[test]
    fn key_validation_uses_registry_status() {
        let handler: Handler = Arc::new(|_: &str| (403, "{}".to_owned()));
        let base = serve(handler);
        let error = validate_key_at(&base, "bad").expect_err("403 must fail");
        assert!(
            error
                .to_string()
                .starts_with("Smithery API key validation failed:")
        );
    }

    #[test]
    fn key_cache_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_file(dir.path()).expect("path");
        assert!(read_key(&path).expect("read").is_none());
        save_key(&path, "  sk-abc \n").expect("save");
        assert_eq!(read_key(&path).expect("read").as_deref(), Some("sk-abc"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(clear_key(&path).expect("clear"));
        assert!(!clear_key(&path).expect("clear again"));
        assert_eq!(
            err_text(save_key(&path, "   ")),
            "Smithery API key cannot be empty."
        );
        assert!(key_file(Path::new("")).is_err());
    }

    #[test]
    fn install_plan_types_values_and_reports_errors() {
        let entry = json!({"id": "x", "qualifiedName": "@acme/tool"});
        let details = json!({"connections": [{"type": "stdio", "configSchema": {
            "properties": {
                "apiKey": {"type": "string", "format": "password"},
                "limit": {"type": "number", "default": "5"},
                "mode": {"type": "string", "enum": ["fast", "slow"]}
            },
            "required": ["apiKey"]
        }}]});
        let result = to_search_result(&entry, &details).expect("result");

        let empty = BTreeMap::new();
        assert_eq!(
            err_text(plan_install(&result, &empty)),
            "Missing required value for \"apiKey\"."
        );

        let mut sets = BTreeMap::new();
        sets.insert("apiKey".to_owned(), "k".to_owned());
        sets.insert("mode".to_owned(), "medium".to_owned());
        assert_eq!(
            err_text(plan_install(&result, &sets)),
            "Invalid value for \"mode\". Use one of: fast, slow."
        );
        sets.insert("mode".to_owned(), "fast".to_owned());
        sets.insert("bogus".to_owned(), "1".to_owned());
        assert_eq!(
            err_text(plan_install(&result, &sets)),
            "Unknown Smithery input \"bogus\"."
        );
        sets.remove("bogus");
        let server = plan_install(&result, &sets).expect("plan");
        assert_eq!(server.program, PathBuf::from("bunx"));
        assert_eq!(server.args[5], r#"{"apiKey":"k","limit":5,"mode":"fast"}"#);
    }

    #[test]
    fn argument_parsing_messages() {
        assert_eq!(
            err_text(parse_search("")),
            "Keyword required. Usage: /mcp smithery-search <keyword> [--scope project|user] [--limit <1-100>] [--semantic]"
        );
        assert_eq!(
            err_text(parse_search("x --limit 0")),
            "Invalid --limit value. Use an integer between 1 and 100."
        );
        assert_eq!(
            err_text(parse_search("x --limit")),
            "Missing value for --limit."
        );
        assert_eq!(
            err_text(parse_search("x --bogus")),
            "Unknown option: --bogus"
        );
        assert_eq!(
            err_text(parse_search("x --scope team")),
            "Invalid --scope value. Use project or user."
        );
        assert_eq!(
            parse_search("x --scope project").expect("parse").scope,
            crate::storage::settings_store::Scope::Project
        );
        let parsed = parse_search("foo bar --limit 3 --semantic").expect("parse");
        assert_eq!(parsed.keyword, "foo bar");
        assert_eq!(parsed.limit, 3);
        assert!(parsed.semantic);
        assert_eq!(
            parse_login("--api-key sk").expect("login"),
            Some("sk".to_owned())
        );
        assert_eq!(parse_login("").expect("login"), None);
    }

    struct ScriptedPrompt {
        answers: std::collections::VecDeque<Option<String>>,
        errors: Vec<String>,
    }

    impl KeyPrompt for ScriptedPrompt {
        fn ask(&mut self, _question: &str) -> Option<String> {
            self.answers.pop_front().expect("unexpected prompt")
        }
        fn error(&mut self, message: &str) {
            self.errors.push(message.to_owned());
        }
    }

    #[test]
    fn pasted_key_is_validated_then_cached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_file(dir.path()).expect("key file");
        let mut ui = ScriptedPrompt {
            answers: std::collections::VecDeque::from([
                Some("  ".to_owned()),
                Some("bad".to_owned()),
                Some(" good ".to_owned()),
            ]),
            errors: Vec::new(),
        };
        let validate = |key: &str| -> Result<()> {
            if key == "good" {
                Ok(())
            } else {
                Err(anyhow!(
                    "Smithery API key validation failed: Smithery search failed with status 401"
                ))
            }
        };
        assert!(paste_key_login(&path, &mut ui, validate).expect("login"));
        assert_eq!(
            ui.errors,
            vec![
                "Smithery API key cannot be empty.",
                "Smithery API key validation failed: Smithery search failed with status 401",
            ]
        );
        assert_eq!(read_key(&path).expect("read"), Some("good".to_owned()));
    }

    #[test]
    fn cancelled_paste_caches_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_file(dir.path()).expect("key file");
        let mut ui = ScriptedPrompt {
            answers: std::collections::VecDeque::from([None]),
            errors: Vec::new(),
        };
        assert!(!paste_key_login(&path, &mut ui, |_| Ok(())).expect("login"));
        assert!(!path.exists());
    }

    #[test]
    fn unauthorized_reauths_once_and_retries_with_the_new_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_file(dir.path()).expect("key file");
        save_key(&path, "stale").expect("save stale");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let base = serve(Arc::new(move |_: &str| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                (401, "{}".to_owned())
            } else {
                (200, json!({"servers": []}).to_string())
            }
        }));
        let seen = std::cell::RefCell::new(Vec::new());
        let found = run_with_reauth(
            dir.path(),
            "required for smithery-search",
            true,
            |key| {
                seen.borrow_mut().push(key.map(str::to_owned));
                let opts = SearchOptions {
                    limit: 1,
                    api_key: key.map(str::to_owned),
                    semantic: false,
                };
                search_at(&base, "x", &opts).map(|results| results.len())
            },
            |reason, may_reauth_again| {
                assert_eq!(reason, UNAUTHORIZED_REASON);
                assert!(!may_reauth_again);
                save_key(&path, "fresh")?;
                Ok(Reauth::Saved)
            },
        )
        .expect("retry succeeds");
        assert_eq!(found, Some(0));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(
            *seen.borrow(),
            vec![Some("stale".to_owned()), Some("fresh".to_owned())]
        );
    }

    #[test]
    fn rate_limit_reauths_once_then_retries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_file(dir.path()).expect("key file");
        save_key(&path, "cached").expect("save");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let base = serve(Arc::new(move |_: &str| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                (429, "{}".to_owned())
            } else {
                (200, json!({"servers": []}).to_string())
            }
        }));
        let reauths = AtomicUsize::new(0);
        let found = run_with_reauth(
            dir.path(),
            "required for smithery-search",
            true,
            |key| {
                let opts = SearchOptions {
                    limit: 1,
                    api_key: key.map(str::to_owned),
                    semantic: false,
                };
                search_at(&base, "x", &opts).map(|results| results.len())
            },
            |reason, _| {
                reauths.fetch_add(1, Ordering::SeqCst);
                assert_eq!(reason, RATE_LIMIT_REASON);
                Ok(Reauth::Saved)
            },
        )
        .expect("retry after rate limit succeeds");
        assert_eq!(found, Some(0));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(reauths.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn rate_limit_surfaces_when_the_retry_is_declined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_file(dir.path()).expect("key file");
        save_key(&path, "cached").expect("save");
        let base = serve(Arc::new(|_: &str| (429, "{}".to_owned())));
        let attempts = AtomicUsize::new(0);
        let reauths = AtomicUsize::new(0);
        let error = run_with_reauth(
            dir.path(),
            "required for smithery-search",
            true,
            |key| {
                attempts.fetch_add(1, Ordering::SeqCst);
                let opts = SearchOptions {
                    limit: 1,
                    api_key: key.map(str::to_owned),
                    semantic: false,
                };
                search_at(&base, "x", &opts).map(|results| results.len())
            },
            |_, _| {
                reauths.fetch_add(1, Ordering::SeqCst);
                Ok(Reauth::Declined)
            },
        )
        .expect_err("429 must surface");
        assert_eq!(error.status, 429);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(reauths.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn resumed_retry_never_reauths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_file(dir.path()).expect("key file");
        save_key(&path, "stale").expect("save");
        let base = serve(Arc::new(|_: &str| (401, "{}".to_owned())));
        let reauths = AtomicUsize::new(0);
        let error = run_with_reauth(
            dir.path(),
            "required for smithery-search",
            false,
            |key| {
                let opts = SearchOptions {
                    limit: 1,
                    api_key: key.map(str::to_owned),
                    semantic: false,
                };
                search_at(&base, "x", &opts).map(|results| results.len())
            },
            |_, _| {
                reauths.fetch_add(1, Ordering::SeqCst);
                Ok(Reauth::Saved)
            },
        )
        .expect_err("401 must surface on the resumed retry");
        assert_eq!(error.status, 401);
        assert_eq!(reauths.load(Ordering::SeqCst), 0);
    }
}
