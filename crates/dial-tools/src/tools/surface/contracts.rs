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
    ("bash", &["session", "pty", "check", "check_paths", "trace"]),
    ("bg", &["persist", "lifetime", "call"]),
    (
        "monitor",
        &["run", "after_secs", "on_exit", "note", "pid", "save"],
    ),
    ("grep", &["output", "preview", "ast", "lang"]),
    ("glob", &["preview"]),
    ("edit", &["allow_syntax_errors", "verify"]),
    (
        "write",
        &[
            "allow_syntax_errors",
            "verify",
            "encoding",
            "source",
            "files",
            "if_absent",
            "expected_sha256",
            "idempotent",
            "receipt",
        ],
    ),
    (
        "read",
        &[
            "format",
            "retain",
            "fetch",
            "raw",
            "max_chars",
            "budget",
            "fresh",
        ],
    ),
    ("ast_edit", &["allow_syntax_errors"]),
    ("skill", &["action", "description", "body", "scope"]),
    ("checkpoint", &["action", "report"]),
    ("ask", &["action", "path", "reason", "sensitive"]),
    (
        "task",
        &[
            "source_session",
            "model",
            "isolation_backend",
            "acceptance_checks",
            "contract",
        ],
    ),
    ("find", &["related"]),
    ("lsp", &["payload", "direction"]),
    ("dap", &["filters", "filterOptions", "exceptionOptions"]),
    ("tool_search", &["load", "names", "unload"]),
    ("eval", &["background", "title"]),
    ("verify", &["acceptance", "plan"]),
    ("web_search", &["domains", "exclude_domains", "fetch_top"]),
    (
        "session_search",
        &["view", "layer", "record", "offset", "start", "format"],
    ),
    ("image_gen", &["image", "frame_count", "frame_duration_ms"]),
    ("model_weights", &["span"]),
    ("gitea_pr", &["style"]),
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
            "observe",
        ],
    ),
    // The evidence family (`evidence_search`, `workbench`, `investigate`, `reality`, `timeline`,
    // `perception`)
    // merged into `evidence`; their properties arrived together with their actions.
    (
        "evidence",
        &[
            "references",
            "query",
            "limit",
            "ranking",
            "depth",
            "correction",
            "question",
            "findings",
            "hypotheses",
            "hypothesis",
            "observation",
            "assessment",
            "expectation",
            "expectation_quote",
            "claim",
            "expected",
            "selector",
            "operator",
            "sources",
            "events",
            "alignments",
            "limits",
            "tool",
            "request",
            "reason",
        ],
    ),
];

/// Enum values added after release. History recorded before them stays valid because the old
/// schema is the current one with exactly these values removed.
const ADDITIVE_ENUM_VALUES: &[(&str, &str, &[&str])] = &[
    ("git", "action", &["reflog"]),
    ("git", "forge", &["gitea"]),
    (
        "git",
        "op",
        &["pr_list", "pr_view", "pr_comment", "pr_approve", "pr_merge"],
    ),
    ("eval", "language", &["py"]),
    ("find", "ranking", &["embedding"]),
    ("monitor", "action", &["saved", "restore", "forget"]),
    ("bg", "action", &["saved", "restore", "forget"]),
    ("codebase_memory", "action", &["navigate"]),
    ("task", "action", &["verify"]),
    ("lsp", "action", &["request"]),
    (
        "lsp",
        "action",
        &["format", "completion", "signature_help", "call_hierarchy"],
    ),
    (
        "dap",
        "action",
        &["setExceptionBreakpoints", "exceptionInfo", "restart"],
    ),
    ("model_weights", "method", &["add"]),
    (
        "data",
        "action",
        &["histogram", "trend", "correlation", "inspect"],
    ),
    ("data", "format", &["jsonl"]),
    ("audio", "action", &["analyze"]),
    ("document", "action", &["inspect"]),
    (
        "dep_source",
        "ecosystem",
        &["jvm", "ruby", "composer", "nuget"],
    ),
    (
        "evidence",
        "action",
        &[
            "search",
            "audit",
            "lineage",
            "correct",
            "diff",
            "interpret",
            "plan",
            "result",
            "check",
            "assess",
            "timeline",
            "budget_open",
            "budget_status",
            "budget_step",
            "budget_close",
        ],
    ),
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

/// Memory tools merged into `memory`: (recorded tool, the `action` that now selects it). A recorded
/// call projects onto `memory` with that action added; its other arguments keep their names.
const MERGED_MEMORY_TOOLS: &[(&str, &str)] = &[
    ("recall", "recall"),
    ("retain", "retain"),
    ("memory_update", "update"),
    ("forget", "forget"),
    ("memory_history", "history"),
    ("memory_import", "import"),
    ("reflect", "reflect"),
    ("learn", "learn"),
];

/// The successor of a removed tool: (successor, field, value) for [`RENAMED_TOOLS`] and
/// [`MERGED_MEMORY_TOOLS`].
fn successor(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    RENAMED_TOOLS
        .iter()
        .find(|(from, ..)| *from == name)
        .map(|&(_, successor, field, value)| (successor, field, value))
        .or_else(|| {
            MERGED_MEMORY_TOOLS
                .iter()
                .find(|(from, _)| *from == name)
                .map(|&(_, action)| ("memory", "action", action))
        })
}

/// A recorded call of a tool merged into `evidence` (`knowledge::evidence::MERGED_ACTIONS`):
/// (call index, the `evidence` arguments, the current `evidence` identity). `None` when the
/// identity is not a retired evidence tool, is still registered, is a catalog callback, or
/// `evidence` is not callable in this request (the history then stays inert, like any other
/// call to a tool outside the current set, and is projected once `evidence` is loaded).
fn merged_evidence_projection(
    messages: &[Value],
    index: usize,
    id: &str,
    old: &Identity,
    current: &BTreeMap<String, Identity>,
) -> Result<Option<(usize, Value, Identity)>> {
    use crate::tools::knowledge::evidence::{MERGED_ACTIONS, merged_action};
    if current.contains_key(&old.name)
        || id.starts_with("callback:")
        || !MERGED_ACTIONS.iter().any(|(tool, ..)| *tool == old.name)
    {
        return Ok(None);
    }
    let Some(target) = current.get("evidence") else {
        return Ok(None);
    };
    ensure!(
        old.revision == 1,
        "tool {} contract revision {} has no merge projection",
        old.name,
        old.revision
    );
    let message = &messages[index];
    let (call_index, _) = paired_call(messages, index, id).ok_or_else(|| {
        anyhow::anyhow!("tool {} history is unpaired; no merge projection", old.name)
    })?;
    let raw = &message["tool_calls"][call_index]["function"]["arguments"];
    let mut arguments: Value = match raw.as_str() {
        Some(text) => serde_json::from_str(text)?,
        None => raw.clone(),
    };
    let action = arguments
        .is_object()
        .then(|| merged_action(&old.name, &arguments))
        .flatten()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "tool {} history has an unrecognised legacy shape; no merge projection",
                old.name
            )
        })?;
    arguments["action"] = json!(action);
    Ok(Some((call_index, arguments, target.clone())))
}

