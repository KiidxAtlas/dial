//! Strict validation of model-produced tool-call arguments against the offered JSON schema.
//!
//! This is the deterministic, model-independent check that makes a repaired tool call usable as a
//! training example: the call form is either valid under the schema we advertised or it is not.
use serde_json::Value;

/// Validate `args` against an OpenAI function `parameters` object (the subset dial emits:
/// object properties of type string/integer/number/boolean, `required`, `minimum`, and
/// `additionalProperties: false`).
pub fn validate(parameters: &Value, args: &Value) -> Result<(), String> {
    let result = if !args.is_object() {
        Err("tool arguments must be a JSON object".into())
    } else {
        super::schema::validate(parameters, args)
    };
    result.map_err(|e: String| {
        let error = if e.contains("below the minimum") {
            format!("{e}; value must be >= minimum")
        } else {
            e
        };
        let guide = argument_guide(parameters);
        if guide.is_empty() {
            error
        } else {
            format!("{error}. {guide}")
        }
    })
}

/// Describe the offered form without echoing submitted values or inventing an executable call.
fn argument_guide(parameters: &Value) -> String {
    let Some(properties) = parameters.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let required = parameters.get("required").and_then(Value::as_array);
    let mut fields = Vec::new();
    for (name, schema) in properties.iter().take(12) {
        let mandatory = required.is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str() == Some(name.as_str()))
        });
        let name = name.chars().take(64).collect::<String>();
        let kind = schema
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("JSON value");
        let kind = kind.chars().take(24).collect::<String>();
        fields.push(format!(
            "{name}: {kind} ({})",
            if mandatory { "required" } else { "optional" }
        ));
    }
    if properties.len() > 12 {
        fields.push("… more fields in the offered schema".into());
    }
    format!(
        "Argument guide: {}. Use the offered field names and JSON types; do not guess missing values",
        fields.join(", ")
    )
}

/// Remove `description` from every property schema (recursively through nested objects and array
/// items), keeping names, types, enums, bounds and required lists. The tool description stays.
pub fn strip_property_descriptions(parameters: &mut Value) {
    let Some(object) = parameters.as_object_mut() else {
        return;
    };
    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for property in properties.values_mut() {
            if let Some(property) = property.as_object_mut() {
                property.remove("description");
            }
            strip_property_descriptions(property);
        }
    }
    if let Some(items) = object.get_mut("items") {
        if let Some(items) = items.as_object_mut() {
            items.remove("description");
        }
        strip_property_descriptions(items);
    }
}

/// Every top-level parameter with its type, enum values, required status and explanation, for a
/// call whose arguments did not fit. Bounded so one error cannot flood the context.
pub fn parameter_help(parameters: &Value) -> String {
    const MAX_BYTES: usize = 4096;
    let Some(properties) = parameters.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let required = parameters.get("required").and_then(Value::as_array);
    let mut lines = Vec::new();
    for (name, schema) in properties {
        let mut line = format!(
            "- {name} ({}{})",
            schema
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("JSON value"),
            if required.is_some_and(|items| items.iter().any(|item| item == name)) {
                ", required"
            } else {
                ""
            }
        );
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            let values: Vec<String> = values.iter().map(Value::to_string).collect();
            line.push_str(&format!(" one of {}", values.join("|")));
        }
        if let Some(text) = schema.get("description").and_then(Value::as_str) {
            line.push_str(&format!(": {text}"));
        }
        lines.push(line);
    }
    let mut help = format!("Parameters:\n{}", lines.join("\n"));
    if help.len() > MAX_BYTES {
        let mut cut = MAX_BYTES;
        while !help.is_char_boundary(cut) {
            cut -= 1;
        }
        help.truncate(cut);
        help.push_str("\n…");
    }
    help
}

/// Whitespace-separated words with simple `'...'` / `"..."` quoting and backslash escapes.
pub fn split_words(s: &str) -> Vec<String> {
    let (mut words, mut cur, mut quote, mut in_word) =
        (Vec::new(), String::new(), None::<char>, false);
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests;
