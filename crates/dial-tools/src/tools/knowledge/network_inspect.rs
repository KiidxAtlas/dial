//! Offline HAR observations with a strict, secret-omitting projection before evidence publication.
mod openapi;
#[cfg(test)]
mod tests;

use super::evidence::{publish, source};
use crate::config::types::Config;
use crate::runtime::evidence::{
    Evidence, EvidenceContext, EvidenceKind, Location, Method, Representation,
};
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use url::Url;

const MAX_INPUT: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_COUNT: usize = 200;
const MAX_MESSAGES: usize = 100;
const MAX_OUTPUT: usize = 1024 * 1024;
const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

/// Register the offline capture inspection tool.
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(NetworkInspect)]
}
struct NetworkInspect;
impl Tool for NetworkInspect {
    fn name(&self) -> &str {
        "network_inspect"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "network_inspect",
            "Inspect a local HAR 1.2 JSON capture offline; optionally compare OpenAPI 3 JSON method/path/declared response statuses, NOT schema conformance. No HTTP, packet capture or external refs. Strict safe projection: opaque endpoint/route IDs only; no URL strings, headers, cookies, bodies or WebSocket data. Capture order is not causality; repeated method+origin/path is only a retry heuristic (query ignored). HAR -1 timings remain unknown. Only sanitized evidence is retained. Limits: 16 MiB per source, 10,000 capture entries, 200 returned entries, 100 WebSocket metadata records per entry, 1 MiB projected JSON. Windowed coverage is explicit.",
            json!({
                "action":{"type":"string","enum":["har"]},
                "path":{"type":"string","maxLength":4096},
                "openapi_path":{"type":"string","maxLength":4096},
                "start":{"type":"integer","minimum":1},
                "count":{"type":"integer","minimum":1,"maximum":200}
            }),
            &["action", "path"],
        )
    }
    fn needs_approval(&self, _: &Value) -> bool {
        false
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        run(ctx.workspace, args).unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    path: String,
    openapi_path: Option<String>,
    start: Option<usize>,
    count: Option<usize>,
}

fn read_json(ws: &Workspace, path: &str) -> Result<(Value, Vec<u8>)> {
    ensure!(
        !path.is_empty() && path.len() <= 4096,
        "source path must contain 1..4096 bytes"
    );
    // Neither OS errors nor JSON errors may echo an untrusted source string.
    let bytes = source::read(ws, path)
        .map_err(|_| anyhow::anyhow!("source unavailable under current workspace read policy"))?;
    ensure!(
        bytes.len() <= MAX_INPUT,
        "source exceeds 16 MiB input limit"
    );
    let value = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("invalid JSON at line {}, column {}", e.line(), e.column()))?;
    Ok((value, bytes))
}

fn method(entry: &Value) -> &'static str {
    let raw = entry
        .pointer("/request/method")
        .and_then(Value::as_str)
        .unwrap_or("");
    METHODS
        .iter()
        .copied()
        .find(|m| *m == raw)
        .unwrap_or("OTHER")
}
fn request_url(entry: &Value) -> Option<Url> {
    let raw = entry.pointer("/request/url")?.as_str()?;
    if raw.len() > 8192 {
        return None;
    }
    let mut url = Url::parse(raw).ok()?;
    if !["http", "https", "ws", "wss"].contains(&url.scheme()) {
        return None;
    }
    // These strings are used only transiently for matching, never published or hashed individually.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    Some(url)
}
fn endpoint(url: &Url) -> String {
    let mut url = url.clone();
    url.set_query(None);
    url.into()
}
fn timing(value: Option<&Value>) -> Value {
    match value.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n >= 0.0 => json!({"ms":n,"state":"recorded"}),
        Some(-1.0) => json!({"ms":null,"state":"unknown_minus_one"}),
        None => json!({"ms":null,"state":"missing"}),
        _ => json!({"ms":null,"state":"invalid"}),
    }
}
fn websocket(entry: &Value) -> Value {
    let Some(messages) = entry.get("_webSocketMessages").and_then(Value::as_array) else {
        return json!({"present":false});
    };
    let metadata: Vec<_> = messages
        .iter()
        .take(MAX_MESSAGES)
        .map(|message| {
            let direction = match message.get("type").and_then(Value::as_str) {
                Some("send") => "send",
                Some("receive") => "receive",
                _ => "unknown",
            };
            let opcode = message
                .get("opcode")
                .and_then(Value::as_u64)
                .filter(|n| *n <= 15);
            let time = message
                .get("time")
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n >= 0.0);
            json!({"direction":direction,"opcode":opcode,"capture_time_seconds":time,
            "data_text_bytes":message.get("data").and_then(Value::as_str).map(str::len)})
        })
        .collect();
    json!({"present":true,"total":messages.len(),"returned":metadata.len(),
        "omitted":messages.len().saturating_sub(metadata.len()),"messages":metadata,
        "limitations":"Chrome extension metadata only; data_text_bytes is UTF-8 text length, not wire size; no frame data or protocol decoding"})
}