/// The `todo` ops that replaced the `working_state` tool's actions (`view`, `update`, `patch`).
const WORKING_STATE_OPS: [(&str, &str); 3] = [
    ("view", "state_view"),
    ("update", "state_update"),
    ("patch", "state_patch"),
];

/// Recorded `working_state` arguments as the equivalent `todo` state call: `op` from the action,
/// `expected_revision` kept, every other field moved into `state`. `None` for an action the
/// tool never had.
pub fn working_state_as_todo(arguments: &Value) -> Option<Value> {
    let fields = arguments.as_object()?;
    let action = fields.get("action")?.as_str()?;
    let op = WORKING_STATE_OPS.iter().find(|(old, _)| *old == action)?.1;
    let mut call = json!({ "op": op });
    let state: serde_json::Map<String, Value> = fields
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "action" | "expected_revision"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(revision) = fields.get("expected_revision") {
        call["expected_revision"] = revision.clone();
    }
    if !state.is_empty() {
        call["state"] = Value::Object(state);
    }
    Some(call)
}

/// A recorded `working_state` call projected onto `todo`: (call index, the `todo` arguments, the
/// current `todo` identity). `None` while `working_state` is still offered, for catalog
/// callbacks, or when `todo` is not callable in this request (the history then stays inert).
fn merged_todo_projection(
    messages: &[Value],
    index: usize,
    id: &str,
    old: &Identity,
    current: &BTreeMap<String, Identity>,
) -> Result<Option<(usize, Value, Identity)>> {
    if old.name != "working_state" || current.contains_key(&old.name) || id.starts_with("callback:")
    {
        return Ok(None);
    }
    let Some(target) = current.get("todo") else {
        return Ok(None);
    };
    ensure!(
        old.revision == 1,
        "tool working_state contract revision {} has no merge projection",
        old.revision
    );
    let (call_index, _) = paired_call(messages, index, id).ok_or_else(|| {
        anyhow::anyhow!("tool working_state history is unpaired; no merge projection")
    })?;
    let raw = &messages[index]["tool_calls"][call_index]["function"]["arguments"];
    let arguments: Value = match raw.as_str() {
        Some(text) => serde_json::from_str(text)?,
        None => raw.clone(),
    };
    let call = working_state_as_todo(&arguments).ok_or_else(|| {
        anyhow::anyhow!(
            "tool working_state history has an unrecognised legacy shape; no merge projection"
        )
    })?;
    Ok(Some((call_index, call, target.clone())))
}

/// `artifact_read` merged into `read`: a recorded call's arguments become the equivalent
/// `artifact://` path selector. `None` for any shape the old tool would not have accepted.
fn artifact_read_path(arguments: &Value) -> Option<String> {
    let object = arguments.as_object()?;
    let known = [
        "uri",
        "info",
        "annotations",
        "start",
        "count",
        "offset",
        "bytes",
    ];
    if object.keys().any(|key| !known.contains(&key.as_str())) {
        return None;
    }
    let uri = object.get("uri")?.as_str()?;
    let number = |key: &str| match object.get(key) {
        None => Some(None),
        Some(value) => value.as_u64().map(Some),
    };
    let (start, count) = (number("start")?, number("count")?);
    let selector = if object.get("annotations").and_then(Value::as_bool) == Some(true) {
        // Annotation paging has no selector form; only the default first page projects.
        if start.is_some() || count.is_some() {
            return None;
        }
        "annotations".to_owned()
    } else if object.get("info").and_then(Value::as_bool) == Some(true) {
        "info".to_owned()
    } else if let Some(offset) = number("offset")? {
        format!("bytes={offset}+{}", number("bytes")?.unwrap_or(8192))
    } else {
        format!("{}+{}", start.unwrap_or(1), count.unwrap_or(100))
    };
    Some(format!("{uri}:{selector}"))
}

