//! Agent construction, session lifecycle hooks and system prompt assembly.

use super::*;

/// System-prompt clauses for the tools they name. A clause is emitted only when its tool is offered
/// or listed as loadable. With every named tool present the text is the fixed prose, byte for byte.
fn capability_prose(offered: &[String], discoverable: &[(String, String)]) -> String {
    let available = |name: &str| {
        offered.iter().any(|tool| tool.as_str() == name)
            || discoverable.iter().any(|(tool, _)| tool.as_str() == name)
    };
    let mut prose: Vec<&str> = Vec::new();
    if available("tool_search") {
        prose.push("tool_search enables up to three strongest matches for the next model round.");
    }
    if available("working_state") {
        prose.push("working_state preserves goals, decisions, constraints and evidence.");
    }
    let task = available("task");
    match (available("artifact_read"), task) {
        (true, true) => prose.push("artifact_read retrieves omitted output; task tasks[] batches independent substantial work, and action=start creates live runs controlled by status/wait/message/cancel."),
        (true, false) => prose.push("artifact_read retrieves omitted output."),
        (false, true) => prose.push("task tasks[] batches independent substantial work, and action=start creates live runs controlled by status/wait/message/cancel."),
        (false, false) => {}
    }
    if task {
        prose.push("Descendants require explicit role spawn permissions and obey the configured depth limit.");
    }
    let mut out = String::new();
    if !prose.is_empty() {
        out.push('\n');
        out.push_str(&prose.join(" "));
    }
    let web: Vec<&str> = ["web_search", "web_fetch"]
        .into_iter()
        .filter(|tool| available(tool))
        .collect();
    if !web.is_empty() {
        out.push_str(&format!("\nFor live web research, discover {} and use their next-round schemas; cite source URLs and report actual search/network failures. Never invent results.", web.join(" and ")));
    }
    out
}
impl Agent<'_> {
    /// Observe `session_end` as the agent goes away. Hooks cannot block teardown; their notices
    /// are recorded on the session. Called from the `Drop` impl in `compaction_runtime`, after
    /// compaction cancellation.
    pub(super) fn observe_session_end(&self) {
        if !self.session_lifecycle {
            return;
        }
        let plugins = self.hook_plugins();
        struct ClosePlugins(Option<Arc<tools::plugin::Host>>);
        impl Drop for ClosePlugins {
            fn drop(&mut self) {
                if let Some(host) = &self.0 {
                    host.close();
                }
            }
        }
        let _close = ClosePlugins(plugins.clone());
        if !hooks::wanted(self.tool_hooks(), plugins.as_deref(), "session_end") {
            return;
        }
        let root = self.workspace.root().to_path_buf();
        let interrupt = std::sync::atomic::AtomicBool::new(false);
        let payload = json!({"session_id":self.session.id()});
        let notices = hooks::observe(
            self.tool_hooks(),
            plugins.as_deref(),
            &root,
            "session_end",
            payload,
            &interrupt,
        );
        for notice in notices {
            let _ = self.record(
                self.cur_step,
                "hook_notice",
                json!({"event":"session_end","notice":notice}),
            );
        }
    }

    /// Open the session lifecycle for a session new to this process: fires `session_start` and
    /// arms `session_end` for teardown. A rebuild of the same session uses
    /// `continue_session_lifecycle` instead.
    pub fn begin_session_lifecycle(&mut self) {
        self.session_lifecycle = true;
        let plugins = self.hook_plugins();
        let root = self.workspace.root().to_path_buf();
        let payload = json!({"session_id":self.session.id()});
        let notices = hooks::observe(
            self.tool_hooks(),
            plugins.as_deref(),
            &root,
            "session_start",
            payload,
            self.interrupt.as_ref(),
        );
        for notice in notices {
            let _ = self.record(
                0,
                "hook_notice",
                json!({"event":"session_start","notice":notice}),
            );
        }
    }

    /// Take over the session lifecycle of the agent this one replaces, without firing
    /// `session_start` again. `session_end` fires when this agent goes away.
    pub fn continue_session_lifecycle(&mut self) {
        self.session_lifecycle = true;
    }

    /// Called before a same-session rebuild drops this agent: the replacement continues the
    /// session, so this agent must not fire `session_end`.
    pub fn hand_off_session_lifecycle(&mut self) {
        self.session_lifecycle = false;
    }

    /// Fire `session_end` while this agent still owns its session. Callers that take the session
    /// out of the agent (to hand it to the next agent) must call this first, or the event reports
    /// the placeholder session.
    pub fn end_session_lifecycle(&mut self) {
        self.observe_session_end();
        self.session_lifecycle = false;
        self.end_local_session();
    }
}

