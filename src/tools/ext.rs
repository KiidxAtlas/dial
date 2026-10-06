//! Pluggable tools: anything beyond the file/shell built-ins that is not tied to the agent loop's
//! own state (todo, memory, skills, task).
//!
//! A tool implements [`Tool`], and [`Registry::standard`] is the only place that lists them. The
//! agent asks the registry for schemas, approval requirements and execution, so adding a tool
//! touches the tool's own module plus one line in the registry.

use super::{ToolOutput, Workspace};
use crate::config::types::Config;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

/// Everything a tool may use while running. Tools hold no reference to the agent itself.
pub struct ToolContext<'a> {
    pub workspace: &'a Workspace,
    pub config: &'a Config,
}

/// Version of the public extension-tool lifecycle API.
pub const EXTENSION_API_VERSION: u32 = 1;

/// A sanitized lifecycle event emitted by the agent for observational integrations.
///
/// Event producers should only populate these fields with identifiers and status, never prompts,
/// source contents, arguments, credentials, or tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub event: String,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub tool_name: Option<String>,
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ToolEvent {
    pub kind: String,
    pub name: String,
    pub text: String,
    pub artifact: Option<String>,
}

pub trait Tool: Send + Sync {
    /// Unique name the model calls; also the entry in `harness.tools` that enables it.
    fn name(&self) -> &str;

    /// OpenAI-style function schema: `{"type":"function","function":{...}}`.
    fn schema(&self) -> Value;

    /// Whether this call changes anything or reaches outside the workspace and so needs approval.
    fn needs_approval(&self, args: &Value) -> bool;

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput;

    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _on_event: &(dyn Fn(ToolEvent) + Sync),
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> ToolOutput {
        self.call(ctx, args)
    }
    fn drain_notifications(&self) -> Vec<ToolEvent> {
        Vec::new()
    }
    /// Interrupt an active blocking operation without acquiring its session mutex.
    fn cancel_operation(&self) {}

    /// Version of this tool's public extension lifecycle contract.
    fn extension_api_version(&self) -> u32 {
        EXTENSION_API_VERSION
    }

    /// Event names this tool is configured to observe. Empty by default.
    fn lifecycle_hooks(&self) -> &[String] {
        &[]
    }

    /// Observes a sanitized lifecycle event. Implementations must not affect tool behavior.
    fn on_lifecycle_event(&self, _event: &LifecycleEvent) {}
}

#[derive(Default, Clone)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    notices: Vec<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every tool dial ships, configured from `config`. MCP servers connect here (concurrently,
    /// with a startup budget); a server that fails only adds a notice.
    pub fn standard(config: &Config) -> Self {
        let mut registry = Self::new();
        let groups = [
            super::background::tools(config),
            super::web::tools(config),
            super::lsp::tools(config),
            super::ast::tools(config),
            super::git::tools(config),
            super::dap::tools(&config.dap)
                .into_iter()
                .filter(|_tool| config.integrations.iter().any(|name| name == "dap"))
                .collect(),
        ];
        for tool in groups.into_iter().flatten() {
            if config.integrations.iter().any(|name| name == tool.name()) {
                registry.register(tool);
            }
        }
        for tool in super::browser::tools(config)
            .into_iter()
            .chain(super::kernel::tools(config))
            .chain(super::navigation::tools(config))
        {
            registry.register(tool);
        }
        let mcp = super::mcp::load(config);
        for tool in mcp.tools {
            registry.register(tool);
        }
        registry.notices = mcp.notices;
        registry
    }

    /// Startup diagnostics (for example an MCP server that failed to connect). Returned once, then cleared.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_owned(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
    /// Deliver only to tools that explicitly subscribed; hook failures cannot alter execution.
    pub fn fire_lifecycle(&self, event: &LifecycleEvent) {
        for tool in self.tools.values().filter(|tool| {
            tool.extension_api_version() == EXTENSION_API_VERSION
                && tool
                    .lifecycle_hooks()
                    .iter()
                    .any(|hook| hook == &event.event)
        }) {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tool.on_lifecycle_event(event)
            }));
        }
    }
}
