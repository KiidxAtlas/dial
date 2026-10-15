//! Agent accessors and session-level settings: session binding, tool registration, plan mode and permission rules.

use super::*;

impl<'a> Agent<'a> {
    pub fn session(&self) -> &Session {
        &self.session
    }
    /// The project this agent is working in; `/move` re-roots it, so callers must not use startup layout.
    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }
    /// Model in force for this session (the `/switch` override when set).
    pub fn model(&self) -> &str {
        &self.effective_config().model
    }
    pub fn session_mut(&mut self) -> &mut Session {
        self.reset_context_lifecycle();
        &mut self.session
    }
    /// Rebind the live backend to the session's provider identity after a session-scoped pin.
    pub fn rebind_session(&mut self) -> Result<()> {
        let Some(backend) = &mut self.backend else {
            return Ok(());
        };
        if let Err(error) = backend.bind_session(self.config, &mut self.session) {
            let message = format!("{error:#}");
            backend.remember_session_error(error);
            anyhow::bail!(message);
        }
        self.auth_config = session_auth_config(self.config, backend, &mut self.session)?;
        self.role_backends.clear();
        self.subagent_backend = None;
        Ok(())
    }
    pub fn version(&self) -> &Version {
        &self.version
    }
    pub fn steering_runtime(&self) -> serde_json::Value {
        self.backend
            .as_ref()
            .map(Backend::steering_runtime)
            .unwrap_or_else(|| serde_json::json!({"state":"unconfirmed"}))
    }

    pub fn adapter_monitor(&self) -> Option<crate::llm::provider::AdapterMonitor> {
        self.backend.as_ref().map(Backend::adapter_monitor)
    }

    /// Called between turns: unload resident models only after all child workers have finished.
    /// Keep backends and session history so the next turn can lazily reload the accepted version.
    pub fn release_idle_models(&mut self) -> bool {
        if self.backend.is_none() {
            return false;
        }
        let Ok(jobs) = self.live_tasks.status(None) else {
            return false;
        };
        if jobs
            .as_array()
            .is_none_or(|jobs| jobs.iter().any(|job| job["status"] == "running"))
        {
            return false;
        }
        if let Some(backend) = &mut self.backend {
            backend.release();
        }
        if let Some((_, backend)) = &mut self.subagent_backend {
            backend.release();
        }
        for backend in self.role_backends.values_mut() {
            backend.release();
        }
        true
    }
    /// Explicitly unload only this agent's managed server; next inference reloads the same model.
    pub fn restart_managed_model(&mut self) -> Result<()> {
        self.cancel_background_compaction();
        anyhow::ensure!(
            self.backend
                .as_ref()
                .is_some_and(Backend::supports_adapters),
            "Only Dial-managed local models can be restarted; external endpoints are never killed."
        );
        let jobs = self.live_tasks.status(None).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            jobs.as_array()
                .is_some_and(|jobs| jobs.iter().all(|job| job["status"] != "running")),
            "Wait for running child jobs before restarting the managed model."
        );
        self.backend.as_mut().unwrap().release();
        Ok(())
    }
    pub fn todo(&self) -> &TodoList {
        &self.todo
    }
    pub fn restore_todo(&mut self, snapshot: &Value) -> Result<()> {
        self.todo.restore(snapshot).map_err(anyhow::Error::msg)
    }
    pub fn checkpoints(&self) -> Option<&Arc<Checkpoints>> {
        self.checkpoints.as_ref()
    }
    pub fn set_approval(&mut self, approval: Approval) {
        self.approval = approval;
    }
    pub fn approval(&self) -> Approval {
        self.approval
    }
    /// Shared UI control handle for the session-owned background process registry.
    pub fn background_control(&self) -> Option<Arc<dyn tools::ext::Tool>> {
        self.ext.get("bg").cloned()
    }
    /// Stateful native adapters belong to one editor session; stateless services remain shared.
    pub(crate) fn fresh_session_tools(&self) -> Registry {
        self.ext.fork_session()
    }
    pub(crate) fn replace_session_tools(&mut self, tools: Registry) -> Registry {
        std::mem::replace(&mut self.ext, tools)
    }
    /// Return the previous writer so multi-session controllers can retain its ownership.
    pub fn swap_session(&mut self, session: Session) -> Session {
        self.cancel_background_compaction();
        self.memory_controller = memory_controller::Controller::default();
        self.reset_context_lifecycle();
        self.live_tasks.cancel_all();
        // The outgoing session ends here, while its id is still the observed one.
        self.observe_session_end();
        let previous = std::mem::replace(&mut self.session, session);
        // The incoming session starts its own lifecycle.
        self.begin_session_lifecycle();
        // A swapped-in session starts its own cost budget: rebase the observed ledger so the
        // previous session's spend is no longer counted against this session's caps.
        self.session_cost = None;
        self.usage_cost_base = -self.usage_scope.observed_cost().unwrap_or(0.0);
        if let Some(layout) = &self.layout
            && self.checkpoints.is_some()
        {
            match Checkpoints::new(
                layout.checkpoints_dir().join(self.session.id()),
                layout.project_root.clone(),
            ) {
                Ok(checkpoints) => {
                    let checkpoints = Arc::new(checkpoints);
                    self.workspace = self
                        .workspace
                        .clone()
                        .with_observer(Arc::new(CheckpointObserver(checkpoints.clone())));
                    self.checkpoints = Some(checkpoints);
                }
                Err(error) => {
                    if let Some(backend) = &mut self.backend {
                        backend.remember_session_error(error);
                    }
                }
            }
        }
        if let Some(memory) = &self.memory_runtime {
            memory.resume(memory_authority(&self.session, 0));
        }
        if let Some(backend) = &mut self.backend {
            match backend.bind_session(self.config, &mut self.session) {
                Ok(()) => match session_auth_config(self.config, backend, &mut self.session) {
                    Ok(config) => self.auth_config = config,
                    Err(error) => backend.remember_session_error(error),
                },
                Err(error) => backend.remember_session_error(error),
            }
        }
        self.role_backends.clear();
        self.subagent_backend = None;
        self.last_prompt_tokens = 0;
        self.pending_plan = None;
        self.latest_plan = None;
        // "Always allow this session" grants belong to the outgoing session; only the saved
        // permissions file carries over.
        self.always = match self
            .layout
            .as_ref()
            .map(|layout| approval::load(&layout.state_dir.join("permissions.toml")))
        {
            Some(Ok(rules)) => rules,
            Some(Err(error)) => {
                if let Some(backend) = &mut self.backend {
                    backend.remember_session_error(error);
                }
                Vec::new()
            }
            None => Vec::new(),
        };
        // The loaded-window cap was probed for the outgoing session's model.
        self.context_cap = None;
        self.context_meter = context_meter::ContextMeter::default();
        self.reported_messages = 0;
        if let Err(error) = self.restore_control()
            && let Some(backend) = &mut self.backend
        {
            backend.remember_session_error(error);
        }
        if let Err(error) = self.reinstall_session_model()
            && let Some(backend) = &mut self.backend
        {
            backend.remember_session_error(error);
        }
        if let Some(memory) = &self.memory {
            for tool in
                crate::storage::provenance::tools(memory.clone(), self.session.id().to_owned())
            {
                self.ext.register(tool);
            }
        }
        if let Some(cache) = &self.codebase {
            self.ext.register(crate::storage::codebase::tool(
                cache.clone(),
                self.session.id().to_owned(),
            ));
        }
        self.rebuild_system();
        previous
    }
    pub fn backend_description(&self) -> String {
        self.backend
            .as_ref()
            .map_or("none".into(), Backend::describe)
    }
    /// The window to plan against: what is configured, limited by what the server really loaded.
    pub fn context_window(&self) -> u32 {
        match self.context_cap {
            Some(loaded) => self
                .effective_config()
                .requested_context_window()
                .map_or(loaded, |limit| limit.min(loaded)),
            None => self.auth_config.context_window,
        }
    }
    /// Window the history budget (compaction and pruning thresholds) is planned against. With the
    /// economy policy on, an absolute cap keeps huge-window models from carrying huge requests.
    pub(super) fn budget_window(&self) -> u32 {
        let economy = self.config.efficiency.enabled && !self.hermetic;
        capped_window(
            self.context_window(),
            self.config.efficiency.history_token_cap,
            economy,
        )
    }
    /// Why the selected memory backend could not open, in plain words (`None` when active or off).
    pub fn memory_open_error(&self) -> Option<String> {
        self.memory_runtime
            .as_ref()
            .and_then(|runtime| runtime.open_error().map(str::to_owned))
    }
    /// One-time plain-words notice when the selected Hindsight service did not answer its startup probe.
    pub fn memory_reachability_notice(&self) -> Option<String> {
        self.memory_runtime
            .as_ref()
            .and_then(|runtime| runtime.take_reachability_notice())
    }
    pub fn tool_names(&self) -> Vec<String> {
        self.offered()
    }
    pub fn available_tool_names(&self) -> Vec<String> {
        self.catalog()
    }
    /// Authoritative schemas and identities for this owner's directly offered and deferred tools.
    /// Reading metadata does not register tools or grant execution authority.
    pub fn tool_catalog(&self) -> Result<tools::catalog::Catalog> {
        let names = self.catalog();
        let schemas = self.schemas(&names)?;
        let mut catalog = tools::catalog::Catalog::from_schemas(&schemas, &self.ext)?;
        for name in self.config.tools.keys() {
            catalog.set_origin(name, tools::catalog::Origin::Configured);
        }
        Ok(catalog)
    }
    /// Physical preparation metrics; these are not model tool-call or success counters.
    pub fn speculation_stats(&self) -> tools::speculation::Stats {
        self.workspace.speculation.stats()
    }
    pub fn session_cost(&self) -> Option<f64> {
        let accounted = self
            .usage_scope
            .observed_cost()
            .map(|cost| self.usage_cost_base + cost);
        match (self.session_cost, accounted) {
            (Some(legacy), Some(accounted)) => Some(legacy.max(accounted)),
            (legacy, accounted) => legacy.or(accounted),
        }
    }
    pub fn set_session_cost(&mut self, cost: Option<f64>) {
        self.session_cost = cost;
        self.usage_cost_base =
            (cost.unwrap_or(0.0) - self.usage_scope.observed_cost().unwrap_or(0.0)).max(0.0);
    }
    pub fn metrics(&self) -> &AgentMetrics {
        &self.metrics
    }
    pub fn usage(&self) -> (llm::usage::Groups, llm::usage::Groups) {
        self.usage_scope.snapshot()
    }
    /// Most recent `thinking = "auto"` judgment this session, as the judged choice name.
    pub fn last_auto_thinking(&self) -> Option<&str> {
        self.last_auto_thinking.as_deref()
    }
    pub fn efficiency_report(&self) -> Value {
        self.session.control_state()["efficiency"].clone()
    }

    /// An isolated benchmark RPC child may compare an adapter without admitting it into a live store.
    pub(crate) fn set_benchmark_adapter(&mut self, path: &Path) -> Result<()> {
        ensure!(
            self.store.is_none() && !self.hermetic,
            "benchmark adapters require an isolated RPC session"
        );
        ensure!(
            self.config.endpoint.is_none() && self.config.selection().provider == "mlx",
            "benchmark adapters require the local MLX backend"
        );
        let path = path.canonicalize().context("resolving benchmark adapter")?;
        ensure!(
            path.join("adapter_config.json").is_file()
                && path.join("adapters.safetensors").is_file(),
            "benchmark adapter must contain adapter_config.json and adapters.safetensors"
        );
        self.version.adapter = Some(path);
        Ok(())
    }
    pub fn register_tool(&mut self, tool: Arc<dyn tools::ext::Tool>) -> Result<()> {
        self.validate_tool_registration(tool.as_ref())?;
        self.ext.register(tool);
        self.rebuild_system();
        Ok(())
    }
    pub fn register_tools(&mut self, tools: Vec<Arc<dyn tools::ext::Tool>>) -> Result<()> {
        let mut names = HashSet::new();
        for tool in &tools {
            self.validate_tool_registration(tool.as_ref())?;
            ensure!(
                names.insert(tool.name()),
                "duplicate tool name in extension batch"
            );
        }
        drop(names);
        for tool in tools {
            self.ext.register(tool);
        }
        self.rebuild_system();
        Ok(())
    }
    pub(super) fn validate_tool_registration(&self, tool: &dyn tools::ext::Tool) -> Result<()> {
        tools::catalog::validate_tool(tool)?;
        let name = tool.name();
        ensure!(
            !tools::BUILTIN.contains(&name)
                && !tools::EXTRA_TOOLS.contains(&name)
                && !matches!(name, "exit_plan" | "message"),
            "extension cannot replace a core/control tool"
        );
        ensure!(
            !self.ext.contains(name)
                && !self.config.tools.contains_key(name)
                && !self.version.harness.tools.iter().any(|n| n == name),
            "tool name already registered"
        );
        Ok(())
    }
    pub(super) fn restore_control(&mut self) -> Result<()> {
        let mut state = self.session.control_state();
        self.exploration = exploration::Lifecycle::restore(&state, self.session.id());
        if !self.config.checkpoint.enabled && self.exploration.busy() {
            self.exploration.invalidate();
            state["exploration"] = json!({"invalidated":"checkpoint feature disabled; existing input, effects and raw archive preserved"});
            self.session.set_control_state(state.clone())?;
        }
        if let Some(checkpoint) = self.exploration.active.as_ref() {
            if self
                .session
                .completed_exploration_checkpoint(&checkpoint.entry_id)
            {
                self.exploration.active = None;
                self.exploration.completed = true;
            } else if !self.session.contains_branch_leaf(&checkpoint.entry_id) {
                self.exploration.invalidate();
            }
        }
        self.enabled_tools = state["enabled_tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        self.todo = TodoList::default();
        if let Some(snapshot) = state.get("todo")
            && let Err(error) = self.todo.restore(snapshot)
            && let Some(backend) = &mut self.backend
        {
            backend.remember_session_error(anyhow::anyhow!("todo list not restored: {error}"));
        }
        // Mode state belongs to the session: a swapped-in session must not inherit the previous session's
        // goal, vibe, prewalk or plan-yolo state. CLI launch overrides belong to the process and are kept.
        let launch = std::mem::take(&mut self.modes.launch);
        self.modes = modes::State::restore(&state);
        self.modes.launch = launch;
        // The workspace's record of shown lines lives in memory; rebuild it from this session's results so
        // a resumed or rebuilt agent can still edit lines the model was shown before.
        let messages = self.session.messages();
        let mut owner: Option<&Value> = None;
        let evidence: Vec<(&str, Value, &str)> = messages
            .iter()
            .filter_map(|message| {
                if message["role"] != "tool" {
                    owner = (message["role"] == "assistant").then_some(message);
                    return None;
                }
                let id = message["tool_call_id"].as_str()?;
                let calls = owner?["tool_calls"].as_array()?;
                let mut matching = calls.iter().filter(|call| call["id"].as_str() == Some(id));
                let call = matching.next()?;
                if matching.next().is_some() {
                    return None;
                }
                let name = call["function"]["name"].as_str()?;
                if !matches!(name, "read" | "write" | "edit") {
                    return None;
                }
                let arguments = match &call["function"]["arguments"] {
                    Value::String(text) => serde_json::from_str(text).ok()?,
                    value if value.is_object() => value.clone(),
                    _ => return None,
                };
                Some((name, arguments, message["content"].as_str()?))
            })
            .collect();
        tools::hashline::rehydrate(
            &self.workspace,
            evidence
                .iter()
                .map(|(name, arguments, result)| (*name, arguments, *result)),
        );
        Ok(())
    }
    pub(super) fn catalog(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .version
            .harness
            .tools
            .iter()
            .cloned()
            .chain(tools::EXTRA_TOOLS.iter().map(|s| s.to_string()))
            .chain(self.ext.names().map(str::to_owned))
            .chain(self.config.tools.keys().cloned())
            .collect();
        if let Some(memory) = &self.memory_runtime {
            names.extend(memory.offered_names().into_iter().map(str::to_owned));
        }
        names.retain(|n| {
            tools::BUILTIN.contains(&n.as_str())
                || matches!(
                    n.as_str(),
                    "todo"
                        | "finish"
                        | "ask"
                        | "tool_search"
                        | "tool_load"
                        | "artifact_read"
                        | "working_state"
                        | "tts"
                )
                || (self.layout.is_some()
                    && matches!(
                        n.as_str(),
                        "task" | "skill" | "skill_manage" | "session_search"
                    ))
                || self.ext.contains(n)
                || self.config.tools.contains_key(n)
                || self
                    .memory_runtime
                    .as_ref()
                    .is_some_and(|memory| memory.offered_names().contains(&n.as_str()))
        });
        names.retain(|name| name != "ask" || self.config.ask.enabled);
        if !self.hermetic && self.layout.is_some() {
            if self.config.solve.enabled {
                names.push("solve".into());
            }
            if self.config.escalation.enabled {
                names.push("escalate".into());
            }
        }
        // `experience` and `edit_preview` stay in the catalog (loadable), not in every request.
        if self.experience.is_some() {
            names.extend(["experience".into(), "edit_preview".into()]);
        }
        names.retain(|name| {
            !context_notes::TOOL_NAMES.contains(&name.as_str()) || self.experimental_context_owner()
        });
        if self.experimental_context_owner() {
            names.extend(context_notes::TOOL_NAMES.into_iter().map(str::to_owned));
            names.push("session_search".to_owned());
        }
        names.retain(|name| {
            !exploration::TOOL_NAMES.contains(&name.as_str()) || self.exploration_owner()
        });
        if self.exploration_owner() {
            names.extend(exploration::TOOL_NAMES.into_iter().map(str::to_owned));
        }
        if let Some(allowed) = self
            .config
            .active_model_profile()
            .and_then(|p| p.tools.as_ref())
        {
            names.retain(|name| allowed.contains(name));
            if self.exploration_owner()
                && exploration::TOOL_NAMES
                    .iter()
                    .any(|name| allowed.iter().any(|allowed| allowed == name))
            {
                names.extend(exploration::TOOL_NAMES.into_iter().map(str::to_owned));
            }
        }
        if self
            .config
            .active_model_profile()
            .is_some_and(|p| p.capabilities.tools == Some(false))
        {
            names.clear();
        }
        if self.hermetic {
            names.retain(|n| tools::BUILTIN.contains(&n.as_str()) || n == "todo");
        }
        names.sort();
        names.dedup();
        names
    }
    pub fn plan_mode(&self) -> bool {
        self.plan_mode
    }
    pub fn set_plan_mode(&mut self, enabled: bool) {
        self.plan_mode = enabled && !self.hermetic;
        self.rebuild_system();
    }
    /// Plan text that must survive a same-session rebuild: the latest proposal for `/plan-review`
    /// and an approved plan not yet delivered to a turn.
    pub fn plan_carry(&self) -> (Option<String>, Option<String>) {
        (self.latest_plan.clone(), self.pending_plan.clone())
    }
    pub fn restore_plan_carry(&mut self, carry: (Option<String>, Option<String>)) {
        self.latest_plan = carry.0;
        self.pending_plan = carry.1;
    }

    /// Turn start: while plan mode is on, runs the session on the `planner` model role;
    /// once plan mode ends, restores the model in force before it. Runs between turns, when the
    /// backend is free, so it also covers plan mode ended inside a turn (`exit_plan` approval).
    pub(crate) fn plan_model_resume(&mut self) -> Result<()> {
        if !self.plan_mode {
            if let Some(prior) = self.plan_model_prior.clone() {
                self.install_model_override(prior)?;
                self.plan_model_prior = None;
                self.prewalk_resume()?;
            }
            return Ok(());
        }
        if !self.effective_config().model_roles.contains_key("planner") {
            return Ok(());
        }
        let selection = modes::switch::resolve(self.effective_config(), "@planner")?;
        let current = self.effective_config().selection();
        if current.provider == selection.provider && current.model == selection.model {
            return Ok(());
        }
        let prior = match &self.plan_model_prior {
            Some(prior) => prior.clone(),
            None => self.model_override.clone(),
        };
        let config = modes::switch::switched_config(self.effective_config(), &selection, None)?;
        self.install_model_override(Some(config))?;
        self.plan_model_prior = Some(prior);
        Ok(())
    }

    /// Rebuilds the backend under `config`, or under the configured model when `None`, and records
    /// it as the session override. Mirrors `switch_model`, which is refused while a turn holds the backend.
    pub(super) fn install_model_override(&mut self, config: Option<Config>) -> Result<()> {
        let effective = config.as_ref().unwrap_or(self.config);
        let backend = make_session_backend(effective, &self.session)?;
        let mut backend = if self.hermetic {
            backend.without_steering()
        } else {
            backend
        };
        backend.bind_session(effective, &mut self.session)?;
        let auth = session_auth_config(effective, &backend, &mut self.session)?;
        self.backend = Some(backend);
        self.auth_config = auth;
        self.model_override = config;
        self.context_cap = None;
        Ok(())
    }
    pub fn permission_rules(&self) -> &[approval::PermissionRule] {
        &self.always
    }
    pub fn set_permission_rules(&mut self, rules: Vec<approval::PermissionRule>) {
        self.always = rules;
    }
    pub fn add_permission_rule(
        &mut self,
        rule: approval::PermissionRule,
        persist: bool,
    ) -> Result<()> {
        rule.validate()?;
        let mut rules = self.always.clone();
        rules.push(rule);
        if persist {
            let layout = self
                .layout
                .as_ref()
                .context("persistent permissions unavailable here")?;
            layout.ensure_state_outside_project()?;
            approval::save(&layout.state_dir.join("permissions.toml"), &rules)?;
        }
        self.always = rules;
        Ok(())
    }
    /// Save the session's permission rules with the project state ("Always allow in this project").
    pub(super) fn persist_permissions(&self) -> Result<()> {
        let layout = self
            .layout
            .as_ref()
            .context("persistent permissions unavailable here")?;
        layout.ensure_state_outside_project()?;
        approval::save(&layout.state_dir.join("permissions.toml"), &self.always)
    }
    pub fn remove_permission_rule(&mut self, index: usize, persist: bool) -> Result<()> {
        ensure!(
            index < self.always.len(),
            "permission rule index out of range"
        );
        let mut rules = self.always.clone();
        rules.remove(index);
        if persist {
            let layout = self
                .layout
                .as_ref()
                .context("persistent permissions unavailable here")?;
            layout.ensure_state_outside_project()?;
            approval::save(&layout.state_dir.join("permissions.toml"), &rules)?;
        }
        self.always = rules;
        Ok(())
    }
    pub fn approve_user_shell(&mut self, command: &str, ui: &mut dyn Ui) -> bool {
        let call = ToolCall {
            id: String::new(),
            name: "bash".into(),
            arguments: json!({"command":command}),
            malformed_arguments: false,
        };
        self.authorize_call(&call, ui).is_ok()
    }
}
