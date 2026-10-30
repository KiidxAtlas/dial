//! Durable tool identities. These describe history; they never authorize execution or replay it.
use crate::tools::integrations::ext::Registry;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub revision: u32,
    pub schema_sha256: String,
}

/// Pure provider-facing view of a settled historical call. The original journal is never rewritten.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoricalCall {
    pub arguments: Value,
    pub output: String,
}

/// Optional built-in properties added after release whose absence keeps the old meaning, so
/// history recorded before them stays valid. Removing a pinned property recovers that identity.
const ADDITIVE_PROPERTIES: &[(&str, &[&str])] = &[
    ("bash", &["session"]),
    ("bg", &["persist", "lifetime"]),
    ("monitor", &["run"]),
    ("grep", &["output"]),
    ("edit", &["allow_syntax_errors"]),
    ("task", &["source_session", "model", "isolation_backend"]),
    ("find", &["related"]),
    ("lsp", &["payload"]),
    ("eval", &["background", "title"]),
    ("web_search", &["domains", "exclude_domains"]),
    ("image_gen", &["image"]),
    (
        "data",
        &[
            "row_ids",
            "brush",
            "calculation_reference",
            "bins",
            "order_by",
            "whole_source",
            "json_pointers",
            "decimal_columns",
            "conversions",
            "dates",
            "dictionary",
            "join",
        ],
    ),
    (
        "audio",
        &[
            "window_ms",
            "silence_dbfs",
            "clip_threshold",
            "transient_db",
        ],
    ),
    (
        "browser",
        &[
            "request_id",
            "format",
            "quality",
            "annotate",
            "if_changed",
            "record_restart",
            "filter",
            "resource_type",
            "limit",
            "content",
            "pattern",
            "route_action",
            "status",
            "headers",
            "content_type",
            "body",
            "source",
            "script_id",
            "params",
            "since",
            "clear",
            "x",
            "y",
            "dx",
            "dy",
            "target",
            "button",
            "duration",
            "name",
            "props",
            "response",
            "viewport",
            "user_agent",
            "ignore_https_errors",
            "device",
            "geolocation",
            "timezone",
            "locale",
            "color_scheme",
            "offline",
            "urls",
            "domain",
            "include_values",
            "cookies",
            "names",
            "area",
            "operation",
            "value",
            "description",
        ],
    ),
];

/// Enum values added after release. History recorded before them stays valid because the old
/// schema is the current one with exactly these values removed.
const ADDITIVE_ENUM_VALUES: &[(&str, &str, &[&str])] = &[
    ("eval", "language", &["py"]),
    ("lsp", "action", &["request"]),
    (
        "data",
        "action",
        &["histogram", "trend", "correlation", "inspect"],
    ),
    ("data", "format", &["jsonl"]),
    ("audio", "action", &["analyze"]),
    ("document", "action", &["inspect"]),
    (
        "browser",
        "action",
        &[
            "requests",
            "request",
            "route",
            "unroute",
            "routes",
            "har_start",
            "har_stop",
            "add_init_script",
            "remove_init_script",
            "init_scripts",
            "webmcp_list",
            "webmcp_invoke",
            "webmcp_events",
        ],
    ),
    (
        "browser",
        "action",
        &[
            "scroll",
            "dblclick",
            "drag",
            "click_at",
            "mouse_move",
            "mouse_down",
            "mouse_up",
            "wheel",
            "key_down",
            "key_up",
            "highlight",
            "text",
            "html",
            "value",
            "attr",
            "count",
            "box",
            "is_visible",
            "is_enabled",
            "is_checked",
            "styles",
        ],
    ),
    (
        "browser",
        "action",
        &[
            "pdf",
            "record_start",
            "record_stop",
            "profile_start",
            "profile_stop",
            "metrics",
            "vitals",
        ],
    ),
    (
        "browser",
        "action",
        &[
            "cookies",
            "set_cookies",
            "clear_cookies",
            "storage",
            "emulate",
            "devices",
            "clipboard_read",
            "clipboard_write",
            "clipboard_copy",
            "clipboard_paste",
            "save_state",
            "load_state",
        ],
    ),
    ("browser", "action", &["locate"]),
];

