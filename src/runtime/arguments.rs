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
mod tests {
    use super::*;
    use serde_json::json;

    fn edit() -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},
            "new_string":{"type":"string"},"limit":{"type":"integer","minimum":1},"replace_all":{"type":"boolean"}},
            "required":["path","old_string","new_string"],"additionalProperties":false})
    }

    #[test]
    fn accepts_valid_and_rejects_each_form_defect() {
        let p = edit();
        assert!(
            validate(
                &p,
                &json!({"path":"a","old_string":"b","new_string":"c","limit":3,"replace_all":true})
            )
            .is_ok()
        );
        assert!(
            validate(&p, &json!({"path":"a","new_string":"c"}))
                .unwrap_err()
                .contains("missing")
        );
        assert!(
            validate(&p, &json!({"path":"a","old_string":2,"new_string":"c"}))
                .unwrap_err()
                .contains("string")
        );
        assert!(
            validate(
                &p,
                &json!({"path":"a","old_string":"b","new_string":"c","extra":1})
            )
            .unwrap_err()
            .contains("unexpected property")
        );
        assert!(
            validate(
                &p,
                &json!({"path":"a","old_string":"b","new_string":"c","limit":0})
            )
            .unwrap_err()
            .contains(">=")
        );
        assert!(
            validate(
                &p,
                &json!({"path":"a","old_string":"b","new_string":"c","limit":1.5})
            )
            .unwrap_err()
            .contains("integer")
        );
        assert!(validate(&p, &json!("path")).is_err());
    }

    #[test]
    fn invalid_calls_include_schema_guidance_without_submitted_values() {
        let args = json!({"path":"private-user-value", "old_string":3, "new_string":"c"});
        let before = args.clone();
        let error = validate(&edit(), &args).unwrap_err();
        assert!(!error.contains("private-user-value"));
        assert_eq!(args, before);
        let error = validate(&edit(), &json!("private-user-value")).unwrap_err();
        assert!(error.contains("JSON object") && error.contains("Argument guide"));
        assert!(!error.contains("private-user-value"));
    }

    #[test]
    fn argument_guide_is_bounded_for_large_schemas() {
        let mut properties = serde_json::Map::new();
        for n in 0..100 {
            properties.insert(
                format!("{n:03}{}", "x".repeat(1000)),
                json!({"type":"string"}),
            );
        }
        let guide = argument_guide(&json!({"properties":properties}));
        assert!(guide.len() < 2000);
        assert_eq!(guide.matches("(optional)").count(), 12);
        assert!(guide.contains("more fields"));
        assert!(argument_guide(&json!({})).is_empty());
    }
}
