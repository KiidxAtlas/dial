//! One-line call summaries and legacy tool presentation projections.

use super::*;

/// Compatibility projection for imported calls that predate tool-owned metadata.
pub fn legacy_tool_presentation(
    name: &str,
    summary: &str,
    input: &str,
) -> crate::tools::presentation::Presentation {
    let args = serde_json::from_str(input).unwrap_or_else(|_| match name {
        "bash" => serde_json::json!({"command":summary}),
        "grep" | "glob" => serde_json::json!({"pattern":summary}),
        "todo" => serde_json::json!({"op":summary}),
        "artifact_read" => serde_json::json!({"uri":summary}),
        "read" | "write" | "edit" => serde_json::json!({"path":summary}),
        _ => serde_json::json!({"query":summary}),
    });
    crate::tools::presentation::render_call(name, &args)
}

pub fn legacy_tool_result(
    name: &str,
    args: &Value,
    text: &str,
    is_error: bool,
) -> crate::runtime::presentation::Presentation {
    crate::tools::presentation::render_result(
        name,
        args,
        &ToolOutput {
            execution: Default::default(),
            text: text.into(),
            is_error,
            data: None,
        },
    )
}

pub(super) fn summarize(call: &ToolCall) -> String {
    summarize_call(&call.name, &call.arguments)
}

/// One line a person can read for a tool call: tool lines, approval prompts and replayed sessions. Tools that can
/// need approval keep their full arguments unless a field below already says everything the approver must see.
pub fn summarize_call(name: &str, a: &Value) -> String {
    let s = |key: &str| a[key].as_str().unwrap_or("");
    let within = |pattern: &str| match (s("path"), s("glob")) {
        ("", "") => pattern.to_owned(),
        (path, "") => format!("{pattern} in {path}"),
        ("", glob) => format!("{pattern} ({glob})"),
        (path, glob) => format!("{pattern} in {path} ({glob})"),
    };
    let text = match name {
        "bash" => s("command").to_owned(),
        "write" | "read" => s("path").to_owned(),
        "edit" => a["input"]
            .as_str()
            .or_else(|| a["path"].as_str())
            .unwrap_or_default()
            .to_owned(),
        "task" => subagents::summarize(a),
        "grep" | "glob" => within(s("pattern")),
        "todo" => summarize_todo(a),
        "finish" if s("outcome") == "completed" => "request completion".into(),
        "finish" => s("outcome").replace('_', " "),
        "tool_search" => s("query").to_owned(),
        "tool_load" => a["names"]
            .as_array()
            .map(|n| {
                n.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "artifact_read" => s("uri").to_owned(),
        "working_state" => s("action").to_owned(),
        _ if a.is_null() => String::new(),
        _ => a.to_string(),
    };
    truncate(&text, 1500)
}

pub(super) fn summarize_todo(a: &Value) -> String {
    let op = a["op"].as_str().unwrap_or("?");
    let count = a["tasks"].as_array().map_or(0, Vec::len);
    let plural = if count == 1 { "" } else { "s" };
    let index = a["index"]
        .as_u64()
        .map(|i| format!(" {i}"))
        .unwrap_or_default();
    match op {
        "set" if count == 0 => "clear".to_owned(),
        "set" => format!("plan {count} task{plural}"),
        "append" => format!("add {count} task{plural}"),
        _ => match a["reason"].as_str() {
            Some(reason) => format!("{op}{index}: {reason}"),
            None => format!("{op}{index}"),
        },
    }
}
