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
    // For `edit`, allow normalizing aliases for any supported edit grammar (Replace, Patch,
    // ApplyPatch, Sloppy, Hashline) even when Hashline is the default offered schema.
    let edit_alt_schema;
    let effective_schema = if name == "edit" {
        let detected =
            args.as_object()
                .map_or(crate::config::types::EditMode::default(), |object| {
                    if object.contains_key("edits") {
                        crate::config::types::EditMode::Patch
                    } else if object.contains_key("old_string")
                        || object.contains_key("oldString")
                        || object.contains_key("old")
                        || object.contains_key("new_string")
                        || object.contains_key("newString")
                        || object.contains_key("new")
                        || ((object.contains_key("path")
                            || object.contains_key("file_path")
                            || object.contains_key("filePath")
                            || object.contains_key("filepath")
                            || object.contains_key("filename"))
                            && !object.contains_key("input")
                            && !object.contains_key("patch")
                            && !object.contains_key("diff"))
                    {
                        crate::config::types::EditMode::Replace
                    } else {
                        crate::tools::edit::editing::effective_mode(
                            crate::config::types::EditMode::default(),
                            args,
                        )
                    }
                });
        edit_alt_schema = crate::tools::edit::editing::schema(detected);
        edit_alt_schema
            .pointer("/function/parameters")
            .unwrap_or(schema)
    } else {
        schema
    };
    normalize_container(effective_schema, args);
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
            ("filepath", "path"),
            ("filename", "path"),
            ("oldString", "old_string"),
            ("newString", "new_string"),
            ("old", "old_string"),
            ("new", "new_string"),
            ("replaceAll", "replace_all"),
            ("old_string", "old"),
            ("new_string", "new"),
            ("patch", "input"),
            ("diff", "input"),
        ],
        "bash" => &[
            ("cmd", "command"),
            ("timeout_secs", "timeout"),
            ("workdir", "cwd"),
            ("working_dir", "cwd"),
        ],
        "eval" => &[
            ("script", "code"),
            ("source", "code"),
            ("src", "code"),
            ("program", "code"),
            ("expression", "code"),
            ("expr", "code"),
            ("lang", "language"),
            ("timeout_secs", "timeout"),
        ],
        "verify" => &[
            ("target", "targets"),
            ("test", "targets"),
            ("tests", "targets"),
            ("files", "targets"),
        ],
        "bg" => &[
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
    if let (Some(object), Some(properties)) = (
        args.as_object_mut(),
        effective_schema["properties"].as_object(),
    ) {
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
    // Language-keyed cells are unambiguous only without another code or language field.
    if name == "eval"
        && let Some(object) = args.as_object_mut()
        && !object.contains_key("code")
        && !object.contains_key("language")
        && let Some((alias, language)) = [("python", "py"), ("js", "js")]
            .into_iter()
            .find(|(key, _)| object.get(*key).is_some_and(Value::is_string))
        && !(object.contains_key("python") && object.contains_key("js"))
        && effective_schema["properties"]["language"]["enum"]
            .as_array()
            .is_some_and(|values| values.iter().any(|value| value == language))
        && effective_schema["properties"]["code"]["type"] == "string"
        && let Some(code) = object.remove(alias)
    {
        object.insert("code".into(), code);
        object.insert("language".into(), Value::from(language));
    }
    if name == "eval"
        && let Some(language) = args.get_mut("language")
        && let Some(raw) = language.as_str()
    {
        let normalized = match raw.trim().to_ascii_lowercase().as_str() {
            "python" | "python3" | "py3" => Some("py"),
            "javascript" | "node" | "nodejs" | "ecmascript" => Some("js"),
            _ => None,
        };
        if let Some(canonical) = normalized {
            *language = Value::from(canonical);
        }
    }
    // Wrap singleton string target for `verify`.
    if name == "verify"
        && let Some(targets) = args.get_mut("targets")
        && targets.is_string()
    {
        let single = targets.take();
        *targets = Value::Array(vec![single]);
    }
    normalize_scalars(effective_schema, args);
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
    // `bg` uses `name` for job control and `job` for `watch`. Map unambiguous swaps without
    // overriding conflicts or changing the schema contract.
    if name == "bg"
        && let Some(object) = args.as_object_mut()
    {
        match object.get("action").and_then(Value::as_str) {
            Some("start" | "output" | "wait" | "input" | "close_stdin" | "resize" | "kill")
                if !object.contains_key("name") && object.contains_key("job") =>
            {
                if let Some(val) = object.remove("job") {
                    object.insert("name".into(), val);
                }
            }
            Some("watch")
                if !object.contains_key("job")
                    && !object.contains_key("path")
                    && !object.contains_key("run")
                    && !object.contains_key("pid")
                    && !object.contains_key("after_secs")
                    && object.contains_key("name") =>
            {
                if let Some(val) = object.remove("name") {
                    object.insert("job".into(), val);
                }
            }
            _ => {}
        }
    }
    // `history` / `code_history`: infer unambiguous `action` when omitted.
    if matches!(name, "history" | "code_history")
        && let Some(object) = args.as_object_mut()
        && !object.contains_key("action")
    {
        let source = object.get("source").and_then(Value::as_str);
        let inferred = if object.contains_key("query") {
            Some("search")
        } else if object.contains_key("id") {
            Some(if source == Some("sessions") {
                "session"
            } else {
                "show"
            })
        } else if source == Some("changes") || name == "code_history" {
            Some("search")
        } else if source == Some("sessions") {
            Some("sessions")
        } else {
            None
        };
        if let Some(action) = inferred {
            object.insert("action".into(), Value::from(action));
        }
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
    normalize_keys(schema, value);
    if let (Some(object), Some(properties)) =
        (value.as_object_mut(), schema["properties"].as_object())
    {
        object.retain(|key, child| {
            if !child.is_null() {
                return true;
            }
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
            required || nullable_enum || sub["type"].as_str().is_none_or(|kind| kind == "null")
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
        // A JSON-quoted exact key is another candidate, not an earlier rename:
        // account for it in the same collision check as every other spelling.
        let target = if key.trim_start().starts_with('"') {
            serde_json::from_str::<String>(key)
                .ok()
                .and_then(|decoded| fields.get_key_value(&decoded).map(|(k, _)| k.as_str()))
        } else {
            let family = field_family(key);
            unique(fields.keys().map(String::as_str).filter(|f| {
                same_spelling(key, f) || family.is_some_and(|group| field_family(f) == Some(group))
            }))
        };
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

/// Use the existing scalar decoder for an explicitly nullable number or boolean.
/// Other unions may accept the original string or admit more than one conversion.
fn scalar_kind(kind: &Value) -> Option<&str> {
    if let Some(single) = kind.as_str() {
        return Some(single);
    }
    let [a, b] = kind.as_array()?.as_slice() else {
        return None;
    };
    let other = if a == "null" {
        b
    } else if b == "null" {
        a
    } else {
        return None;
    };
    other
        .as_str()
        .filter(|k| matches!(*k, "number" | "integer" | "boolean"))
}

fn normalize_scalars(schema: &Value, value: &mut Value) {
    normalize_container(schema, value);
    if let Some(text) = value.as_str() {
        let converted = match scalar_kind(&schema["type"]) {
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
