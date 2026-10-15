//! Canonical schema/identity and argument-validation contracts for every dispatch surface.
//! A catalog describes availability, never grants authorization or executes tools.
use super::{
    contracts::{Identity, identities},
    ext::{Registry, Tool},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Native,
    Controller,
    Extension,
    Configured,
    Mcp,
}
#[derive(Clone, Debug, Serialize)]
pub struct Definition {
    pub name: String,
    pub origin: Origin,
    pub identity: Identity,
    pub schema: Value,
}
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    definitions: BTreeMap<String, Definition>,
}
impl Catalog {
    pub fn from_schemas(schemas: &[Value], registry: &Registry) -> Result<Self> {
        let identities = identities(schemas, registry);
        let mut definitions = BTreeMap::new();
        for schema in schemas {
            let name = validate_definition(schema)?;
            ensure!(
                !definitions.contains_key(name),
                "duplicate tool definition {name}"
            );
            let origin = registry.origin(name).unwrap_or_else(|| {
                if name.starts_with("mcp__") {
                    Origin::Mcp
                } else if super::BUILTIN.contains(&name) {
                    Origin::Native
                } else {
                    Origin::Controller
                }
            });
            let identity = identities
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("tool {name} has no contract identity"))?
                .clone();
            definitions.insert(
                name.to_owned(),
                Definition {
                    name: name.into(),
                    origin,
                    identity,
                    schema: schema.clone(),
                },
            );
        }
        Ok(Self { definitions })
    }
    pub fn set_origin(&mut self, name: &str, origin: Origin) {
        if let Some(definition) = self.definitions.get_mut(name) {
            definition.origin = origin;
        }
    }
    pub fn get(&self, name: &str) -> Option<&Definition> {
        self.definitions.get(name)
    }
    pub fn definitions(&self) -> impl Iterator<Item = &Definition> {
        self.definitions.values()
    }
    pub fn validate(&self, name: &str, arguments: &Value) -> Result<(), String> {
        let definition = self
            .get(name)
            .ok_or_else(|| format!("unknown or unavailable tool: {name}"))?;
        validate_arguments(&definition.schema, arguments)
    }
}
pub fn validate_definition(schema: &Value) -> Result<&str> {
    ensure!(
        schema["type"] == "function",
        "tool schema must describe a function"
    );
    let name = schema["function"]["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("tool schema has no name"))?;
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "invalid tool name"
    );
    ensure!(
        schema["function"]["parameters"].is_object(),
        "tool parameters must be a schema object"
    );
    crate::runtime::schema::check_schema(&schema["function"]["parameters"])
        .map_err(anyhow::Error::msg)?;
    Ok(name)
}
pub fn validate_tool(tool: &dyn Tool) -> Result<()> {
    ensure!(
        tool.extension_api_version() == super::ext::EXTENSION_API_VERSION,
        "unsupported extension tool version"
    );
    let schema = tool.schema();
    ensure!(
        validate_definition(&schema)? == tool.name(),
        "tool name and schema name disagree"
    );
    ensure!(
        tool.contract_revision() > 0,
        "tool semantic revision must be positive"
    );
    Ok(())
}
pub fn validate_arguments(schema: &Value, arguments: &Value) -> Result<(), String> {
    let parameters = schema
        .pointer("/function/parameters")
        .ok_or("tool has no parameter schema")?;
    let mut arguments = arguments.clone();
    crate::reliability::omit_optional_nulls(&mut arguments, parameters);
    crate::runtime::arguments::validate(parameters, &arguments)
}

/// The request's offered schemas are authoritative, including role restrictions and deferred tools.
pub fn validate_offered_call(
    call: &crate::llm::ToolCall,
    schemas: &[Value],
    names: &[String],
) -> Option<(&'static str, String)> {
    if call.malformed_arguments || call.name.is_empty() {
        return Some((
            "malformed_arguments",
            "tool arguments were not a valid JSON object".into(),
        ));
    }
    if !names.contains(&call.name) {
        return Some((
            "unknown_tool",
            format!("unknown or unavailable tool: {}", call.name),
        ));
    }
    let Some(schema) = schemas
        .iter()
        .find(|schema| schema["function"]["name"].as_str() == Some(&call.name))
    else {
        return Some((
            "schema",
            format!(
                "offered tool {} has no schema; nothing was executed",
                call.name
            ),
        ));
    };
    validate_arguments(schema, &call.arguments)
        .err()
        .map(|error| ("schema", error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn missing_offered_schema_and_duplicate_definitions_fail_before_execution() {
        let call = crate::llm::ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: json!({}),
            malformed_arguments: false,
        };
        assert!(validate_offered_call(&call, &[], &["read".into()]).is_some());
        let schema = super::super::schema("read", crate::config::types::EditMode::Replace).unwrap();
        assert!(Catalog::from_schemas(&[schema.clone(), schema], &Registry::new()).is_err());
    }
}
