//! Canonical schema/identity and argument-validation contracts for every dispatch surface.
//! A catalog describes availability, never grants authorization or executes tools.
use crate::config::tools::integrations::INTEGRATIONS;

use crate::tools::{
    integrations::ext::{Registry, Tool},
    surface::contracts::{Identity, identities},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub fn integration_enabled(name: &str, configured: &[String]) -> bool {
    INTEGRATIONS
        .iter()
        .any(|entry| entry.name == name && entry.always_registered)
        || configured.iter().any(|entry| entry == name)
}

/// Advisory prerequisites for execution, not authorization or a promise that a call succeeds.
/// Unknown tools remain discoverable; querying readiness must never start a process or probe a service.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Readiness {
    Ready,
    MissingPrerequisite {
        reason: String,
    },
    ApprovalRequired {
        reason: String,
    },
    Unsupported {
        reason: String,
    },
    #[default]
    Unknown,
}

impl Readiness {
    /// Only known execution blockers defer automatic routing. Discovery and explicit loads do not use this.
    pub fn automatic_candidate(&self) -> bool {
        !matches!(
            self,
            Self::MissingPrerequisite { .. } | Self::Unsupported { .. }
        )
    }

    /// Stable preference among otherwise relevant tools. Approval still happens at dispatch.
    pub fn routing_priority(&self) -> u8 {
        match self {
            Self::Ready => 0,
            Self::Unknown | Self::ApprovalRequired { .. } => 1,
            Self::MissingPrerequisite { .. } | Self::Unsupported { .. } => 2,
        }
    }
}

/// Recomputed from current configuration and owner-held metadata, without cached absence claims.
pub fn readiness(
    name: &str,
    config: &crate::config::types::Config,
    registry: &Registry,
) -> Readiness {
    if name == "eval"
        && let Some(reason) = execution_platform(name).reason
    {
        return Readiness::Unsupported { reason };
    }
    if let Some(adapter) = name
        .strip_prefix("dap_")
        .and_then(|name| config.dap.get(name))
        && adapter.program.as_os_str().is_empty()
        && adapter.address.is_empty()
    {
        return Readiness::MissingPrerequisite {
            reason: "The configured debug adapter has neither a program nor a connection address"
                .into(),
        };
    }
    if let Some(tool) = registry.get(name) {
        return tool.readiness(config);
    }
    match name {
        // These native/controller operations have no service or interpreter prerequisite.
        "read" | "write" | "edit" | "grep" | "glob" | "tool_search" | "tool_load" => {
            Readiness::Ready
        }
        // Registry absence alone never proves that a controller, deferred or future tool is unsupported.
        _ => Readiness::Unknown,
    }
}

/// Operation-specific prerequisites must not disable unrelated operations of a multi-action tool.
pub fn operation_readiness(
    name: &str,
    config: &crate::config::types::Config,
) -> BTreeMap<String, Readiness> {
    if name != "computer" {
        return BTreeMap::new();
    }
    let locate = if crate::tools::vision::grounding::configured(config) {
        Readiness::Unknown // Assignment is not proof that the model is loaded or reachable.
    } else {
        Readiness::MissingPrerequisite {
            reason:
                "Locate requires a grounding model; run /setup grounding or set vision.grounding"
                    .into(),
        }
    };
    BTreeMap::from([("locate".into(), locate)])
}

/// Static OS support only: no execution grant, containment probe or interpreter health check.
#[derive(Clone, Debug, Serialize)]
pub struct PlatformAvailability {
    pub available: bool,
    pub reason: Option<String>,
}
pub fn execution_platform(name: &str) -> PlatformAvailability {
    let reason = if cfg!(target_os = "macos") || (cfg!(windows) && name == "eval") {
        None // Actual launch still requires the runtime's containment check.
    } else {
        Some(format!(
            "{name} requires OS process-fork containment; {}",
            if name == "eval" {
                "only macOS and Windows are supported"
            } else {
                "no unsandboxed or Linux fallback is provided"
            }
        ))
    };
    PlatformAvailability {
        available: reason.is_none(),
        reason,
    }
}
pub(crate) fn execution_description(name: &str, description: &str) -> String {
    match execution_platform(name).reason {
        Some(reason) => format!(
            "{description} Unavailable on this host: {reason}. status/reset/cancel remain available."
        ),
        None => description.to_owned(),
    }
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformAvailability>,
    pub readiness: Readiness,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub operation_readiness: BTreeMap<String, Readiness>,
}
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    definitions: BTreeMap<String, Definition>,
}
impl Catalog {
    /// The checks [`Self::from_schemas`] makes (valid definitions, no duplicate names) without
    /// building the catalog: no contract-identity hashing or schema copies. Request preparation
    /// validates its offer on every round; only the definitions are needed for that.
    pub fn validate_schemas(schemas: &[Value]) -> Result<()> {
        let mut names = std::collections::BTreeSet::new();
        for schema in schemas {
            let name = validate_definition(schema)?;
            ensure!(names.insert(name), "duplicate tool definition {name}");
        }
        Ok(())
    }
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
                } else if crate::tools::BUILTIN.contains(&name) {
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
                    platform: matches!(name, "eval").then(|| execution_platform(name)),
                    readiness: Readiness::Unknown,
                    operation_readiness: BTreeMap::new(),
                },
            );
        }
        Ok(Self { definitions })
    }
    /// Refresh advisory metadata at the observation boundary, never at registration alone.
    pub fn refresh_readiness(
        &mut self,
        config: &crate::config::types::Config,
        registry: &Registry,
    ) {
        for definition in self.definitions.values_mut() {
            definition.readiness = readiness(&definition.name, config, registry);
            definition.operation_readiness = operation_readiness(&definition.name, config);
        }
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
        tool.extension_api_version() == crate::tools::integrations::ext::EXTENSION_API_VERSION,
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
    crate::llm::reliability::omit_optional_nulls(&mut arguments, parameters);
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
mod tests;
