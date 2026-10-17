//! Request-schema and reply repair policies for tool contracts. Pure JSON transforms that providers
//! apply to outbound tool schemas and inbound tool-call arguments.
use super::profiles::ModelProfile;
use serde_json::{Value, json};
use std::collections::HashSet;

/// Strict schemas encode optional fields as nullable. Unsupported schema compositions stay unmodified.
pub fn strict_parameters(schema: &Value) -> Option<Value> {
    let mut schema = schema.clone();
    if ["oneOf", "anyOf", "allOf", "$ref", "if", "then", "else"]
        .iter()
        .any(|key| schema.get(key).is_some())
    {
        return None;
    }
    match schema["type"].as_str()? {
        "object" => {
            if schema["additionalProperties"] != false {
                return None;
            }
            let required: HashSet<String> = schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let properties = schema["properties"].as_object_mut()?;
            let keys: Vec<String> = properties.keys().cloned().collect();
            for (key, child) in properties.iter_mut() {
                *child = strict_parameters(child)?;
                if !required.contains(key) {
                    let kind = child["type"].clone();
                    child["type"] = json!([kind, "null"]);
                    if let Some(values) = child["enum"].as_array_mut() {
                        values.push(Value::Null);
                    }
                }
            }
            schema["required"] = json!(keys);
        }
        "array" => {
            schema["items"] = strict_parameters(&schema["items"])?;
        }
        "string" | "integer" | "number" | "boolean" | "null" => {}
        _ => return None,
    }
    Some(schema)
}
pub fn constrain_openai(body: &mut Value, profile: &ModelProfile) {
    if let Some(tools) = body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .filter(|tools| !tools.is_empty())
    {
        for tool in tools {
            if profile.capabilities.strict_tools
                && let Some(parameters) = strict_parameters(&tool["function"]["parameters"])
            {
                tool["function"]["parameters"] = parameters;
                tool["function"]["strict"] = json!(true);
            }
        }
        if let Some(parallel) = profile.capabilities.parallel_tool_calls {
            body["parallel_tool_calls"] = json!(parallel);
        }
    }
}
/// Null introduced only for a formerly optional field means omission, not executable data repair.
pub fn omit_optional_nulls(args: &mut Value, schema: &Value) {
    if let Some(object) = args.as_object_mut() {
        let required: HashSet<&str> = schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let properties = &schema["properties"];
        object.retain(|key, value| {
            !value.is_null()
                || required.contains(key.as_str())
                || properties.get(key).is_none()
                || properties[key]["type"] == "null"
                || properties[key]["type"]
                    .as_array()
                    .is_some_and(|types| types.iter().any(|t| t == "null"))
        });
        for (key, value) in object {
            if let Some(child) = properties.get(key) {
                omit_optional_nulls(value, child);
            }
        }
    } else if let Some(items) = args.as_array_mut() {
        for value in items {
            omit_optional_nulls(value, &schema["items"]);
        }
    }
}
pub fn normalize_reply(reply: &mut super::Reply, schemas: &[Value]) {
    for call in &mut reply.tool_calls {
        if let Some(schema) = schemas.iter().find(|s| s["function"]["name"] == call.name)
            && strict_parameters(&schema["function"]["parameters"]).is_some()
        {
            omit_optional_nulls(&mut call.arguments, &schema["function"]["parameters"]);
        }
    }
}