/// Tools removed by a rename. A recorded call of the old name projects onto the successor: the
/// name is rewritten and the arguments gain the value that selected the old behaviour. Nothing else
/// about the old contract is reinterpreted; an unrecognised legacy shape is refused.
const RENAMED_TOOLS: &[(&str, &str, &str, &str)] = &[("kernel", "eval", "language", "py")];

/// Positions of call `id` in the assistant message at `index` and of its tool result.
fn paired_call(messages: &[Value], index: usize, id: &str) -> Option<(usize, usize)> {
    let call_index = messages[index]["tool_calls"]
        .as_array()?
        .iter()
        .position(|call| call["id"] == id)?;
    let result_index = messages
        .iter()
        .enumerate()
        .skip(index + 1)
        .take_while(|(_, message)| message["role"] == "tool")
        .find_map(|(index, message)| (message["tool_call_id"] == id).then_some(index))?;
    Some((call_index, result_index))
}

/// `check.expect`, offered only under `experimental.check_predictions`. The controller owns the
/// `check` schema; this shape lives here so history recorded with or without it stays valid when
/// the flag changes between runs.
pub fn check_prediction_property() -> Value {
    json!({"type":"string","enum":["pass","fail"],"description":"Optional prediction of the overall outcome before running: pass (every selected check passes) or fail. A mismatch is reported."})
}

/// Every additive variant of the current `check` schema: with or without `repro` (added after
/// release) and with or without `expect` (experimental). `None` unless the current schema has `repro`.
fn check_variants(schema: &Value) -> Option<Vec<Value>> {
    let properties = schema.pointer("/function/parameters/properties")?;
    // Only the canonical `expect` shape is the additive property; any other shape is a change.
    if properties.get("repro").is_none()
        || properties
            .get("expect")
            .is_some_and(|expect| *expect != check_prediction_property())
    {
        return None;
    }
    let mut variants = Vec::new();
    for keep_repro in [true, false] {
        for with_expect in [true, false] {
            let mut variant = schema.clone();
            let properties = variant
                .pointer_mut("/function/parameters/properties")
                .and_then(Value::as_object_mut)?;
            if !keep_repro {
                properties.remove("repro");
            }
            if with_expect {
                properties.insert("expect".into(), check_prediction_property());
            } else {
                properties.remove("expect");
            }
            variants.push(variant);
        }
    }
    Some(variants)
}

fn native_compatible(old: &Identity, new: &Identity, registry: &Registry) -> bool {
    if old.revision != 1
        || new.revision != 1
        || registry.get(&old.name).is_some()
        || !crate::tools::BUILTIN.contains(&old.name.as_str())
    {
        return false;
    }
    // Editor dialects are current-call contracts, not instructions to reinterpret old effects.
    // Recognize their exact identities while leaving old arguments/results as inert history.
    use crate::config::types::EditMode;
    let additive = ADDITIVE_PROPERTIES
        .iter()
        .find(|(name, _)| *name == old.name)
        .map_or(&[][..], |(_, properties)| *properties);
    let known: Vec<_> = [
        EditMode::Hashline,
        EditMode::Replace,
        EditMode::Patch,
        EditMode::ApplyPatch,
        EditMode::Sloppy,
    ]
    .into_iter()
    .filter_map(|mode| crate::tools::schema(&old.name, mode))
    .flat_map(|schema| {
        let mut before = schema.clone();
        if let Some(properties) = before
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut)
        {
            for property in additive {
                properties.remove(*property);
            }
        }
        // The pre-widening schema is the current one without the values added since release.
        let mut narrowed = before.clone();
        if let Some(properties) = narrowed
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut)
        {
            for (tool, property, values) in ADDITIVE_ENUM_VALUES {
                let Some(options) = properties
                    .get_mut(*property)
                    .filter(|_| *tool == old.name)
                    .and_then(|property| property.get_mut("enum"))
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                options.retain(|value| !value.as_str().is_some_and(|s| values.contains(&s)));
            }
        }
        [schema, before, narrowed]
    })
    .filter_map(|schema| identities(&[schema], &Registry::new()).remove(&old.name))
    .collect();
    known.contains(old) && known.contains(new)
}

