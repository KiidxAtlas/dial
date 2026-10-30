//! Compact model-only file references. Execution and user presentation keep expanded paths.
use crate::{llm::ToolCall, tools::Workspace};
use serde_json::{Value, json};

pub(in crate::agent) const GUIDANCE: &str = "File references: use issued @fN handles in structured path arguments and edit headers (for example @f7:120-160 or [@f7#TAG]). Discovery labels @f7(path) identify the file; pass only @f7, not the parenthesized label. Dial expands them before ordinary validation, approval and confinement; users see the actual path. Full paths still work for new files. Never invent handles or interpolate them into shell commands, source text, patterns or prose. Unknown handles require a fresh lookup.";

/// Only fields with native filesystem semantics participate. Arbitrary plugin arguments, source
/// text, shell scripts and search patterns must never be rewritten.
fn path_fields(name: &str) -> &'static [&'static str] {
    match name {
        "read" | "write" | "grep" | "glob" | "find" | "ast_search" | "ast_edit" => &["path"],
        "edit" => &["path"],
        "bash" | "bg" => &["cwd"],
        "lsp" => &["path"],
        "git" => &["path"],
        _ => &[],
    }
}

fn decoded_path(path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.starts_with('"') {
        serde_json::from_str(path).map_err(|error| format!("invalid quoted edit path: {error}"))
    } else {
        Ok(path.to_owned())
    }
}

fn displayed_path(path: String) -> String {
    if path.trim() != path || path.starts_with('"') || path.chars().any(char::is_control) {
        serde_json::to_string(&path).expect("path strings serialize")
    } else {
        path
    }
}

fn map_paths(
    name: &str,
    args: &mut Value,
    map: &mut impl FnMut(&str) -> Result<String, String>,
) -> Result<(), String> {
    for key in path_fields(name) {
        if let Some(Value::String(path)) = args.get_mut(*key) {
            // grep/glob/find accept several roots in one string.
            *path = if matches!(name, "grep" | "glob" | "find") {
                path.split(';')
                    .map(&mut *map)
                    .collect::<Result<Vec<_>, _>>()?
                    .join(";")
            } else {
                map(path)?
            };
        }
    }
    if name == "lsp"
        && args["action"] == "rename_file"
        && let Some(Value::String(path)) = args.get_mut("new_name")
    {
        *path = map(path)?;
    }
    let array = match name {
        "read" | "git" => "paths",
        _ => "",
    };
    if let Some(paths) = args.get_mut(array).and_then(Value::as_array_mut) {
        for value in paths {
            if let Some(path) = value.as_str() {
                *value = json!(map(path)?);
            }
        }
    }
    if name == "edit"
        && let Some(input) = args.get("input").and_then(Value::as_str)
    {
        let mut result = String::with_capacity(input.len());
        for line in input.split_inclusive('\n') {
            let (body, ending) = if let Some(body) = line.strip_suffix("\r\n") {
                (body, "\r\n")
            } else if let Some(body) = line.strip_suffix('\n') {
                (body, "\n")
            } else {
                (line, "")
            };
            let trimmed = body.trim();
            let leading = &body[..body.len() - body.trim_start().len()];
            let trailing = &body[body.trim_end().len()..];
            if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
                && let Some((path, tag)) = inner.rsplit_once('#')
            {
                result.push_str(leading);
                result.push_str(&format!(
                    "[{}#{tag}]",
                    displayed_path(map(&decoded_path(path)?)?)
                ));
                result.push_str(trailing);
            } else if let Some(path) = trimmed.strip_prefix("MV ") {
                result.push_str(leading);
                result.push_str("MV ");
                result.push_str(&displayed_path(map(&decoded_path(path)?)?));
                result.push_str(trailing);
            } else {
                result.push_str(body);
            }
            result.push_str(ending);
        }
        args["input"] = json!(result);
    }
    Ok(())
}

/// Normalize atomically before scoped guidance, schemas, hooks, approval or native dispatch.
pub(in crate::agent) fn expand(workspace: &Workspace, call: &mut ToolCall) -> Result<(), String> {
    let mut args = call.arguments.clone();
    map_paths(&call.name, &mut args, &mut |path| {
        workspace.expand_file_handle(path)
    })?;
    call.arguments = args;
    Ok(())
}

