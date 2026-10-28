//! Pluggable tools: anything beyond the file/shell built-ins that is not tied to the agent loop's
//! own state (todo, memory, skills, task).
//!
//! A tool implements [`Tool`], and [`Registry::standard`] is the only place that lists them. The
//! agent asks the registry for schemas, approval requirements and execution, so adding a tool
//! touches the tool's own module plus one line in the registry.

use crate::config::types::Config;
use crate::tools::{ToolOutput, Workspace};
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
    fn accepts_historical_contract(
        &self,
        _recorded: &crate::tools::surface::contracts::Identity,
    ) -> bool {
        false
    }
    /// Deterministic projection of a settled old call; never executes the call or changes its journal.
    fn project_historical_call(
        &self,
        _recorded: &crate::tools::surface::contracts::Identity,
        _call: &crate::tools::surface::contracts::HistoricalCall,
    ) -> Option<crate::tools::surface::contracts::HistoricalCall> {
        None
    }

    /// Portable equivalents of tool-owned call and result renderer hooks.
    fn render_call(&self, args: &Value) -> crate::tools::surface::presentation::Presentation {
        crate::tools::surface::presentation::render_call(self.name(), args)
    }
    fn render_result(
        &self,
        args: &Value,
        output: &ToolOutput,
    ) -> crate::tools::surface::presentation::Presentation {
        let mut view = self.render_call(args);
        let result = crate::tools::surface::presentation::render_result(self.name(), args, output);
        view.facts.extend(result.facts);
        view
    }

    /// The completed execution's state; renderers cannot change it.
    fn outcome(&self, args: &Value, output: &ToolOutput) -> crate::runtime::Phase {
        crate::tools::surface::outcome::result_phase(self.name(), args, output)
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
        _adopt: &dyn Fn(crate::tools::exec::background::AdoptedJob) -> Result<String, String>,
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

    /// Whether this tool can serve a session rooted at `root`. Tools bound to one project's
    /// state (caches, change history) return false for other roots, such as an isolated worker's
    /// copy, so they are not offered where every call would fail.
    fn serves_root(&self, _root: &std::path::Path) -> bool {
        true
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
    /// Transfer a foreground shell command or native cell to a managed background owner.
    #[doc(hidden)]
    fn adopt_job(
        &self,
        _job: crate::tools::exec::background::AdoptedJob,
    ) -> Result<String, String> {
        Err("this tool does not accept foreground jobs".into())
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
    origins: BTreeMap<String, crate::tools::surface::catalog::Origin>,
    /// MCP tools mirrored from the connected servers; [`Registry::sync_mcp`] applies their changes.
    mcp: super::mcp::Live,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn outcome(&self, name: &str, args: &Value, output: &ToolOutput) -> crate::runtime::Phase {
        let phase = self.get(name).map_or_else(
            || crate::tools::surface::outcome::result_phase(name, args, output),
            |tool| tool.outcome(args, output),
        );
        crate::tools::surface::outcome::normalize(phase, output)
    }

    pub fn render_call(
        &self,
        name: &str,
        args: &Value,
    ) -> crate::tools::surface::presentation::Presentation {
        self.get(name).map_or_else(
            || crate::tools::surface::presentation::render_call(name, args),
            |tool| tool.render_call(args),
        )
    }
    pub fn render_result(
        &self,
        name: &str,
        args: &Value,
        output: &ToolOutput,
    ) -> crate::tools::surface::presentation::Presentation {
        self.get(name).map_or_else(
            || crate::tools::surface::presentation::render_result(name, args, output),
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
            mcp: self.mcp.clone(),
            plugins: self
                .plugins
                .as_ref()
                .map(|host| Arc::new(host.fork_session())),
        }
    }

    /// Every tool dial ships, configured from `config`. `native` carries the tool groups owned by
    /// layers above `tools` (computer use); they register with the built-in groups, before MCP
    /// servers. MCP servers connect here (concurrently, with a startup budget); a server that
    /// fails only adds a notice.
    pub fn standard(config: &Config, native: Vec<Arc<dyn Tool>>) -> Self {
        let mut registry = Self::new();
        let groups = [
            crate::tools::exec::background::tools(config),
            crate::tools::net::web::tools(config),
            crate::tools::code::lsp::tools(config),
            crate::tools::edit::ast::tools(config),
            crate::tools::vcs::git::tools(config),
            super::ssh::tools(config),
            crate::tools::fs::notebook::tools(config),
            crate::tools::vcs::gh::tools(config),
            crate::tools::vcs::glab::tools(config),
            native,
            crate::security::tools(config),
            crate::tools::net::browser::tools(config),
            crate::tools::net::image_gen::tools(config),
            super::report_tool_issue::tools(config),
            crate::tools::exec::eval::tools(config),
            crate::tools::code::navigation::tools(config),
            crate::tools::code::ida::tools(config),
            crate::tools::knowledge::autoresearch::tools(config),
            crate::tools::fs::archive::tools(config),
            crate::tools::knowledge::deps::tools(config),
        ];
        for tool in crate::tools::code::dap::tools(&config.dap) {
            if config
                .integrations
                .iter()
                .any(|name| name == "dap" || name == tool.name())
            {
                registry.register_native(tool);
            }
        }
        for tool in groups.into_iter().flatten() {
            if crate::tools::surface::catalog::integration_enabled(
                tool.name(),
                &config.integrations,
            ) {
                registry.register_native(tool);
            }
        }
        // Offered without an `integrations` entry when its endpoint has a key (`image.auto`).
        match crate::tools::net::image_gen::auto_tools(config) {
            Ok(tools) => {
                for tool in tools {
                    if !registry.contains(tool.name()) {
                        registry.register_native(tool);
                    }
                }
            }
            Err(error) if !registry.contains(crate::tools::net::image_gen::TOOL) => {
                registry.notices.push(format!(
                    "image_gen auto-registration unavailable: {}",
                    crate::storage::redact::redact_text(&format!("{error:#}"))
                ))
            }
            Err(_) => {}
        }
        let mut mcp = super::mcp::load(config);
        registry.register_native(mcp.inventory_tool(config));
        registry.mcp = std::mem::take(&mut mcp.live);
        registry.sync_mcp();
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
    /// Removes a tool and its origin; returns whether it was registered.
    pub fn remove(&mut self, name: &str) -> bool {
        self.origins.remove(name);
        self.tools.remove(name).is_some()
    }
    pub fn try_register(&mut self, tool: Arc<dyn Tool>) -> anyhow::Result<()> {
        crate::tools::surface::catalog::validate_tool(tool.as_ref())?;
        anyhow::ensure!(
            !self.contains(tool.name()),
            "duplicate tool name {}",
            tool.name()
        );
        self.origins.insert(
            tool.name().to_owned(),
            crate::tools::surface::catalog::Origin::Extension,
        );
        self.tools.insert(tool.name().to_owned(), tool);
        Ok(())
    }
    pub fn replace(&mut self, tool: Arc<dyn Tool>) -> anyhow::Result<()> {
        crate::tools::surface::catalog::validate_tool(tool.as_ref())?;
        let prior = self
            .get(tool.name())
            .ok_or_else(|| anyhow::anyhow!("tool is not registered"))?;
        anyhow::ensure!(
            tool.contract_revision() >= prior.contract_revision(),
            "tool semantic revision cannot go backwards"
        );
        self.origins.insert(
            tool.name().to_owned(),
            crate::tools::surface::catalog::Origin::Extension,
        );
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
                crate::tools::surface::catalog::Origin::Mcp
            } else {
                crate::tools::surface::catalog::Origin::Native
            },
        );
    }
    pub fn origin(&self, name: &str) -> Option<crate::tools::surface::catalog::Origin> {
        self.origins.get(name).copied()
    }
    /// Applies the MCP servers' current tool lists. Returns whether any registered tool changed,
    /// so the caller rebuilds the offered schemas. Removed tools leave the registry; redefined
    /// tools replace theirs.
    pub fn sync_mcp(&mut self) -> bool {
        let change = self.mcp.sync();
        self.notices.extend(change.notices);
        let changed = !(change.upserted.is_empty() && change.removed.is_empty());
        for name in change.removed {
            self.tools.remove(&name);
            self.origins.remove(&name);
        }
        for (name, tool) in change.upserted {
            self.origins
                .insert(name.clone(), crate::tools::surface::catalog::Origin::Mcp);
            self.tools.insert(name, tool);
        }
        changed
    }

    /// MCP events queued since the last call, for plugin subscribers (`mcp_notification`).
    pub fn take_mcp_events(&self) -> Vec<super::mcp::McpEvent> {
        self.mcp.take_events()
    }

    /// Whether `name` was an MCP tool its server withdrew and has not returned.
    pub fn is_retired_mcp(&self, name: &str) -> bool {
        self.mcp.is_retired(name)
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
