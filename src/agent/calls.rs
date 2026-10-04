//! Strict validation of model-produced tool-call arguments against the offered JSON schema.
//!
//! This is the deterministic, model-independent check that makes a repaired tool call usable as a
//! training example: the call form is either valid under the schema we advertised or it is not.
use serde_json::Value;

/// Validate `args` against an OpenAI function `parameters` object (the subset dial emits:
/// object properties of type string/integer/number/boolean, `required`, `minimum`, and
/// `additionalProperties: false`).
pub fn validate(parameters: &Value, args: &Value) -> Result<(), String> {
    if !args.is_object() { return Err("tool arguments must be a JSON object".into()); }
    super::output_schema::validate(parameters, args).map_err(|e| if e.contains("below the minimum") { format!("{e}; value must be >= minimum") } else { e })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edit() -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},
            "new":{"type":"string"},"limit":{"type":"integer","minimum":1},"replace_all":{"type":"boolean"}},
            "required":["path","old","new"],"additionalProperties":false})
    }

    #[test]
    fn accepts_valid_and_rejects_each_form_defect() {
        let p = edit();
        assert!(validate(&p, &json!({"path":"a","old":"b","new":"c","limit":3,"replace_all":true})).is_ok());
        assert!(validate(&p, &json!({"path":"a","new":"c"})).unwrap_err().contains("missing"));
        assert!(validate(&p, &json!({"path":"a","old":2,"new":"c"})).unwrap_err().contains("string"));
        assert!(validate(&p, &json!({"path":"a","old":"b","new":"c","extra":1})).unwrap_err().contains("unexpected property"));
        assert!(validate(&p, &json!({"path":"a","old":"b","new":"c","limit":0})).unwrap_err().contains(">="));
        assert!(validate(&p, &json!({"path":"a","old":"b","new":"c","limit":1.5})).unwrap_err().contains("integer"));
        assert!(validate(&p, &json!("path")).is_err());
    }
}