/// Change only protocol path rows, never read source rows or matched source text.
pub(in crate::agent) fn model_result(workspace: &Workspace, call: &ToolCall, text: &str) -> String {
    let name = call.name.as_str();
    let raw = call.arguments["path"]
        .as_str()
        .into_iter()
        .chain(
            call.arguments["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        )
        .any(|path| path.contains(":raw"));
    if raw || !matches!(name, "read" | "edit" | "write" | "grep" | "glob" | "find") {
        return text.to_owned();
    }
    let mut result = String::with_capacity(text.len());
    let mut labeled = std::collections::HashSet::new();
    let mut had_header = false;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let replacement = if let Some(inner) =
            body.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
            && let Some((path, tag)) = inner.rsplit_once('#')
            && tag.len() == 4
            && tag.bytes().all(|b| b.is_ascii_hexdigit())
        {
            had_header = true;
            decoded_path(path)
                .ok()
                .and_then(|path| workspace.file_handle(&path).ok())
                .map(|handle| format!("[{handle}#{tag}]"))
        } else if name == "glob" && !body.starts_with('[') {
            workspace
                .resolve(body)
                .ok()
                .filter(|path| path.exists())
                .and_then(|_| workspace.file_handle(body).ok())
                .map(|handle| format!("{handle}({body})"))
        } else if matches!(name, "grep" | "find") {
            // Native grep rows use path:line:text or path-line-text; source after the first
            // numeric location is deliberately untouched, even when it contains a known path.
            body.char_indices().find_map(|(i, c)| {
                if !matches!(c, ':' | '-') {
                    return None;
                }
                let rest = &body[i + 1..];
                let n = rest.bytes().take_while(u8::is_ascii_digit).count();
                if n == 0 || !rest[n..].starts_with([':', '-']) {
                    return None;
                }
                if !workspace
                    .resolve(&body[..i])
                    .ok()
                    .is_some_and(|path| path.is_file())
                {
                    return None;
                }
                workspace.file_handle(&body[..i]).ok().map(|handle| {
                    let label = if labeled.insert(handle.clone()) {
                        format!("{handle}({})", &body[..i])
                    } else {
                        handle
                    };
                    format!("{label}{}", &body[i..])
                })
            })
        } else {
            None
        };
        result.push_str(replacement.as_deref().unwrap_or(body));
        if line.ends_with('\n') {
            result.push('\n');
        }
    }
    // Replace-mode reads have numbered rows but no hashline header. Introduce references
    // separately, without rewriting source rows or guessing where selectors end.
    if name == "read" && !had_header {
        let labels: Vec<_> = call.arguments["path"]
            .as_str()
            .into_iter()
            .chain(
                call.arguments["paths"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            )
            .filter_map(|path| workspace.read_file_handle(path).ok())
            .filter_map(|handle| {
                workspace
                    .expand_file_handle(&handle)
                    .ok()
                    .map(|path| format!("{handle}({path})"))
            })
            .collect();
        if !labels.is_empty() {
            return format!("[File references: {}]\n{result}", labels.join(", "));
        }
    }
    result
}

/// Freeze call projection when recording the assistant message, not while previewing a request.
/// New registry allocations must never retroactively invalidate an older cached prefix.
pub(in crate::agent) fn capture_calls(workspace: &Workspace, message: &mut Value) {
    if message["role"] != "assistant" || message.get("dial_provider_content").is_some() {
        return;
    }
    let Some(mut calls) = message["tool_calls"].as_array().cloned() else {
        return;
    };
    for call in &mut calls {
        let Some(name) = call["function"]["name"].as_str().map(str::to_owned) else {
            continue;
        };
        let string = call["function"]["arguments"].is_string();
        let mut args = if string {
            match serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("")) {
                Ok(args) => args,
                Err(_) => continue,
            }
        } else {
            call["function"]["arguments"].clone()
        };
        if map_paths(&name, &mut args, &mut |path| {
            if let Some(handle) = workspace.existing_file_handle(path) {
                return Ok(handle);
            }
            if name == "read" {
                for (i, _) in path.match_indices(':') {
                    if let Some(handle) = workspace.existing_file_handle(&path[..i]) {
                        return Ok(format!("{handle}{}", &path[i..]));
                    }
                }
            }
            Ok(path.to_owned())
        })
        .is_ok()
        {
            call["function"]["arguments"] = if string {
                json!(args.to_string())
            } else {
                args
            };
        }
    }
    if message["tool_calls"] != json!(calls) {
        message["dial_model_tool_calls"] = json!(calls);
    }
}

/// Persisted contents remain full-path evidence. Only request copies use frozen compact views.
pub(in crate::agent) fn project(_workspace: &Workspace, messages: &mut [Value]) {
    for message in messages {
        if message["role"] == "tool" {
            if let Some(content) = message["dial_model_content"].as_str() {
                message["content"] = json!(content);
            }
        } else if message["role"] == "assistant"
            && message.get("dial_provider_content").is_none()
            && let Some(calls) = message.get("dial_model_tool_calls").cloned()
        {
            message["tool_calls"] = calls;
        }
    }
}

#[cfg(test)]
mod tests;
