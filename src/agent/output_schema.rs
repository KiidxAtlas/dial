//! A small JSON Schema validator for `task`'s `output_schema`.
//!
//! Supported keywords: `type` (one name or a list), `enum`, `const`, `properties`, `required`,
//! `additionalProperties` (`false` or a schema), `items`, `minimum`, `maximum`, `minItems` and
//! `maxItems`. Other keywords (`$ref`, `oneOf`, `pattern`, …) are not evaluated; `check_schema`
//! tells the caller about the ones that would silently weaken the guarantee.
//!
//! Unlike `calls::validate`, which checks flat tool arguments against dial's own schemas, this one
//! recurses, because a delegated result may be any shape the parent asked for.
use serde_json::Value;

const TYPES: &[&str] = &[
    "string", "number", "integer", "boolean", "object", "array", "null",
];
/// Keywords that change what is valid but are not evaluated here; refusing them beats pretending.
const UNSUPPORTED: &[&str] = &[
    "$ref",
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "if",
    "pattern",
    "format",
    "patternProperties",
];

/// Reject a schema that is not an object, names an unknown `type`, or relies on a keyword this
/// validator does not evaluate.
pub fn check_schema(schema: &Value) -> Result<(), String> {
    check_at(schema, "$")
}

fn check_at(schema: &Value, path: &str) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| format!("{path}: a schema must be a JSON object"))?;
    if let Some(keyword) = UNSUPPORTED.iter().find(|k| object.contains_key(**k)) {
        return Err(format!("{path}: unsupported keyword {keyword:?}"));
    }
    for kind in type_names(object.get("type")).map_err(|why| format!("{path}: {why}"))? {
        if !TYPES.contains(&kind) {
            return Err(format!("{path}: unknown type {kind:?}"));
        }
    }
    if let Some(required) = object.get("required") {
        let ok = required
            .as_array()
            .is_some_and(|names| names.iter().all(Value::is_string));
        if !ok {
            return Err(format!("{path}: `required` must be an array of strings"));
        }
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| format!("{path}: `properties` must be an object"))?;
        for (name, sub) in properties {
            check_at(sub, &format!("{path}.{name}"))?;
        }
    }
    match object.get("additionalProperties") {
        None | Some(Value::Bool(_)) => {}
        Some(sub) => check_at(sub, &format!("{path}.*"))?,
    }
    if let Some(items) = object.get("items") {
        check_at(items, &format!("{path}[]"))?;
    }
    if let Some(values) = object.get("enum")
        && !values.as_array().is_some_and(|v| !v.is_empty())
    {
        return Err(format!("{path}: `enum` must be a non-empty array"));
    }
    Ok(())
}

fn type_names(value: Option<&Value>) -> Result<Vec<&str>, &'static str> {
    match value {
        None => Ok(vec![]),
        Some(Value::String(name)) => Ok(vec![name]),
        Some(Value::Array(names)) => names
            .iter()
            .map(|n| n.as_str().ok_or("`type` entries must be strings"))
            .collect(),
        Some(_) => Err("`type` must be a string or an array of strings"),
    }
}

/// Check `value` against `schema`; the error names the first offending location.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    validate_at(schema, value, "$")
}

fn validate_at(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    let kinds = type_names(schema.get("type")).unwrap_or_default();
    if !kinds.is_empty() && !kinds.iter().any(|kind| is_type(kind, value)) {
        return Err(format!(
            "{path}: expected {}, got {}",
            kinds.join(" or "),
            type_of(value)
        ));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!(
            "{path}: {value} is not one of {}",
            Value::Array(allowed.clone())
        ));
    }
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        return Err(format!("{path}: must equal {expected}"));
    }
    if let Some(n) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            return Err(format!("{path}: {n} is below the minimum {min}"));
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            return Err(format!("{path}: {n} is above the maximum {max}"));
        }
    }
    match value {
        Value::Object(map) => validate_object(schema, map, path),
        Value::Array(items) => validate_array(schema, items, path),
        _ => Ok(()),
    }
}