/// Inspect authorized local capture bytes; retain only the bounded sanitized representation.
///
/// Errors are generic or numeric-location-only to avoid echoing captured secrets.
pub fn run(ws: &Workspace, value: &Value) -> Result<ToolOutput> {
    let args: Args = serde_json::from_value(value.clone())
        .map_err(|_| anyhow::anyhow!("invalid network_inspect arguments"))?;
    ensure!(args.action == "har", "unsupported network_inspect action");
    let start = args.start.unwrap_or(1);
    let count = args.count.unwrap_or(100);
    ensure!(
        start > 0 && (1..=MAX_COUNT).contains(&count),
        "start must be positive; count must be 1..200"
    );
    ws.artifacts()
        .context("network_inspect requires artifact storage")?;
    let (har, bytes) = read_json(ws, &args.path)?;
    ensure!(
        har.pointer("/log/version").and_then(Value::as_str) == Some("1.2"),
        "requires HAR log.version 1.2"
    );
    let entries = har
        .pointer("/log/entries")
        .and_then(Value::as_array)
        .context("HAR log.entries must be an array")?;
    ensure!(
        entries.len() <= MAX_ENTRIES,
        "HAR exceeds 10000 entry limit"
    );
    ensure!(
        start <= entries.len().max(1),
        "start exceeds capture entry count"
    );
    let snapshot = source::snapshot(&bytes)?;
    let spec = args
        .openapi_path
        .as_deref()
        .map(|path| {
            let (value, bytes) = read_json(ws, path)?;
            Ok::<_, anyhow::Error>((openapi::Spec::parse(value)?, source::snapshot(&bytes)?))
        })
        .transpose()?;
    let urls: Vec<_> = entries.iter().map(request_url).collect();
    let mut endpoint_ids = BTreeMap::new();
    for url in urls.iter().flatten() {
        let next = endpoint_ids.len() + 1;
        endpoint_ids.entry(endpoint(url)).or_insert(next);
    }
    let end = entries
        .len()
        .min(start.saturating_sub(1).saturating_add(count));
    let mut observations = Vec::new();
    let mut statuses: BTreeMap<&str, usize> = BTreeMap::new();
    let mut previous = BTreeMap::new();
    for index in start - 1..end {
        let entry = &entries[index];
        ensure!(entry.is_object(), "HAR entry must be an object");
        let method = method(entry);
        let url = urls[index].as_ref();
        let endpoint_id = url
            .and_then(|url| endpoint_ids.get(&endpoint(url)))
            .copied();
        let status = entry
            .pointer("/response/status")
            .and_then(Value::as_u64)
            .filter(|n| *n <= 599);
        let group = match status {
            Some(100..=199) => "1xx",
            Some(200..=299) => "2xx",
            Some(300..=399) => "3xx",
            Some(400..=499) => "4xx",
            Some(500..=599) => "5xx",
            Some(0) => "no_http_status",
            _ => "unknown",
        };
        *statuses.entry(group).or_default() += 1;
        let repeated = endpoint_id
            .filter(|_| method != "OTHER")
            .and_then(|id| previous.insert((method, id), index + 1));
        let redirect = if matches!(status, Some(300..=399)) {
            entry
                .pointer("/response/redirectURL")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 8192)
                .and_then(|raw| url?.join(raw).ok())
                .and_then(|mut target| {
                    let _ = target.set_username("");
                    let _ = target.set_password(None);
                    target.set_fragment(None);
                    urls.iter()
                        .enumerate()
                        .skip(index + 1)
                        .find(|(_, candidate)| candidate.as_ref() == Some(&target))
                        .map(|(i, _)| i + 1)
                })
        } else {
            None
        };
        let mut timings = serde_json::Map::new();
        for phase in [
            "blocked", "dns", "connect", "send", "wait", "receive", "ssl",
        ] {
            timings.insert(
                phase.into(),
                timing(entry.get("timings").and_then(|t| t.get(phase))),
            );
        }
        observations.push(json!({"sequence":index+1,"method":method,"endpoint_id":endpoint_id,
            "url_state":if url.is_some() {"parsed_redacted"} else {"invalid_or_missing"},
            "status":status,"status_group":group,"total_time":timing(entry.get("time")),"timings":timings,
            "retry_heuristic":{"previous_same_method_endpoint_sequence":repeated,"basis":"same method and origin/path, query ignored; repetition is NOT proof of retry; selected window only"},
            "explicit_redirect_target_sequence":redirect,"websocket":websocket(entry),
            "api_comparison":spec.as_ref().map(|(spec, _)| spec.compare(url, method, status))}));
    }
    let result = json!({"type":"offline_har_observation","verification_credit":false,
        "source_snapshot":snapshot,"openapi_snapshot":spec.as_ref().map(|(_, identity)| identity),
        "coverage":{"total_entries":entries.len(),"start":start,"returned":observations.len(),"omitted":entries.len()-observations.len(),"order":"HAR array order, not necessarily chronological or causal"},
        "limits":{"input_bytes":MAX_INPUT,"capture_entries":MAX_ENTRIES,"returned_entries":MAX_COUNT,"websocket_messages_per_entry":MAX_MESSAGES,"projected_json_bytes":MAX_OUTPUT,"openapi_paths":openapi::MAX_PATHS,"openapi_compiled_matcher_bytes_per_route":openapi::MAX_TEMPLATE_REGEX_BYTES},
        "redaction":"No URL strings (including origin, userinfo, paths, query and fragments), header names/values, cookies, postData, bodies, arbitrary strings or frame data. Endpoint and route IDs are local opaque ordinals, not stable identities.",
        "limitations":["Recorded capture only, no live traffic or causality. Missing/invalid timings remain unknown; SSL overlaps connect and must not be summed independently.","OpenAPI comparison only declared path/method/status; no schemas, security, refs, server/base-path resolution, percent-decoding or conformance verification. Route templates are opaque; ambiguous matches stay ambiguous."],
        "status_groups":statuses,"entries":observations});
    let projected = serde_json::to_vec(&result)?;
    ensure!(
        projected.len() <= MAX_OUTPUT,
        "sanitized output exceeds 1 MiB; request fewer entries"
    );
    let item = Evidence {
        source: format!("local-har:sha256:{}", snapshot.sha256), location: Location::Whole,
        method: Method::Calculation, producer: "dial/network-inspect-v1".into(), kind: EvidenceKind::Observation,
        summary: "Sanitized offline HAR window and optional limited OpenAPI comparison".into(),
        representation: Representation::of_bytes("application/json", &projected),
        limitations: vec!["Only the sanitized projection is retained; no original capture or spec, no verification credit, no network I/O. Omitted entries and metadata bounds are explicit in representation.".into()],
        context: EvidenceContext { snapshot: Some(snapshot), ..Default::default() },
    };
    publish(ws, ToolOutput::structured(result), item, &projected)
}
