//! Tool exposure: offered tools, discovery, schemas and request records.
use crate::config;

use crate::agent::*;
use crate::config::types::{LEAN_AUTO_MAX_WINDOW, LeanLocal};

impl<'a> Agent<'a> {
    /// Local servers serve a context of their own choosing, often far below Dial's default: LM Studio
    /// loads on demand, llama.cpp splits `-c` across slots, Ollama defaults small and truncates silently.
    /// Their native APIs report the served size (see `catalog::loaded_context`); hosted and unknown
    /// servers are left alone, and an unloaded model keeps the last known cap rather than forgetting it.
    pub(in crate::agent) fn probe_context(&mut self) {
        if self.hermetic {
            return;
        }
        // The active route's configuration: a retained fallback is probed, not the configured primary.
        let config = &self.auth_config;
        let Some(endpoint) = &config.endpoint else {
            return;
        };
        let token = crate::llm::catalog::resolve_credential(
            &config.user_dir,
            &endpoint.base_url,
            endpoint.api_key_env.as_deref(),
        )
        .ok()
        .flatten();
        let loaded = crate::llm::catalog::loaded_context(
            &endpoint.base_url,
            &config.model,
            token.as_deref(),
            config.request_headers(),
        );
        // Ollama: a window this machine's memory cannot hold is planned at what it can.
        let loaded = match (loaded, self.local_affordable_context()) {
            (Some(loaded), Some(affordable)) => Some(loaded.min(affordable)),
            (loaded, affordable) => loaded.or(affordable),
        };
        if let Some(loaded) = loaded {
            let profile = (self.lean_local(), self.minimal_tools());
            self.context_cap = Some(loaded);
            // The loaded window can switch the lean profile or the local tier; re-render the prompt
            // once here.
            if (self.lean_local(), self.minimal_tools()) != profile {
                self.rebuild_system();
            }
        }
    }

    /// Minimal first-request offer: after the full set cannot fit the context window, on the small
    /// local tier, or after a local escalation (`local_profile`).
    const LEAN_TOOLS: [&str; 11] = [
        "read",
        "write",
        "edit",
        "bash",
        "grep",
        "glob",
        "todo",
        "finish",
        "check",
        "tool_search",
        "tool_load",
    ];

    /// Initial tool budget under the lean local profile. A configured 0 (unlimited) becomes this bound.
    const LEAN_INITIAL_TOOL_LIMIT: usize = 8;

    /// Lean local profile (`assistance.lean_local`): compact tool interface, a bounded initial tool
    /// budget, omitted optional prompt sections and, on a single-request server, no up-front delegation.
    /// It applies only when the main model is served locally (no endpoint, or a loopback or LAN host,
    /// as `Config::local_main_model` classifies it); hosted routes and hermetic runs are unchanged.
    ///
    /// `auto` decides from the planned window (`context_window`). That window changes only when the
    /// server probe reports a loaded size or the session switches; `probe_context` re-renders the system
    /// prompt when such a change flips the profile, so the prefix stays byte-stable within a turn.
    pub(in crate::agent) fn lean_local(&self) -> bool {
        if self.hermetic || !self.config.local_main_model() {
            return false;
        }
        match self.config.assistance.lean_local {
            LeanLocal::Off => false,
            LeanLocal::On => true,
            LeanLocal::Auto => self.context_window() <= LEAN_AUTO_MAX_WINDOW,
        }
    }

    /// Whether the selected provider serves one request at a time: `task.provider_concurrency` is unset
    /// or at most 1 (the slot limit `provider_slots` applies to a local server).
    fn single_request_server(&self) -> bool {
        self.config
            .task
            .provider_concurrency
            .get(&self.config.selection().provider)
            .copied()
            .is_none_or(|limit| limit <= 1)
    }

