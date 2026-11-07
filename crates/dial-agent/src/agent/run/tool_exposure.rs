//! Tool exposure: offered tools, discovery, schemas and request records.
use crate::config;

use crate::agent::*;
use crate::config::types::{LEAN_AUTO_MAX_WINDOW, LeanLocal};
use std::collections::BTreeSet;

mod task_context;
mod task_routing;
pub(in crate::agent) use task_routing::ToolRouting;

impl<'a> Agent<'a> {
    /// Initial built-in schemas stay below this bound on lean and local model routes.
    const CORE_SCHEMA_BYTES: usize = 12 * 1024;
    /// Hosted routes ([`Self::hosted_route`]) have room for delegation and job control up front.
    const HOSTED_CORE_SCHEMA_BYTES: usize = 24 * 1024;
    /// The stable first-request offer, in priority order: each tool joins while it fits the route's
    /// budget. `bg` is the only way to wait for or stop a command that became a background job.
    /// Basic workspace operations precede questions, which precede optional execution helpers.
    /// `task` (and `message`, which follows it) is offered only on hosted routes. `eval` joins only
    /// when containment is available and a cell runtime resolves ([`Self::automatic_eval_ready`]).
    const CORE_TOOLS: &[&str] = &[
        "tool_search",
        "tool_load",
        "finish",
        "read",
        "edit",
        "write",
        "bash",
        "grep",
        "glob",
        "ask",
        "todo",
        "verify",
        "eval",
        "bg",
        "task",
    ];

    /// The main model is served by a hosted provider: not a local route (`local_tier`) and not a
    /// hermetic run. Hosted routes get the larger core budget and the full discovery descriptions.
    pub(in crate::agent) fn hosted_route(&self) -> bool {
        !self.hermetic && self.local_tier().is_none()
    }

    /// Byte budget of the stable core offer on this route.
    pub(in crate::agent) fn core_schema_budget(&self) -> usize {
        if self.hosted_route() {
            Self::HOSTED_CORE_SCHEMA_BYTES
        } else {
            Self::CORE_SCHEMA_BYTES
        }
    }