/// Parameters renamed to simpler names: (tool, recorded name, current name). History recorded under
/// the old name projects onto the new one by moving the key; nothing else is reinterpreted. Live
/// calls that still use the old name are normalized the same way ([`legacy_arguments`]).
const RENAMED_PROPERTIES: &[(&str, &str, &str)] = &[
    ("grep", "fixed_string", "literal"),
    ("grep", "max_results", "limit"),
    ("find", "grep_keywords", "keywords"),
];

/// Removed boolean aliases: (tool, recorded alias, the property it duplicated). A recorded
/// `alias: true` meant `property: true`; any other value meant nothing.
const REMOVED_ALIASES: &[(&str, &str, &str)] = &[("bash", "async", "background")];

fn has_legacy_names(tool: &str) -> bool {
    RENAMED_PROPERTIES.iter().any(|(t, ..)| *t == tool)
        || REMOVED_ALIASES.iter().any(|(t, ..)| *t == tool)
        || matches!(tool, "grep" | "glob")
}

/// The schema as recorded before the renames: current names moved back, removed aliases restored,
/// and `grep`/`glob` paging as `skip` plus a structured cursor ([`paged_cursor_schema`]).
fn legacy_schema(schema: &mut Value) {
    paged_cursor_schema(schema);
    let Some(name) = schema
        .pointer("/function/name")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let Some(properties) = schema
        .pointer_mut("/function/parameters/properties")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for (tool, recorded, current) in RENAMED_PROPERTIES {
        if *tool == name
            && let Some(value) = properties.remove(*current)
        {
            properties.insert((*recorded).into(), value);
        }
    }
    for (tool, alias, canonical) in REMOVED_ALIASES {
        if *tool == name
            && let Some(value) = properties.get(*canonical).cloned()
        {
            properties.insert((*alias).into(), value);
        }
    }
}

/// Move recorded or habitual old parameter names of `tool` onto the current names in `parameters`
/// (the tool's current parameter schema). An explicit current-name value wins over a renamed one.
pub fn legacy_arguments(tool: &str, parameters: &Value, arguments: &mut Value) {
    let Some(properties) = parameters.get("properties").and_then(Value::as_object) else {
        return;
    };
    let Some(object) = arguments.as_object_mut() else {
        return;
    };
    for (owner, recorded, current) in RENAMED_PROPERTIES {
        if *owner == tool
            && properties.contains_key(*current)
            && !properties.contains_key(*recorded)
            && let Some(value) = object.remove(*recorded)
            && !object.contains_key(*current)
        {
            object.insert((*current).into(), value);
        }
    }
    for (owner, alias, canonical) in REMOVED_ALIASES {
        if *owner == tool
            && properties.contains_key(*canonical)
            && !properties.contains_key(*alias)
            && let Some(value) = object.remove(*alias)
            && value == Value::Bool(true)
        {
            object.insert((*canonical).into(), value);
        }
    }
    legacy_paging_arguments(tool, properties, object);
}