    /// The configured initial tool budget, capped under the lean local profile.
    fn initial_tool_limit(&self) -> usize {
        let configured = self.config.assistance.initial_tool_limit;
        match (self.lean_local(), configured) {
            (true, 0) => Self::LEAN_INITIAL_TOOL_LIMIT,
            (true, limit) => limit.min(Self::LEAN_INITIAL_TOOL_LIMIT),
            (false, limit) => limit,
        }
    }

    /// Discovery mode: offer a bounded core and let the model load specialists with `tool_search`/
    /// `tool_load`. Meant for small local models whose schema budget is tight; off by default
    /// because every discovery costs a full extra request and the tool list change invalidates the
    /// provider's prompt cache.
    pub(in crate::agent) fn tool_discovery(&self) -> bool {
        !self.hermetic
            && self.config.efficiency.enabled
            && (self.config.assistance.compact_tools || self.lean_local())
    }

    /// Most tools that discovery (`tool_search`, harness compatibility loads) keeps active at once in compact
    /// mode. Explicit `tool_load` pins live in `enabled_tools` and are never evicted by discovery.
    const DISCOVERED_TOOL_LIMIT: usize = 8;

    /// Working set of tools activated by discovery, least recently activated first. It is stored in the session
    /// control state, so a resumed session restores it; pins are restored separately from `enabled_tools`.
    pub(in crate::agent) fn discovered_tools(&self) -> Vec<String> {
        self.session.control_state()["discovered_tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }

    /// Capability eligibility is independent of whether a tool's schema is loaded yet.
    pub(in crate::agent) fn tool_eligible(&self, name: &str) -> bool {
        self.plan_tool_available(name)
            && self.modes.vibe_allows(name)
            && self.modes.launch.allows_tool(name)
    }

    /// Activate tools chosen by discovery. In compact discovery mode they join the bounded working set and the
    /// oldest entries are evicted; otherwise they keep the legacy `enabled_tools` behaviour. Pinned tools are not
    /// recorded as discovered.
    pub(in crate::agent) fn activate_tools(&mut self, names: &[String]) -> Result<()> {
        if let Some(name) = names.iter().find(|name| !self.tool_eligible(name)) {
            anyhow::bail!("tool {name} is unavailable under the current mode or tool allowlist");
        }
        let mut state = self.session.control_state();
        if self.tool_discovery() {
            let before = self.discovered_tools();
            let mut working = before.clone();
            for name in names {
                if self.enabled_tools.contains(name) {
                    continue;
                }
                working.retain(|existing| existing != name);
                working.push(name.clone());
            }
            let excess = working.len().saturating_sub(Self::DISCOVERED_TOOL_LIMIT);
            working.drain(..excess);
            if working == before {
                return Ok(());
            }
            state["discovered_tools"] = json!(working);
            self.session.set_control_state(state)?;
            self.rebuild_system();
            return Ok(());
        }
        let mut enabled = self.enabled_tools.clone();
        enabled.extend(names.iter().cloned());
        if enabled == self.enabled_tools {
            return Ok(());
        }
        let mut sorted: Vec<_> = enabled.iter().cloned().collect();
        sorted.sort();
        state["enabled_tools"] = json!(sorted);
        self.session.set_control_state(state)?;
        self.enabled_tools = enabled;
        self.rebuild_system();
        Ok(())
    }

    /// Integrations that earn a place in every request's schema when configured: the ones a
    /// coding turn reaches for without being told. Everything else in the catalog (browser,
    /// debug adapters, kernels, archives, MCP servers, and the overlapping specialists in
    /// [`tools::DEFERRED_BY_DEFAULT`] such as `ast_search`) is listed in the system prompt and
    /// loaded on demand. Fifteen `dap_*` schemas on every request would cost more than the round
    /// they save.
    const ESSENTIAL_INTEGRATIONS: &'static [&'static str] = &[
        // Clarifying questions are in the catalog only when `ask` is enabled; when it is, the
        // model should not have to load it first (a failed call is a confusing card).
        "ask",
        "find",
        "lsp",
        "bg",
        "git",
        "web_fetch",
        "web_search",
    ];

    /// An MCP server with at most this many tools is offered up front.
    const SMALL_MCP_SERVER_TOOLS: usize = 6;
    /// Cap across all auto-offered MCP tools so many small servers cannot bloat every request.
    const AUTO_MCP_TOOL_LIMIT: usize = 16;

    /// Tools of MCP servers small enough to offer up front, whole servers only, in server-name
    /// order, until the overall cap would be exceeded. Names look like `mcp__<server>__<tool>`.
    fn small_mcp_tools(catalog: &[String]) -> Vec<String> {
        let mut servers: std::collections::BTreeMap<&str, Vec<&String>> = Default::default();
        for name in catalog {
            if let Some((server, _)) = name.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
                servers.entry(server).or_default().push(name);
            }
        }
        let mut offered: Vec<String> = Vec::new();
        for tools in servers.values() {
            if tools.len() <= Self::SMALL_MCP_SERVER_TOOLS
                && offered.len() + tools.len() <= Self::AUTO_MCP_TOOL_LIMIT
            {
                offered.extend(tools.iter().map(|name| (*name).clone()));
            }
        }
        offered
    }