    pub(in crate::agent) fn stable_tool_offer(&self) -> bool {
        !self.hermetic
            && self.config.assistance.compact_tools
            && self.config.harness.tools == config::tools::default_tools()
    }
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
        "verify",
        "tool_search",
        "tool_load",
    ];

    /// Initial tool budget under the lean local profile. A configured 0 (unlimited) becomes this bound.
    const LEAN_INITIAL_TOOL_LIMIT: usize = 8;

    /// Helpers for a long, steerable session that an unattended run (`Launch::unattended`) keeps
    /// loadable: delegation (`message` follows `task`), the compaction-surviving working record,
    /// progress bookkeeping, `find` (which `grep`/`glob` cover), cross-session `memory`, background
    /// job control (`bash` backgrounds on its own and names `bg` as loadable), the optional
    /// structured `finish` (a plain answer ends the turn), on-demand `verify` (the completion gate
    /// still runs its checks) and `git` (`bash` covers it). In native Harbor print runs (94
    /// requests) none of these was called. A run that needs one pays a single `tool_load` round.
    const UNATTENDED_DEFERRED: &[&str] = &[
        "task",
        "working_state",
        "todo",
        "find",
        "memory",
        "bg",
        "finish",
        "verify",
        "git",
    ];

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

    /// Compact schema descriptions and discovery; the default stable offer uses explicit loads
    /// on local and hosted routes. Legacy noncompact profiles retain bounded automatic discovery.
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
        if self.stable_tool_offer() {
            anyhow::ensure!(
                names.is_empty(),
                "Specialist schemas are loaded only by tool_load; tool_search describes them without changing the offer"
            );
            return Ok(());
        }
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

    /// The one owner of explicit tool selection: `tool_load`, `tool_search` with `load`, and the
    /// controller's `bg` load after a command became a background job. Every requested name must
    /// be in the catalog and eligible under the current mode and allowlists; the selection persists
    /// in the session and takes effect on the next request. Selecting grants no permission.
    pub(in crate::agent) fn select_tools(
        &mut self,
        requested: &[String],
        unload: &[String],
    ) -> std::result::Result<(), String> {
        if let Some(name) = requested.iter().find(|name| unload.contains(name)) {
            return Err(format!(
                "tool_load cannot load and unload {name} in the same call; no selection changed"
            ));
        }
        if requested
            .iter()
            .chain(unload)
            .any(|name| !self.ext.contains(name) && self.ext.mcp_connecting().is_some())
        {
            self.ext.adopt_mcp(true);
        }
        let catalog = self.catalog();
        if (requested.is_empty() && unload.is_empty())
            || requested
                .iter()
                .chain(unload)
                .any(|name| !catalog.contains(name))
        {
            return Err("tool_load contains an unavailable tool; use tool_search first".into());
        }
        if let Some(name) = requested.iter().find(|name| !self.tool_eligible(name)) {
            return Err(format!(
                "tool {name} is unavailable under the current mode or tool allowlist"
            ));
        }
        let mut enabled = self.enabled_tools.clone();
        enabled.extend(requested.iter().cloned());
        for name in unload {
            enabled.remove(name);
        }
        let mut discovered = self.discovered_tools();
        discovered.retain(|name| !unload.contains(name) && !enabled.contains(name));
        let mut state = self.session.control_state();
        let mut sorted: Vec<_> = enabled.iter().cloned().collect();
        sorted.sort();
        state["enabled_tools"] = json!(sorted);
        state["discovered_tools"] = json!(discovered);
        self.session
            .set_control_state(state)
            .map_err(|error| format!("persisting enabled tools: {error:#}"))?;
        self.enabled_tools = enabled;
        self.tool_routing.unload(unload);
        self.rebuild_system();
        Ok(())
    }

    /// Mode tools an `allowed_tools` surface always sends: their offer follows session state (plan
    /// mode, an active goal), so they stay in `tools` and only `tool_choice` changes.
    fn mode_tools(&self) -> Vec<String> {
        ["exit_plan", "goal"]
            .into_iter()
            .filter(|name| self.tool_eligible(name))
            .map(str::to_owned)
            .collect()
    }

    /// Remember that the result of `call_id` made `names` callable, so a route that defers
    /// tools ([`Self::wire_tool_surface`]) can return them as `tool_reference` blocks instead of
    /// the result text. Only a selection that loaded every name, none of them already sent
    /// loaded, qualifies: the references then say everything the text said. Grants nothing.
    pub(in crate::agent) fn note_tool_references(
        &mut self,
        call_id: &str,
        names: &[String],
    ) -> Result<()> {
        if call_id.is_empty() || names.is_empty() || !self.stable_tool_offer() {
            return Ok(());
        }
        let offered = self.offered();
        let core = self.stable_core_tools();
        // A known readiness blocker is part of the result text, so that result keeps its text.
        if names.iter().any(|name| {
            !offered.contains(name)
                || core.contains(name)
                || !self.tool_readiness(name).automatic_candidate()
        }) {
            return Ok(());
        }
        let mut state = self.session.control_state();
        if !state["tool_references"].is_object() {
            state["tool_references"] = json!({});
        }
        state["tool_references"][call_id] = json!(names);
        self.session.set_control_state(state)
    }

    /// The request's `tools` on a route that keeps them byte-stable ([`crate::llm::tool_offer`]).
    /// `request.tools` arrives as the callable contract (the turn's `offer` after request-scoped
    /// narrowing); admission, accounting and compaction use that contract, never this surface,
    /// and a call to an uncallable tool is refused as before.
    ///
    /// - `Deferred` (first-party Anthropic): the stable core is sent loaded, callable or not.
    ///   Every other eligible catalog tool, offered tool or tool referenced by a kept loading
    ///   result is sent with `defer_loading`, so a `tool_load` changes no definition: its result
    ///   returns `tool_reference` blocks instead. An offered tool without a kept reference (a
    ///   mode tool, a controller load, a compacted result) is sent loaded so it stays visible.
    /// - `AllowedTools` (OpenAI Responses): the full offer plus the mode tools; narrowing and mode
    ///   state change only `tool_choice`.
    /// - `Plain`, hermetic runs and (for `Deferred`) operator-pinned offers: unchanged.
    pub(in crate::agent) fn wire_tool_surface(
        &self,
        exposure: crate::llm::tool_offer::Exposure,
        offer: &[Value],
        request: &mut Chat,
    ) -> Result<()> {
        use crate::llm::tool_offer::{self, Exposure, Mark};
        if self.hermetic
            || request.tools.is_empty()
            || match exposure {
                Exposure::Plain => true,
                Exposure::Deferred => !self.stable_tool_offer(),
                Exposure::AllowedTools => false,
            }
        {
            return Ok(());
        }
        let name_of = |schema: &Value| schema["function"]["name"].as_str().map(str::to_owned);
        let callable: BTreeSet<String> = request.tools.iter().filter_map(name_of).collect();
        let offered: std::collections::BTreeMap<String, &Value> = offer
            .iter()
            .filter_map(|schema| Some((name_of(schema)?, schema)))
            .collect();
        let schema_of = |name: &str| -> Option<Value> {
            match offered.get(name) {
                Some(schema) => Some((*schema).clone()),
                None => self
                    .schemas(&[name.to_owned()])
                    .ok()
                    .and_then(|mut schemas| schemas.pop()),
            }
        };
        if exposure == Exposure::AllowedTools {
            let modes = self.mode_tools();
            let names: BTreeSet<&str> = offered
                .keys()
                .map(String::as_str)
                .chain(modes.iter().map(String::as_str))
                .collect();
            request.tools = names
                .into_iter()
                .filter_map(|name| {
                    let mut schema = schema_of(name)?;
                    tool_offer::mark(
                        &mut schema,
                        Mark {
                            callable: callable.contains(name),
                            deferred: false,
                        },
                    );
                    Some(schema)
                })
                .collect();
            return Ok(());
        }
        // The mode tools are not sent loaded here: Messages has no verified way to show a deferred
        // tool without a tool result, and an always-visible `goal` invites calls the harness must
        // refuse. They follow their state like any offered tool, as before.
        let core: BTreeSet<String> = self.stable_core_tools().into_iter().collect();
        // Loading results still in the projected history, with the names each one loaded.
        let recorded = self.session.control_state()["tool_references"].clone();
        let pending: Vec<(usize, Vec<String>)> = request
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message["role"] == "tool" && message["is_error"] != true)
            .filter_map(|(index, message)| {
                let names: Vec<String> = recorded[message["tool_call_id"].as_str()?]
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                (!names.is_empty()).then_some((index, names))
            })
            .collect();
        let mut names: BTreeSet<String> = core.clone();
        names.extend(offered.keys().cloned());
        names.extend(
            self.catalog()
                .into_iter()
                .filter(|name| self.tool_eligible(name)),
        );
        names.extend(pending.iter().flat_map(|(_, names)| names.iter().cloned()));
        let surface: Vec<(String, Value)> = names
            .into_iter()
            .filter_map(|name| {
                let schema = schema_of(&name)?;
                Some((name, schema))
            })
            .collect();
        let sent: BTreeSet<String> = surface.iter().map(|(name, _)| name.clone()).collect();
        // A result is shown by reference only when every tool it loaded is sent deferred.
        let effective: Vec<&(usize, Vec<String>)> = pending
            .iter()
            .filter(|(_, names)| {
                names
                    .iter()
                    .all(|name| sent.contains(name) && !core.contains(name))
            })
            .collect();
        let referenced: BTreeSet<&str> = effective
            .iter()
            .flat_map(|(_, names)| names.iter().map(String::as_str))
            .collect();
        let mut tools = Vec::with_capacity(surface.len());
        for (name, mut schema) in surface {
            let deferred = !core.contains(&name)
                && (referenced.contains(name.as_str()) || !offered.contains_key(&name));
            tool_offer::mark(
                &mut schema,
                Mark {
                    callable: callable.contains(&name),
                    deferred,
                },
            );
            tools.push(schema);
        }
        for (index, names) in effective {
            request.messages[*index][tool_offer::REFERENCES] = json!(names);
        }
        request.tools = tools;
        Ok(())
    }

    /// Offer `bg` for the next request after a command became a background job, so waiting for
    /// or stopping it costs no `tool_load` round. The stable offer selects it through
    /// [`Self::select_tools`]; legacy discovery activates it. True when `bg` is then offered.
    pub(in crate::agent) fn load_bg_for_background_job(&mut self) -> bool {
        if !self.bg_loadable() {
            return false;
        }
        let bg = ["bg".to_owned()];
        let loaded = if self.stable_tool_offer() {
            self.select_tools(&bg, &[]).is_ok()
        } else {
            self.activate_tools(&bg).is_ok()
        };
        loaded && self.offered().iter().any(|name| name == "bg")
    }

    /// Integrations that earn a place in every request's schema when configured: the ones a
    /// coding turn reaches for without being told. Everything else in the catalog (browser,
    /// debug adapters, kernels, archives, MCP servers, and the specialists in
    /// [`tools::DEFERRED_BY_DEFAULT`] such as `lsp`, `git` and `ast_search`) is listed in the system
    /// prompt and loaded on demand. Fifteen `dap_*` schemas on every request would cost more than
    /// the round they save.
    const ESSENTIAL_INTEGRATIONS: &'static [&'static str] = &[
        // Clarifying questions are in the catalog only when `ask` is enabled; when it is, the
        // model should not have to load it first (a failed call is a confusing card).
        "ask", "find", "bg", "eval",
    ];

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
            // Known blockers (no image route, unsupported host...) stay out of the prompt list;
            // `tool_search` still finds them and reports the readiness reason.
            if offered.contains(&name)
                || name == "tool_search"
                || name == "tool_load"
                || !self.tool_eligible(&name)
                || !self.tool_readiness(&name).automatic_candidate()
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

    /// `bg` is in the catalog and eligible but not in the current offer, and `tool_load` is, so
    /// text that names `bg` must say it is a Dial tool to load first.
    pub(in crate::agent) fn bg_loadable(&self) -> bool {
        if self.stable_tool_offer() {
            // The core offer includes `bg` wherever the route's budget allows it.
            return self.catalog().iter().any(|name| name == "bg")
                && self.tool_eligible("bg")
                && !self.offered().iter().any(|name| name == "bg");
        }
        let offered = self.offered();
        !self.hermetic
            && self.catalog().iter().any(|name| name == "bg")
            && self.tool_eligible("bg")
            && !offered.iter().any(|name| name == "bg")
            && offered.iter().any(|name| name == "tool_load")
    }

    /// The tool list a child agent's allowlist is intersected with: the harness selection plus
    /// explicitly loaded tools, as before catalog-wide offers. Read-only children must not inherit
    /// every registered integration just because the parent can call it.
    pub(in crate::agent) fn offered_for_children(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .offered_with(true)
            .into_iter()
            .filter(|name| name != "side_request")
            .collect();
        let pinned = self.config.harness.tools != config::tools::default_tools();
        if !pinned && !self.hermetic && !self.lean_tools {
            let catalog = self.catalog();
            for tool in ["lsp", "git", "ast_search", "code_history", "dep_source"] {
                if catalog.iter().any(|name| name == tool) && !out.iter().any(|name| name == tool) {
                    out.push(tool.into());
                }
            }
        }
        out
    }

    pub(in crate::agent) fn offered_with(&self, selection_only: bool) -> Vec<String> {
        self.offered_with_eval_readiness(selection_only, || self.automatic_eval_ready())
    }

    /// An automatic (not explicitly loaded) `eval` offer prepares containment and needs at least
    /// one cell runtime: an operator interpreter or the embedded JavaScript engine.
    fn automatic_eval_ready(&self) -> bool {
        self.config.eval.auto_offer
            && tools::exec::eval::prepare_containment().is_ok()
            && !tools::exec::eval::installed_languages(self.workspace.root(), self.config)
                .is_empty()
    }

    fn offered_with_eval_readiness(
        &self,
        selection_only: bool,
        prepare_eval: impl Fn() -> bool,
    ) -> Vec<String> {
        let catalog = self.catalog();
        if self.stable_tool_offer() && !selection_only {
            let mut names = self.stable_core_tools_with(&prepare_eval);
            names.extend(
                self.enabled_tools
                    .iter()
                    .filter(|name| catalog.contains(name) && self.tool_eligible(name))
                    .cloned(),
            );
            if self.plan_mode && self.tool_eligible("exit_plan") {
                names.push("exit_plan".into());
            }
            if self.modes.goal.tool_offered() {
                names.push("goal".into());
            }
            names.sort();
            names.dedup();
            return names;
        }
        let discovery = self.tool_discovery();
        let request_tools: Vec<String> = if selection_only || self.lean_tools {
            Vec::new()
        } else {
            self.tool_routing
                .retained
                .iter()
                .chain(&self.tool_routing.selected)
                .cloned()
                .collect()
        };
        let discovered = self.discovered_tools();
        let loaded: std::collections::HashSet<String> = self
            .enabled_tools
            .iter()
            .chain(discovered.iter())
            .chain(request_tools.iter())
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
            .chain(request_tools.iter())
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
        // Only automatic offers prepare containment and require an installed cell runtime.
        // Explicit loads/discovery and operator/child selections retain the control actions even
        // when new cell execution is unavailable. Apply this to retained task routing as well as
        // the essential integration above.
        if !(pinned || self.hermetic || selection_only)
            && !self.enabled_tools.contains("eval")
            && !discovered.iter().any(|name| name == "eval")
        {
            names.retain(|name| name != "eval" || prepare_eval());
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
        let verify = crate::agent::verify::verify_tool::NAME;
        if !self.hermetic && !pinned && catalog.iter().any(|name| name == verify) {
            names.push(verify.into());
        }
        if !self.hermetic && !pinned && catalog.iter().any(|name| name == "peers") {
            names.push("peers".into());
        }
        // `codebase_memory`, `experience` and `edit_preview` are never offered up front: they stay
        // in the catalog as loadable specialists, so a turn that needs one pays one `tool_load`.
        // Specialists most turns never call stay loadable rather than costing schema bytes on every
        // request; an explicit load, a pinned selection, hermetic runs and the selection lent to
        // children keep them offered. An unattended run also defers the long-session helpers,
        // unless the operator prefers delegation.
        let unattended = |name: &str| {
            self.modes.launch.unattended
                && Self::UNATTENDED_DEFERRED.contains(&name)
                && !(name == "task" && self.config.task.prefer_subagents)
        };
        // Repository work should not spend a model request loading its native VCS tool.
        // Respect pinned/hermetic selections and small-context budgets.
        let repository_git = !pinned
            && !self.hermetic
            && !minimal
            && self
                .workspace
                .root()
                .ancestors()
                .any(|dir| dir.join(".git").exists());
        if repository_git && catalog.iter().any(|name| name == "git") {
            names.push("git".into());
        }
        let deferred = |name: &str| {
            !(pinned || self.hermetic || selection_only)
                && (tools::DEFERRED_BY_DEFAULT.contains(&name) || unattended(name))
                && !(name == "git" && repository_git && !unattended(name))
                && !loaded.contains(name)
        };
        names.retain(|name| !deferred(name));

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
            && !self.stable_tool_offer()
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
        // The notes-backed context window needs raw-history recovery up front; investigation
        // checkpoints only once exploration is under way (its tools join the offer just below).
        let exploring = self.exploration.busy()
            || crate::agent::workflow::exploration::TOOL_NAMES
                .iter()
                .any(|tool| names.iter().any(|name| name == tool));
        if (self.config.compaction.snapcompact.overlay
            || crate::agent::compact::snapcompact::views::available_in(&self.session)
            || self.experimental_context_owner()
            || (self.exploration_owner() && (exploring || !deferred("session_search"))))
            && catalog.iter().any(|name| name == "session_search")
            && self.plan_tool_available("session_search")
        {
            names.push("session_search".to_owned());
        }
        if exploring {
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
        if catalog.iter().any(|name| name == "bg")
            && !names.iter().any(|name| name == "bg")
            && !deferred("bg")
        {
            names.push("bg".into());
            names.sort();
        }
        // Side reasoning is in the default harness selection but deferred like other specialists;
        // once loaded (or pinned, hermetic, or lent to children) it survives the discovery
        // compaction above. Mode/profile restrictions and the minimal-context budget still apply.
        if catalog.iter().any(|name| name == "side_request")
            && !deferred("side_request")
            && !names.iter().any(|name| name == "side_request")
        {
            names.push("side_request".into());
            names.sort();
        }
        // Late additions must obey the same restrictions as discovery and explicit loads.
        // Independent-session coordination belongs to the foreground controller, not its children.
        if selection_only {
            names.retain(|name| name != "peers");
        }
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

    pub(in crate::agent) fn stable_core_tools(&self) -> Vec<String> {
        self.stable_core_tools_with(&|| self.automatic_eval_ready())
    }

    fn stable_core_tools_with(&self, eval_ready: &dyn Fn() -> bool) -> Vec<String> {
        let catalog = self.catalog();
        let mut core = Vec::new();
        let mut bytes = 2;
        let limit = self.initial_tool_limit();
        let budget = self.core_schema_budget();
        // `bash` is counted with its note that `bg` is loadable until `bg` itself joins.
        let schema_size = |name: &str| self.budgeted_schema_bytes(name, true).unwrap_or(budget);
        let bg_note_bytes = || {
            schema_size("bash")
                .saturating_sub(self.budgeted_schema_bytes("bash", false).unwrap_or(budget))
        };
        for &name in Self::CORE_TOOLS {
            if !catalog.iter().any(|entry| entry == name) || !self.tool_eligible(name) {
                continue;
            }
            if name == "eval" && !eval_ready() {
                continue;
            }
            // Delegation is a hosted-route default; an unattended run defers it unless the
            // operator prefers subagents. Peer messaging joins with it (one budget decision).
            let delegation = name == "task";
            if delegation
                && (!self.hosted_route()
                    || (self.modes.launch.unattended && !self.config.task.prefer_subagents))
            {
                continue;
            }
            let mut size = schema_size(name);
            if delegation {
                size += schema_size("message");
            }
            let note = if name == "bg" && core.iter().any(|tool| tool == "bash") {
                bg_note_bytes()
            } else {
                0
            };
            let control = matches!(name, "tool_search" | "tool_load" | "finish");
            if control || ((limit == 0 || core.len() < limit) && bytes + size <= budget + note) {
                bytes = bytes + size - note;
                core.push(name.to_owned());
                if delegation {
                    core.push("message".to_owned());
                }
            }
        }
        core.sort();
        core.dedup();
        core
    }

    /// The selected memory backend's `memory` tool when it is in the catalog: the runtime's, or
    /// provenance's registered one.
    fn memory_core_tools(&self, catalog: &[String]) -> Vec<String> {
        let offered = match (&self.memory_runtime, &self.memory) {
            (Some(runtime), _) => runtime.offers_tool(),
            (None, Some(_)) => true,
            (None, None) => false,
        };
        let name = crate::storage::memory_backend::TOOL_NAME;
        if offered && catalog.iter().any(|entry| entry == name) {
            vec![name.to_owned()]
        } else {
            Vec::new()
        }
    }

    /// Schemas offered to the model: every parameter with its name, type, enum and required
    /// status, but without per-parameter explanations for Dial-owned tools. The explanations
    /// return in the error of a call whose arguments do not fit ([`Self::parameter_help`]).
    /// Contract identities ignore descriptions, so recorded sessions are unaffected.
    pub(in crate::agent) fn schemas(&self, names: &[String]) -> Result<Vec<Value>> {
        let mut schemas = self.full_schemas(names)?;
        bare_schemas(&mut schemas, self.config, &self.ext);
        Ok(schemas)
    }

    /// Offered size of one schema for the core budget, with or without the `bash` note that `bg`
    /// is loadable: whether that note applies depends on the budget decision itself.
    fn budgeted_schema_bytes(&self, name: &str, bg_note: bool) -> Option<usize> {
        let mut schemas = self
            .full_schemas_with(&[name.to_owned()], Some(bg_note))
            .ok()?;
        bare_schemas(&mut schemas, self.config, &self.ext);
        schemas.pop().map(|schema| schema.to_string().len() + 1)
    }

    /// Complete parameter explanations for one offered tool, for argument-error feedback.
    pub(in crate::agent) fn parameter_help(&self, name: &str) -> Option<String> {
        parameter_help(&self.full_schemas(&[name.to_owned()]).ok()?.pop()?)
    }

    pub(in crate::agent) fn full_schemas(&self, names: &[String]) -> Result<Vec<Value>> {
        self.full_schemas_with(names, None)
    }

    /// `bg_note` forces the `bash` note that `bg` is a loadable Dial tool; `None` decides it from
    /// the current offer ([`Self::bg_loadable`]).
    fn full_schemas_with(&self, names: &[String], bg_note: Option<bool>) -> Result<Vec<Value>> {
        let mode = self.edit_mode();
        let mut schemas = Vec::new();
        for name in names {
            schemas.push(match name.as_str() {
                "request_directory" => crate::agent::run::directory_access::schema(),
                "context_notes" | "new_context" => crate::agent::window::context_notes::schema(name).expect("known context tool"),
                "checkpoint" | "rewind" => crate::agent::workflow::exploration::schema(name).expect("known exploration tool"),
                "session_search" if self.config.compaction.snapcompact.overlay
                    || crate::agent::compact::snapcompact::views::available_in(&self.session) =>
                    crate::agent::compact::snapcompact::views::schema(
                        if self.experimental_context_owner() || self.exploration_owner() {
                            session::session_search_tool::schema_owned()
                        } else {
                            session::session_search_tool::schema()
                        }
                    ),
                "session_search" if self.experimental_context_owner() || self.exploration_owner() => session::session_search_tool::schema_owned(),
                crate::storage::memory_backend::TOOL_NAME if self.memory_runtime.is_some()
                    && crate::storage::memory_backend::schema(&self.config.memory).is_some() =>
                    crate::storage::memory_backend::schema(&self.config.memory).expect("selected memory tool has a schema"),
                n if config::tools::BUILTIN.contains(&n) => tools::schemas(&[n], mode)?.remove(0),
                n if self.ext.contains(n) => self.ext.get(n).map(|tool| tool.schema()).context("extension tool vanished")?,
                "experience" => crate::agent::model::assistance::schema(),
                "edit_preview" => crate::tools::edit::preview::schema(),
                "tts" => crate::voice::tool::schema(),
                "todo" => crate::agent::workflow::todo::schema(),
                "finish" => crate::agent::verify::completion::schema(),
                crate::agent::verify::verify_tool::NAME => crate::agent::verify::verify_tool::schema(self.config.experimental.check_predictions),
                "revert_edit" => crate::agent::verify::revert_edit::schema(),
                "goal" => modes::goal::schema(),
                "ask" => crate::agent::workflow::questions::schema(),
                "task" => crate::agent::workers::subagents::schema(self.config),
                "side_request" => crate::agent::workers::side_questions::schema(),
                "message" => crate::agent::workers::live_tasks::message_schema(self.coordination_enabled()),
                "peers" => crate::agent::run::coordination::schema(),
                "solve" => crate::agent::workflow::solve::schema(),
                "escalate" => crate::agent::workflow::solve::escalation_schema(),
                "skill" => skills::skill_tool::schema(),
                "skill_manage" => skills::manage_tool::schema(),
                "session_search" => session::session_search_tool::schema(),
                "working_state" => crate::agent::workflow::evidence::working_schema(),
                "tool_search" => json!({"type":"function","function":{"name":"tool_search","description":"Find a tool the offer lacks: search eligible tools by capability or exact name, or name them with `select:a,b`. Returns each match's description, parameters, readiness and status. `load: true` also loads the exact or top matches for the next request (tool_load's checks). A miss returns catalog page 0; follow next_query (\"*:N\", zero-based) while present. Never executes tools or grants permission. Legacy noncompact mode may activate exact matches.","parameters":{"type":"object","properties":{"query":{"type":"string"},"load":{"type":"boolean"}},"required":["query"],"additionalProperties":false}}}),
                "tool_load" => json!({"type":"function","function":{"name":"tool_load","description":"Explicitly enable or unload exact eligible tool names. Changes take effect next round, never in this batch, and grant no permissions.","parameters":{"type":"object","properties":{"names":{"type":"array","maxItems":32,"items":{"type":"string"}},"unload":{"type":"array","maxItems":32,"items":{"type":"string"}}},"additionalProperties":false}}}),
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
            // Lean and local routes take shorter texts for the most-used tools; each keeps the
            // facts a first call depends on (regex dialect, output format, scopes, paging, the
            // sandbox). Hosted routes keep the tools' own descriptions.
            if self.tool_discovery() && !self.hosted_route() {
                let description = match name.as_str() {
                    "write" => Some(
                        "Write complete UTF-8 source to a workspace path; guarded syntax and normal approvals apply.".to_owned(),
                    ),
                    "bash" => Some(format!(
                        "Run a shell command in the project root; set relative `cwd` instead of prefixing `cd`. {} Normal approvals apply; inspect exit status. A foreground command returns when it finishes or reaches `timeout` (seconds); `background: true` starts a server, watcher or long job as a `bg` job.",
                        if cfg!(windows) {
                            "Windows has no isolation: execution requires `sandbox.mode = \"off\"`."
                        } else {
                            "May be sandboxed: writes confined to the project, network possibly blocked, temp files in `$TMPDIR`."
                        }
                    )),
                    "glob" => Some(
                        "List workspace paths matching a glob such as `src/**/*.rs`; ignore-aware, sorted, 500 per page, the footer gives next_cursor.".to_owned(),
                    ),
                    "grep" => Some(
                        "Search file contents with a Rust regex (no lookaround or backreferences); `path` takes files, directories or `;`-separated scopes. Returns path:line:text; the footer names capped scopes and next_cursor.".to_owned(),
                    ),
                    _ => None,
                };
                if let Some(description) = description {
                    schemas.last_mut().unwrap()["function"]["description"] = json!(description);
                }
            }
            // The shell text names `bg` for background jobs; when the offer does not include it,
            // say it is a Dial tool to load, so `bg` is not mistaken for the shell builtin.
            if name == "bash" && bg_note.unwrap_or_else(|| self.bg_loadable()) {
                let schema = schemas.last_mut().expect("schema was just inserted");
                let description = schema["function"]["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                schema["function"]["description"] = json!(format!(
                    "{description} `bg` is a Dial tool, not a shell command, and is not offered yet: Dial loads it for the next request when a command becomes a background job, or load it with `tool_load`."
                ));
            }
            // An offered `eval` may have only one runtime installed (a Node-only or Python-only
            // image). Name the missing one so the first cell does not discover it by failing.
            if name == "eval" {
                let installed =
                    tools::exec::eval::installed_languages(self.workspace.root(), self.config);
                let missing: Vec<&str> = [("js", "JavaScript"), ("py", "Python")]
                    .into_iter()
                    .filter(|(language, _)| !installed.contains(language))
                    .map(|(_, label)| label)
                    .collect();
                if !missing.is_empty() {
                    let schema = schemas.last_mut().expect("schema was just inserted");
                    let description = schema["function"]["description"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let advice = installed.first().map_or_else(
                        || "No eval interpreter resolves here; loading preserves status/reset/cancel but does not install an interpreter.".to_owned(),
                        |language| format!("Use language {language:?}."),
                    );
                    schema["function"]["description"] = json!(format!(
                        "{description} Not installed here: {} cells. {advice}",
                        missing.join(", ")
                    ));
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
        crate::tools::surface::catalog::Catalog::validate_schemas(&schemas)?;
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
            && self.session.messages().iter().any(|message| {
                message["tool_call_id"].as_str() == Some(call_id)
                    && message["dial_execution"]["output"]["presentation"] != "partial"
            })
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
/// A capability gap the controller observed during a turn. Each gap maps to one fixed query owned
/// here; no tool output or model text feeds routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::agent) enum CapabilityGap {
    /// A required visual check has no rendered evidence yet.
    MissingVisualEvidence,
    /// A check or command failed and its diagnostic is not yet inspected.
    DiagnosticFailure,
}

impl CapabilityGap {
    fn query(self) -> &'static str {
        match self {
            Self::MissingVisualEvidence => "browser rendered screenshot",
            Self::DiagnosticFailure => "debugger runtime diagnostics",
        }
    }
}

impl Agent<'_> {
    /// Every eligible catalog tool with its description and search-index text, in catalog order.
    /// This is the candidate set `tool_search` ranks, so dispatch and gap routing share one boundary.
    pub(in crate::agent) fn eligible_candidates(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for name in self.catalog() {
            if !self.tool_eligible(&name) {
                continue;
            }
            let Some(schema) = self
                .schemas(std::slice::from_ref(&name))
                .ok()
                .and_then(|mut schemas| schemas.pop())
            else {
                continue;
            };
            let description = schema["function"]["description"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let indexed = super::dispatch::search_index::text(&schema);
            out.push((name, description, indexed));
        }
        out
    }

    /// Fresh, side-effect-free advisory metadata. Explicit discovery does not filter on readiness.
    pub(in crate::agent) fn tool_readiness(
        &self,
        name: &str,
    ) -> tools::surface::catalog::Readiness {
        tools::surface::catalog::readiness_at(name, self.workspace.root(), self.config, &self.ext)
    }

    /// Automatic routing shares discovery's eligibility boundary, then removes only known execution
    /// blockers and explicit unloads. Unknown prerequisites cannot silently hide a capability.
    fn automatic_candidates(&self) -> Vec<(String, String)> {
        let offered = self.offered();
        self.eligible_candidates()
            .into_iter()
            .filter(|(name, _, _)| {
                !offered.contains(name)
                    && !matches!(name.as_str(), "tool_search" | "tool_load")
                    && !self.tool_routing.is_suppressed(name)
                    && self.tool_readiness(name).automatic_candidate()
            })
            .map(|(name, _, indexed)| (name, indexed))
            .collect()
    }

    /// Offer the eligible tools that match an observed gap, for the next round only. The caller keeps
    /// `spent`, so one gap activates at most once per turn and a repeated gap cannot churn the working
    /// set. No model call, no execution and no permission change; pinned, hermetic and explicitly
    /// unloaded tools are left alone. Returns the names newly offered.
    pub(in crate::agent) fn activate_gap_capabilities(
        &mut self,
        gap: CapabilityGap,
        spent: &mut BTreeSet<CapabilityGap>,
    ) -> Result<Vec<String>> {
        if self.hermetic
            || self.config.harness.tools != config::tools::default_tools()
            || spent.contains(&gap)
        {
            return Ok(Vec::new());
        }
        spent.insert(gap);
        let activated: Vec<String> = crate::agent::model::assistance::discover_with_priority(
            gap.query(),
            self.automatic_candidates(),
            &|name| self.tool_readiness(name).routing_priority(),
        )
        .into_iter()
        .filter(|entry| entry.2)
        .map(|entry| entry.0)
        .collect();
        if activated.is_empty() {
            return Ok(Vec::new());
        }
        self.activate_tools(&activated)?;
        let after = self.offered();
        Ok(activated
            .into_iter()
            .filter(|name| after.contains(name))
            .collect())
    }

    /// Catalog tools that accept the arguments an offered tool just rejected, e.g. `pty` on `bash`
    /// names `bg`. Returns the rejected keys and the owners: only tools that are eligible, not
    /// offered and not unloaded by the user, whose schema declares every rejected key, ranked by how
    /// many of the call's keys they declare. An ambiguous match (more than three tied) returns no
    /// owner. No tool runs and no permission changes; the caller loads the owners for the next round.
    pub(in crate::agent) fn argument_owners(
        &self,
        call: &ToolCall,
        offer: &[Value],
    ) -> Option<(Vec<String>, Vec<String>)> {
        let schema = offer
            .iter()
            .find(|schema| schema["function"]["name"] == call.name.as_str())?;
        let arguments = call.arguments.as_object()?;
        if crate::tools::surface::catalog::validate_arguments(schema, &call.arguments).is_ok() {
            return None;
        }
        let declared = |schema: &Value, key: &str| {
            schema
                .pointer("/function/parameters/properties")
                .and_then(Value::as_object)
                .is_some_and(|properties| properties.contains_key(key))
        };
        let rejected: Vec<String> = arguments
            .iter()
            .filter(|(key, value)| !value.is_null() && !declared(schema, key))
            .map(|(key, _)| key.clone())
            .collect();
        if rejected.is_empty() {
            return None;
        }
        let offered = self.offered();
        let mut ranked: Vec<(usize, String)> = Vec::new();
        for name in self.catalog() {
            if offered.contains(&name)
                || matches!(name.as_str(), "tool_search" | "tool_load")
                || !self.tool_eligible(&name)
                || self.tool_routing.is_suppressed(&name)
            {
                continue;
            }
            let Some(candidate) = self
                .schemas(std::slice::from_ref(&name))
                .ok()
                .and_then(|mut schemas| schemas.pop())
            else {
                continue;
            };
            if !rejected.iter().all(|key| declared(&candidate, key)) {
                continue;
            }
            let score = arguments
                .keys()
                .filter(|key| declared(&candidate, key))
                .count();
            ranked.push((score, name));
        }
        let best = ranked.iter().map(|(score, _)| *score).max()?;
        let owners: Vec<String> = ranked
            .into_iter()
            .filter(|(score, _)| *score == best)
            .map(|(_, name)| name)
            .collect();
        (owners.len() <= 3).then_some((rejected, owners))
    }
}

#[cfg(test)]
pub(crate) mod default_offer_tests;
#[cfg(test)]
mod discovery_tests;
#[cfg(test)]
mod gap_tests;
#[cfg(test)]
mod stable_offer_tests;
#[cfg(test)]
mod wire_surface_tests;

/// Tools whose parameter explanations may be their only documentation: MCP, configured and
/// non-native extension tools. Their schemas are offered unchanged.
fn foreign_tool(
    name: &str,
    config: &config::types::Config,
    ext: &crate::tools::integrations::ext::Registry,
) -> bool {
    name.starts_with("mcp__")
        || config.tools.contains_key(name)
        || ext.contains(name)
            && ext.origin(name) != Some(crate::tools::surface::catalog::Origin::Native)
}

/// Core and multi-action tools whose top-level parameters keep a short explanation: their units,
/// defaults, maxima and per-action meaning are otherwise visible only after a failed call.
const DESCRIBED_PARAMETER_TOOLS: &[&str] = &[
    "read", "edit", "bash", "grep", "glob", "todo", "verify", "bg", "lsp",
];
/// Longest parameter explanation kept in an offered schema, in words.
const PARAMETER_DESCRIPTION_WORDS: usize = 15;

/// Offer Dial-owned schemas lean: every parameter keeps its name, type, enum, bounds and required
/// status; only [`DESCRIBED_PARAMETER_TOOLS`] keep a short explanation of each top-level parameter
/// ([`short_parameter_description`]). Shared by the parent loop and subagents; the complete
/// explanations return in argument errors ([`parameter_help`]). Contract identities ignore
/// descriptions, so recorded sessions are unaffected.
pub(in crate::agent) fn bare_schemas(
    schemas: &mut [Value],
    config: &config::types::Config,
    ext: &crate::tools::integrations::ext::Registry,
) {
    for schema in schemas {
        let name = schema["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if foreign_tool(&name, config, ext) {
            continue;
        }
        let Some(parameters) = schema.pointer_mut("/function/parameters") else {
            continue;
        };
        let kept: Vec<(String, String)> = if DESCRIBED_PARAMETER_TOOLS.contains(&name.as_str()) {
            parameters["properties"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(key, property)| {
                    let text = property["description"].as_str()?;
                    Some((key.clone(), short_parameter_description(text)?))
                })
                .collect()
        } else {
            Vec::new()
        };
        crate::runtime::arguments::strip_property_descriptions(parameters);
        for (key, text) in kept {
            parameters["properties"][key]["description"] = json!(text);
        }
    }
}

/// The first sentence of a parameter explanation when it has at most
/// [`PARAMETER_DESCRIPTION_WORDS`] words, otherwise its leading clause (before a parenthesis,
/// semicolon or colon) when that is short enough; `None` drops the explanation.
fn short_parameter_description(text: &str) -> Option<String> {
    let words = |text: &str| text.split_whitespace().count();
    let text = text.trim();
    let sentence = text
        .find(". ")
        .map_or(text, |end| &text[..end])
        .trim_end_matches('.');
    if (1..=PARAMETER_DESCRIPTION_WORDS).contains(&words(sentence)) {
        return Some(sentence.to_owned());
    }
    [" (", "; ", ": "].iter().find_map(|separator| {
        let clause = sentence[..sentence.find(separator)?].trim_end_matches([',', '.']);
        (2..=PARAMETER_DESCRIPTION_WORDS)
            .contains(&words(clause))
            .then(|| clause.to_owned())
    })
}

/// Compact list of a schema's top-level parameters for search results: `name*` marks a required
/// parameter, then its type or its enum values. Bounded so a large schema cannot flood a result.
pub(in crate::agent) fn parameter_summary(schema: &Value) -> String {
    const MAX_CHARS: usize = 400;
    const MAX_ENUM: usize = 12;
    let parameters = &schema["function"]["parameters"];
    let required: Vec<&str> = parameters["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let entries: Vec<String> = parameters["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, property)| {
            let marker = if required.contains(&name.as_str()) {
                "*"
            } else {
                ""
            };
            let kind = match property["enum"].as_array() {
                Some(values) => {
                    let mut shown: Vec<String> = values
                        .iter()
                        .take(MAX_ENUM)
                        .map(|value| {
                            value
                                .as_str()
                                .map_or_else(|| value.to_string(), str::to_owned)
                        })
                        .collect();
                    if values.len() > MAX_ENUM {
                        shown.push("…".into());
                    }
                    shown.join("|")
                }
                None => property["type"].as_str().unwrap_or("any").to_owned(),
            };
            format!("{name}{marker}: {kind}")
        })
        .collect();
    let summary = entries.join("; ");
    if summary.chars().count() <= MAX_CHARS {
        return summary;
    }
    let mut cut: String = summary.chars().take(MAX_CHARS).collect();
    cut.push('…');
    cut
}

/// Complete parameter explanations from one full schema, for an argument error.
pub(in crate::agent) fn parameter_help(schema: &Value) -> Option<String> {
    let help = crate::runtime::arguments::parameter_help(schema.pointer("/function/parameters")?);
    (!help.is_empty()).then_some(help)
}
