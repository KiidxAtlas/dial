//! Durable tool identities. These describe history; they never authorize execution or replay it.
use super::ext::Registry;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub revision: u32,
    pub schema_sha256: String,
}

/// Pure provider-facing view of a settled historical call. The original journal is never rewritten.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoricalCall {
    pub arguments: Value,
    pub output: String,
}

fn native_compatible(old: &Identity, new: &Identity, registry: &Registry) -> bool {
    if old.revision != 1
        || new.revision != 1
        || registry.get(&old.name).is_some()
        || !super::BUILTIN.contains(&old.name.as_str())
    {
        return false;
    }
    // Editor dialects are current-call contracts, not instructions to reinterpret old effects.
    // Recognize their exact identities while leaving old arguments/results as inert history.
    use crate::config::types::EditMode;
    let known: Vec<_> = [
        EditMode::Hashline,
        EditMode::Replace,
        EditMode::Patch,
        EditMode::ApplyPatch,
        EditMode::Sloppy,
    ]
    .into_iter()
    .filter_map(|mode| super::schema(&old.name, mode))
    .filter_map(|schema| identities(&[schema], &Registry::new()).remove(&old.name))
    .collect();
    known.contains(old) && known.contains(new)
}

/// Project settled history onto the current contracts. `None` means no historical call needed a
/// rewrite, so callers send the borrowed history as-is; the history is copied only on first rewrite.
pub fn project_history(
    messages: &[Value],
    schemas: &[Value],
    registry: &Registry,
) -> Result<Option<Vec<Value>>> {
    let current = identities(schemas, registry);
    let mut projected: Option<Vec<Value>> = None;
    for (index, message) in messages.iter().enumerate() {
        let Some(recorded) = message.get("dial_tool_contracts") else {
            continue;
        };
        let recorded: BTreeMap<String, Identity> = serde_json::from_value(recorded.clone())?;
        for (id, old) in &recorded {
            let Some(new) = current.get(&old.name) else {
                continue;
            };
            if old == new
                || native_compatible(old, new, registry)
                || registry
                    .get(&old.name)
                    .is_some_and(|tool| tool.accepts_historical_contract(old))
            {
                continue;
            }
            let tool = registry.get(&old.name).ok_or_else(|| {
                anyhow::anyhow!("tool {} history needs a revision projector", old.name)
            })?;
            let call_index = message["tool_calls"]
                .as_array()
                .and_then(|calls| calls.iter().position(|call| call["id"] == *id));
            let result_index = messages
                .iter()
                .enumerate()
                .skip(index + 1)
                .take_while(|(_, message)| message["role"] == "tool")
                .find_map(|(index, message)| (message["tool_call_id"] == *id).then_some(index));
            let (Some(call_index), Some(result_index)) = (call_index, result_index) else {
                anyhow::bail!(
                    "tool {} contract changed; unpaired or callback-catalog history requires explicit compatibility",
                    old.name
                );
            };
            let arguments = &message["tool_calls"][call_index]["function"]["arguments"];
            let original = HistoricalCall {
                arguments: if let Some(text) = arguments.as_str() {
                    serde_json::from_str(text)?
                } else {
                    arguments.clone()
                },
                output: messages[result_index]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
            };
            let lifted = tool.project_historical_call(old, &original)
                .ok_or_else(|| anyhow::anyhow!("tool {} contract changed (revision {} -> {}); no compatible historical projection", old.name, old.revision, new.revision))?;
            ensure!(
                tool.project_historical_call(old, &original).as_ref() == Some(&lifted),
                "tool {} historical projection is nondeterministic",
                old.name
            );
            let projected = projected.get_or_insert_with(|| messages.to_vec());
            projected[index]["tool_calls"][call_index]["function"]["arguments"] =
                json!(lifted.arguments.to_string());
            projected[result_index]["content"] = json!(lifted.output);
            projected[index]["dial_tool_contracts"][id] = json!(new);
        }
    }
    Ok(projected)
}