/// The additive tables for tools that are not file/shell built-ins (registry tools such as `eval`,
/// `lsp`, `browser`, `find`, `web_search`). History recorded before this release's additions matches
/// the current schema with the listed optional properties and enum values removed, under the same
/// tool revision. Any other difference is still refused.
fn registry_additive_compatible(
    old: &Identity,
    new: &Identity,
    schemas: &[Value],
    registry: &Registry,
) -> bool {
    if old.revision != new.revision || crate::tools::BUILTIN.contains(&old.name.as_str()) {
        return false;
    }
    let properties = ADDITIVE_PROPERTIES
        .iter()
        .find(|(name, _)| *name == old.name)
        .map_or(&[][..], |(_, properties)| *properties);
    let has_enum_values = ADDITIVE_ENUM_VALUES
        .iter()
        .any(|(tool, ..)| *tool == old.name);
    if properties.is_empty() && !has_enum_values {
        return false;
    }
    let Some(current) = schemas
        .iter()
        .find(|schema| schema.pointer("/function/name").and_then(Value::as_str) == Some(&old.name))
    else {
        return false;
    };
    let mut without_properties = current.clone();
    if let Some(map) = without_properties
        .pointer_mut("/function/parameters/properties")
        .and_then(Value::as_object_mut)
    {
        for property in properties {
            map.remove(*property);
        }
    }
    let narrow = |schema: &Value| {
        let mut narrowed = schema.clone();
        if let Some(map) = narrowed
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut)
        {
            for (tool, property, values) in ADDITIVE_ENUM_VALUES {
                let Some(options) = map
                    .get_mut(*property)
                    .filter(|_| *tool == old.name)
                    .and_then(|property| property.get_mut("enum"))
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                options.retain(|value| !value.as_str().is_some_and(|s| values.contains(&s)));
            }
        }
        narrowed
    };
    let candidates = [
        narrow(current),
        narrow(&without_properties),
        without_properties,
    ];
    candidates.iter().any(|schema| {
        identities(std::slice::from_ref(schema), registry).get(&old.name) == Some(old)
    })
}

/// Recognize exact additive changes to controller-owned schemas. Historical calls remain inert,
/// never replayed or rewritten; every other part of the old contract must still match.
fn controller_compatible(
    old: &Identity,
    new: &Identity,
    schemas: &[Value],
    registry: &Registry,
) -> bool {
    if !matches!(old.name.as_str(), "working_state" | "check" | "task")
        || new.name != old.name
        || old.revision != 1
        || new.revision != 1
        || registry.get(&old.name).is_some()
    {
        return false;
    }
    let Some(schema) = schemas
        .iter()
        .find(|schema| schema["function"]["name"] == old.name)
    else {
        return false;
    };
    if old.name == "check" {
        return check_variants(schema).is_some_and(|variants| {
            variants
                .into_iter()
                .any(|variant| identities(&[variant], &Registry::new()).get(&old.name) == Some(old))
        });
    }
    if old.name == "task" {
        return task_variants(schema).is_some_and(|variants| {
            variants
                .into_iter()
                .any(|variant| identities(&[variant], &Registry::new()).get(&old.name) == Some(old))
        });
    }
    let mut before = schema.clone();
    let Some(properties) = before
        .pointer_mut("/function/parameters/properties")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    match old.name.as_str() {
        "working_state" => {
            if properties.remove("expected_revision").is_none()
                || properties.remove("claims").is_none()
                || properties["action"]["enum"] != json!(["view", "update", "patch"])
            {
                return false;
            }
            properties["action"]["enum"] = json!(["view", "update"]);
        }
        _ => return false,
    }
    identities(&[before], &Registry::new()).get(&old.name) == Some(old)
}

/// Optional `task` properties added since the prior release, each present at the top level and in
/// `tasks[]` items. Their absence keeps the old meaning.
const TASK_ADDITIVE: &[&[&str]] = &[&["source_session", "model"], &["isolation_backend"]];

