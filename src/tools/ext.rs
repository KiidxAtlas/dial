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

/// Authorizes `(tool, paths, unbounded_effects)` before a native tool mutates or starts a process.
pub type MutationAuthorizer<'a> =
    &'a dyn Fn(&str, &[std::path::PathBuf], bool) -> Result<(), String>;

/// Everything a tool may use while running. Tools hold no reference to the agent itself.
pub struct ToolContext<'a> {
    pub workspace: &'a Workspace,
    pub config: &'a Config,
    /// Controller-owned authorization for targets discovered inside native tools.
    /// Missing authority permits inspection, never native mutation or process startup.
    pub mutation_authorizer: Option<MutationAuthorizer<'a>>,
    /// Controller-owned user confirmation for effects a tool gates on its own (for example, computer actions).
    /// `None` means no interactive owner is attached; the tool must refuse. The argument is a summary of the action.
    pub confirm: Option<&'a dyn Fn(&str) -> bool>,
}
impl ToolContext<'_> {
    pub fn authorize_mutation(
        &self,
        tool: &str,
        paths: &[std::path::PathBuf],
        unbounded_effects: bool,
    ) -> Result<(), String> {
        let authorize = self.mutation_authorizer.ok_or_else(|| {
            "native mutation requires execution-controller authorization".to_owned()
        })?;
        authorize(tool, paths, unbounded_effects)
    }
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

/// Code-mode requests return to the execution owner's controller. A tool never
/// receives direct registry access or an approval-bypassing executor.
#[derive(Debug, Clone)]
pub enum ToolCallback {
    Schemas,
    Call { name: String, arguments: Value },
}

/// How a call may be scheduled within one assistant batch. Shared calls between exclusive
/// barriers run concurrently; an exclusive call waits for everything before it and blocks
/// everything after it until it finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    Shared,
    Exclusive,
}

pub trait Tool: Send + Sync {
    /// Unique name the model calls; also the entry in `harness.tools` that enables it.
    fn name(&self) -> &str;

    /// OpenAI-style function schema: `{"type":"function","function":{...}}`.
    fn schema(&self) -> Value;

    /// Semantic behavior revision, independent of the extension SDK and schema fingerprint.
    /// Increment when identical arguments can acquire a different meaning.
    fn contract_revision(&self) -> u32 {
        1
    }

    /// Explicit compatibility for settled historical calls. This never permits re-execution,
    /// weakens current schema validation, or changes the durable original record.
    fn accepts_historical_contract(&self, _recorded: &super::contracts::Identity) -> bool {
        false
    }
    /// Deterministic projection of a settled old call; never executes the call or changes its journal.
    fn project_historical_call(
        &self,
        _recorded: &super::contracts::Identity,
        _call: &super::contracts::HistoricalCall,
    ) -> Option<super::contracts::HistoricalCall> {
        None
    }

    /// Portable equivalents of tool-owned call and result renderer hooks.
    fn render_call(&self, args: &Value) -> super::presentation::Presentation {
        super::presentation::render_call(self.name(), args)
    }
    fn render_result(
        &self,
        args: &Value,
        output: &ToolOutput,
    ) -> super::presentation::Presentation {
        let mut view = self.render_call(args);
        let result = super::presentation::render_result(self.name(), args, output);
        view.facts.extend(result.facts);
        view
    }

    /// The completed execution's state; renderers cannot change it.
    fn outcome(&self, args: &Value, output: &ToolOutput) -> crate::runtime::Phase {
        super::outcome::result_phase(self.name(), args, output)
    }

