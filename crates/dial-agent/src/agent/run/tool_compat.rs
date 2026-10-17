//! Conservative, schema-directed compatibility for model tool calls.
//! Normalization never invents values or bypasses validation and authorization.
use serde_json::Value;

/// Exact catalog names take precedence over compatibility aliases.
pub(in crate::agent) fn canonical_name<'a>(name: &'a str, catalog: &[String]) -> &'a str {
    if catalog.iter().any(|n| n == name) {
        return name;
    }
    let bare = name.strip_prefix("functions.").unwrap_or(name);
    let target = match bare {
        "read_file" | "Read" => "read",
        "write_file" | "Write" => "write",
        "edit_file" | "Edit" => "edit",
        "run_command" | "shell" | "Bash" => "bash",
        "search_files" | "Grep" => "grep",
        "find_files" | "Glob" => "glob",
        "fetch_url" => "web_fetch",
        "search_web" => "web_search",
        _ => bare,
    };
    if catalog.iter().any(|n| n == target) {
        target
    } else {
        name
    }
}

pub(in crate::agent) fn normalize(name: &str, schema: &Value, args: &mut Value) {
    normalize_container(schema, args);
    let aliases: &[(&str, &str)] = match name {
        "read" => &[
            ("file_path", "path"),
            ("filepath", "path"),
            ("filename", "path"),
            ("start_line", "offset"),
            ("max_lines", "limit"),
        ],
        "write" => &[
            ("file_path", "path"),
            ("filepath", "path"),
            ("filename", "path"),
            ("text", "content"),
        ],
        "edit" => &[
            ("file_path", "path"),
            ("old_string", "old"),
            ("new_string", "new"),
            ("patch", "input"),
        ],
        "bash" => &[("cmd", "command"), ("timeout_secs", "timeout")],
        "grep" => &[
            ("regex", "pattern"),
            ("query", "pattern"),
            ("include", "glob"),
            ("max_matches", "max_results"),
        ],
        "glob" => &[("glob", "pattern")],
        "web_fetch" => &[("uri", "url")],
        "web_search" => &[("search_query", "query")],
        _ => &[],
    };
    if let (Some(object), Some(properties)) =
        (args.as_object_mut(), schema["properties"].as_object())
    {
        for &(alias, key) in aliases {
            if !properties.contains_key(alias)
                && properties.contains_key(key)
                && !object.contains_key(key)
                && object.contains_key(alias)
            {
                let value = object.remove(alias).unwrap();
                object.insert(key.into(), value);
            }
        }
    }
    normalize_scalars(schema, args);
}

/// Applies recursively to every tool's schema, including extensions. Unknown properties, conflicting
/// aliases and out-of-range values stay errors. Edit payloads and arbitrary strings stay untouched.
fn normalize_container(schema: &Value, value: &mut Value) {
    // Only explicitly typed containers may be JSON encoded. Never reinterpret edit text/commands.
    if let Some(text) = value.as_str()
        && matches!(schema["type"].as_str(), Some("object" | "array"))
        && let Ok(decoded) = serde_json::from_str::<Value>(text)
        && ((schema["type"] == "object" && decoded.is_object())
            || (schema["type"] == "array" && decoded.is_array()))
    {
        *value = decoded;
    }
    if let (Some(object), Some(properties)) =
        (value.as_object_mut(), schema["properties"].as_object())
    {
        let keys: Vec<_> = object.keys().cloned().collect();
        for key in keys {
            if !properties.contains_key(&key)
                && let Ok(decoded) = serde_json::from_str::<String>(&key)
                && properties.contains_key(&decoded)
                && !object.contains_key(&decoded)
            {
                let child = object.remove(&key).unwrap();
                object.insert(decoded, child);
            }
        }
        object.retain(|key, child| {
            let Some(sub) = properties.get(key) else {
                return true;
            };
            let required = schema["required"]
                .as_array()
                .is_some_and(|keys| keys.iter().any(|k| k == key));
            let nullable_enum = sub["enum"]
                .as_array()
                .is_some_and(|values| values.iter().any(Value::is_null));
            // Unknown/union/nullable types and required values remain the validator's decision.
            !(child.is_null()
                && !required
                && !nullable_enum
                && sub["type"].as_str().is_some_and(|kind| kind != "null"))
        });
    }
}

fn normalize_scalars(schema: &Value, value: &mut Value) {
    normalize_container(schema, value);
    if let Some(text) = value.as_str() {
        let converted = match schema["type"].as_str() {
            Some("integer") => text
                .trim()
                .parse::<i64>()
                .ok()
                .map(Value::from)
                .or_else(|| text.trim().parse::<u64>().ok().map(Value::from)),
            Some("number") => serde_json::from_str::<Value>(text.trim())
                .ok()
                .filter(Value::is_number),
            Some("boolean") => match text.trim() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            _ => None,
        };
        if let Some(converted) = converted {
            *value = converted;
        }
    }
    if let (Some(object), Some(properties)) =
        (value.as_object_mut(), schema["properties"].as_object())
    {
        for (key, child) in object {
            if let Some(sub) = properties.get(key) {
                normalize_scalars(sub, child);
            }
        }
    } else if let (Some(items), Some(sub)) = (value.as_array_mut(), schema.get("items")) {
        for item in items {
            normalize_scalars(sub, item);
        }
    }
}

#[cfg(test)]
mod tests;