fn validate_object(
    schema: &serde_json::Map<String, Value>,
    map: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), String> {
    for name in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !map.contains_key(name) {
            return Err(format!("{path}: missing required property {name:?}"));
        }
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    for (name, value) in map {
        let here = format!("{path}.{name}");
        match (
            properties.and_then(|p| p.get(name)),
            schema.get("additionalProperties"),
        ) {
            (Some(sub), _) => validate_at(sub, value, &here)?,
            (None, Some(Value::Bool(false))) => {
                return Err(format!("{path}: unexpected property {name:?}"));
            }
            (None, Some(extra @ Value::Object(_))) => validate_at(extra, value, &here)?,
            (None, _) => {}
        }
    }
    Ok(())
}

fn validate_array(
    schema: &serde_json::Map<String, Value>,
    items: &[Value],
    path: &str,
) -> Result<(), String> {
    if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
        && (items.len() as u64) < min
    {
        return Err(format!(
            "{path}: needs at least {min} items, has {}",
            items.len()
        ));
    }
    if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
        && items.len() as u64 > max
    {
        return Err(format!(
            "{path}: allows at most {max} items, has {}",
            items.len()
        ));
    }
    if let Some(sub) = schema.get("items") {
        for (i, item) in items.iter().enumerate() {
            validate_at(sub, item, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

fn is_type(kind: &str, value: &Value) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        // JSON has one number type: 3.0 is an integer.
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn findings() -> Value {
        json!({"type":"object","required":["summary","files"],"additionalProperties":false,"properties":{
            "summary":{"type":"string"},
            "severity":{"enum":["low","high"]},
            "files":{"type":"array","minItems":1,"items":{"type":"object","required":["path"],"properties":{"path":{"type":"string"},"line":{"type":"integer","minimum":1}}}}}})
    }

    #[test]
    fn accepts_a_conforming_value() {
        let value = json!({"summary":"ok","severity":"low","files":[{"path":"a.rs","line":3},{"path":"b.rs"}]});
        assert_eq!(validate(&findings(), &value), Ok(()));
    }

    #[test]
    fn names_the_location_of_each_kind_of_violation() {
        let s = findings();
        let cases = [
            (json!("text"), "$: expected object, got string"),
            (json!({"files":[]}), "missing required property \"summary\""),
            (
                json!({"summary":1,"files":[{"path":"a"}]}),
                "$.summary: expected string, got number",
            ),
            (
                json!({"summary":"x","severity":"mid","files":[{"path":"a"}]}),
                "$.severity",
            ),
            (json!({"summary":"x","files":[]}), "needs at least 1"),
            (
                json!({"summary":"x","files":[{"path":"a"},{"line":2}]}),
                "$.files[1]: missing required property \"path\"",
            ),
            (
                json!({"summary":"x","files":[{"path":"a","line":0}]}),
                "$.files[0].line: 0 is below the minimum 1",
            ),
            (
                json!({"summary":"x","files":[{"path":"a"}],"extra":true}),
                "unexpected property \"extra\"",
            ),
        ];
        for (value, expected) in cases {
            let error = validate(&s, &value).unwrap_err();
            assert!(error.contains(expected), "{value}: {error}");
        }
    }

    #[test]
    fn integers_accept_whole_floats_and_types_may_be_unions() {
        assert!(validate(&json!({"type":"integer"}), &json!(3.0)).is_ok());
        assert!(validate(&json!({"type":"integer"}), &json!(3.5)).is_err());
        let nullable = json!({"type":["string","null"]});
        assert!(
            validate(&nullable, &json!(null)).is_ok() && validate(&nullable, &json!(4)).is_err()
        );
    }

    #[test]
    fn schemas_the_validator_cannot_honour_are_rejected_up_front() {
        assert!(check_schema(&findings()).is_ok());
        for bad in [
            json!("string"),
            json!({"type":"text"}),
            json!({"oneOf":[]}),
            json!({"properties":{"a":{"$ref":"#/x"}}}),
            json!({"required":"a"}),
            json!({"enum":[]}),
            json!({"items":{"type":7}}),
        ] {
            assert!(check_schema(&bad).is_err(), "{bad}");
        }
    }
}
