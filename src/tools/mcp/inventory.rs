//! MCP inventory and discovery for the agent. `list` reports what is configured and connected
//! (configuration presence is not evidence of a live connection). `search` reads the official MCP
//! registry, Smithery and the curated list. `install` adds one server to the global config after a
//! health check, and needs the user's approval (see [`Tool::needs_approval`]).
use super::catalog::{self, Endpoints, Input};
use super::smithery;
use super::{McpLoad, Session};
use crate::config::types::{Config, McpServerConfig};
use crate::storage::paths::Layout;
use crate::storage::settings_store::{self, Scope};
use crate::tools::ToolOutput;
use crate::tools::ext::{Tool, ToolContext};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

struct Server {
    name: String,
    enabled: bool,
    session: Option<Arc<Session>>,
    tools: Vec<Value>,
}

pub(super) struct Inventory {
    servers: Vec<Server>,
}

impl Inventory {
    pub(super) fn new(config: &Config, loaded: &McpLoad) -> Self {
        let servers = config
            .mcp
            .iter()
            .map(|(name, config)| {
                let session = loaded
                    .sessions
                    .iter()
                    .find(|session| session.name() == name)
                    .cloned();
                let tools = loaded
                    .tools
                    .iter()
                    .filter(|tool| tool.lifecycle_channel() == Some(name.as_str()))
                    .map(|tool| {
                        json!({
                            "name": tool.name(),
                            "description": tool.schema()["function"]["description"],
                            "read_only": tool.read_only_hint(),
                        })
                    })
                    .collect();
                Server {
                    name: name.clone(),
                    enabled: config.enabled,
                    session,
                    tools,
                }
            })
            .collect();
        Self { servers }
    }

    fn snapshot(&self) -> Value {
        let servers: Vec<_> = self
            .servers
            .iter()
            .map(|server| {
                let status = if !server.enabled {
                    "disabled"
                } else if let Some(session) = &server.session {
                    if session.current().is_closed() {
                        "disconnected"
                    } else {
                        "connected"
                    }
                } else {
                    "failed"
                };
                let capabilities: Vec<_> = server
                    .session
                    .as_ref()
                    .map(|session| {
                        let capabilities = session.capabilities();
                        ["tools", "resources", "prompts"]
                            .into_iter()
                            .filter(|key| capabilities.get(*key).is_some())
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"name": server.name, "status": status,
                    "capabilities": capabilities, "tools": server.tools})
            })
            .collect();
        json!({"servers": servers,
            "note": "Only connected servers have completed initialization and discovery. Listed tool names are registered capabilities, not permission grants; model/profile eligibility and approvals still apply. Use tool_search or tool_load for next-round schemas. Failed/disabled servers have no callable tools. Use /mcp for operator diagnostics and configuration; disconnected sessions reconnect on demand."})
    }
}

fn input_json(input: &Input) -> Value {
    json!({
        "name": input.key,
        "required": input.required,
        "secret": input.secret,
        "description": input.description,
    })
}

/// Registry search for the agent: every listing merged, with what each server needs.
fn search(config: &Config, query: &str) -> ToolOutput {
    let found = catalog::discover(&Endpoints::live(), config, query, 1);
    let results: Vec<Value> = found
        .candidates
        .iter()
        .map(|candidate| {
            json!({
                "id": candidate.name,
                "title": candidate.title,
                "description": candidate.description,
                "sources": candidate.sources.iter().map(|s| s.label()).collect::<Vec<_>>(),
                "installable": candidate.installable(),
                "install": candidate.install_summary(),
                "inputs": candidate.inputs.iter().map(input_json).collect::<Vec<_>>(),
                "repository": candidate.repository,
            })
        })
        .collect();
    ToolOutput::structured(json!({
        "results": results,
        "more": found.more,
        "notices": found.notices,
        "note": "Search reads listings only; nothing was downloaded or started. Install a result with action install and its id. Environment values are never guessed: inputs marked required must be entered by the user in /extensions.",
    }))
}