    /// Whether this call changes anything or reaches outside the workspace and so needs approval.
    fn needs_approval(&self, args: &Value) -> bool;
    /// The tool's provider declares it read-only (for example MCP `annotations.readOnlyHint`).
    /// Read-only subagents are offered such tools. The hint bypasses approval only where the
    /// provider's config trusts it (MCP `trust_read_only_hint`); it is never a boundary by itself.
    fn read_only_hint(&self) -> bool {
        false
    }
    /// Whether this operation may change workspace files outside `Workspace`'s write observer.
    /// The default is conservative for approval-requiring tools; integrations backed only by
    /// external state should return false.
    fn may_modify_workspace(&self, args: &Value) -> bool {
        self.needs_approval(args)
    }
    /// Owns the workspace mutation barrier internally. The controller must not
    /// acquire the same non-reentrant lock around this tool's operation.
    fn manages_workspace_serialization(&self) -> bool {
        false
    }
    /// Scheduling class for this call. Shared calls must have no side effects and must not depend
    /// on effects of sibling calls in the same batch; approvals and hooks still apply. The default
    /// follows the provider's read-only declaration (MCP `readOnlyHint`, extension manifests).
    fn concurrency(&self, _args: &Value) -> Concurrency {
        if self.read_only_hint() {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        }
    }
    /// A pure wait (no work of its own to finish): queued user steering ends it early through the
    /// per-call cancel flag and `signals::wait_interrupted()`.
    fn interruptible(&self, _args: &Value) -> bool {
        false
    }

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
    /// Whether this tool can suspend code execution for controller-authorized calls.
    /// Ordinary integrations use the streaming lifecycle without callback access.
    fn supports_tool_dispatch(&self) -> bool {
        false
    }
    fn call_with_dispatch(
        &self,
        ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(ToolEvent) + Sync),
        cancel: &std::sync::atomic::AtomicBool,
        _dispatch: &mut dyn FnMut(ToolCallback) -> ToolOutput,
    ) -> ToolOutput {
        self.call_streaming(ctx, args, on_event, cancel)
    }
    fn drain_notifications(&self) -> Vec<ToolEvent> {
        Vec::new()
    }
    /// Session-owned asynchronous work which is still running between controller turns.
    /// Stateless tools have no pending wake; process owners report their actual live state.
    fn has_live_work(&self) -> bool {
        false
    }
    /// Names of `bash`-started jobs this session still awaits. A turn ending while any remain
    /// waits for them (see `await_work`), and `finish completed` is refused.
    fn awaited_work(&self) -> Vec<String> {
        Vec::new()
    }
    /// Interrupt an active blocking operation without acquiring its session mutex.
    fn cancel_operation(&self) {}

    /// Give a child execution its own mutable tool session. Stateless tools and
    /// shared request-multiplexed services may return `None` to share this instance.
    /// Tools owning a process, REPL or workspace-rooted client must return a fresh
    /// instance so child cancellation cannot stop a parent or sibling operation.
    /// Edit write-through (LSP) follows the forked owner, so a child's edits never reach its parent's servers.
    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        None
    }

    /// Write explicit interactive keyboard input to a user-attached PTY owned by this tool.
    fn attach_user_pty(&self, _name: &str, _bytes: &[u8]) -> Result<usize, String> {
        Err("this tool does not own interactive PTY jobs".into())
    }
    /// Job table and the rendered screen of `attached` (a PTY job), for the interactive jobs panel.
    /// Tools without background jobs return `None`.
    fn jobs_view(&self, _attached: Option<&str>) -> Option<JobsView> {
        None
    }
    /// Transfer a foreground process to a managed background owner.
    #[doc(hidden)]
    fn adopt_foreground(&self, _job: crate::tools::shell::ForegroundJob) -> Result<String, String> {
        Err("this tool does not accept foreground processes".into())
    }

    /// Version of this tool's public extension lifecycle contract.
    fn extension_api_version(&self) -> u32 {
        EXTENSION_API_VERSION
    }

    /// Event names this tool is configured to observe. Empty by default.
    fn lifecycle_hooks(&self) -> &[String] {
        &[]
    }

    /// Key of the event stream this tool delivers through. Tools that share one stream (every
    /// tool of one MCP server) return the same key so an event is delivered once per stream.
    fn lifecycle_channel(&self) -> Option<&str> {
        None
    }

    /// Observes a sanitized lifecycle event. Implementations must not affect tool behavior.
    fn on_lifecycle_event(&self, _event: &LifecycleEvent) {}
}

/// Plain snapshot of background jobs for the interactive panel. Carries no tool-internal types, so
/// the UI depends only on this shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobsView {
    pub jobs: Vec<JobEntry>,
    /// Rendered rows of the attached PTY job's current screen (empty when none is attached).
    pub screen: Vec<String>,
    /// Cursor `(row, col)` on that screen.
    pub cursor: Option<(u16, u16)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobEntry {
    pub name: String,
    pub command: String,
    pub status: String,
    pub running: bool,
    /// Runs on a pseudo-terminal.
    pub terminal: bool,
}

