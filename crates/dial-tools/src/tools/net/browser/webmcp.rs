//! WebMCP discovery, invocation, and catalog events. The page-side bridge in `webmcp.cjs` mirrors
//! `navigator.modelContext` registrations; everything it returns is page-authored and untrusted, so results are
//! bounded, control-stripped, and marked `untrusted`.

use super::requests::bound_text;
use serde_json::{Map, Value, json};

/// Most tools summarized by one `webmcp_list` call.
pub const WEBMCP_TOOLS_MAX: usize = 16;
/// Largest page-provided description kept in a summary.
const DESCRIPTION_MAX_BYTES: usize = 160;
/// Largest serialized page result returned to the model.
pub const WEBMCP_RESULT_MAX_BYTES: usize = 64 * 1024;
/// Largest page error text returned to the model.
const ERROR_MAX_BYTES: usize = 2048;
/// Largest `params` object accepted for an invocation.
const PARAMS_MAX_BYTES: usize = 16 * 1024;
/// Largest event-type or tool-name text kept per event.
const EVENT_NAME_MAX_BYTES: usize = 128;

/// Whether `name` matches the WebMCP tool-name grammar (1-128 ASCII letters, digits, `_`, `-`, `.`).
pub fn is_valid_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn name_arg(args: &Value) -> Result<Option<String>, String> {
    match args.get("name") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) if is_valid_name(name) => Ok(Some(name.clone())),
        Some(_) => {
            Err("WebMCP tool name must be 1-128 ASCII letters, digits, '_', '-', or '.'".into())
        }
    }
}

/// Validate a WebMCP action and build the driver request fields.
pub fn parse(action: &str, args: &Value) -> Result<Value, String> {
    match action {
        "webmcp_list" => Ok(json!({"name": name_arg(args)?})),
        "webmcp_invoke" => {
            let name = name_arg(args)?.ok_or("webmcp_invoke requires name")?;
            let params = match args.get("params") {
                None | Some(Value::Null) => json!({}),
                Some(value @ Value::Object(_)) => value.clone(),
                Some(_) => return Err("webmcp_invoke params must be a JSON object".into()),
            };
            if params.to_string().len() > PARAMS_MAX_BYTES {
                return Err(format!(
                    "webmcp_invoke params exceed {PARAMS_MAX_BYTES} bytes"
                ));
            }
            Ok(json!({"name": name, "params": params}))
        }
        "webmcp_events" => {
            let since = match args.get("since") {
                None | Some(Value::Null) => 0,
                Some(value) => value
                    .as_u64()
                    .ok_or("webmcp_events since must be a non-negative integer cursor")?,
            };
            let clear = match args.get("clear") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(clear)) => *clear,
                Some(_) => return Err("webmcp_events clear must be a boolean".into()),
            };
            Ok(json!({"since": since, "clear": clear}))
        }
        _ => Err(format!("unsupported WebMCP action {action:?}")),
    }
}

fn text_field(raw: &Value, key: &str, max_bytes: usize) -> (String, bool) {
    bound_text(
        raw.get(key).and_then(Value::as_str).unwrap_or(""),
        max_bytes,
    )
}

/// Shape the driver's `webmcp_list` reply. The schema is returned only for an exact `name` lookup.
pub fn shape_list(raw: &Value) -> Value {
    let filtered = raw.get("name").is_some_and(|name| !name.is_null());
    let mut truncated = false;
    let mut tools = Vec::new();
    for tool in raw
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if tools.len() >= WEBMCP_TOOLS_MAX {
            truncated = true;
            break;
        }
        let (name, _) = text_field(tool, "name", EVENT_NAME_MAX_BYTES);
        let (description, cut) = text_field(tool, "description", DESCRIPTION_MAX_BYTES);
        truncated |= cut;
        let mut entry = Map::new();
        entry.insert("name".into(), json!(name));
        entry.insert("description".into(), json!(description));
        if filtered {
            let (schema, cut) = text_field(tool, "input_schema_json", WEBMCP_RESULT_MAX_BYTES);
            truncated |= cut;
            entry.insert("input_schema".into(), json!(schema));
        }
        tools.push(Value::Object(entry));
    }
    let status = raw
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unavailable");
    let mut shaped = Map::new();
    shaped.insert("status".into(), json!(status));
    shaped.insert("tools".into(), Value::Array(tools));
    shaped.insert("truncated".into(), json!(truncated));
    shaped.insert("untrusted".into(), json!(true));
    if status == "unavailable" {
        let (detail, _) = text_field(raw, "detail", ERROR_MAX_BYTES);
        shaped.insert("reason".into(), json!(detail));
    } else if shaped["tools"].as_array().is_some_and(Vec::is_empty) {
        shaped.insert(
            "reason".into(),
            json!("no WebMCP tools are registered on this page"),
        );
    }
    Value::Object(shaped)
}

/// Shape the driver's `webmcp_invoke` reply, parsing the page's JSON result under the size bound.
pub fn shape_invoke(raw: &Value) -> Result<Value, String> {
    if raw.get("ok").and_then(Value::as_bool) != Some(true) {
        let (error, _) = text_field(raw, "error", ERROR_MAX_BYTES);
        return Ok(json!({"ok": false, "error": error, "untrusted": true}));
    }
    let encoded = raw
        .get("result_json")
        .and_then(Value::as_str)
        .ok_or("WebMCP invoke reply is missing result_json")?;
    let truncated = raw
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let result = if truncated {
        json!({"truncated_json": encoded})
    } else {
        serde_json::from_str::<Value>(encoded)
            .map_err(|error| format!("WebMCP result is not JSON: {error}"))?
    };
    Ok(json!({"ok": true, "result": result, "truncated": truncated, "untrusted": true}))
}

/// Shape the driver's `webmcp_events` reply.
pub fn shape_events(raw: &Value) -> Value {
    let events: Vec<Value> = raw
        .get("events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|event| {
            let (kind, _) = text_field(event, "type", 32);
            let (name, _) = text_field(event, "name", EVENT_NAME_MAX_BYTES);
            json!({
                "sequence": event.get("sequence").and_then(Value::as_u64).unwrap_or(0),
                "type": kind,
                "name": name,
            })
        })
        .collect();
    json!({
        "events": events,
        "cursor": raw.get("cursor").and_then(Value::as_u64).unwrap_or(0),
        "truncated": raw.get("truncated").and_then(Value::as_bool).unwrap_or(false),
        "untrusted": true,
    })
}

#[cfg(test)]
mod tests;
