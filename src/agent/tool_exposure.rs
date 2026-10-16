//! Tool exposure: offered tools, discovery, schemas and request records.

use super::*;
use crate::config::types::{LEAN_AUTO_MAX_WINDOW, LeanLocal};

impl<'a> Agent<'a> {
    /// Local servers serve a context of their own choosing, often far below Dial's default: LM Studio
    /// loads on demand, llama.cpp splits `-c` across slots, Ollama defaults small and truncates silently.
    /// Their native APIs report the served size (see `catalog::loaded_context`); hosted and unknown
    /// servers are left alone, and an unloaded model keeps the last known cap rather than forgetting it.
    pub(super) fn probe_context(&mut self) {
        if self.hermetic {
            return;
        }
        let config = self.effective_config();
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
            let lean = self.lean_local();
            self.context_cap = Some(loaded);
            // The loaded window can switch the lean profile on or off; re-render the prompt once here.
            if self.lean_local() != lean {
                self.rebuild_system();
            }
        }
    }

    /// Minimal first-request offer, used only after the full set cannot fit the context window.
    const LEAN_TOOLS: [&str; 10] = [
        "read",
        "write",
        "edit",
        "bash",
        "grep",
        "glob",
        "todo",
        "finish",
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
    pub(super) fn lean_local(&self) -> bool {
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
    pub(super) fn tool_discovery(&self) -> bool {
        !self.hermetic
            && self.config.efficiency.enabled
            && (self.config.assistance.compact_tools || self.lean_local())
    }

    /// Most tools that discovery (`tool_search`, harness compatibility loads) keeps active at once in compact
    /// mode. Explicit `tool_load` pins live in `enabled_tools` and are never evicted by discovery.
    const DISCOVERED_TOOL_LIMIT: usize = 8;

    /// Working set of tools activated by discovery, least recently activated first. It is stored in the session
    /// control state, so a resumed session restores it; pins are restored separately from `enabled_tools`.
    pub(super) fn discovered_tools(&self) -> Vec<String> {
        self.session.control_state()["discovered_tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }

    /// Capability eligibility is independent of whether a tool's schema is loaded yet.
    pub(super) fn tool_eligible(&self, name: &str) -> bool {
        self.plan_tool_available(name)
            && self.modes.vibe_allows(name)
            && self.modes.launch.allows_tool(name)
    }

    /// Activate tools chosen by discovery. In compact discovery mode they join the bounded working set and the
    /// oldest entries are evicted; otherwise they keep the legacy `enabled_tools` behaviour. Pinned tools are not
    /// recorded as discovered.
    pub(super) fn activate_tools(&mut self, names: &[String]) -> Result<()> {
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

    /// Tools actually offered: the harness selection, explicitly loaded tools and the essential
    /// integrations present in the catalog. The loaders are offered only while the catalog holds
    /// something not offered. An operator-pinned `[harness] tools` list, a model-profile allowlist
    /// and hermetic runs restrict the offer; discovery mode compacts it to the initial budget.
    pub(super) fn offered(&self) -> Vec<String> {
        self.offered_with(false)
    }

    /// Catalog tools not in the current offer, with the first sentence of each description, for
    /// the system prompt's loadable-tools list.
    pub(super) fn discoverable_tools(&self, offered: &[String]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for name in self.catalog() {
            if offered.iter().any(|tool| *tool == name)
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
                        prompt::truncate_chars(first.trim_end_matches(['.', '\n']), 110)
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
    pub(super) fn offered_for_children(&self) -> Vec<String> {
        self.offered_with(true)
    }

    pub(super) fn offered_with(&self, selection_only: bool) -> Vec<String> {
        let catalog = self.catalog();
        let discovery = self.tool_discovery();
        let discovered = self.discovered_tools();
        let loaded: std::collections::HashSet<String> = self
            .enabled_tools
            .iter()
            .chain(discovered.iter())
            .cloned()
            .collect();
        let pinned = self.config.harness.tools != tools::default_tools();
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
        if !(discovery || pinned || self.hermetic || self.lean_tools || selection_only) {
            names.extend(
                Self::ESSENTIAL_INTEGRATIONS
                    .iter()
                    .filter(|tool| catalog.iter().any(|name| name == *tool))
                    .map(|tool| (*tool).to_owned()),
            );
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
        let loaders = discovery || pinned || self.lean_tools || selection_only || hidden;
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
            assistance::compact(
                &mut names,
                &loaded,
                self.initial_tool_limit(),
                &self.assistance.scores,
            );
        }
        // The lean budget can trim `message` while `task` stays; peer messaging follows `task` (line 280).
        if names.iter().any(|name| name == "task") && !names.iter().any(|name| name == "message") {
            names.push("message".into());
            names.sort();
        }
        for tool in context_notes::TOOL_NAMES {
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
            || exploration::TOOL_NAMES
                .iter()
                .any(|tool| names.iter().any(|name| name == tool))
        {
            names.extend(
                exploration::TOOL_NAMES
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
        if self.lean_tools {
            names.retain(|name| Self::LEAN_TOOLS.contains(&name.as_str()) || loaded.contains(name));
        }
        // The plan exit is a control operation, not an optional schema-budget candidate.
        if self.plan_mode && self.tool_eligible("exit_plan") {
            names.push("exit_plan".into());
            names.sort();
            names.dedup();
        }
        names
    }

    pub(super) fn schemas(&self, names: &[String]) -> Result<Vec<Value>> {
        let mode = self.edit_mode();
        let mut schemas = Vec::new();
        for name in names {
            schemas.push(match name.as_str() {
                "context_notes" | "new_context" => context_notes::schema(name).expect("known context tool"),
                "checkpoint" | "rewind" => exploration::schema(name).expect("known exploration tool"),
                "session_search" if self.experimental_context_owner() || self.exploration_owner() => session::session_search_tool::schema_owned(),
                n if crate::storage::memory_backend::schema(&self.config.memory, n).is_some() =>
                    crate::storage::memory_backend::schema(&self.config.memory, n).expect("selected memory tool has a schema"),
                n if tools::BUILTIN.contains(&n) => tools::schemas(&[n], mode)?.remove(0),
                n if self.ext.contains(n) => self.ext.get(n).map(|tool| tool.schema()).context("extension tool vanished")?,
                "experience" => assistance::schema(),
                "edit_preview" => crate::tools::preview::schema(),
                "tts" => crate::voice::tool::schema(),
                "todo" => todo::schema(),
                "finish" => completion::schema(),
                "goal" => modes::goal::schema(),
                "ask" => questions::schema(),
                "task" => subagents::schema(self.config),
                "message" => live_tasks::message_schema(),
                "solve" => solve::schema(),
                "escalate" => solve::escalation_schema(),
                "skill" => skills::skill_tool::schema(),
                "skill_manage" => skills::manage_tool::schema(),
                "session_search" => session::session_search_tool::schema(),
                "artifact_read" => tools::artifact::schema(),
                "working_state" => evidence::working_schema(),
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
                        let agents: Vec<&str> = subagents::AGENTS
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
        tools::catalog::Catalog::from_schemas(&schemas, &self.ext)?;
        Ok(schemas)
    }

    pub(super) fn record(&self, step: u32, kind: &str, mut data: Value) -> Result<()> {
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

    pub(super) fn save_request_policy(
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
    pub(super) fn holds_full_result(&self, call_id: &str) -> bool {
        self.superseded.holds_full(call_id)
            && self
                .session
                .messages()
                .iter()
                .any(|message| message["tool_call_id"].as_str() == Some(call_id))
    }

    /// Append to the durable session; training capture happens at the inference boundary.
    pub(super) fn push(&mut self, message: Value) -> Result<()> {
        self.session.append(message)?;
        if let Some(memory) = &self.memory_runtime {
            memory.resume(memory_authority(&self.session, 0));
        }
        Ok(())
    }
}

#[cfg(test)]
mod default_offer_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// An interactive (non-hermetic) agent over default configuration plus `overlay` and an
    /// unreachable endpoint; these tests inspect the offer and prompt only, so no request is made.
    fn with_agent<T>(overlay: &str, f: impl FnOnce(&mut Agent<'_>) -> T) -> T {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let layout =
            crate::storage::paths::Layout::with_user_dir(&project, temp.path().join("home"))
                .unwrap();
        let overlay: toml::Table = toml::from_str(&format!(
            "model='test'\n{overlay}\n[improve]\nenabled=false"
        ))
        .unwrap();
        let config = Config::load(&layout, Some(overlay)).unwrap();
        let backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            std::path::PathBuf::new(),
            5,
        );
        let session = crate::storage::session::Session::create(
            &layout,
            crate::storage::session::SessionMeta {
                model: config.model.clone(),
            },
        )
        .unwrap();
        let mut agent = Agent::interactive(
            &config,
            backend,
            layout,
            session,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        f(&mut agent)
    }

    fn schema_bytes(agent: &Agent<'_>, names: &[String]) -> usize {
        serde_json::to_string(&agent.schemas(names).unwrap())
            .unwrap()
            .len()
    }

    /// System messages with the per-test project directory masked, so prompts built in different
    /// temporary projects compare equal.
    fn system_text(agent: &Agent<'_>) -> String {
        let root = agent.workspace.root().display().to_string();
        serde_json::to_string(&agent.system_messages())
            .unwrap()
            .replace(&root, "<project>")
    }

    #[test]
    fn lean_local_profile_shrinks_the_request_and_stays_byte_stable_across_turns() {
        // The bytes a local server prefills: the system messages and the offered tool schemas.
        fn request(agent: &Agent<'_>) -> (String, String, Vec<String>) {
            let offered = agent.offered();
            let tools = serde_json::to_string(&agent.schemas(&offered).unwrap()).unwrap();
            (system_text(agent), tools, offered)
        }
        let default = with_agent("[assistance]\nlean_local=false", |agent| request(agent));
        let lean = with_agent("[assistance]\nlean_local=true", |agent| {
            let first = request(agent);
            // A following turn rebuilds the prefix from the same state and must not change a byte.
            agent.rebuild_system();
            assert_eq!(request(agent), first);
            first
        });
        // Discovery lists the tools it does not offer, so the system text may grow slightly;
        // the prefill as a whole must shrink.
        let (lean_total, default_total) = (
            lean.0.len() + lean.1.len(),
            default.0.len() + default.1.len(),
        );
        assert!(
            lean_total < default_total,
            "prefill {lean_total} vs {default_total}"
        );
        assert!(
            lean.1.len() < default.1.len(),
            "schemas {} vs {}",
            lean.1.len(),
            default.1.len()
        );
        assert!(lean.2.len() < default.2.len(), "offered {:?}", lean.2);
        assert!(!lean.0.contains("# Visual verification"));
        assert!(default.0.contains("# Visual verification"));
    }

    #[test]
    fn lean_local_profile_leaves_hosted_routes_unchanged() {
        let overlay = "[endpoint]\nbase_url='https://api.example.com/v1'\n";
        let default = with_agent(overlay, |agent| (system_text(agent), agent.offered()));
        let lean = with_agent(
            &format!("{overlay}[assistance]\nlean_local=true"),
            |agent| {
                assert!(!agent.lean_local());
                (system_text(agent), agent.offered())
            },
        );
        assert_eq!(lean, default);
    }

    #[test]
    fn message_is_offered_with_task_whether_or_not_a_worker_is_live() {
        with_agent("[assistance]\nlean_local=false", |agent| {
            // No worker runs here, so the offer must already include `message` with `task`; a live
            // worker cannot change it because the offer no longer reads worker state.
            assert!(!agent.live_task_control().any_running());
            let offered = agent.offered();
            assert!(offered.iter().any(|name| name == "task"), "{offered:?}");
            assert!(offered.iter().any(|name| name == "message"), "{offered:?}");
            let discoverable = agent.discoverable_tools(&offered);
            assert!(!discoverable.iter().any(|(name, _)| name == "message"));
            let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
            assert!(prompt.contains("Descendants require explicit role spawn permissions"));
        });
    }

    #[test]
    fn default_offer_defers_overlapping_specialists_but_lists_them_as_loadable() {
        with_agent("[assistance]\nlean_local=false", |agent| {
            let offered = agent.offered();
            let catalog = agent.catalog();
            for core in [
                "read",
                "grep",
                "glob",
                "find",
                "recall",
                "retain",
                "codebase_memory",
                "tool_load",
                "tool_search",
            ] {
                assert!(
                    offered.iter().any(|name| name == core),
                    "{core}: {offered:?}"
                );
            }
            let discoverable = agent.discoverable_tools(&offered);
            let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
            assert!(prompt.contains("Loadable tools, not in the schema yet"));
            for deferred in tools::DEFERRED_BY_DEFAULT {
                assert!(catalog.iter().any(|name| name == deferred), "{deferred}");
                assert!(!offered.iter().any(|name| name == deferred), "{offered:?}");
                assert!(
                    discoverable.iter().any(|(name, _)| name == deferred),
                    "{discoverable:?}"
                );
                assert!(prompt.contains(&format!("  - {deferred}: ")), "{prompt}");
            }
            // Serialized default schema size, before (deferred tools offered) and after.
            let mut before = offered.clone();
            before.extend(
                tools::DEFERRED_BY_DEFAULT
                    .iter()
                    .map(|name| (*name).to_owned()),
            );
            before.sort();
            let (before, after) = (schema_bytes(agent, &before), schema_bytes(agent, &offered));
            eprintln!("default tool schemas: {before} -> {after} bytes");
            assert!(after < before);
        });
    }

    #[test]
    fn tool_load_offers_a_deferred_tool_next_round_and_unload_defers_it_again() {
        with_agent("", |agent| {
            struct Quiet;
            impl Ui for Quiet {}
            let mut backend = agent.backend.take().unwrap();
            let mut call = |agent: &mut Agent<'_>, arguments: Value| {
                let call = ToolCall {
                    id: "call-load".into(),
                    name: "tool_load".into(),
                    arguments,
                    malformed_arguments: false,
                };
                agent.dispatch(&mut backend, &call, &mut Quiet)
            };
            let names: Vec<&str> = tools::DEFERRED_BY_DEFAULT.to_vec();
            let out = call(agent, json!({"names": names}));
            assert!(!out.is_error, "{}", out.text);
            let offered = agent.offered();
            for deferred in tools::DEFERRED_BY_DEFAULT {
                assert!(offered.iter().any(|name| name == deferred), "{offered:?}");
            }
            let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
            assert!(!prompt.contains("  - memory_update: "), "{prompt}");
            let out = call(agent, json!({"unload": ["memory_update"]}));
            assert!(!out.is_error, "{}", out.text);
            let offered = agent.offered();
            assert!(!offered.iter().any(|name| name == "memory_update"));
            assert!(offered.iter().any(|name| name == "ast_search"));
        });
    }

    #[test]
    fn pinned_harness_selection_keeps_deferred_tools_offered() {
        with_agent(
            "[harness]\ntools=['read','edit','bash','recall','retain','memory_update']\n[assistance]\nlean_local=false",
            |agent| {
                let offered = agent.offered();
                assert!(
                    offered.iter().any(|name| name == "memory_update"),
                    "{offered:?}"
                );
            },
        );
    }

    #[test]
    fn discovered_working_set_is_bounded_persisted_and_separate_from_pins() {
        with_agent("[assistance]\ncompact_tools=true", |agent| {
            let base = agent.offered();
            let limit = Agent::DISCOVERED_TOOL_LIMIT;
            let candidates: Vec<String> = agent
                .catalog()
                .into_iter()
                .filter(|name| {
                    !base.contains(name) && !matches!(name.as_str(), "tool_search" | "tool_load")
                })
                .collect();
            assert!(candidates.len() > limit, "{candidates:?}");
            for name in &candidates {
                agent.activate_tools(std::slice::from_ref(name)).unwrap();
            }
            let working = agent.discovered_tools();
            assert_eq!(working.len(), limit);
            assert_eq!(working, candidates[candidates.len() - limit..].to_vec());
            let offered = agent.offered();
            assert!(working.iter().all(|name| offered.contains(name)));
            assert!(
                !offered.contains(&candidates[0]),
                "evicted tools stop being offered"
            );
            // Persisted in the session control state, which a resumed session restores from.
            assert_eq!(
                agent.session.control_state()["discovered_tools"],
                json!(working)
            );
            // A pinned tool is never recorded as discovered and survives discovery eviction.
            let pin = candidates[0].clone();
            agent.enabled_tools.insert(pin.clone());
            agent.activate_tools(std::slice::from_ref(&pin)).unwrap();
            assert_eq!(agent.discovered_tools(), working);
            assert!(agent.offered().contains(&pin));
        });
    }

    #[test]
    fn compact_discovery_pins_and_unloads_do_not_leave_hidden_activation_state() {
        with_agent("[assistance]\ncompact_tools=true", |agent| {
            struct Quiet;
            impl Ui for Quiet {}
            let name = "ast_search".to_owned();
            agent.activate_tools(std::slice::from_ref(&name)).unwrap();
            let mut backend = agent.backend.take().unwrap();
            for arguments in [json!({"names":[name]}), json!({"unload":[name]})] {
                let call = ToolCall {
                    id: "pin-discovered".into(),
                    name: "tool_load".into(),
                    arguments,
                    malformed_arguments: false,
                };
                let output = agent.dispatch(&mut backend, &call, &mut Quiet);
                assert!(!output.is_error, "{}", output.text);
                assert!(!agent.discovered_tools().contains(&name));
            }
            assert!(!agent.offered().contains(&name));
            assert!(!agent.enabled_tools.contains(&name));
            assert_eq!(agent.session.control_state()["discovered_tools"], json!([]));
        });
    }

    #[test]
    fn default_stale_reduction_precedes_automatic_summary_pressure() {
        with_agent("context_window=128000", |agent| {
            let mut messages = vec![
                json!({"role":"user","content":"task"}),
                json!({"role":"assistant","content":"","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]}),
                json!({"role":"tool","tool_call_id":"old","content":"x".repeat(200_000),"dial_evidence":{"artifact":"artifact://old","observation":"same-range"}}),
                json!({"role":"assistant","content":"","tool_calls":[{"id":"new","function":{"name":"read","arguments":"{}"}}]}),
                json!({"role":"tool","tool_call_id":"new","content":"y".repeat(160_000),"dial_evidence":{"artifact":"artifact://new","observation":"same-range"}}),
            ];
            let raw = context_meter::estimate_parts(&messages, &[]);
            let selection = agent.auth_config.selection();
            let threshold = agent
                .config
                .compaction
                .effective_threshold(
                    compaction::CompactionIdentity {
                        provider: &selection.provider,
                        model: &selection.model,
                        agent: None,
                    },
                    agent.budget_window(),
                )
                .unwrap();
            assert!(raw > threshold);
            assert!(raw < u64::from(agent.budget_window()) * 75 / 100);
            let mut packet = task_packet::PacketCache::default();
            let mut memo = evidence::Supersession::default();
            agent
                .project_history(&mut messages, &[], 0, &mut packet, &mut memo, None)
                .unwrap();
            let old = messages
                .iter()
                .find(|m| m["tool_call_id"] == "old")
                .unwrap();
            assert!(
                old["content"]
                    .as_str()
                    .unwrap()
                    .starts_with("[Older read result omitted")
            );
            assert!(context_meter::estimate_parts(&messages, &[]) < threshold);
        });
    }

    #[test]
    fn pruning_protects_the_capped_budget_tail_not_the_raw_model_window() {
        with_agent(
            "context_window=1000000\n[efficiency]\nhistory_token_cap=16000",
            |agent| {
                assert_eq!(agent.budget_window(), 16_000);
                let mut messages = vec![
                    json!({"role":"user","content":"task"}),
                    json!({"role":"assistant","content":"","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]}),
                    json!({"role":"tool","tool_call_id":"old","content":"x".repeat(80_000),"dial_evidence":{"artifact":"artifact://old","observation":"k"}}),
                    json!({"role":"assistant","content":"","tool_calls":[{"id":"new","function":{"name":"read","arguments":"{}"}}]}),
                    json!({"role":"tool","tool_call_id":"new","content":"y".repeat(20_000),"dial_evidence":{"artifact":"artifact://new","observation":"k"}}),
                ];
                let mut packet = task_packet::PacketCache::default();
                let mut superseded = evidence::Supersession::default();
                agent
                    .project_history(&mut messages, &[], 0, &mut packet, &mut superseded, None)
                    .unwrap();
                let old = messages
                    .iter()
                    .find(|m| m["tool_call_id"] == "old")
                    .and_then(|m| m["content"].as_str())
                    .unwrap();
                assert!(
                    old.starts_with("[Older read result omitted"),
                    "a 1M-token model under a 16k history cap must not protect a 200k-token tail: {old}"
                );
            },
        );
    }

    /// `auto` follows the planned window on local routes only; an explicit value overrides the window, and
    /// hosted routes never lean.
    #[test]
    fn lean_local_auto_follows_the_planned_window_and_explicit_values_override_it() {
        let lean = |overlay: &str| with_agent(overlay, |agent| agent.lean_local());
        // Auto is the default: a local route (no endpoint, or loopback) leans at or under 32k only.
        assert!(lean("context_window=32768"));
        assert!(!lean("context_window=32769"));
        assert!(lean(
            "context_window=8192\n[endpoint]\nbase_url='http://127.0.0.1:9/v1'"
        ));
        assert!(lean(
            "context_window=32768\n[assistance]\nlean_local='auto'"
        ));
        assert!(!lean(
            "context_window=65536\n[assistance]\nlean_local='auto'"
        ));
        // Explicit false opts out even on a small local window; explicit true forces a large one.
        assert!(!lean("context_window=8192\n[assistance]\nlean_local=false"));
        assert!(lean("context_window=65536\n[assistance]\nlean_local=true"));
        // Hosted routes never lean, in any mode.
        for mode in [
            "",
            "[assistance]\nlean_local=true",
            "[assistance]\nlean_local='auto'",
        ] {
            let overlay = format!(
                "context_window=8192\n[endpoint]\nbase_url='https://api.example.com/v1'\n{mode}"
            );
            assert!(!lean(&overlay), "{overlay}");
        }
    }

    /// On a single-request local server the lean profile keeps `task` and `message` out of the first
    /// request and the delegation section with them; loading `task` brings all three back.
    #[test]
    fn lean_single_slot_server_defers_delegation_until_loaded() {
        let overlay = "[endpoint]\nbase_url='http://127.0.0.1:9/v1'\n[assistance]\nlean_local=true";
        with_agent(overlay, |agent| {
            assert!(agent.lean_local());
            let offered = agent.offered();
            assert!(!offered.iter().any(|name| name == "task"), "{offered:?}");
            assert!(!offered.iter().any(|name| name == "message"), "{offered:?}");
            assert!(
                agent
                    .discoverable_tools(&offered)
                    .iter()
                    .any(|(name, _)| name == "task")
            );
            let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
            assert!(!prompt.contains("# Delegation"), "{prompt}");
            agent.activate_tools(&["task".to_owned()]).unwrap();
            let offered = agent.offered();
            assert!(offered.iter().any(|name| name == "task"), "{offered:?}");
            assert!(offered.iter().any(|name| name == "message"), "{offered:?}");
            let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
            assert!(prompt.contains("# Delegation"));
        });
    }

    /// A server with a concurrency limit above one can run fan-out, so the lean profile offers it up front.
    #[test]
    fn lean_multi_slot_server_keeps_delegation_offered() {
        let overlay = "[endpoint]\nbase_url='http://127.0.0.1:9/v1'\n[assistance]\nlean_local=true\n[task.provider_concurrency]\nendpoint=2";
        with_agent(overlay, |agent| {
            assert!(agent.lean_local());
            let offered = agent.offered();
            assert!(offered.iter().any(|name| name == "task"), "{offered:?}");
            assert!(offered.iter().any(|name| name == "message"), "{offered:?}");
            let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
            assert!(prompt.contains("# Delegation"));
        });
    }

    #[test]
    fn restrictions_apply_to_discovery_loading_and_late_offers() {
        for mode in ["plan", "vibe", "allowlist"] {
            with_agent("", |agent| {
                match mode {
                    "plan" => agent.plan_mode = true,
                    "vibe" => agent.modes.vibe = true,
                    _ => {
                        agent.modes.launch.tools = Some(
                            [
                                "read".to_owned(),
                                "tool_load".to_owned(),
                                "tool_search".to_owned(),
                            ]
                            .into(),
                        )
                    }
                }
                let offered = agent.offered();
                if mode == "plan" {
                    assert!(
                        offered.iter().any(|name| name == "exit_plan"),
                        "{offered:?}"
                    );
                    if offered.iter().any(|name| name == "task") {
                        assert!(offered.iter().any(|name| name == "message"), "{offered:?}");
                    }
                }
                assert!(
                    !offered.iter().any(|name| name == "bash"),
                    "{mode}: {offered:?}"
                );
                assert!(
                    !agent
                        .discoverable_tools(&offered)
                        .iter()
                        .any(|(name, _)| name == "bash")
                );
                if mode != "plan" {
                    assert!(
                        !offered.iter().any(|name| name == "bg"),
                        "{mode}: {offered:?}"
                    );
                }
                assert!(agent.activate_tools(&["bash".into()]).is_err());
                struct Quiet;
                impl Ui for Quiet {}
                let mut backend = agent.backend.take().unwrap();
                let out = agent.dispatch(
                    &mut backend,
                    &ToolCall {
                        id: "restricted-load".into(),
                        name: "tool_load".into(),
                        arguments: json!({"names":["bash"]}),
                        malformed_arguments: false,
                    },
                    &mut Quiet,
                );
                assert!(out.is_error, "{mode}: {}", out.text);
                assert!(!agent.enabled_tools.contains("bash"));
            });
        }
    }

    #[test]
    fn pending_background_work_returns_to_the_model_without_a_hidden_wait() {
        struct Job(std::sync::atomic::AtomicUsize);
        impl tools::ext::Tool for Job {
            fn name(&self) -> &str {
                "pending_probe"
            }
            fn schema(&self) -> Value {
                json!({"type":"function","function":{"name":"pending_probe","description":"pending job fixture","parameters":{"type":"object","properties":{}}}})
            }
            fn needs_approval(&self, _: &Value) -> bool {
                false
            }
            fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
                ToolOutput::ok("unused".into())
            }
            fn awaited_work(&self) -> Vec<String> {
                // A legacy spin/wait would consume the second observation and incorrectly finish.
                if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                    vec!["pending-probe still running".into()]
                } else {
                    vec![]
                }
            }
        }
        with_agent("", |agent| {
            struct Quiet;
            impl Ui for Quiet {}
            agent
                .ext
                .try_register(Arc::new(Job(std::sync::atomic::AtomicUsize::new(0))))
                .unwrap();
            assert!(agent.await_background_jobs(&mut Quiet));
            assert!(
                agent
                    .background_notices
                    .iter()
                    .any(|notice| notice.contains("pending-probe"))
            );
        });
    }

    #[test]
    fn live_job_output_reaches_the_ui_without_a_matching_foreground_tool() {
        struct Job;
        impl tools::ext::Tool for Job {
            fn name(&self) -> &str {
                "output_probe"
            }
            fn schema(&self) -> Value {
                json!({"type":"function","function":{"name":"output_probe","description":"job output fixture","parameters":{"type":"object","properties":{}}}})
            }
            fn needs_approval(&self, _: &Value) -> bool {
                false
            }
            fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
                ToolOutput::ok("done".into())
            }
            fn drain_notifications(&self) -> Vec<tools::ext::ToolEvent> {
                vec![
                    tools::ext::ToolEvent {
                        kind: "background_output".into(),
                        name: "job-identity".into(),
                        text: "pipe delta".into(),
                        artifact: None,
                    },
                    tools::ext::ToolEvent {
                        kind: "background_progress".into(),
                        name: "pty-identity".into(),
                        text: "screen".into(),
                        artifact: None,
                    },
                ]
            }
            fn call_streaming(
                &self,
                ctx: &ToolContext,
                args: &Value,
                on_event: &(dyn Fn(tools::ext::ToolEvent) + Sync),
                _: &AtomicBool,
            ) -> ToolOutput {
                for event in self.drain_notifications() {
                    on_event(event);
                }
                self.call(ctx, args)
            }
        }
        #[derive(Default)]
        struct Recording(Vec<(String, String, bool)>);
        impl Ui for Recording {
            fn background_output(&mut self, name: &str, text: &str, replace: bool) {
                self.0.push((name.into(), text.into(), replace));
            }
            fn tool_output(&mut self, _: &str, _: &str) {
                panic!("job output must not route to a foreground tool");
            }
        }
        with_agent("", |agent| {
            agent.ext.try_register(Arc::new(Job)).unwrap();
            let mut ui = Recording::default();
            agent.pump_subagents(&mut ui);
            assert!(ui.0.contains(&("job-identity".into(), "pipe delta".into(), false)));
            assert!(ui.0.contains(&("pty-identity".into(), "screen".into(), true)));
            ui.0.clear();
            let out = agent
                .run_background_tool("output_probe", &json!({}), &mut ui, None)
                .unwrap();
            assert!(!out.is_error, "{}", out.text);
            assert!(ui.0.contains(&("job-identity".into(), "pipe delta".into(), false)));
            assert!(ui.0.contains(&("pty-identity".into(), "screen".into(), true)));
        });
    }
}
