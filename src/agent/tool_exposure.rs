//! Tool exposure: offered tools, discovery, schemas and request records.

use super::*;

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
            self.context_cap = Some(loaded);
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

    /// Discovery mode: offer a bounded core and let the model load specialists with `tool_search`/
    /// `tool_load`. Meant for small local models whose schema budget is tight; off by default
    /// because every discovery costs a full extra request and the tool list change invalidates the
    /// provider's prompt cache.
    pub(super) fn tool_discovery(&self) -> bool {
        !self.hermetic && self.config.efficiency.enabled && self.config.assistance.compact_tools
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
        let pinned = self.config.harness.tools != tools::default_tools();
        let mut names: Vec<String> = self
            .version
            .harness
            .tools
            .iter()
            .chain(self.enabled_tools.iter())
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
        // Peer messaging is offered while a worker is live, so the parent can answer or steer it.
        if self.live_task_control().any_running() && catalog.iter().any(|name| name == "message") {
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
                !tools::DEFERRED_BY_DEFAULT.contains(&name.as_str())
                    || self.enabled_tools.contains(name)
            });
        }

        // The loaders are offered only while the catalog holds something the offer does not;
        // with nothing left to discover they would only add schema bytes.
        let hidden = catalog.iter().any(|name| {
            !names.iter().any(|offered| offered == name)
                && !matches!(name.as_str(), "tool_search" | "tool_load")
                && self.plan_tool_available(name)
        });
        let loaders = discovery || pinned || self.lean_tools || selection_only || hidden;
        for tool in ["tool_search", "tool_load"] {
            if loaders && catalog.iter().any(|n| n == tool) {
                names.push(tool.into());
            } else if !loaders {
                names.retain(|name| name != tool);
            }
        }
        names.retain(|name| self.plan_tool_available(name));
        names.retain(|name| self.modes.vibe_allows(name));
        names.retain(|name| self.modes.launch.allows_tool(name));
        if self.modes.goal.tool_offered() {
            names.push("goal".into());
        }
        if self.plan_mode {
            names.push("exit_plan".into());
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
                &self.enabled_tools,
                self.config.assistance.initial_tool_limit,
                &self.assistance.scores,
            );
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
        if self.lean_tools {
            names.retain(|name| {
                Self::LEAN_TOOLS.contains(&name.as_str()) || self.enabled_tools.contains(name)
            });
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
            if !self.hermetic
                && self.config.efficiency.enabled
                && self.config.assistance.compact_tools
            {
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

    #[test]
    fn default_offer_defers_overlapping_specialists_but_lists_them_as_loadable() {
        with_agent("", |agent| {
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
            "[harness]\ntools=['read','edit','bash','recall','retain','memory_update']",
            |agent| {
                let offered = agent.offered();
                assert!(
                    offered.iter().any(|name| name == "memory_update"),
                    "{offered:?}"
                );
            },
        );
    }
}