#[derive(Default, Clone)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    notices: Vec<String>,
    mcp_instructions: Vec<(String, String)>,
    plugins: Option<Arc<super::plugin::Host>>,
    origins: BTreeMap<String, super::catalog::Origin>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn outcome(&self, name: &str, args: &Value, output: &ToolOutput) -> crate::runtime::Phase {
        let phase = self.get(name).map_or_else(
            || super::outcome::result_phase(name, args, output),
            |tool| tool.outcome(args, output),
        );
        super::outcome::normalize(phase, output)
    }

    pub fn render_call(&self, name: &str, args: &Value) -> super::presentation::Presentation {
        self.get(name).map_or_else(
            || super::presentation::render_call(name, args),
            |tool| tool.render_call(args),
        )
    }
    pub fn render_result(
        &self,
        name: &str,
        args: &Value,
        output: &ToolOutput,
    ) -> super::presentation::Presentation {
        self.get(name).map_or_else(
            || super::presentation::render_result(name, args, output),
            |tool| tool.render_result(args, output),
        )
    }

    pub fn has_live_work(&self) -> bool {
        self.tools.values().any(|tool| tool.has_live_work())
    }

    /// `(server, text)` instructions that connected MCP servers sent at initialize.
    pub fn mcp_instructions(&self) -> &[(String, String)] {
        &self.mcp_instructions
    }

    /// Share immutable definitions and multiplexed services (including MCP), but
    /// give every execution owner fresh workspace/process/native-tool state.
    pub fn fork_session(&self) -> Self {
        Self {
            tools: self
                .tools
                .iter()
                .map(|(name, tool)| {
                    (
                        name.clone(),
                        tool.fork_session().unwrap_or_else(|| Arc::clone(tool)),
                    )
                })
                .collect(),
            notices: Vec::new(),
            mcp_instructions: self.mcp_instructions.clone(),
            origins: self.origins.clone(),
            plugins: self
                .plugins
                .as_ref()
                .map(|host| Arc::new(host.fork_session())),
        }
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
            super::ssh::tools(config),
            super::notebook::tools(config),
            super::gh::tools(config),
            crate::computer::tools(config),
            crate::security::tools(config),
            super::browser::tools(config),
            super::kernel::tools(config),
            super::eval::tools(config),
            super::navigation::tools(config),
            super::autoresearch::tools(config),
            super::archive::tools(config),
        ];
        for tool in super::dap::tools(&config.dap) {
            if config
                .integrations
                .iter()
                .any(|name| name == "dap" || name == tool.name())
            {
                registry.register_native(tool);
            }
        }
        for tool in groups.into_iter().flatten() {
            if super::catalog::integration_enabled(tool.name(), &config.integrations) {
                registry.register_native(tool);
            }
        }
        let mcp = super::mcp::load(config);
        registry.register_native(mcp.inventory_tool(config));
        for tool in mcp.tools {
            registry.register_native(tool);
        }
        let host = Arc::new(super::plugin::Host::default());
        host.load_configured(config);
        registry.notices.extend(host.take_notices());
        if config.extensions.enabled {
            registry.plugins = Some(host);
        }
        registry.notices = registry.notices.into_iter().chain(mcp.notices).collect();
        registry.mcp_instructions = mcp.instructions;
        registry
    }

    /// Register an in-process lifecycle extension before the registry is shared with agents.
    pub fn register_plugin(
        &mut self,
        plugin: Arc<dyn super::plugin::Plugin>,
    ) -> anyhow::Result<()> {
        let host = self
            .plugins
            .get_or_insert_with(|| Arc::new(super::plugin::Host::default()));
        host.register(plugin)
    }

    /// Shared plugin dispatcher used by parent and child agent hooks.
    pub fn plugins(&self) -> Option<Arc<super::plugin::Host>> {
        self.plugins.clone()
    }
    /// Startup diagnostics (for example an MCP server that failed to connect). Returned once, then cleared.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let result = if self.contains(tool.name()) {
            self.replace(tool)
        } else {
            self.try_register(tool)
        };
        if let Err(error) = result {
            self.notices
                .push(format!("tool registration refused: {error:#}"));
        }
    }
    pub fn try_register(&mut self, tool: Arc<dyn Tool>) -> anyhow::Result<()> {
        super::catalog::validate_tool(tool.as_ref())?;
        anyhow::ensure!(
            !self.contains(tool.name()),
            "duplicate tool name {}",
            tool.name()
        );
        self.origins
            .insert(tool.name().to_owned(), super::catalog::Origin::Extension);
        self.tools.insert(tool.name().to_owned(), tool);
        Ok(())
    }
    pub fn replace(&mut self, tool: Arc<dyn Tool>) -> anyhow::Result<()> {
        super::catalog::validate_tool(tool.as_ref())?;
        let prior = self
            .get(tool.name())
            .ok_or_else(|| anyhow::anyhow!("tool is not registered"))?;
        anyhow::ensure!(
            tool.contract_revision() >= prior.contract_revision(),
            "tool semantic revision cannot go backwards"
        );
        self.origins
            .insert(tool.name().to_owned(), super::catalog::Origin::Extension);
        self.tools.insert(tool.name().to_owned(), tool);
        Ok(())
    }

    fn register_native(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_owned();
        let result = if self.contains(&name) {
            self.replace(tool)
        } else {
            self.try_register(tool)
        };
        if let Err(error) = result {
            self.notices
                .push(format!("native tool {name} refused: {error:#}"));
            return;
        }
        self.origins.insert(
            name.clone(),
            if name.starts_with("mcp__") {
                super::catalog::Origin::Mcp
            } else {
                super::catalog::Origin::Native
            },
        );
    }
    pub fn origin(&self, name: &str) -> Option<super::catalog::Origin> {
        self.origins.get(name).copied()
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
        let mut channels = std::collections::HashSet::new();
        for tool in self.tools.values().filter(|tool| {
            tool.extension_api_version() == EXTENSION_API_VERSION
                && tool
                    .lifecycle_hooks()
                    .iter()
                    .any(|hook| hook == &event.event)
        }) {
            if tool
                .lifecycle_channel()
                .is_some_and(|channel| !channels.insert(channel))
            {
                continue;
            }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tool.on_lifecycle_event(event)
            }));
        }
    }
}

#[cfg(test)]
mod tests;