fn parameter_shape(value: &Value) -> Value {
    let Value::Object(original) = value else {
        return value.clone();
    };
    let mut shape = original.clone();
    for annotation in ["description", "title", "examples", "$comment", "deprecated"] {
        shape.remove(annotation);
    }
    // Traverse schema nodes only: property names and literal enum/const/default objects are data.
    for keyword in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(Value::Object(children)) = shape.get_mut(keyword) {
            for child in children.values_mut() {
                *child = parameter_shape(child);
            }
        }
    }
    for keyword in [
        "items",
        "additionalItems",
        "additionalProperties",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "allOf",
        "anyOf",
        "oneOf",
        "prefixItems",
    ] {
        if let Some(child) = shape.get_mut(keyword) {
            if let Value::Array(children) = child {
                for child in children {
                    *child = parameter_shape(child);
                }
            } else {
                *child = parameter_shape(child);
            }
        }
    }
    Value::Object(shape)
}

pub fn identities(schemas: &[Value], registry: &Registry) -> BTreeMap<String, Identity> {
    schemas.iter().filter_map(|schema| {
        let name = schema.pointer("/function/name")?.as_str()?;
        // Documentation/presentation changes do not alter the executable contract. Parameters,
        // strictness and the tool-owned semantic revision do. serde_json maps are canonical here.
        let shape = json!({"parameters":parameter_shape(&schema["function"]["parameters"]), "strict":schema["function"]["strict"]});
        Some((name.to_owned(), Identity {
            name: name.to_owned(),
            revision: registry.get(name).map_or(1, |tool| tool.contract_revision()),
            schema_sha256: format!("{:x}", Sha256::digest(shape.to_string().as_bytes())),
        }))
    }).collect()
}

pub fn assistant_message(
    reply: &crate::llm::Reply,
    schemas: &[Value],
    registry: &Registry,
) -> Value {
    let mut message = crate::llm::assistant_message(reply);
    let available = identities(schemas, registry);
    let mut calls: BTreeMap<_, _> = reply
        .tool_calls
        .iter()
        .filter_map(|call| {
            available
                .get(&call.name)
                .map(|identity| (call.id.clone(), identity))
        })
        .collect();
    if reply.tool_calls.iter().any(|call| {
        registry
            .get(&call.name)
            .is_some_and(|tool| tool.supports_tool_dispatch())
    }) {
        // Bind the catalog callable through code-mode callbacks as well as direct calls.
        calls.extend(
            available
                .iter()
                .map(|(name, identity)| (format!("callback:{name}"), identity)),
        );
    }
    if !calls.is_empty() {
        message["dial_tool_contracts"] = json!(calls);
    }
    message
}

