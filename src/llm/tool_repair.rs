//! Constrained tool repair for local endpoints. After a malformed or schema-invalid tool call, the
//! harness re-issues the request once with output forced to a JSON object that names one offered
//! tool. That object is converted back into an ordinary tool call; runtime validation and approvals
//! still run on the converted call.

use super::ToolCall;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

/// Schema name sent with the LM Studio `json_schema` response format.
pub const RESPONSE_NAME: &str = "dial_tool_call";

/// `oneOf` over the offered tools. Each branch pins `name` to one tool and constrains `arguments`
/// to that tool's parameter schema. Accepts both `{"type":"function","function":{..}}` tools and
/// bare `{"name","parameters"}` tools.
pub fn schema_for_tools(tools: &[Value]) -> Result<Value> {
    ensure!(
        !tools.is_empty(),
        "constrained repair needs at least one offered tool"
    );
    let branches = tools
        .iter()
        .map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            let name = function["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow::anyhow!("constrained repair tool has no name"))?;
            ensure!(
                function["parameters"].is_object(),
                "tool {name} parameters must be a JSON Schema object"
            );
            Ok(json!({
                "type": "object",
                "properties": {
                    "name": {"const": name},
                    "arguments": function["parameters"]
                },
                "required": ["name", "arguments"],
                "additionalProperties": false
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"oneOf": branches}))
}

/// OpenAI-compatible `response_format` carrying the schema (LM Studio).
pub fn lmstudio_response_format(schema: Value) -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {"name": RESPONSE_NAME, "strict": true, "schema": schema}
    })
}

/// Converts constrained output into a tool call. Accepts only exactly `{"name", "arguments"}` with
/// an offered name and an object of arguments; anything else is returned as the rejection reason.
pub fn call_from_reply(text: &str, offered: &[String], id: String) -> Result<ToolCall, String> {
    let value: Value =
        serde_json::from_str(text.trim()).map_err(|error| format!("not JSON: {error}"))?;
    let object = value.as_object().ok_or("output is not a JSON object")?;
    if object.len() != 2 {
        return Err("output must contain exactly name and arguments".into());
    }
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or("name is not a string")?;
    if !offered.iter().any(|tool| tool == name) {
        return Err(format!("name {name} is not an offered tool"));
    }
    let arguments = object
        .get("arguments")
        .filter(|arguments| arguments.is_object())
        .ok_or("arguments is not an object")?;
    Ok(ToolCall {
        id,
        name: name.to_owned(),
        arguments: arguments.clone(),
        malformed_arguments: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Value {
        json!({"type":"function","function":{"name":name,"description":"d","parameters":{
            "type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}})
    }

    #[test]
    fn schema_has_one_branch_per_tool_with_const_name() {
        let schema = schema_for_tools(&[tool("read"), tool("write")]).unwrap();
        let branches = schema["oneOf"].as_array().unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[1]["properties"]["name"]["const"], "write");
        assert_eq!(
            branches[0]["properties"]["arguments"]["required"][0],
            "path"
        );
        assert_eq!(branches[0]["additionalProperties"], false);
    }

    #[test]
    fn schema_rejects_no_tools_and_non_object_parameters() {
        assert!(schema_for_tools(&[]).is_err());
        let bad = json!({"type":"function","function":{"name":"x","parameters":true}});
        assert!(schema_for_tools(&[bad]).is_err());
    }

    #[test]
    fn constrained_output_becomes_tool_call() {
        let offered = vec!["read".to_owned()];
        let call = call_from_reply(
            r#" {"name":"read","arguments":{"path":"a.rs"}} "#,
            &offered,
            "repair-1".into(),
        )
        .unwrap();
        assert_eq!(call.id, "repair-1");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, json!({"path":"a.rs"}));
        assert!(!call.malformed_arguments);
    }

    #[test]
    fn invalid_constrained_output_is_rejected() {
        let offered = vec!["read".to_owned()];
        for text in [
            "plain prose",
            r#"{"name":"delete","arguments":{}}"#,
            r#"{"name":"read","arguments":["a"]}"#,
            r#"{"name":"read"}"#,
            r#"{"name":"read","arguments":{},"extra":1}"#,
        ] {
            assert!(
                call_from_reply(text, &offered, "x".into()).is_err(),
                "accepted {text}"
            );
        }
    }
}
