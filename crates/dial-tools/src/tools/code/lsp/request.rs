//! The `request` action: one raw LSP method call with model-supplied params.
//!
//! Everything here is pure. The method is checked against a refusal list and a capability table,
//! params are built from the model's `payload` plus per-field defaults, and results are bounded
//! JSON. Sending and capability checks against a live server stay in `mod.rs`.
use serde_json::{Map, Value, json};

use super::check_cancel;
use super::client::Client;
use super::format::clip;
use super::workspace_edit::{Plan, Snapshot};
use crate::tools::Workspace;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Longest method name accepted.
pub const MAX_METHOD_CHARS: usize = 128;
/// Largest `payload` accepted, in serialized bytes.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
/// Longest params preview echoed in an error.
pub const MAX_PREVIEW_CHARS: usize = 400;
/// Longest result text returned to the model.
pub const MAX_RESULT_CHARS: usize = 32_000;

/// A validated request ready to send.
#[derive(Debug, PartialEq)]
pub struct Call {
    pub method: String,
    /// The `ServerCapabilities` pointer that advertises `method`, when the method is known.
    pub pointer: Option<&'static str>,
    pub params: Value,
}

/// Capability pointers for the methods Dial knows. Unknown methods have no pointer and must be
/// advertised in `initialize` experimental capabilities or dynamic registrations.
const CAPABILITIES: &[(&str, &str)] = &[
    ("textDocument/hover", "/hoverProvider"),
    ("textDocument/definition", "/definitionProvider"),
    ("textDocument/references", "/referencesProvider"),
    ("textDocument/documentSymbol", "/documentSymbolProvider"),
    ("textDocument/implementation", "/implementationProvider"),
    ("textDocument/typeDefinition", "/typeDefinitionProvider"),
    ("textDocument/completion", "/completionProvider"),
    ("textDocument/signatureHelp", "/signatureHelpProvider"),
    (
        "textDocument/documentHighlight",
        "/documentHighlightProvider",
    ),
    ("textDocument/documentLink", "/documentLinkProvider"),
    ("textDocument/codeLens", "/codeLensProvider"),
    ("textDocument/inlayHint", "/inlayHintProvider"),
    ("textDocument/foldingRange", "/foldingRangeProvider"),
    ("textDocument/selectionRange", "/selectionRangeProvider"),
    (
        "textDocument/semanticTokens/full",
        "/semanticTokensProvider",
    ),
    ("textDocument/formatting", "/documentFormattingProvider"),
    (
        "textDocument/rangeFormatting",
        "/documentRangeFormattingProvider",
    ),
    ("textDocument/diagnostic", "/diagnosticProvider"),
    ("textDocument/codeAction", "/codeActionProvider"),
    ("textDocument/prepareRename", "/renameProvider"),
    ("textDocument/rename", "/renameProvider"),
    ("workspace/symbol", "/workspaceSymbolProvider"),
    (
        "workspace/willRenameFiles",
        "/workspace/fileOperations/willRename",
    ),
    ("codeAction/resolve", "/codeActionProvider/resolveProvider"),
    (
        "completionItem/resolve",
        "/completionProvider/resolveProvider",
    ),
    ("codeLens/resolve", "/codeLensProvider/resolveProvider"),
    ("inlayHint/resolve", "/inlayHintProvider/resolveProvider"),
    (
        "documentLink/resolve",
        "/documentLinkProvider/resolveProvider",
    ),
];

/// Validates the method name and returns the reason it is refused, if it is.
fn validate_method(query: Option<&str>) -> Result<&str, String> {
    let method = query.map(str::trim).unwrap_or_default();
    if method.is_empty() {
        return Err(
            "action=request needs `query` set to an LSP method name, e.g. textDocument/inlayHint"
                .to_owned(),
        );
    }
    if method.chars().count() > MAX_METHOD_CHARS {
        return Err(format!(
            "LSP method name is longer than {MAX_METHOD_CHARS} characters"
        ));
    }
    let well_formed = method
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '$' | '_' | '-' | '.'));
    if !well_formed {
        return Err(format!("`{method}` is not a valid LSP method name"));
    }
    if let Some(reason) = refusal(method) {
        return Err(format!("LSP method `{method}` is refused: {reason}"));
    }
    Ok(method)
}

/// Why a method must not go through `request`, or `None` when it may.
fn refusal(method: &str) -> Option<&'static str> {
    let last = method.rsplit('/').next().unwrap_or(method);
    match method {
        "initialize" | "initialized" | "shutdown" | "exit" => {
            Some("the session lifecycle is managed by Dial")
        }
        "workspace/executeCommand" => {
            Some("use the execute_command action, which needs approval and reports its effects")
        }
        "workspace/applyEdit" => {
            Some("it is server-to-client only; Dial never applies unsolicited edits")
        }
        "codeAction/resolve" => {
            Some("use the code_action_resolve action, which resolves only cached actions")
        }
        "workspace/configuration" | "workspace/workspaceFolders" => {
            Some("it is a server-to-client request")
        }
        _ if method.starts_with("$/") => Some("protocol notifications are not requests"),
        _ if method.starts_with("window/") || method.starts_with("client/") => {
            Some("it is a server-to-client request")
        }
        _ if last == "refresh" => Some("it is a server-to-client refresh request"),
        _ if last.starts_with("did") || last == "willSave" => {
            Some("document and workspace notifications are sent by Dial as files change")
        }
        _ => None,
    }
}

/// The capability pointer for a known method, `None` for an unknown one.
fn capability_pointer(method: &str) -> Option<&'static str> {
    CAPABILITIES
        .iter()
        .find(|(known, _)| *known == method)
        .map(|(_, pointer)| *pointer)
}