/// Approval-gated install: resolve, refuse anything that needs values, start once to check, save.
fn install(config: &Config, id: &str) -> ToolOutput {
    let id = id.trim();
    if id.is_empty() {
        return ToolOutput::err("install needs `id`: use the id of a search result");
    }
    let candidate = match catalog::resolve(&Endpoints::live(), config, id) {
        Ok(candidate) => candidate,
        Err(error) => return ToolOutput::err(format!("{error:#}")),
    };
    let local = catalog::config_name(&candidate.name);
    if config.mcp.contains_key(&local) {
        return ToolOutput::err(format!(
            "`{local}` is already configured. Check it with action list."
        ));
    }
    let required: Vec<&Input> = candidate
        .inputs
        .iter()
        .filter(|input| input.required)
        .collect();
    if !required.is_empty() {
        return ToolOutput::structured(json!({
            "status": "needs_input",
            "id": candidate.name,
            "required": required.iter().copied().map(input_json).collect::<Vec<_>>(),
            "message": format!(
                "`{}` needs values the user must enter. Nothing was installed. Ask the user to open /extensions, select `{}` and press i to enter them.",
                candidate.title, candidate.name
            ),
        }));
    }
    let server = match candidate.server(&BTreeMap::new()) {
        Ok(server) => server,
        Err(error) => return ToolOutput::err(format!("{error:#}")),
    };
    if server.url.is_none() && !catalog::on_path(&server.program.to_string_lossy()) {
        return ToolOutput::err(format!(
            "{} is not on PATH, so `{local}` cannot start. Install it first.",
            server.program.display()
        ));
    }
    let tools = match catalog::check(&config.project_root, &local, &server) {
        Ok(count) => count,
        Err(why) => return ToolOutput::err(format!("health check failed, nothing saved: {why}")),
    };
    match save(config, &local, &server) {
        Ok(()) => ToolOutput::structured(json!({
            "status": "installed",
            "name": local,
            "tools": tools,
            "message": format!(
                "Installed `{local}` with {tools} tool(s) in the global config. It takes effect when Dial starts its next session; /mcp shows its status."
            ),
        })),
        Err(error) => ToolOutput::err(format!(
            "health check passed, but saving the config failed: {error:#}"
        )),
    }
}

/// Writes the server into the global config, validated like any other settings change.
fn save(config: &Config, name: &str, server: &McpServerConfig) -> Result<()> {
    let layout = Layout::with_user_dir(&config.project_root, config.user_dir.clone())?;
    let change = smithery::config_change(name, server)?;
    settings_store::save_config(&layout, Scope::Global, &[change])?;
    Ok(())
}

impl Tool for Inventory {
    fn name(&self) -> &str {
        "mcp_servers"
    }

    fn schema(&self) -> Value {
        json!({"type":"function","function":{
            "name":self.name(),
            "description":"Inspect and find MCP servers. action `list` (default): configured servers with runtime connection status (connected, disconnected, failed or disabled), negotiated capabilities and registered tool names; starts nothing. action `search` with optional `query`: find servers in the official MCP registry, Smithery, npm, GitHub and the curated list (read-only). action `install` with `id` from a search result: needs the user's approval, starts the server once to check it, then saves it to the global config; it takes effect in the next session. Servers that need environment values are not installed; the result names them for the user to enter in /extensions. Discover exact listed tools with tool_search or tool_load to use them next round.",
            "parameters":{"type":"object","properties":{
                "action":{"type":"string","enum":["list","search","install"]},
                "query":{"type":"string","description":"Search text for action search; empty lists the catalog"},
                "id":{"type":"string","description":"Result id from a search, for action install"}
            },"additionalProperties":false}
        }})
    }

    /// Only `install` changes anything; list and search are reads.
    fn needs_approval(&self, args: &Value) -> bool {
        args.get("action").and_then(Value::as_str) == Some("install")
    }
    fn read_only_hint(&self) -> bool {
        false
    }
    fn may_modify_workspace(&self, _args: &Value) -> bool {
        false
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match args.get("action").and_then(Value::as_str).unwrap_or("list") {
            "list" => ToolOutput::structured(self.snapshot()),
            "search" => search(
                ctx.config,
                args.get("query").and_then(Value::as_str).unwrap_or(""),
            ),
            "install" => install(
                ctx.config,
                args.get("id").and_then(Value::as_str).unwrap_or(""),
            ),
            other => ToolOutput::err(format!(
                "unknown action `{other}`; use list, search or install"
            )),
        }
    }
}

#[cfg(test)]
mod tests;