/// Validate historical contracts against currently available tools before continuation. Removed
/// tools are inert history, and legacy unversioned rows remain readable; neither grants authority.
/// A tool may explicitly attest compatibility with an old identity. Schema changes are otherwise
/// refused instead of guessing how old arguments or results should be reinterpreted.
pub fn validate_history(messages: &[Value], schemas: &[Value], registry: &Registry) -> Result<()> {
    project_history(messages, schemas, registry)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn schema(kind: &str) -> Value {
        json!({"type":"function","function":{"name":"test","parameters":{"type":"object","properties":{"value":{"type":kind}}}}})
    }
    #[test]
    fn changing_native_edit_dialect_preserves_settled_history_without_replaying_it() {
        let registry = Registry::new();
        let old = super::super::schema("edit", crate::config::types::EditMode::Hashline).unwrap();
        let new = super::super::schema("edit", crate::config::types::EditMode::Replace).unwrap();
        let identity = identities(&[old], &registry)["edit"].clone();
        let history = vec![
            json!({"role":"assistant", "tool_calls":[{"id":"call", "type":"function", "function":{"name":"edit", "arguments":"{\"input\":\"historical hashline edit\"}"}}], "dial_tool_contracts":{"call":identity}}),
            json!({"role":"tool", "tool_call_id":"call", "content":"actual recorded edit result"}),
        ];
        assert!(
            project_history(&history, &[new], &registry)
                .unwrap()
                .is_none(),
            "compatible history needs no projection copy"
        );
    }
    #[test]
    fn pure_revision_projection_is_idempotent_and_never_rewrites_or_executes_history() {
        struct Lift;
        impl super::super::ext::Tool for Lift {
            fn name(&self) -> &str {
                "test"
            }
            fn schema(&self) -> Value {
                schema("integer")
            }
            fn needs_approval(&self, _: &Value) -> bool {
                false
            }
            fn contract_revision(&self) -> u32 {
                2
            }
            fn call(
                &self,
                _: &super::super::ext::ToolContext,
                _: &Value,
            ) -> super::super::ToolOutput {
                panic!("projection must never execute")
            }
            fn project_historical_call(
                &self,
                old: &Identity,
                call: &HistoricalCall,
            ) -> Option<HistoricalCall> {
                if old != &identities(&[schema("string")], &Registry::new())["test"] {
                    return None;
                }
                Some(HistoricalCall {
                    arguments: json!({"value":call.arguments["value"].as_str()?.parse::<u64>().ok()?}),
                    output: format!("revision 2: {}", call.output),
                })
            }
        }
        let old = identities(&[schema("string")], &Registry::new());
        let history = vec![
            json!({"role":"assistant", "tool_calls":[{"id":"a", "type":"function", "function":{"name":"test", "arguments":"{\"value\":\"7\"}"}}], "dial_tool_contracts":{"a":old["test"]}}),
            json!({"role":"tool", "tool_call_id":"a", "content":"kept result"}),
        ];
        let original = history.clone();
        let mut registry = Registry::new();
        registry.register(std::sync::Arc::new(Lift));
        let projected = project_history(&history, &[schema("integer")], &registry)
            .unwrap()
            .expect("a revised historical call must be projected");
        assert_eq!(
            projected[0]["tool_calls"][0]["function"]["arguments"],
            "{\"value\":7}"
        );
        assert_eq!(projected[1]["content"], "revision 2: kept result");
        assert!(
            project_history(&projected, &[schema("integer")], &registry)
                .unwrap()
                .is_none(),
            "an already projected history is idempotent without another copy"
        );
        assert_eq!(history, original);
    }
    #[test]
    fn schema_annotations_do_not_erase_property_names_or_literal_values() {
        let schema = json!({
            "description":"help",
            "properties":{
                "description":{"type":"string", "description":"parameter help"},
                "payload":{"const":{"description":"literal data"}}
            },
            "anyOf":[{"title":"label", "required":["description"]}]
        });
        assert_eq!(
            parameter_shape(&schema),
            json!({
                "properties":{
                    "description":{"type":"string"},
                    "payload":{"const":{"description":"literal data"}}
                },
                "anyOf":[{"required":["description"]}]
            })
        );
    }
    #[test]
    fn compatibility_requires_explicit_tool_attestation_and_preserves_history() {
        struct Compatible;
        impl super::super::ext::Tool for Compatible {
            fn needs_approval(&self, _: &Value) -> bool {
                false
            }
            fn name(&self) -> &str {
                "test"
            }
            fn schema(&self) -> Value {
                schema("integer")
            }
            fn contract_revision(&self) -> u32 {
                2
            }
            fn accepts_historical_contract(&self, old: &Identity) -> bool {
                old == &identities(&[schema("string")], &Registry::new())["test"]
            }
            fn call(
                &self,
                _: &super::super::ext::ToolContext,
                _: &Value,
            ) -> super::super::ToolOutput {
                panic!("history must never execute")
            }
        }
        let old = identities(&[schema("string")], &Registry::new());
        let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old["test"]}})];
        let original = history.clone();
        let mut registry = Registry::new();
        registry.register(std::sync::Arc::new(Compatible));
        validate_history(&history, &[schema("integer")], &registry).unwrap();
        assert_eq!(history, original);
        let mut unsupported = history;
        unsupported[0]["dial_tool_contracts"]["id"]["revision"] = json!(99);
        assert!(validate_history(&unsupported, &[schema("integer")], &registry).is_err());
    }
    #[test]
    fn changed_schema_refuses_continuation_but_legacy_and_documentation_remain_readable() {
        let registry = Registry::new();
        let old = identities(&[schema("string")], &registry);
        let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old["test"]}})];
        validate_history(&history, &[schema("string")], &registry).unwrap();
        assert!(validate_history(&history, &[schema("integer")], &registry).is_err());
        let mut documented = schema("string");
        documented["function"]["description"] = json!("new help");
        documented["function"]["parameters"]["properties"]["value"]["description"] =
            json!("new parameter help");
        validate_history(&history, &[documented], &registry).unwrap();
        validate_history(
            &[json!({"role":"assistant","tool_calls":[]})],
            &[schema("integer")],
            &registry,
        )
        .unwrap();
        validate_history(&history, &[], &registry).unwrap();
    }
}
