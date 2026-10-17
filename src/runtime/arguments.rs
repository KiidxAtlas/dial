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

#[cfg(test)]
mod tests;