/// The payload as an object of params. Absent or null is an empty object.
fn payload_map(payload: Option<&Value>) -> Result<Map<String, Value>, String> {
    match payload {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(map)) => {
            let bytes = serde_json::to_vec(map)
                .map_err(|error| format!("payload is not serializable: {error}"))?;
            if bytes.len() > MAX_PAYLOAD_BYTES {
                return Err(format!("payload is larger than {MAX_PAYLOAD_BYTES} bytes"));
            }
            Ok(map.clone())
        }
        Some(_) => Err("payload must be a JSON object of LSP params".to_owned()),
    }
}

/// Builds the call. `document` is the file's URI; `position` is the zero-based UTF-16 position
/// when the model gave `line`. Each default is added only when the payload does not already set it.
pub fn prepare(
    query: Option<&str>,
    payload: Option<&Value>,
    document: &str,
    position: Option<(u32, u32)>,
) -> Result<Call, String> {
    let method = validate_method(query)?;
    let mut params = payload_map(payload)?;
    params
        .entry("textDocument")
        .or_insert_with(|| json!({"uri": document}));
    if let Some((line, character)) = position {
        params
            .entry("position")
            .or_insert_with(|| json!({"line": line, "character": character}));
    }
    Ok(Call {
        method: method.to_owned(),
        pointer: capability_pointer(method),
        params: Value::Object(params),
    })
}

/// The params, clipped for an error message.
pub fn preview(params: &Value) -> String {
    clip(&params.to_string(), MAX_PREVIEW_CHARS)
}

/// The error text for a failed call, with the params that caused it.
pub fn failure(server: &str, call: &Call, error: &str) -> String {
    format!(
        "LSP error from {server} on {}: {error}\n  params: {}",
        call.method,
        preview(&call.params)
    )
}

/// The result as pretty JSON, bounded to `MAX_RESULT_CHARS`. JSON escapes control characters, so
/// untrusted server text cannot inject terminal sequences.
pub fn result_text(result: &Value) -> String {
    let text = serde_json::to_string_pretty(result)
        .unwrap_or_else(|error| format!("<unserializable result: {error}>"));
    let total = text.chars().count();
    if total <= MAX_RESULT_CHARS {
        return text;
    }
    format!(
        "{}\n[truncated: result is {total} characters; showing the first {MAX_RESULT_CHARS}]",
        clip(&text, MAX_RESULT_CHARS)
    )
}

fn is_workspace_edit(value: &Value) -> bool {
    value.is_object() && (value.get("changes").is_some() || value.get("documentChanges").is_some())
}

/// The `WorkspaceEdit`s a result carries: the result itself, or the `edit` of each code action in
/// an array. Every one must be validated before it is reported.
pub fn workspace_edits(result: &Value) -> Vec<&Value> {
    if is_workspace_edit(result) {
        return vec![result];
    }
    result.as_array().map_or_else(Vec::new, |items| {
        items
            .iter()
            .filter_map(|item| item.get("edit").filter(|edit| is_workspace_edit(edit)))
            .collect()
    })
}

/// Whether the server advertises the call's method: its capability pointer for a known method, or
/// an `initialize` experimental entry or dynamic registration naming it for an unknown one.
pub fn advertised(client: &Client, call: &Call) -> bool {
    match call.pointer {
        Some(pointer) => client.supports(pointer, &call.method),
        None => {
            let capabilities = client.capabilities();
            names_method(&capabilities["initialize"]["experimental"], &call.method)
                || capabilities["dynamic_registrations"]
                    .as_array()
                    .is_some_and(|registrations| {
                        registrations
                            .iter()
                            .any(|registration| registration["method"] == call.method)
                    })
        }
    }
}

/// Whether an experimental capability value names `method`: as a string, in an array, or as a key.
fn names_method(value: &Value, method: &str) -> bool {
    match value {
        Value::String(name) => name == method,
        Value::Array(items) => items.iter().any(|item| names_method(item, method)),
        Value::Object(map) => map
            .get(method)
            .is_some_and(|flag| !flag.is_null() && *flag != json!(false)),
        _ => false,
    }
}

/// Sends one prepared call and returns its report. Results carrying a `WorkspaceEdit` are validated
/// as one text transaction and previewed; they are never applied here.
pub fn send(
    ws: &Workspace,
    client: &Client,
    server: &str,
    abs: &Path,
    source: &str,
    call: &Call,
    cancel: &AtomicBool,
) -> Result<String, String> {
    if !advertised(client, call) {
        return Err(format!(
            "{server} server does not advertise {}",
            call.method
        ));
    }
    let snapshot = Snapshot::capture(ws)?;
    snapshot.validate_source(ws, abs, source)?;
    check_cancel(cancel)?;
    let result = client
        .request(&call.method, call.params.clone())
        .map_err(|error| failure(server, call, &format!("{error:#}")))?;
    let edits = workspace_edits(&result);
    let mut report = result_text(&result);
    if !edits.is_empty() {
        snapshot.validate(ws, abs)?;
        let sources: Vec<(&Client, &Value)> = edits.iter().map(|edit| (client, *edit)).collect();
        let plan = Plan::prepare_sources(ws, &sources, &snapshot).map_err(|error| {
            format!("{server} returned a WorkspaceEdit that failed validation: {error}")
        })?;
        report.push_str(
            "\nWorkspaceEdit validated as one text transaction but NOT applied; apply it with rename or code_action_apply:\n",
        );
        report.push_str(&plan.summary(ws));
    }
    Ok(format!("{server} <- {}:\n{report}", call.method))
}

#[cfg(test)]
mod tests;