/// `grep`/`glob` paging before the single string `cursor`: an integer `skip` plus a structured
/// `{path, match}` cursor (`match` was always 0 for glob). Other schemas are left unchanged.
fn paged_cursor_schema(schema: &mut Value) {
    let name = schema["function"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if !matches!(name.as_str(), "grep" | "glob") {
        return;
    }
    let Some(properties) = schema
        .pointer_mut("/function/parameters/properties")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if properties
        .get("cursor")
        .and_then(|cursor| cursor.get("type"))
        != Some(&json!("string"))
    {
        return;
    }
    let mut matched = json!({"type": "integer", "minimum": 0});
    if name == "glob" {
        matched["maximum"] = json!(0);
    }
    properties.insert(
        "cursor".into(),
        json!({"type": "object", "properties": {"path": {"type": "string"}, "match": matched},
            "required": ["path", "match"], "additionalProperties": false}),
    );
    properties.insert("skip".into(), json!({"type": "integer", "minimum": 0}));
}

/// A habitual pre-single-cursor `grep`/`glob` call: a structured `{path, match}` cursor and/or
/// integer `skip` become the one string cursor (`"consumed:skip:path"` or `"skip"`). Anything
/// else is left for ordinary validation to refuse.
fn legacy_paging_arguments(
    tool: &str,
    properties: &serde_json::Map<String, Value>,
    object: &mut serde_json::Map<String, Value>,
) {
    if !matches!(tool, "grep" | "glob")
        || properties.contains_key("skip")
        || properties
            .get("cursor")
            .and_then(|cursor| cursor.get("type"))
            != Some(&json!("string"))
    {
        return;
    }
    let skip = match object.get("skip") {
        None | Some(Value::Null) => 0,
        Some(value) => match value.as_u64() {
            Some(skip) => skip,
            None => return,
        },
    };
    let cursor = match object.get("cursor") {
        None | Some(Value::Null) => skip.to_string(),
        Some(Value::Object(cursor)) => {
            let (Some(path), Some(consumed)) = (
                cursor.get("path").and_then(Value::as_str),
                cursor.get("match").and_then(Value::as_u64),
            ) else {
                return;
            };
            format!("{consumed}:{skip}:{path}")
        }
        Some(_) => return,
    };
    object.remove("skip");
    if cursor == "0" {
        object.remove("cursor");
    } else {
        object.insert("cursor".into(), json!(cursor));
    }
}

/// History recorded before [`RENAMED_PROPERTIES`] / [`REMOVED_ALIASES`]: the old identity matches
/// the current schema with those names restored, under the same revision and additive rules.
fn legacy_compatible(
    old: &Identity,
    new: &Identity,
    schemas: &[Value],
    registry: &Registry,
) -> bool {
    if !has_legacy_names(&old.name) {
        return false;
    }
    if crate::tools::BUILTIN.contains(&old.name.as_str()) {
        return old.revision == 1
            && new.revision == 1
            && registry.get(&old.name).is_none()
            && native_identities(&old.name, true).contains(old)
            && native_identities(&old.name, false).contains(new);
    }
    let mut legacy = schemas.to_vec();
    for schema in &mut legacy {
        if schema.pointer("/function/name").and_then(Value::as_str) == Some(&old.name) {
            legacy_schema(schema);
        }
    }
    identities(&legacy, registry).get(&old.name) == Some(old)
        || registry_additive_compatible(old, new, &legacy, registry)
}

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

/// `verify.expect`, offered only under `experimental.check_predictions`. The controller owns the
/// `verify` schema; this shape lives here so history recorded with or without it stays valid when
/// the flag changes between runs.
pub fn check_prediction_property() -> Value {
    json!({"type":"string","enum":["pass","fail"],"description":"Optional prediction of the overall outcome before running: pass (every selected check passes) or fail. A mismatch is reported."})
}

/// Every additive variant of the current `verify` schema: with or without `repro` and with or
/// without `expect` (experimental). `None` unless the current schema has `repro`.
fn verify_variants(schema: &Value) -> Option<Vec<Value>> {
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
    let known = native_identities(&old.name, false);
    known.contains(old) && known.contains(new)
}

/// Every recognized identity of a file/shell built-in across editor dialects and additive releases;
/// `legacy` restores names renamed since release ([`legacy_schema`]).
fn native_identities(name: &str, legacy: bool) -> Vec<Identity> {
    // Editor dialects are current-call contracts, not instructions to reinterpret old effects.
    // Recognize their exact identities while leaving old arguments/results as inert history.
    use crate::config::types::EditMode;
    let additive = ADDITIVE_PROPERTIES
        .iter()
        .find(|(tool, _)| *tool == name)
        .map_or(&[][..], |(_, properties)| *properties);
    [
        EditMode::Hashline,
        EditMode::Replace,
        EditMode::Patch,
        EditMode::ApplyPatch,
        EditMode::Sloppy,
    ]
    .into_iter()
    .filter_map(|mode| crate::tools::schema(name, mode))
    .flat_map(|schema| {
        if !legacy {
            return vec![schema];
        }
        // History before the single string cursor, with and without the older renames.
        let mut paged = schema.clone();
        paged_cursor_schema(&mut paged);
        let mut renamed = schema;
        legacy_schema(&mut renamed);
        vec![renamed, paged]
    })
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
                    .filter(|_| *tool == name)
                    .and_then(|property| property.get_mut("enum"))
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                options.retain(|value| !value.as_str().is_some_and(|s| values.contains(&s)));
            }
        }
        // Optional properties can be introduced in separate releases. Preserve intermediate
        // identities too, not only the schema with every additive property removed.
        let mut variants = vec![schema];
        for property in additive {
            for index in 0..variants.len() {
                let mut variant = variants[index].clone();
                if variant
                    .pointer_mut("/function/parameters/properties")
                    .and_then(Value::as_object_mut)
                    .and_then(|properties| properties.remove(*property))
                    .is_some()
                {
                    variants.push(variant);
                }
            }
        }
        variants.push(narrowed);
        if name == "write" {
            // Before artifact/batch inputs, path and content were required. Only recognize
            // that exact historical requirement when both alternative inputs are absent.
            let historical: Vec<_> = variants
                .iter()
                .filter_map(|variant| {
                    let properties = variant
                        .pointer("/function/parameters/properties")?
                        .as_object()?;
                    if properties.contains_key("source") || properties.contains_key("files") {
                        return None;
                    }
                    let mut old = variant.clone();
                    old["function"]["parameters"]["required"] = json!(["path", "content"]);
                    Some(old)
                })
                .collect();
            variants.extend(historical);
        }
        variants
    })
    .filter_map(|schema| identities(&[schema], &Registry::new()).remove(name))
    .collect()
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
    // A consolidated successor's schema is the union of its members' schemas
    // (`facade::GROUPS`), so a member's additions are additions to the successor too.
    let owners: Vec<&str> = std::iter::once(old.name.as_str())
        // Pinned adapter tools expose the same protocol schema as the selecting DAP owner.
        .chain(old.name.starts_with("dap_").then_some("dap"))
        .chain(
            crate::tools::surface::facade::GROUPS
                .iter()
                .filter(|group| group.successor == old.name)
                .flat_map(|group| group.members.iter().map(|(member, _)| *member)),
        )
        .collect();
    let additions: Vec<&[&str]> = owners
        .iter()
        .filter_map(|owner| {
            ADDITIVE_PROPERTIES
                .iter()
                .find(|(name, _)| name == owner)
                .map(|(_, properties)| *properties)
        })
        .collect();
    let has_enum_values = ADDITIVE_ENUM_VALUES
        .iter()
        .any(|(tool, ..)| owners.contains(tool));
    if additions.is_empty() && !has_enum_values {
        return false;
    }
    let Some(current) = schemas
        .iter()
        .find(|schema| schema.pointer("/function/name").and_then(Value::as_str) == Some(&old.name))
    else {
        return false;
    };
    // Additions are appended in release order per owner. Walking suffixes preserves every
    // intermediate release without enumerating a browser-sized powerset. Owners and enum groups
    // can land independently, so match their bounded cross product. Never change required
    // fields, existing property types, strictness or semantic revisions.
    let mut kept: Vec<usize> = additions.iter().map(|list| list.len()).collect();
    loop {
        let mut candidate = current.clone();
        if let Some(map) = candidate
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut)
        {
            for (list, kept) in additions.iter().zip(&kept) {
                for property in &list[*kept..] {
                    map.remove(*property);
                }
            }
        }
        if identities(std::slice::from_ref(&candidate), registry).get(&old.name) == Some(old) {
            return true;
        }
        for (tool, property, values) in ADDITIVE_ENUM_VALUES.iter().rev() {
            if !owners.contains(tool) {
                continue;
            }
            if let Some(options) = candidate
                .pointer_mut("/function/parameters/properties")
                .and_then(Value::as_object_mut)
                .and_then(|map| map.get_mut(*property))
                .and_then(|property| property.get_mut("enum"))
                .and_then(Value::as_array_mut)
            {
                options.retain(|value| !value.as_str().is_some_and(|s| values.contains(&s)));
            }
            if identities(std::slice::from_ref(&candidate), registry).get(&old.name) == Some(old) {
                return true;
            }
        }
        // Next combination: shorten the first owner's list that still has an addition.
        let Some(index) = kept.iter().position(|kept| *kept > 0) else {
            return false;
        };
        kept[index] -= 1;
        for (earlier, list) in kept[..index].iter_mut().zip(&additions) {
            *earlier = list.len();
        }
    }
}