/// Every prior shape of the current `task` schema: with or without the pool actions and
/// `concurrency`, and with or without each group in [`TASK_ADDITIVE`]. `None` unless the current
/// schema has the pool actions, the only shape these variants are derived from.
fn task_variants(schema: &Value) -> Option<Vec<Value>> {
    let properties = schema.pointer("/function/parameters/properties")?;
    if properties.get("concurrency").is_none()
        || properties["action"]["enum"]
            != json!([
                "run", "start", "resume", "status", "wait", "cancel", "create", "push", "peek",
                "close"
            ])
    {
        return None;
    }
    let mut variants = Vec::new();
    for mask in 0..(1u32 << (TASK_ADDITIVE.len() + 1)) {
        let mut variant = schema.clone();
        let properties = variant
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut)?;
        if mask & 1 != 0 {
            properties.remove("concurrency");
            properties["action"]["enum"] =
                json!(["run", "start", "resume", "status", "wait", "cancel"]);
        }
        for (bit, group) in TASK_ADDITIVE.iter().enumerate() {
            if mask & (2 << bit) == 0 {
                continue;
            }
            for property in *group {
                properties.remove(*property);
                if let Some(items) = properties
                    .get_mut("tasks")
                    .and_then(|tasks| tasks.pointer_mut("/items/properties"))
                    .and_then(Value::as_object_mut)
                {
                    items.remove(*property);
                }
            }
        }
        variants.push(variant);
    }
    Some(variants)
}

/// Project settled history onto the current contracts. `None` means no historical call needed a
/// rewrite, so callers send the borrowed history as-is; the history is copied only on first rewrite.
pub fn project_history(
    messages: &[Value],
    schemas: &[Value],
    registry: &Registry,
) -> Result<Option<Vec<Value>>> {
    let current = identities(schemas, registry);
    let mut projected: Option<Vec<Value>> = None;
    for (index, message) in messages.iter().enumerate() {
        let Some(recorded) = message.get("dial_tool_contracts") else {
            continue;
        };
        let recorded: BTreeMap<String, Identity> = serde_json::from_value(recorded.clone())?;
        for (id, old) in &recorded {
            if let Some(&(_, successor, field, value)) = RENAMED_TOOLS
                .iter()
                .find(|(from, ..)| *from == old.name)
                .filter(|_| !current.contains_key(&old.name))
            {
                // Catalog-level callback identities are never replayed, so they stay inert.
                if id.starts_with("callback:") {
                    continue;
                }
                let target = current.get(successor).ok_or_else(|| {
                    anyhow::anyhow!(
                        "tool {} was renamed to {successor}, which is unavailable",
                        old.name
                    )
                })?;
                ensure!(
                    old.revision == 1,
                    "tool {} contract revision {} has no rename projection",
                    old.name,
                    old.revision
                );
                let (call_index, _) = paired_call(messages, index, id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "tool {} history is unpaired; no rename projection",
                        old.name
                    )
                })?;
                let raw = &message["tool_calls"][call_index]["function"]["arguments"];
                let mut arguments: Value = match raw.as_str() {
                    Some(text) => serde_json::from_str(text)?,
                    None => raw.clone(),
                };
                let action = arguments.get("action").and_then(Value::as_str);
                // Legacy calls only selected the Python runtime; any other shape is refused.
                ensure!(
                    arguments.get(field).is_none()
                        && action
                            .is_some_and(|a| matches!(a, "eval" | "reset" | "cancel" | "status")),
                    "tool {} history has an unrecognised legacy shape; no rename projection",
                    old.name
                );
                arguments[field] = json!(value);
                let projected = projected.get_or_insert_with(|| messages.to_vec());
                projected[index]["tool_calls"][call_index]["function"]["name"] = json!(successor);
                projected[index]["tool_calls"][call_index]["function"]["arguments"] =
                    json!(arguments.to_string());
                projected[index]["dial_tool_contracts"][id] = json!(target);
                continue;
            }
            let Some(new) = current.get(&old.name) else {
                continue;
            };
            if old == new
                || native_compatible(old, new, registry)
                || registry_additive_compatible(old, new, schemas, registry)
                || controller_compatible(old, new, schemas, registry)
                || registry
                    .get(&old.name)
                    .is_some_and(|tool| tool.accepts_historical_contract(old))
            {
                continue;
            }
            let tool = registry.get(&old.name).ok_or_else(|| {
                anyhow::anyhow!("tool {} history needs a revision projector", old.name)
            })?;
            let call_index = message["tool_calls"]
                .as_array()
                .and_then(|calls| calls.iter().position(|call| call["id"] == *id));
            let result_index = messages
                .iter()
                .enumerate()
                .skip(index + 1)
                .take_while(|(_, message)| message["role"] == "tool")
                .find_map(|(index, message)| (message["tool_call_id"] == *id).then_some(index));
            let (Some(call_index), Some(result_index)) = (call_index, result_index) else {
                anyhow::bail!(
                    "tool {} contract changed; unpaired or callback-catalog history requires explicit compatibility",
                    old.name
                );
            };
            let arguments = &message["tool_calls"][call_index]["function"]["arguments"];
            let original = HistoricalCall {
                arguments: if let Some(text) = arguments.as_str() {
                    serde_json::from_str(text)?
                } else {
                    arguments.clone()
                },
                output: messages[result_index]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
            };
            let lifted = tool.project_historical_call(old, &original)
                .ok_or_else(|| anyhow::anyhow!("tool {} contract changed (revision {} -> {}); no compatible historical projection", old.name, old.revision, new.revision))?;
            ensure!(
                tool.project_historical_call(old, &original).as_ref() == Some(&lifted),
                "tool {} historical projection is nondeterministic",
                old.name
            );
            let projected = projected.get_or_insert_with(|| messages.to_vec());
            projected[index]["tool_calls"][call_index]["function"]["arguments"] =
                json!(lifted.arguments.to_string());
            projected[result_index]["content"] = json!(lifted.output);
            projected[index]["dial_tool_contracts"][id] = json!(new);
        }
    }
    Ok(projected)
}

