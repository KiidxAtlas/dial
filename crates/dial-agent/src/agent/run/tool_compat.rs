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

// Compare ASCII identifiers, not arbitrary text. Ignore single word separators, but keep
// namespace boundaries (`mcp__...`) and leading/trailing underscores significant.
fn spelling(name: &str) -> Option<impl Iterator<Item = u8> + '_> {
    let bytes = name.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 256
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    valid.then(|| {
        bytes.iter().enumerate().filter_map(|(i, b)| {
            if matches!(b, b'-' | b'_') {
                let between_words = i > 0
                    && bytes[i - 1].is_ascii_alphanumeric()
                    && bytes.get(i + 1).is_some_and(u8::is_ascii_alphanumeric);
                (!between_words).then_some(b'_')
            } else {
                Some(b.to_ascii_lowercase())
            }
        })
    })
}

fn same_spelling(left: &str, right: &str) -> bool {
    spelling(left)
        .zip(spelling(right))
        .is_some_and(|(a, b)| a.eq(b))
}

fn unique<'a>(mut matches: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let first = matches.next()?;
    matches.all(|m| m == first).then_some(first)
}

const TOOL_ALIASES: &[(&str, &str)] = &[
    ("read_file", "read"),
    ("write_file", "write"),
    ("edit_file", "edit"),
    ("run_command", "bash"),
    ("shell", "bash"),
    ("search_files", "grep"),
    ("find_files", "glob"),
    ("fetch_url", "web_fetch"),
    ("search_web", "web_search"),
];

/// Prefer real names (including after removing provider decoration). Otherwise accept a
/// spelling or known alias only when it identifies one tool in this caller's catalog.
pub(in crate::agent) fn canonical_name<'a>(name: &'a str, catalog: &[String]) -> Cow<'a, str> {
    if catalog.iter().any(|n| n == name) {
        return Cow::Borrowed(name);
    }
    let bare = bare_name(name);
    if catalog.iter().any(|n| n == bare) {
        return Cow::Borrowed(bare);
    }
    let targets = TOOL_ALIASES
        .iter()
        .filter(|(alias, _)| same_spelling(alias, bare))
        .map(|(_, target)| *target);
    let real_names = catalog
        .iter()
        .map(String::as_str)
        .filter(|n| same_spelling(n, bare));
    let found =
        unique(real_names.chain(targets.filter(|target| catalog.iter().any(|n| n == *target))));
    found.map_or(Cow::Borrowed(name), |n| Cow::Owned(n.to_owned()))
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
    // This synonym means pixels, never a DOM snapshot. Don't override a declared action.
    if name == "browser"
        && let Some(action) = args.get_mut("action")
        && action
            .as_str()
            .is_some_and(|s| same_spelling(s.trim(), "capture"))
        && let Some(actions) = schema["properties"]["action"]["enum"].as_array()
        && !actions.contains(action)
        && actions.iter().any(|v| v == "screenshot")
    {
        *action = Value::from("screenshot");
    }
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

/// Common alternative labels, only used when one declared field in the record matches.
fn field_family(name: &str) -> Option<&'static str> {
    const FAMILIES: &[(&str, &[&str])] = &[
        ("action", &["action", "operation", "op"]),
        ("path", &["path", "file_path"]),
        ("paths", &["paths", "file_paths"]),
    ];
    FAMILIES.iter().find_map(|(group, names)| {
        names
            .iter()
            .any(|n| same_spelling(n, name))
            .then_some(*group)
    })
}

/// Rewrite only unknown fields of closed records. Open maps can carry user data, and a
/// second spelling for a supplied field must remain a validation error, not last-write-wins.
fn normalize_keys(schema: &Value, value: &mut Value) {
    if schema["additionalProperties"] != false || schema.get("patternProperties").is_some() {
        return;
    }
    let (Some(fields), Some(object)) = (schema["properties"].as_object(), value.as_object_mut())
    else {
        return;
    };
    let mut renames = std::collections::BTreeMap::new();
    for key in object.keys().filter(|k| !fields.contains_key(*k)) {
        let family = field_family(key);
        let target = unique(fields.keys().map(String::as_str).filter(|f| {
            same_spelling(key, f) || family.is_some_and(|group| field_family(f) == Some(group))
        }));
        if let Some(target) = target
            && !object.contains_key(target)
        {
            renames
                .entry(target)
                .and_modify(|from| *from = None)
                .or_insert_with(|| Some(key.clone()));
        }
    }
    for (to, from) in renames {
        if let Some(v) = from.and_then(|key| object.remove(&key)) {
            object.insert(to.to_owned(), v);
        }
    }
}

fn normalize_scalars(schema: &Value, value: &mut Value) {
    normalize_container(schema, value);
    normalize_keys(schema, value);
    if let Some(text) = value.as_str() {
        let converted = match schema["type"].as_str() {
            Some("string") => schema["enum"]
                .as_array()
                .filter(|choices| !choices.contains(value))
                .and_then(|choices| {
                    unique(
                        choices
                            .iter()
                            .filter_map(Value::as_str)
                            .filter(|choice| same_spelling(text.trim(), choice)),
                    )
                })
                .map(Value::from),
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
