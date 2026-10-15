//! Read-only runtime inventory. Configuration presence is not evidence of a live connection.
use super::{McpLoad, Session};
use crate::config::types::Config;
use crate::tools::ToolOutput;
use crate::tools::ext::{Tool, ToolContext};
use serde_json::{Value, json};
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

impl Tool for Inventory {
    fn name(&self) -> &str {
        "mcp_servers"
    }

    fn schema(&self) -> Value {
        json!({"type":"function","function":{
            "name":self.name(),
            "description":"List installed/configured MCP servers and their runtime connection status (connected, disconnected, failed or disabled), negotiated capabilities and registered tool names. Inspect MCP server inventory without starting servers, executing remote tools or granting approval. Discover exact listed tools with tool_search or tool_load to use them next round.",
            "parameters":{"type":"object","properties":{},"additionalProperties":false}
        }})
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    fn may_modify_workspace(&self, _args: &Value) -> bool {
        false
    }

    fn call(&self, _ctx: &ToolContext, _args: &Value) -> ToolOutput {
        ToolOutput::structured(self.snapshot())
    }
}