impl<'a> Agent<'a> {
    /// Full interactive/print agent for the project in `layout`.
    pub fn interactive(
        config: &'a Config,
        mut backend: Backend,
        layout: Layout,
        mut session: Session,
        subagent_backend: Option<(String, Backend)>,
        store: Option<&'a Store>,
        interrupt: Arc<AtomicBool>,
    ) -> Result<Self> {
        backend.bind_session(config, &mut session)?;
        let auth_config = session_auth_config(config, &backend, &mut session)?;
        let subagent_backend =
            if auth_config.for_role("small").provider.as_deref() == Some("chatgpt") {
                None
            } else {
                subagent_backend
            };
        let mut version = match store {
            Some(store) => store.active()?,
            None => Version {
                id: 0,
                parent: None,
                kind: "config".into(),
                harness: config.harness.clone(),
                adapter: None,
                status: "active".into(),
            },
        };
        let legacy_memory_migrated =
            crate::config::types::migrate_memory_tools(&mut version.harness);
        config.validate_harness(&version.harness)?;
        version.harness = config.effective_harness(&version.harness)?;
        let memory = if config.memory.backend
            == crate::config::memory_backend::MemoryBackendId::Provenance
        {
            Some(Arc::new(ProvenanceMemory::open(&layout)?))
        } else {
            None
        };
        let memory_runtime = if matches!(
            config.memory.backend,
            crate::config::memory_backend::MemoryBackendId::Off
                | crate::config::memory_backend::MemoryBackendId::Provenance
        ) {
            None
        } else {
            Some(
                crate::storage::memory_backend::MemoryRuntime::open_configured(
                    &layout,
                    config,
                    memory_authority(&session, 0),
                )?,
            )
        };
        if let Some(runtime) = &memory_runtime {
            runtime.bind_owner_cancel(interrupt.clone());
        }
        let codebase = if config.codebase.enabled {
            Some(Arc::new(Codebase::open(&layout)?))
        } else {
            None
        };
        let history = if config.codebase.enabled && config.codebase.history {
            Some(Arc::new(crate::storage::history::ChangeHistory::open(
                &layout,
            )?))
        } else {
            None
        };
        let checkpoints = Arc::new(Checkpoints::new(
            layout.checkpoints_dir().join(session.id()),
            layout.project_root.clone(),
        )?);
        let mode = config.resolved_edit_mode(&version.harness);
        layout.ensure_project_outside_storage()?;
        let workspace = configure(
            Workspace::new(&layout.project_root)?,
            config,
            mode,
            Some(&layout),
            Some(checkpoints.clone()),
        )?
        .with_artifacts(tools::artifact::ArtifactStore::new(
            &layout.state_dir.join("artifacts"),
        )?);
        let skills = skills::discover(&layout);
        let parts = PromptParts {
            soul: context::soul(&layout),
            overrides: context::system_overrides(&layout),
            context: context::discover(
                &layout,
                config.experimental.context_overlap_dedup,
                &config.foreign_sources,
            ),
            skills: skills::catalog_block(&skills),
            // Standing memory guidance and auto-learn guidance; children receive the same text in `child.rs`.
            memory: [
                crate::storage::memory_backend::memory_guidance(&config.memory),
                crate::storage::memory_backend::autolearn_instructions(&config.memory),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n\n"),
            system_template: None,
            system_template_error: None,
        };
        layout.ensure_project_outside_storage()?;
        let always = approval::load(&layout.state_dir.join("permissions.toml"))?;
        let context_rule_catalog = (config.compaction.experimental_context_management
            && config.portable_rules.enabled)
            .then(|| Arc::new(rules::Catalog::discover(&layout, workspace.root())));
        let mut agent = Self {
            cancellation: Default::default(),
            step_probe: None,
            session_lifecycle: false,
            config,
            auth_config,
            backend: Some(backend),
            experience: if config.improve.enabled && config.assistance.enabled {
                crate::storage::experience::Experience::open(&layout)
                    .ok()
                    .map(Arc::new)
            } else {
                None
            },
            assistance: assistance::State::restore(&session.control_state()),
            modes: modes::State::restore(&session.control_state()),
            layout: Some(layout),
            session,
            workspace,
            ext: Registry::standard(config),
            background_notices: Vec::new(),
            todo: TodoList::default(),
            memory,
            memory_runtime,
            codebase,
            history,
            checkpoints: Some(checkpoints),
            store,
            approval: config.approval,
            always,
            interrupt,
            plan_mode: false,
            session_cost: None,
            tan_jobs: modes::tan::Jobs::default(),
            pending_plan: None,
            model_override: None,
            plan_model_prior: None,
            latest_plan: None,
            callback_question_results: Vec::new(),
            pending_context_rollover: None,
            exploration: Default::default(),
            context_notes_reminder_boundary: None,
            active_side_namespace: None,
            context_rule_catalog,
            hermetic: false,
            domain_override: None,
            trajectory: None,
            last_trajectory: None,
            training_capture: training::Capture::default(),
            logging: true,
            verifications: Vec::new(),
            behavioral_checks: Vec::new(),
            step_checks: StepChecks::default(),
            portable_callback_intercepted: false,
            edit_preview_intercepted: false,
            cur_step: 0,
            last_prompt_tokens: 0,
            context_meter: context_meter::ContextMeter::default(),
            reported_messages: 0,
            version,
            system: String::new(),
            system_messages: Vec::new(),
            agents_messages: Vec::new(),
            parts,
            subagent_backend,
            role_backends: BTreeMap::new(),
            context_cap: None,
            route: Default::default(),
            enabled_tools: HashSet::new(),
            lean_tools: false,
            local_escalation: Default::default(),
            local_session: Default::default(),
            metrics: AgentMetrics::default(),
            usage_scope: llm::usage::Scope::default().next_turn(config.harness.max_steps)?,
            compaction_runtime: compaction_runtime::CompactionRuntime::default(),
            memory_controller: memory_controller::Controller::default(),
            usage_cost_base: 0.0,
            last_auto_thinking: None,
            reasoning_recovery: reasoning::Recovery::default(),
            live_tasks: live_tasks::LiveTasks::default(),
            superseded: evidence::Supersession::default(),
            legacy_memory_migrated,
            accepted_live_steering: Vec::new(),
            pending_tool_steering: Vec::new(),
            request_prefix: Default::default(),
            workflow_metrics: Default::default(),
            efficiency_metrics: Default::default(),
            workflow_started: Instant::now(),
            request_phase: crate::adaptive::State::begin(
                crate::learn::task_policy::Kind::General,
                "",
            ),
            task_policy: crate::learn::task_policy::Policy::for_prompt(
                "",
                config.reasoning_guard.max_pre_action_secs,
            ),
            repair_tracker: Default::default(),
            repair_sequence: 0,
            task_packet: Default::default(),
        };
        if let Some(memory) = &agent.memory {
            for tool in
                crate::storage::provenance::tools(memory.clone(), agent.session.id().to_owned())
            {
                agent.ext.register(tool);
            }
        }
        if let Some(cache) = &agent.codebase {
            agent.ext.register(crate::storage::codebase::tool(
                cache.clone(),
                agent.session.id().to_owned(),
            ));
        }
        if let Some(history) = &agent.history {
            agent
                .ext
                .register(crate::storage::history::tool(history.clone()));
        }
        agent.restore_control()?;
        agent.rebuild_system();
        Ok(agent)
    }

    /// Hermetic agent for sealed evaluation and self-play: no user memory, skills, context files or
    /// persistence of its own. With a `store` it records trajectories (self-play only).
    pub(super) fn hermetic(
        config: &'a Config,
        backend: Backend,
        version: &Version,
        workspace: &Path,
        store: Option<&'a Store>,
        domain: Option<String>,
    ) -> Result<Self> {
        let mut version = version.clone();
        // Bound evaluation cost: a stuck candidate must fail, not run for an hour.
        version.harness.max_steps = version.harness.capped_steps(24);
        let legacy_memory_migrated =
            crate::config::types::migrate_memory_tools(&mut version.harness);
        config.validate_harness(&version.harness)?;
        let mut strict = config.clone();
        strict.sandbox.mode = SandboxMode::Strict;
        // Hermetic evaluation measures the harness itself, without the local-route grammar default.
        let mode = config.base_edit_mode(&version.harness);
        let workspace = configure_strict(Workspace::new(workspace)?, &strict, mode)?;
        let mut agent = Self {
            cancellation: Default::default(),
            step_probe: None,
            session_lifecycle: false,
            config,
            auth_config: config.clone(),
            backend: Some(backend.without_steering()),
            experience: None,
            assistance: Default::default(),
            layout: None,
            session: Session::ephemeral(),
            workspace,
            ext: Registry::new(),
            background_notices: Vec::new(),
            todo: TodoList::default(),
            memory: None,
            codebase: None,
            history: None,
            memory_runtime: None,
            checkpoints: None,
            store,
            approval: Approval::Auto,
            always: Vec::new(),
            plan_mode: false,
            session_cost: None,
            tan_jobs: modes::tan::Jobs::default(),
            pending_plan: None,
            model_override: None,
            plan_model_prior: None,
            latest_plan: None,
            callback_question_results: Vec::new(),
            pending_context_rollover: None,
            exploration: Default::default(),
            modes: modes::State::default(),
            context_notes_reminder_boundary: None,
            active_side_namespace: None,
            context_rule_catalog: None,
            interrupt: Arc::new(AtomicBool::new(false)),
            hermetic: true,
            domain_override: domain,
            trajectory: None,
            last_trajectory: None,
            training_capture: training::Capture::default(),
            logging: true,
            verifications: Vec::new(),
            behavioral_checks: Vec::new(),
            step_checks: StepChecks::default(),
            portable_callback_intercepted: false,
            edit_preview_intercepted: false,
            cur_step: 0,
            last_prompt_tokens: 0,
            context_meter: context_meter::ContextMeter::default(),
            reported_messages: 0,
            version,
            workflow_metrics: Default::default(),
            efficiency_metrics: Default::default(),
            workflow_started: Instant::now(),
            request_phase: crate::adaptive::State::begin(
                crate::learn::task_policy::Kind::General,
                "",
            ),
            task_policy: crate::learn::task_policy::Policy::for_prompt(
                "",
                config.reasoning_guard.max_pre_action_secs,
            ),
            repair_tracker: Default::default(),
            repair_sequence: 0,
            task_packet: Default::default(),
            system: String::new(),
            system_messages: Vec::new(),
            agents_messages: Vec::new(),
            parts: PromptParts {
                soul: None,
                overrides: (None, None),
                context: Vec::new(),
                skills: String::new(),
                memory: String::new(),
                system_template: None,
                system_template_error: None,
            },
            subagent_backend: None,
            role_backends: BTreeMap::new(),
            context_cap: None,
            route: Default::default(),
            enabled_tools: HashSet::new(),
            lean_tools: false,
            local_escalation: Default::default(),
            local_session: Default::default(),
            metrics: AgentMetrics::default(),
            usage_scope: llm::usage::Scope::default().next_turn(config.harness.max_steps)?,
            compaction_runtime: compaction_runtime::CompactionRuntime::default(),
            memory_controller: memory_controller::Controller::default(),
            usage_cost_base: 0.0,
            last_auto_thinking: None,
            reasoning_recovery: reasoning::Recovery::default(),
            live_tasks: live_tasks::LiveTasks::default(),
            superseded: evidence::Supersession::default(),
            legacy_memory_migrated,
            accepted_live_steering: Vec::new(),
            pending_tool_steering: Vec::new(),
            request_prefix: Default::default(),
        };
        agent.rebuild_system();
        Ok(agent)
    }

    /// Deterministic pointer to enabled skills whose names or descriptions match the user's words.
    pub(super) fn skill_hint(&self, user_text: &str) -> Option<String> {
        if self.modes.skillful == Some(false) {
            return None;
        }
        let layout = self.layout.as_ref()?;
        let visible: Vec<_> = skills::discover(layout)
            .into_iter()
            .filter(|skill| self.modes.launch.skill_visible(&skill.name))
            .collect();
        skills::match_hint(&visible, user_text)
    }

    pub(super) fn rebuild_system(&mut self) {
        let offered = self.offered();
        // The interceptor names a dedicated tool only when this request offers it.
        self.workspace = self
            .workspace
            .clone()
            .with_bash_interceptor(self.config.bash_interceptor.clone(), offered.clone());
        let discoverable = self.discoverable_tools(&offered);
        let lean = self.lean_local() || self.local_escalated();
        let built = prompt::system_messages(prompt::Inputs {
            cwd: self.workspace.root(),
            harness: &self.version.harness,
            edit_mode: self.edit_mode(),
            soul: self.parts.soul.clone(),
            overrides: self.parts.overrides.clone(),
            context: &self.parts.context,
            skills: if (self.minimal_tools() || lean) && !offered.iter().any(|tool| tool == "skill")
            {
                String::new()
            } else {
                self.parts.skills.clone()
            },
            memory: self.parts.memory.clone(),
            tools: &offered,
            discoverable: &discoverable,
            template: self.parts.system_template.as_ref(),
            lean,
        });
        let mut messages = match built {
            Ok(messages) => {
                self.parts.system_template_error = None;
                messages
            }
            // Keep the last good prompt; turns refuse to run until the template renders again.
            Err(error) => {
                self.parts.system_template_error = Some(format!("{error:#}"));
                return;
            }
        };
        let mut system = String::new();
        if !self.hermetic {
            system.push_str(&capability_prose(&offered, &discoverable));
            if self.catalog().iter().any(|name| name == "mcp_servers") {
                system.push_str("\nFor installed MCP integrations, discover mcp_servers to inspect actual connection status and registered capabilities. Configuration or installation alone does not mean a server connected. Discover or load exact listed tool names for next-round use; normal approvals still apply.");
            }
        }
        if self.plan_mode {
            system.push_str("\n\nPlan mode: explore using read-only tools and produce a concrete plan. Do not mutate files or execute code. Use exit_plan with your complete plan to request user approval before implementation.");
        }
        if let Some(instruction) = self.external_thinking_instruction() {
            system.push_str("\n\n");
            system.push_str(instruction);
        }
        if let Some(servers) = prompt::mcp_instructions(self.ext.mcp_instructions()) {
            messages.push(json!({"role":"system","content":servers}));
        }
        if !system.is_empty() {
            messages.push(json!({"role":"system","content":system}));
        }
        llm::prompt::order(&mut messages);
        if messages == self.system_messages {
            return;
        }
        self.system = messages
            .iter()
            .filter_map(|message| message["content"].as_str())
            .collect();
        self.agents_messages = messages
            .iter()
            .filter(|message| message["dial_agents_md"] == true)
            .cloned()
            .collect();
        self.system_messages = messages;
    }

    /// Load, compile and validate a custom system-prompt template (`--system-prompt-template`).
    /// A template that fails to render leaves the previous prompt in place and is an error.
    pub fn set_system_prompt_template(&mut self, path: &Path) -> Result<()> {
        let template = prompt_template::compile(&prompt_template::load_file(path)?)?;
        let previous = self.parts.system_template.replace(template);
        self.rebuild_system();
        if let Some(error) = self.parts.system_template_error.clone() {
            self.parts.system_template = previous;
            self.rebuild_system();
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    pub(crate) fn system_messages(&self) -> Vec<Value> {
        self.system_messages.clone()
    }

    /// The edit grammar in effect: an escalated local route uses `replace` unless pinned; hermetic
    /// runs ignore the local-route default.
    pub(super) fn edit_mode(&self) -> crate::config::types::EditMode {
        if self.hermetic {
            return self.config.base_edit_mode(&self.version.harness);
        }
        self.escalated_edit_mode()
            .unwrap_or_else(|| self.config.resolved_edit_mode(&self.version.harness))
    }

    /// Pick up a version the improvement loop admitted since this session started (between turns only,
    /// so a turn never changes harness mid-flight).
    pub(super) fn refresh_version(&mut self, ui: &mut dyn Ui, prompt: &str) -> Result<()> {
        let Some(store) = self.store else {
            return Ok(());
        };
        if self.hermetic || self.version.id == 0 {
            return Ok(());
        }
        let mut active = crate::learn::profiles::select(store, store.active()?, prompt)?;
        if active.id == self.version.id {
            return Ok(());
        }
        let migrated = crate::config::types::migrate_memory_tools(&mut active.harness);
        if migrated {
            ui.diagnostic("Migrated legacy memory tool selection to provenance memory tools for this runtime; admitted history is unchanged.");
        }
        self.config.validate_harness(&active.harness)?;
        active.harness = self.config.effective_harness(&active.harness)?;
        crate::learn::improve::verify_adapter(store, &active)?;
        self.version = active;
        self.workspace = self.workspace.clone().with_edit_mode(self.edit_mode());
        self.rebuild_system();
        ui.diagnostic(&format!("now running accepted version v{} (selected for this task from measured evaluation results)", self.version.id));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