/// Recognize exact additive changes to controller-owned schemas. Historical calls remain inert,
/// never replayed or rewritten; every other part of the old contract must still match.
fn controller_compatible(
    old: &Identity,
    new: &Identity,
    schemas: &[Value],
    registry: &Registry,
) -> bool {
    if !matches!(
        old.name.as_str(),
        "working_state"
            | "verify"
            | "task"
            | "todo"
            | "finish"
            | "ask"
            | "checkpoint"
            | "skill"
            | "tool_search"
    ) || new.name != old.name
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
    if old.name == "verify" {
        return verify_variants(schema).is_some_and(|variants| {
            variants
                .into_iter()
                .any(|variant| identities(&[variant], &Registry::new()).get(&old.name) == Some(old))
        });
    }
    if old.name == "task" {
        // Recorded before identities ignored the profile list, or after: accept either hash.
        return task_variants(schema).is_some_and(|variants| {
            variants.iter().any(|variant| {
                shape_sha256(variant) == old.schema_sha256
                    || stable_schema("task", variant)
                        .is_some_and(|stable| shape_sha256(&stable) == old.schema_sha256)
            })
        });
    }
    if old.name == "tool_search" {
        let mut without_load = schema.clone();
        if let Some(properties) = without_load
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut)
        {
            properties.remove("load");
        }
        if identities(&[without_load], registry).get(&old.name) == Some(old) {
            return true;
        }
    }
    let mut before = schema.clone();
    if matches!(
        old.name.as_str(),
        "ask" | "checkpoint" | "skill" | "tool_search"
    ) {
        let properties = before
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut);
        let Some(properties) = properties else {
            return false;
        };
        match old.name.as_str() {
            "ask" => {
                properties.retain(|key, _| key == "questions");
                before["function"]["parameters"]["required"] = json!(["questions"]);
            }
            "checkpoint" => {
                properties.retain(|key, _| key == "goal");
                before["function"]["parameters"]["required"] = json!(["goal"]);
            }
            "skill" => {
                properties.retain(|key, _| matches!(key.as_str(), "name" | "file"));
            }
            "tool_search" => {
                properties.remove("names");
                properties.remove("unload");
                before["function"]["parameters"]["required"] = json!(["query"]);
            }
            _ => return false,
        }
        return identities(std::slice::from_ref(&before), registry).get(&old.name) == Some(old)
            || registry_additive_compatible(old, new, &[before], registry);
    }
    if old.name == "working_state" {
        let Some(properties) = before
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut)
        else {
            return false;
        };
        // Admit the revisioned schema immediately preceding optional deliverables too,
        // not just the older replacement-only schema below.
        properties.remove("deliverables");
        if identities(std::slice::from_ref(&before), &Registry::new()).get(&old.name) == Some(old) {
            return true;
        }
    }
    if old.name == "todo" {
        // The state ops that replaced `working_state` were added in one release: the schema
        // before them lacks `state`, `expected_revision` and the `state_*` ops. A schema without
        // them predates nothing here and falls through to the older `kind` addition below.
        let Some(properties) = before
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut)
        else {
            return false;
        };
        match (
            properties.remove("state"),
            properties.remove("expected_revision"),
        ) {
            (None, None) => {}
            (Some(state), Some(revision)) => {
                if parameter_shape(&state) != json!({"type":"object"})
                    || parameter_shape(&revision) != json!({"type":"integer","minimum":0})
                {
                    return false;
                }
                let Some(ops) = properties
                    .get_mut("op")
                    .and_then(|op| op.get_mut("enum"))
                    .and_then(Value::as_array_mut)
                else {
                    return false;
                };
                let count = ops.len();
                ops.retain(|op| !WORKING_STATE_OPS.iter().any(|(_, new)| op == new));
                if count - ops.len() != WORKING_STATE_OPS.len() {
                    return false;
                }
                if identities(std::slice::from_ref(&before), &Registry::new()).get(&old.name)
                    == Some(old)
                {
                    return true;
                }
            }
            _ => return false,
        }
    }
    let Some(properties) = before
        .pointer_mut("/function/parameters/properties")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    match old.name.as_str() {
        "todo" => {
            let Some(items) = properties
                .get_mut("tasks")
                .and_then(|tasks| tasks.get_mut("items"))
            else {
                return false;
            };
            if items["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|field| field == "kind"))
            {
                return false;
            }
            let Some(kind) = items
                .get_mut("properties")
                .and_then(Value::as_object_mut)
                .and_then(|properties| properties.remove("kind"))
            else {
                return false;
            };
            if parameter_shape(&kind) != json!({"type":"string","enum":["step","follow_up"]}) {
                return false;
            }
        }
        "finish" => {
            if properties["outcome"]["enum"]
                != json!(["completed", "answered", "blocked", "needs_input"])
            {
                return false;
            }
            properties["outcome"]["enum"] = json!(["completed", "blocked", "needs_input"]);
        }
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

/// Optional `task` properties added since release or enabled by configuration (`work_packet`,
/// `model_profile`), each removed at the top level and in `tasks[]` items. Their absence keeps
/// the old meaning.
const TASK_OPTIONAL: &[&[&str]] = &[
    &["source_session", "model"],
    &["isolation_backend"],
    &["acceptance_checks"],
    &["contract"],
    &["work_packet"],
    &["model_profile"],
];
const TASK_RELEASED_ACTIONS: &[&str] = &["run", "start", "resume", "status", "wait", "cancel"];
const TASK_POOL_ACTIONS: &[&str] = &["create", "push", "peek", "close"];

/// Every prior shape of the current `task` schema: with or without the pool actions and
/// `concurrency`, with or without the `verify` action, and with or without each group in
/// [`TASK_OPTIONAL`]. `None` unless the current schema has the pool actions (optionally followed
/// by `verify`), the only shape these variants are derived from.
fn task_variants(schema: &Value) -> Option<Vec<Value>> {
    let question = schema
        .pointer("/function/parameters/properties/question")
        .cloned();
    let mut before_questions = schema.clone();
    let properties = before_questions
        .pointer_mut("/function/parameters/properties")?
        .as_object_mut()?;
    if properties.remove("question").is_some() {
        let actions = properties
            .get_mut("action")?
            .get_mut("enum")?
            .as_array_mut()?;
        let question_actions = [
            "question",
            "question_status",
            "question_wait",
            "question_cancel",
        ];
        if !question_actions
            .iter()
            .all(|action| actions.contains(&json!(action)))
        {
            return None;
        }
        actions.retain(|action| {
            !action
                .as_str()
                .is_some_and(|action| question_actions.contains(&action))
        });
    }
    let schema = &before_questions;
    let properties = schema.pointer("/function/parameters/properties")?;
    let pooled: Vec<&str> = TASK_RELEASED_ACTIONS
        .iter()
        .chain(TASK_POOL_ACTIONS)
        .copied()
        .collect();
    let mut verified = pooled.clone();
    verified.push("verify");
    let actions = &properties["action"]["enum"];
    if properties.get("concurrency").is_none()
        || (*actions != json!(pooled) && *actions != json!(verified))
    {
        return None;
    }
    let mut variants = Vec::new();
    for mask in 0..(1u32 << (TASK_OPTIONAL.len() + 2)) {
        let mut variant = schema.clone();
        let properties = variant
            .pointer_mut("/function/parameters/properties")
            .and_then(Value::as_object_mut)?;
        let mut removed_actions: Vec<&str> = Vec::new();
        if mask & 1 != 0 {
            properties.remove("concurrency");
            removed_actions.extend(TASK_POOL_ACTIONS);
        }
        if mask & 2 != 0 {
            removed_actions.push("verify");
        }
        if let Some(options) = properties["action"]["enum"].as_array_mut() {
            options.retain(|value| !value.as_str().is_some_and(|a| removed_actions.contains(&a)));
        }
        for (bit, group) in TASK_OPTIONAL.iter().enumerate() {
            if mask & (4 << bit) == 0 {
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
    if let Some(question) = question {
        let with_questions: Vec<Value> = variants
            .iter()
            .map(|variant| {
                let mut variant = variant.clone();
                variant["function"]["parameters"]["properties"]["question"] = question.clone();
                if let Some(actions) =
                    variant["function"]["parameters"]["properties"]["action"]["enum"].as_array_mut()
                {
                    actions.extend(
                        [
                            "question",
                            "question_status",
                            "question_wait",
                            "question_cancel",
                        ]
                        .map(|action| json!(action)),
                    );
                }
                variant
            })
            .collect();
        variants.extend(with_questions);
    }
    variants.sort_by_cached_key(Value::to_string);
    variants.dedup();
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
    // Compatibility is a pure function of (old, current); decide each recorded identity once.
    let mut accepted: Vec<Identity> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let Some(recorded) = message.get("dial_tool_contracts") else {
            continue;
        };
        let recorded: BTreeMap<String, Identity> = serde_json::from_value(recorded.clone())?;
        for (id, old) in &recorded {
            if old.name == "artifact_read" && !current.contains_key("artifact_read") {
                if id.starts_with("callback:") {
                    continue;
                }
                let target = current.get("read").ok_or_else(|| {
                    anyhow::anyhow!("tool artifact_read was merged into read, which is unavailable")
                })?;
                ensure!(
                    old.revision == 1,
                    "tool artifact_read contract revision {} has no merge projection",
                    old.revision
                );
                let (call_index, _) = paired_call(messages, index, id).ok_or_else(|| {
                    anyhow::anyhow!("tool artifact_read history is unpaired; no merge projection")
                })?;
                let raw = &message["tool_calls"][call_index]["function"]["arguments"];
                let arguments: Value = match raw.as_str() {
                    Some(text) => serde_json::from_str(text)?,
                    None => raw.clone(),
                };
                let path = artifact_read_path(&arguments).ok_or_else(|| {
                    anyhow::anyhow!(
                        "tool artifact_read history has an unrecognised legacy shape; no merge projection"
                    )
                })?;
                let projected = projected.get_or_insert_with(|| messages.to_vec());
                projected[index]["tool_calls"][call_index]["function"]["name"] = json!("read");
                projected[index]["tool_calls"][call_index]["function"]["arguments"] =
                    json!(json!({ "path": path }).to_string());
                projected[index]["dial_tool_contracts"][id] = json!(target);
                continue;
            }
            if let Some(merged) = merged_evidence_projection(messages, index, id, old, &current)? {
                let (call_index, arguments, target) = merged;
                let projected = projected.get_or_insert_with(|| messages.to_vec());
                projected[index]["tool_calls"][call_index]["function"]["name"] = json!("evidence");
                projected[index]["tool_calls"][call_index]["function"]["arguments"] =
                    json!(arguments.to_string());
                projected[index]["dial_tool_contracts"][id] = json!(target);
                continue;
            }
            if let Some(merged) = merged_todo_projection(messages, index, id, old, &current)? {
                let (call_index, arguments, target) = merged;
                let projected = projected.get_or_insert_with(|| messages.to_vec());
                projected[index]["tool_calls"][call_index]["function"]["name"] = json!("todo");
                projected[index]["tool_calls"][call_index]["function"]["arguments"] =
                    json!(arguments.to_string());
                projected[index]["dial_tool_contracts"][id] = json!(target);
                continue;
            }
            if !id.starts_with("callback:")
                && !current.contains_key(&old.name)
                && let Some((call_index, _)) = paired_call(messages, index, id)
            {
                let raw = &message["tool_calls"][call_index]["function"]["arguments"];
                let arguments = match raw.as_str() {
                    Some(text) => serde_json::from_str(text)?,
                    None => raw.clone(),
                };
                if let Some((successor, arguments)) =
                    registry.historical_mcp_projection(old, &arguments)
                    && let Some(target) = current.get(&successor)
                {
                    let projected = projected.get_or_insert_with(|| messages.to_vec());
                    projected[index]["tool_calls"][call_index]["function"]["name"] =
                        json!(successor);
                    projected[index]["tool_calls"][call_index]["function"]["arguments"] =
                        json!(arguments.to_string());
                    projected[index]["dial_tool_contracts"][id] = json!(target);
                    continue;
                }
            }
            if let Some(successor) = crate::tools::surface::facade::successor_of(&old.name)
                .filter(|_| !current.contains_key(&old.name))
            {
                // Callback identities are never replayed; a successor outside this request leaves
                // the call inert until it is loaded, like any call to an unoffered tool.
                if id.starts_with("callback:") {
                    continue;
                }
                let Some(target) = current.get(successor) else {
                    continue;
                };
                let owner = registry.get(&old.name);
                let controller = matches!(
                    old.name.as_str(),
                    "tool_load" | "skill_manage" | "rewind" | "side_request" | "request_directory"
                );
                ensure!(
                    owner.is_some() || controller,
                    "tool {} was consolidated into {successor} but is unavailable",
                    old.name
                );
                ensure!(
                    old.revision == owner.map_or(1, |owner| owner.contract_revision()),
                    "tool {} contract revision {} has no consolidation projection",
                    old.name,
                    old.revision
                );
                if let Some(owner) = owner {
                    let schema = owner.schema();
                    let identity = identities(std::slice::from_ref(&schema), registry)
                        .remove(&old.name)
                        .ok_or_else(|| {
                            anyhow::anyhow!("missing owner identity for {}", old.name)
                        })?;
                    ensure!(
                        old == &identity
                            || registry_additive_compatible(old, &identity, &[schema], registry)
                            || owner.accepts_historical_contract(old),
                        "tool {} history has an unrecognised legacy contract; no consolidation projection",
                        old.name
                    );
                }
                let (call_index, _) = paired_call(messages, index, id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "tool {} history is unpaired; no consolidation projection",
                        old.name
                    )
                })?;
                let raw = &message["tool_calls"][call_index]["function"]["arguments"];
                let arguments: Value = match raw.as_str() {
                    Some(text) => serde_json::from_str(text)?,
                    None => raw.clone(),
                };
                let (_, arguments) =
                    crate::tools::surface::facade::successor_call(&old.name, &arguments)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "tool {} history has an unrecognised legacy shape; no consolidation projection",
                                old.name
                            )
                        })?;
                let projected = projected.get_or_insert_with(|| messages.to_vec());
                projected[index]["tool_calls"][call_index]["function"]["name"] = json!(successor);
                projected[index]["tool_calls"][call_index]["function"]["arguments"] =
                    json!(arguments.to_string());
                projected[index]["dial_tool_contracts"][id] = json!(target);
                continue;
            }
            if let Some((successor, field, value)) =
                successor(&old.name).filter(|_| !current.contains_key(&old.name))
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
                // Legacy kernel calls only selected the Python runtime and merged memory calls had
                // no action; any other shape is refused.
                let recognised = if successor == "memory" {
                    arguments.is_object() && arguments.get(field).is_none()
                } else {
                    arguments.get(field).is_none()
                        && action
                            .is_some_and(|a| matches!(a, "eval" | "reset" | "cancel" | "status"))
                };
                ensure!(
                    recognised,
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
            if accepted.contains(old) {
                continue;
            }
            if old == new
                || native_compatible(old, new, registry)
                || registry_additive_compatible(old, new, schemas, registry)
                || controller_compatible(old, new, schemas, registry)
                || registry
                    .get(&old.name)
                    .is_some_and(|tool| tool.accepts_historical_contract(old))
            {
                accepted.push(old.clone());
                continue;
            }
            if legacy_compatible(old, new, schemas, registry) {
                // Rewrite only the paired call's argument keys; callback catalogs stay inert.
                let parameters = schemas
                    .iter()
                    .find(|schema| schema["function"]["name"] == old.name.as_str())
                    .map(|schema| schema["function"]["parameters"].clone())
                    .unwrap_or(Value::Null);
                if let Some(call_index) = message["tool_calls"]
                    .as_array()
                    .and_then(|calls| calls.iter().position(|call| call["id"] == *id))
                {
                    let raw = &message["tool_calls"][call_index]["function"]["arguments"];
                    let mut arguments: Value = match raw.as_str() {
                        Some(text) => serde_json::from_str(text)?,
                        None => raw.clone(),
                    };
                    legacy_arguments(&old.name, &parameters, &mut arguments);
                    let projected = projected.get_or_insert_with(|| messages.to_vec());
                    projected[index]["tool_calls"][call_index]["function"]["arguments"] =
                        json!(arguments.to_string());
                    projected[index]["dial_tool_contracts"][id] = json!(new);
                }
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

fn sort_object_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.sort_keys();
            for value in map.values_mut() {
                sort_object_keys(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                sort_object_keys(value);
            }
        }
        _ => {}
    }
}

pub fn identities(schemas: &[Value], registry: &Registry) -> BTreeMap<String, Identity> {
    schemas
        .iter()
        .filter_map(|schema| {
            let name = schema.pointer("/function/name")?.as_str()?;
            let stable = stable_schema(name, schema);
            Some((
                name.to_owned(),
                Identity {
                    name: name.to_owned(),
                    revision: registry
                        .get(name)
                        .map_or(1, |tool| tool.contract_revision()),
                    schema_sha256: shape_sha256(stable.as_ref().unwrap_or(schema)),
                },
            ))
        })
        .collect()
}

/// Hash of a schema's executable contract. Documentation/presentation changes do not alter it;
/// parameters and strictness do (the tool-owned semantic revision is recorded beside it). Keys
/// are sorted explicitly even when a dependency enables serde_json's preserve_order feature.
fn shape_sha256(schema: &Value) -> String {
    let mut shape = json!({"parameters":parameter_shape(&schema["function"]["parameters"]), "strict":schema["function"]["strict"]});
    sort_object_keys(&mut shape);
    format!("{:x}", Sha256::digest(shape.to_string().as_bytes()))
}

/// `task.model_profile` enumerates the configured profile names, which change with settings and
/// the model catalog rather than with the tool's contract. Its identity omits that list, so adding
/// a profile mid-session no longer makes recorded `task` history unresumable.
fn stable_schema(name: &str, schema: &Value) -> Option<Value> {
    if name != "task" {
        return None;
    }
    let mut stable = schema.clone();
    stable
        .pointer_mut("/function/parameters/properties/model_profile")
        .and_then(Value::as_object_mut)
        .and_then(|property| property.remove("enum"))
        .map(|_| stable)
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