    /// Tools actually offered: the harness selection, explicitly loaded tools and the essential
    /// integrations present in the catalog. The loaders are offered only while the catalog holds
    /// something not offered. An operator-pinned `[harness] tools` list, a model-profile allowlist
    /// and hermetic runs restrict the offer; discovery mode compacts it to the initial budget.
    pub(in crate::agent) fn offered(&self) -> Vec<String> {
        self.offered_with(false)
    }

    /// Catalog tools not in the current offer, with the first sentence of each description, for
    /// the system prompt's loadable-tools list.
    pub(in crate::agent) fn discoverable_tools(&self, offered: &[String]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for name in self.catalog() {
            if offered.contains(&name)
                || name == "tool_search"
                || name == "tool_load"
                || !self.tool_eligible(&name)
            {
                continue;
            }
            let description = self
                .schemas(std::slice::from_ref(&name))
                .ok()
                .and_then(|schemas| {
                    schemas[0]["function"]["description"].as_str().map(|text| {
                        let first = text.split_inclusive(['.', '\n']).next().unwrap_or(text);
                        crate::agent::window::prompt::truncate_chars(
                            first.trim_end_matches(['.', '\n']),
                            110,
                        )
                    })
                })
                .unwrap_or_default();
            out.push((name, description));
        }
        // One per-adapter debug tool per installed adapter would be fifteen near-identical lines;
        // name the family once and list the adapters.
        let adapters: Vec<String> = out
            .iter()
            .filter_map(|(name, _)| name.strip_prefix("dap_").map(str::to_owned))
            .collect();
        if adapters.len() > 1 {
            out.retain(|(name, _)| !name.starts_with("dap_"));
            out.push((
                "dap_<adapter>".into(),
                format!(
                    "debug-adapter control for one installed adapter; load the exact name: {}",
                    adapters
                        .iter()
                        .map(|adapter| format!("dap_{adapter}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
            out.sort();
        }
        out
    }

    /// The tool list a child agent's allowlist is intersected with: the harness selection plus
    /// explicitly loaded tools, as before catalog-wide offers. Read-only children must not inherit
    /// every registered integration just because the parent can call it.
    pub(in crate::agent) fn offered_for_children(&self) -> Vec<String> {
        self.offered_with(true)
    }

    pub(in crate::agent) fn offered_with(&self, selection_only: bool) -> Vec<String> {
        let catalog = self.catalog();
        let discovery = self.tool_discovery();
        let discovered = self.discovered_tools();
        let loaded: std::collections::HashSet<String> = self
            .enabled_tools
            .iter()
            .chain(discovered.iter())
            .cloned()
            .collect();
        let pinned = self.config.harness.tools != config::tools::default_tools();
        let mut names: Vec<String> = self
            .version
            .harness
            .tools
            .iter()
            .chain(self.enabled_tools.iter())
            .chain(discovered.iter())
            .filter(|n| catalog.contains(n))
            .cloned()
            .collect();
        // The selected backend's memory tools are core: offered up front (including `reflect`
        // and `learn`, which the default harness list does not name) and kept by the compact,
        // lean and fallback budgets below. A pinned harness list still decides for itself.
        let memory_core = self.memory_core_tools(&catalog);
        if !pinned {
            names.extend(memory_core.iter().cloned());
        }
        // A pinned harness list and the selection lent to a child decide for themselves; only a
        // context overflow narrows them.
        let minimal = self.lean_tools || (!pinned && !selection_only && self.minimal_tools());
        if !(discovery || pinned || self.hermetic || minimal || selection_only) {
            names.extend(
                Self::ESSENTIAL_INTEGRATIONS
                    .iter()
                    .filter(|tool| catalog.iter().any(|name| name == *tool))
                    .map(|tool| (*tool).to_owned()),
            );
        }
        // Small MCP servers (a handful of tools) cost little schema and are otherwise invisible
        // to a model that never thinks to load them; large ones stay loadable on demand.
        if !(discovery || pinned || self.hermetic || minimal || selection_only) {
            names.extend(Self::small_mcp_tools(&catalog));
        }
        // `task` and `skill` are offered whenever the harness selects them; explicit profile
        // allowlists still gate them through the profile tool filter.
        for tool in ["find", "task", "skill"] {
            if catalog.iter().any(|name| name == tool) {
                names.push(tool.into());
            }
        }
        // Peer messaging is offered whenever `task` is, not only while a worker is live, so the
        // offer and the system prompt's loadable list do not change as workers start and stop.
        // `message` is a live-task tool rather than a catalog entry: dispatch and schema come from
        // `live_tasks`. With no live worker it lists an empty roster or reports an unknown recipient.
        if names.iter().any(|name| name == "task") {
            names.push("message".into());
        }
        // Completion is a control tool, including for older admitted harnesses that predate it.
        if !self.hermetic && catalog.iter().any(|name| name == "finish") {
            names.push("finish".into());
        }
        // The agent and the completion gate agree on "passing" only if the gate's checks are callable;
        // an operator-pinned list still decides for itself.
        if !self.hermetic && !pinned && catalog.iter().any(|name| name == "check") {
            names.push("check".into());
        }
        if catalog.iter().any(|name| name == "codebase_memory") {
            names.push("codebase_memory".into());
        }
        // Offered only while the operator has self-improvement on; otherwise not in the catalog.
        for tool in ["experience", "edit_preview"] {
            if catalog.iter().any(|n| n == tool) {
                names.push(tool.into());
            }
        }
        // Overlapping specialists stay loadable rather than costing schema bytes on every
        // request; an explicit load, a pinned selection, hermetic runs and the selection lent to
        // children keep them offered.
        if !(pinned || self.hermetic || selection_only) {
            names.retain(|name| {
                !tools::DEFERRED_BY_DEFAULT.contains(&name.as_str()) || loaded.contains(name)
            });
        }

        // The loaders are offered only while the catalog holds something the offer does not;
        // with nothing left to discover they would only add schema bytes.
        let hidden = catalog.iter().any(|name| {
            !names.iter().any(|offered| offered == name)
                && !matches!(name.as_str(), "tool_search" | "tool_load")
                && self.tool_eligible(name)
        });
        let loaders = discovery || pinned || minimal || selection_only || hidden;
        for tool in ["tool_search", "tool_load"] {
            if loaders && catalog.iter().any(|n| n == tool) {
                names.push(tool.into());
            } else if !loaders {
                names.retain(|name| name != tool);
            }
        }
        names.retain(|name| self.tool_eligible(name));
        if self.modes.goal.tool_offered() {
            names.push("goal".into());
        }
        names.sort();
        names.dedup();
        if discovery
            && self
                .config
                .active_model_profile()
                .is_none_or(|p| p.tools.is_none())
        {
            let mut required = loaded.clone();
            required.extend(memory_core.iter().cloned());
            crate::agent::model::assistance::compact(
                &mut names,
                &required,
                self.initial_tool_limit(),
                &self.assistance.scores,
            );
        }
        // The lean budget can trim `message` while `task` stays; peer messaging follows `task` (line 280).
        if names.iter().any(|name| name == "task") && !names.iter().any(|name| name == "message") {
            names.push("message".into());
            names.sort();
        }
        for tool in crate::agent::window::context_notes::TOOL_NAMES {
            if catalog.iter().any(|name| name == tool) && self.plan_tool_available(tool) {
                names.push(tool.to_owned());
            }
        }
        if (self.experimental_context_owner() || self.exploration_owner())
            && catalog.iter().any(|name| name == "session_search")
            && self.plan_tool_available("session_search")
        {
            names.push("session_search".to_owned());
        }
        if self.exploration.busy()
            || crate::agent::workflow::exploration::TOOL_NAMES
                .iter()
                .any(|tool| names.iter().any(|name| name == tool))
        {
            names.extend(
                crate::agent::workflow::exploration::TOOL_NAMES
                    .iter()
                    .filter(|tool| catalog.iter().any(|name| name == **tool))
                    .map(|tool| (*tool).to_owned()),
            );
        }
        names.sort();
        names.dedup();
        // `bg` is the only way to inspect or stop a command that outlived its bash call (and to run
        // servers). Re-add it after compaction so it is never dropped from the offer; exposing a
        // tool starts no process and integrations config still controls whether it is in the catalog.
        if catalog.iter().any(|name| name == "bg") && !names.iter().any(|name| name == "bg") {
            names.push("bg".into());
            names.sort();
        }
        // Late additions must obey the same restrictions as discovery and explicit loads.
        names.retain(|name| self.tool_eligible(name));
        // Peer messaging follows the `task` offer after mode and profile filters have settled.
        if !names.iter().any(|name| name == "task") {
            names.retain(|name| name != "message");
        }
        // A single-request local server queues every fan-out behind the running request, so the lean
        // profile does not offer `task` (and so `message`) up front; `tool_load` or the loadable list
        // still reaches them, and an explicit load keeps them offered.
        if self.lean_local() && self.single_request_server() && !loaded.contains("task") {
            names.retain(|name| name != "task" && name != "message");
        }
        if minimal {
            names.retain(|name| {
                Self::LEAN_TOOLS.contains(&name.as_str())
                    || loaded.contains(name)
                    || memory_core.contains(name)
            });
        }
        // The plan exit is a control operation, not an optional schema-budget candidate.
        if self.plan_mode && self.tool_eligible("exit_plan") {
            names.push("exit_plan".into());
            names.sort();
            names.dedup();
        }
        names
    }

    /// The selected memory backend's canonical tools present in the catalog: the runtime's
    /// `recall`/`retain`/`reflect`/`learn`, or provenance's registered `recall`/`retain`.
    fn memory_core_tools(&self, catalog: &[String]) -> Vec<String> {
        let names: Vec<&str> = match (&self.memory_runtime, &self.memory) {
            (Some(runtime), _) => runtime
                .offered_names()
                .into_iter()
                .filter(|name| crate::storage::memory_backend::TOOL_NAMES.contains(name))
                .collect(),
            (None, Some(_)) => vec!["recall", "retain"],
            (None, None) => Vec::new(),
        };
        names
            .into_iter()
            .filter(|name| catalog.iter().any(|entry| entry == name))
            .map(str::to_owned)
            .collect()
    }

    pub(in crate::agent) fn schemas(&self, names: &[String]) -> Result<Vec<Value>> {
        let mode = self.edit_mode();
        let mut schemas = Vec::new();
        for name in names {
            schemas.push(match name.as_str() {
                "context_notes" | "new_context" => crate::agent::window::context_notes::schema(name).expect("known context tool"),
                "checkpoint" | "rewind" => crate::agent::workflow::exploration::schema(name).expect("known exploration tool"),
                "session_search" if self.experimental_context_owner() || self.exploration_owner() => session::session_search_tool::schema_owned(),
                n if crate::storage::memory_backend::schema(&self.config.memory, n).is_some() =>
                    crate::storage::memory_backend::schema(&self.config.memory, n).expect("selected memory tool has a schema"),
                n if config::tools::BUILTIN.contains(&n) => tools::schemas(&[n], mode)?.remove(0),
                n if self.ext.contains(n) => self.ext.get(n).map(|tool| tool.schema()).context("extension tool vanished")?,
                "experience" => crate::agent::model::assistance::schema(),
                "edit_preview" => crate::tools::edit::preview::schema(),
                "tts" => crate::voice::tool::schema(),
                "todo" => crate::agent::workflow::todo::schema(),
                "finish" => crate::agent::verify::completion::schema(),
                "check" => crate::agent::verify::check_tool::schema(),
                "revert_edit" => crate::agent::verify::revert_edit::schema(),
                "goal" => modes::goal::schema(),
                "ask" => crate::agent::workflow::questions::schema(),
                "task" => crate::agent::workers::subagents::schema(self.config),
                "message" => crate::agent::workers::live_tasks::message_schema(),
                "solve" => crate::agent::workflow::solve::schema(),
                "escalate" => crate::agent::workflow::solve::escalation_schema(),
                "skill" => skills::skill_tool::schema(),
                "skill_manage" => skills::manage_tool::schema(),
                "session_search" => session::session_search_tool::schema(),
                "artifact_read" => crate::storage::artifact::schema(),
                "working_state" => crate::agent::workflow::evidence::working_schema(),
                "tool_search" => json!({"type":"function","function":{"name":"tool_search","description":"Rank eligible tools by capability or exact name and enable up to three strongest matches for the next model round. Returns names, descriptions and activation status. Never executes tools, grants permission or enables same-batch calls; do not rediscover tools already offered.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}}),
                "tool_load" => json!({"type":"function","function":{"name":"tool_load","description":"Explicitly enable or unload exact eligible tool names. Search already enables its strongest matches; use this for exact control. Changes take effect next round, never in this batch, and grant no permissions.","parameters":{"type":"object","properties":{"names":{"type":"array","maxItems":32,"items":{"type":"string"}},"unload":{"type":"array","maxItems":32,"items":{"type":"string"}}},"additionalProperties":false}}}),
                "exit_plan" => json!({"type":"function","function":{"name":"exit_plan","description":"Ask the user to approve this plan and leave plan mode.","parameters":{"type":"object","properties":{"plan":{"type":"string","minLength":1}},"required":["plan"],"additionalProperties":false}}}),
                other => {
                    let configured = self.config.tools.get(other);
                    let fallback = if configured.is_some_and(|tool| tool.parameters.is_some()) {
                        "Operator-approved tool. Pass the request as a JSON object matching its parameters."
                    } else {
                        "Operator-approved tool. Pass the request as `input`."
                    };
                    let description = configured
                        .map(|tool| tool.description.trim())
                        .filter(|text| !text.is_empty())
                        .unwrap_or(fallback);
                    let parameters = configured
                        .and_then(|tool| tool.parameters.clone())
                        .unwrap_or_else(|| json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}));
                    json!({"type":"function","function":{"name":other,"description":description,"parameters":parameters}})
                }
            });
            if self.tool_discovery() {
                let description = match name.as_str() {
                    "write" => Some(
                        "Write complete UTF-8 source to a workspace path; guarded syntax and normal approvals apply.",
                    ),
                    "bash" => Some(
                        "Run a shell command. Runs in the project root by default; set `cwd` (relative) instead of prefixing `cd`. Commands run with normal approvals; inspect exit status. Long commands (servers, watchers, builds) may keep running: pass `background: true` to start one now, or it moves to background automatically after ~15s. Check or stop background jobs with `bg`.",
                    ),
                    "glob" => Some("Find workspace paths with an ignore-aware glob."),
                    "grep" => Some(
                        "Search workspace source with a regular expression; provide path scope when known.",
                    ),
                    _ => None,
                };
                if let Some(description) = description {
                    schemas.last_mut().unwrap()["function"]["description"] = json!(description);
                }
            }
            if self.plan_mode {
                let schema = schemas.last_mut().expect("schema was just inserted");
                let properties = &mut schema["function"]["parameters"]["properties"];
                match name.as_str() {
                    "bg" => properties["action"]["enum"] = json!(["list", "output", "wait"]),
                    "ast_edit" => {
                        properties["dry_run"] =
                            json!({"type":"boolean","enum":[true],"default":true})
                    }
                    "task" => {
                        let agents: Vec<&str> = crate::config::subagents::AGENTS
                            .iter()
                            .filter(|a| !a.mutates)
                            .map(|a| a.name)
                            .chain(self.config.subagents.keys().map(String::as_str))
                            .collect();
                        properties["agent"]["enum"] = json!(agents);
                        properties["tasks"]["items"]["properties"]["agent"]["enum"] = json!(agents);
                    }
                    _ => {}
                }
            }
        }
        crate::tools::surface::catalog::Catalog::from_schemas(&schemas, &self.ext)?;
        Ok(schemas)
    }

    pub(in crate::agent) fn record(&self, step: u32, kind: &str, mut data: Value) -> Result<()> {
        if let Some(object) = data.as_object_mut() {
            object.insert(
                "wall_ms".into(),
                json!(self.workflow_started.elapsed().as_millis() as u64),
            );
        }
        self.workflow_metrics.borrow_mut().observe(kind, &data);
        if let Some(probe) = &self.step_probe {
            let risk = probe.risk(&crate::learn::probe::features(kind, step, &data));
            if let Some(object) = data.as_object_mut() {
                object.insert("predicted_failure_risk".into(), json!(risk));
            }
        }
        if let (Some(store), Some((id, domain, task))) = (self.store, &self.trajectory) {
            store.event(&Event {
                trajectory: *id,
                version: self.version.id,
                domain: domain.clone(),
                task: task.clone(),
                step,
                kind: kind.into(),
                data,
                score: None,
            })?;
        }
        Ok(())
    }

    pub(in crate::agent) fn save_request_policy(
        &mut self,
        selection: Option<Value>,
        result: Value,
        ui: &mut dyn Ui,
    ) -> Result<()> {
        if self.hermetic {
            return Ok(());
        }
        let mut control = self.session.control_state();
        if let Some(selection) = selection {
            let mut history = control["adaptive_history"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if control["adaptive_policy"].is_object() {
                history.push(control["adaptive_policy"].clone());
            }
            if history.len() > 12 {
                history.drain(..history.len() - 12);
            }
            control["adaptive_history"] = json!(history);
            control["adaptive_policy"] = json!({"selection":selection,"model":self.config.model,"version":self.version.id,"result":result});
        } else {
            control["adaptive_policy"]["result"] = result;
        }
        ui.request_policy(&control);
        self.session.set_control_state(control)
    }

    /// Whether a read result is still shown in full: it exists in the session and was not superseded or pruned.
    pub(in crate::agent) fn holds_full_result(&self, call_id: &str) -> bool {
        self.superseded.holds_full(call_id)
            && self
                .session
                .messages()
                .iter()
                .any(|message| message["tool_call_id"].as_str() == Some(call_id))
    }

    /// Append to the durable session; training capture happens at the inference boundary.
    pub(in crate::agent) fn push(&mut self, message: Value) -> Result<()> {
        self.session.append(message)?;
        if let Some(memory) = &self.memory_runtime {
            memory.resume(memory_authority(&self.session, 0));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod default_offer_tests;
