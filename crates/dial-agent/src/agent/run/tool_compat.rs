//! Conservative, schema-directed compatibility for model tool calls.
//! Normalization never invents values or bypasses validation and authorization.
use serde_json::Value;
use std::borrow::Cow;

/// Namespace prefixes local chat templates put in front of a function name.
const NAME_PREFIXES: [&str; 5] = [
    "functions.",
    "functions/",
    "default_api.",
    "tools.",
    "tool.",
];

/// The name without a namespace prefix or a trailing `:N` call index (`functions.read:0`).
fn bare_name(name: &str) -> &str {
    let mut bare = name.trim();
    while let Some(rest) = NAME_PREFIXES.iter().find_map(|p| bare.strip_prefix(p)) {
        bare = rest;
    }
    match bare.rsplit_once(':') {
        Some((head, index))
            if !head.is_empty()
                && !index.is_empty()
                && index.bytes().all(|b| b.is_ascii_digit()) =>
        {
            head
        }
        _ => bare,
    }
}

fn folded(name: &str) -> String {
    name.to_ascii_lowercase().replace('-', "_")
}

/// Exact catalog names take precedence over compatibility aliases. A name that is not offered is
/// stripped of its namespace prefix and call index, mapped through the alias table, and finally
/// matched ignoring case and `-` versus `_` when exactly one offered tool matches. Anything else
/// is returned unchanged and fails as an unknown tool.
pub(in crate::agent) fn canonical_name<'a>(name: &'a str, catalog: &[String]) -> Cow<'a, str> {
    if catalog.iter().any(|n| n == name) {
        return Cow::Borrowed(name);
    }
    let bare = bare_name(name);
    let target = match bare {
        "read_file" | "Read" => "read",
        "write_file" | "Write" => "write",
        "edit_file" | "Edit" => "edit",
        "run_command" | "shell" | "Bash" => "bash",
        "search_files" | "Grep" => "grep",
        "find_files" | "Glob" => "glob",
        "fetch_url" | "WebFetch" => "web_fetch",
        "search_web" | "WebSearch" => "web_search",
        _ => bare,
    };
    if catalog.iter().any(|n| n == target) {
        return Cow::Borrowed(target);
    }
    let wanted = folded(bare);
    let mut matches = catalog.iter().filter(|n| folded(n) == wanted);
    match (matches.next(), matches.next()) {
        (Some(only), None) => Cow::Owned(only.clone()),
        _ => Cow::Borrowed(name),
    }
}

pub(in crate::agent) fn normalize(name: &str, schema: &Value, args: &mut Value) {
    // Recorded or habitual pre-rename parameter names map onto the current ones.
    crate::tools::surface::contracts::legacy_arguments(name, schema, args);
    normalize_container(schema, args);
    let aliases: &[(&str, &str)] = match name {
        "read" => &[
            ("file_path", "path"),
            ("filePath", "path"),
            ("filepath", "path"),
            ("filename", "path"),
            ("start_line", "offset"),
            ("max_lines", "limit"),
        ],
        "write" => &[
            ("file_path", "path"),
            ("filepath", "path"),
            ("filePath", "path"),
            ("filename", "path"),
            ("text", "content"),
        ],
        "edit" => &[
            ("file_path", "path"),
            ("filePath", "path"),
            ("oldString", "old_string"),
            ("newString", "new_string"),
            ("replaceAll", "replace_all"),
            ("old_string", "old"),
            ("new_string", "new"),
            ("patch", "input"),
        ],
        "bash" => &[
            ("cmd", "command"),
            ("timeout_secs", "timeout"),
            ("workdir", "cwd"),
            ("working_dir", "cwd"),
        ],
        "grep" => &[
            ("regex", "pattern"),
            ("query", "pattern"),
            ("include", "glob"),
            ("max_matches", "limit"),
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
    // `find` keywords are literal text. A singleton string is unambiguous; unlike
    // general array repair, preserve commas, quotes and whitespace as one keyword.
    if name == "find"
        && schema["properties"]["keywords"]["type"] == "array"
        && schema["properties"]["keywords"]["items"]["type"] == "string"
        && let Some(keyword) = args.get_mut("keywords")
        && keyword.is_string()
    {
        let literal = keyword.take();
        *keyword = Value::Array(vec![literal]);
    }
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
    if let Some(items) = string_list(schema, value) {
        *value = items;
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

/// A string where the schema declares an array of strings: models send a list's JSON text without
/// its brackets (`a", "b` for `["a", "b"]`). It is decoded only when the bracketed text is a list
/// of two or more strings; any other string stays a type error rather than a guessed list.
fn string_list(schema: &Value, value: &Value) -> Option<Value> {
    let text = value.as_str()?;
    if schema["type"] != "array" || schema["items"]["type"] != "string" {
        return None;
    }
    serde_json::from_str::<Vec<String>>(&format!("[\"{text}\"]"))
        .ok()
        .filter(|items| items.len() > 1)
        .map(Value::from)
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