fn parameter_shape(value: &Value) -> Value {
    let Value::Object(original) = value else {
        return value.clone();
    };
    let mut shape = original.clone();
    for annotation in ["description", "title", "examples", "$comment", "deprecated"] {
        shape.remove(annotation);
    }
    // Traverse schema nodes only: property names and literal enum/const/default objects are data.
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(Value::Object(children)) = shape.get_mut(keyword) {
            for child in children.values_mut() {
                *child = parameter_shape(child);
            }
        }
    }
    for keyword in [
        "items",
        "additionalItems",
        "additionalProperties",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "allOf",
        "anyOf",
        "oneOf",
        "prefixItems",
    ] {
        if let Some(child) = shape.get_mut(keyword) {
            if let Value::Array(children) = child {
                for child in children {
                    *child = parameter_shape(child);
                }
            } else {
                *child = parameter_shape(child);
            }
        }
    }
    Value::Object(shape)
}

pub fn identities(schemas: &[Value], registry: &Registry) -> BTreeMap<String, Identity> {
    schemas.iter().filter_map(|schema| {
        let name = schema.pointer("/function/name")?.as_str()?;
        // Documentation/presentation changes do not alter the executable contract. Parameters,
        // strictness and the tool-owned semantic revision do. serde_json maps are canonical here.
        let shape = json!({"parameters":parameter_shape(&schema["function"]["parameters"]), "strict":schema["function"]["strict"]});
        Some((name.to_owned(), Identity {
            name: name.to_owned(),
            revision: registry.get(name).map_or(1, |tool| tool.contract_revision()),
            schema_sha256: format!("{:x}", Sha256::digest(shape.to_string().as_bytes())),
        }))
    }).collect()
}

pub fn assistant_message(
    reply: &crate::llm::Reply,
    schemas: &[Value],
    registry: &Registry,
) -> Value {
    let mut message = crate::llm::assistant_message(reply);
    let available = identities(schemas, registry);
    let mut calls: BTreeMap<_, _> = reply
        .tool_calls
        .iter()
        .filter_map(|call| {
            available
                .get(&call.name)
                .map(|identity| (call.id.clone(), identity))
        })
        .collect();
    if reply.tool_calls.iter().any(|call| {
        registry
            .get(&call.name)
            .is_some_and(|tool| tool.supports_tool_dispatch())
    }) {
        // Bind the catalog callable through code-mode callbacks as well as direct calls.
        calls.extend(
            available
                .iter()
                .map(|(name, identity)| (format!("callback:{name}"), identity)),
        );
    }
    if !calls.is_empty() {
        message["dial_tool_contracts"] = json!(calls);
    }
    message
}

/// Validate historical contracts against currently available tools before continuation. Removed
/// tools are inert history, and legacy unversioned rows remain readable; neither grants authority.
/// A tool may explicitly attest compatibility with an old identity. Schema changes are otherwise
/// refused instead of guessing how old arguments or results should be reinterpreted.
pub fn validate_history(messages: &[Value], schemas: &[Value], registry: &Registry) -> Result<()> {
    project_history(messages, schemas, registry)?;
    Ok(())
}

#[cfg(test)]
mod tests;
