//! Agent construction, session lifecycle hooks and system prompt assembly.

use crate::agent::*;

/// System-prompt clauses for judgement the tool descriptions do not carry. A clause is emitted only
/// when its tool is offered or listed as loadable; mechanics (next-round activation, permissions,
/// batching, live runs) stay in each tool's own description.
fn capability_prose(offered: &[String], discoverable: &[(String, String)]) -> String {
    let available = |name: &str| {
        offered.iter().any(|tool| tool.as_str() == name)
            || discoverable.iter().any(|(tool, _)| tool.as_str() == name)
    };
    // Discovery itself (`tool_search`/`tool_load`) is stated once in the environment section.
    let mut out = String::new();
    // Offered web tools are used directly; only loadable ones need loading first, so the model is
    // never told to rediscover a tool already in its schema.
    let offered_tool = |name: &str| offered.iter().any(|tool| tool.as_str() == name);
    let (ready, loadable): (Vec<&str>, Vec<&str>) = ["web_search", "web_fetch"]
        .into_iter()
        .filter(|tool| available(tool))
        .partition(|tool| offered_tool(tool));
    if !ready.is_empty() || !loadable.is_empty() {
        let mut steps = Vec::new();
        if !ready.is_empty() {
            steps.push(format!("use {}", ready.join(" and ")));
        }
        if !loadable.is_empty() {
            steps.push(format!("load {} first", loadable.join(" and ")));
        }
        out.push_str(&format!("\nFor live web research, {}; cite source URLs and report real search or network failures rather than inventing results.", steps.join("; ")));
    }
    out
}
/// The skills catalog plus one line naming how to load a skill with the tools this prompt offers:
/// `skill` directly, or `tool_load` for it when only discovery is offered. Decided from the policy
/// offer, so loading a tool mid-session leaves the cached prefix unchanged.
fn with_skill_loader(catalog: &str, tools: &[String]) -> String {
    let has = |name: &str| tools.iter().any(|tool| tool == name);
    if catalog.trim().is_empty() {
        return catalog.to_owned();
    }
    let loader = if has("skill") {
        "Load a skill by name with the `skill` tool."
    } else if has("tool_search") || has("tool_load") {
        "To load a skill, first load the `skill` tool with `tool_search` names:[\"skill\"], then call it with the skill name."
    } else {
        return catalog.to_owned();
    };
    format!("{}\n{loader}\n", catalog.trim_end())
}

impl Agent<'_> {
    /// Observe `session_end` as the agent goes away. Hooks cannot block teardown; their notices
    /// are recorded on the session. Called from the `Drop` impl in `compaction_runtime`, after
    /// compaction cancellation.
    pub(in crate::agent) fn observe_session_end(&self) {
        if !self.session_lifecycle {
            return;
        }
        if !crate::agent::safety::hooks::wanted(self.tool_hooks(), "session_end") {
            return;
        }
        let root = self.workspace.root().to_path_buf();
        let interrupt = std::sync::atomic::AtomicBool::new(false);
        let payload = json!({"session_id":self.session.id()});
        let notices = crate::agent::safety::hooks::observe(
            self.tool_hooks(),
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
        self.initialize_coordination();
        self.session_lifecycle = true;
        let root = self.workspace.root().to_path_buf();
        let payload = json!({"session_id":self.session.id()});
        let notices = crate::agent::safety::hooks::observe(
            self.tool_hooks(),
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

    /// Queue a bounded `additional_context` note from a lifecycle hook for the next model
    /// request. Notes are transient: delivered once, never written to session history.
    pub(in crate::agent) fn queue_hook_context(&mut self, note: Option<String>) {
        /// Notes held between requests; the oldest are dropped past this.
        const HELD: usize = 8;
        let Some(note) = note.filter(|note| !note.trim().is_empty()) else {
            return;
        };
        if !self.hook_context.contains(&note) {
            self.hook_context.push(note);
        }
        let excess = self.hook_context.len().saturating_sub(HELD);
        self.hook_context.drain(..excess);
    }

    /// The queued hook notes as transient request messages, emptying the queue.
    pub(in crate::agent) fn take_hook_context(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.hook_context)
            .into_iter()
            .map(|note| json!({"role":"user","content":note,"dial_transient":true}))
            .collect()
    }

    /// `after_provider_response`: tell observers about a completed model response. Hooks cannot
    /// change it; their notices are recorded on the session.
    pub(in crate::agent) fn observe_provider_response(
        &self,
        model: &str,
        reply: &crate::llm::Reply,
        latency: std::time::Duration,
    ) {
        let mut notices = Vec::new();
        crate::agent::safety::hooks::provider_response(
            self.tool_hooks(),
            self.workspace.root(),
            model,
            reply,
            latency,
            &mut |notice| notices.push(notice.to_owned()),
        );
        for notice in notices {
            let _ = self.record(
                self.cur_step,
                "hook_notice",
                json!({"event":"after_provider_response","notice":notice}),
            );
        }
    }

    /// `assistant_message`: a final assistant reply is complete. The text is already persisted
    /// and is never replaced or blocked here; a hook's `additional_context` is delivered with the
    /// next request (the next turn, since this reply ends the step).
    pub(in crate::agent) fn observe_assistant_message(
        &mut self,
        text: &str,
        step: u32,
        ui: &mut dyn Ui,
    ) {
        if !crate::agent::safety::hooks::wanted(self.tool_hooks(), "assistant_message") {
            return;
        }
        let payload = json!({
            "session_id": self.session.id(),
            "step": step,
            "text": crate::agent::window::prompt::truncate_chars(text, 8000),
        });
        let configured_hooks = self.tool_hooks();
        let root = self.workspace.root();
        let interrupt = self.interrupt.as_ref();
        let outcome = crate::agent::safety::hooks::with_heartbeat(ui, || {
            crate::agent::safety::hooks::fire_with(
                configured_hooks,
                root,
                "assistant_message",
                None,
                payload,
                interrupt,
            )
        });
        for notice in &outcome.notices {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Hook,
                notice,
            ));
        }
        // Observe-only: a block or failure never rewrites or withholds the reply.
        let context = outcome.context_note();
        self.queue_hook_context(context);
    }

    /// Run a cancelable `session_before_branch` / `session_before_switch` hook. `Err(reason)`
    /// means a hook refused the change and the caller must leave the session as it is.
    pub fn gate_session_change(&self, event: &str, detail: Value) -> Result<(), String> {
        let root = self.workspace.root().to_path_buf();
        let mut payload = detail;
        payload["session_id"] = json!(self.session.id());
        let mut notices = Vec::new();
        let result = crate::agent::safety::hooks::gate(
            self.tool_hooks(),
            &root,
            event,
            payload,
            self.interrupt.as_ref(),
            &mut |notice| notices.push(notice.to_owned()),
        );
        for notice in notices {
            let _ = self.record(
                self.cur_step,
                "hook_notice",
                json!({"event":event,"notice":notice}),
            );
        }
        result.map_err(|reason| format!("{event} hook refused: {reason}"))
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
            Some(Arc::new(
                crate::tools::knowledge::history::ChangeHistory::open(&layout)?,
            ))
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
        .with_artifacts(crate::storage::artifact::ArtifactStore::new(
            &layout.state_dir.join("artifacts"),
        )?);
        // Cargo build output is scoped per session so concurrent sessions do not share one target lock.
        let workspace = workspace.with_cargo_session(session.id())?;
        {
            // A new session takes over an idle earlier session's build output (one rename) so its
            // first build is incremental; output that has gone stale is removed in the background.
            crate::tools::exec::cargo_target::adopt_idle(
                workspace.root(),
                session.id(),
                crate::tools::exec::cargo_target::ADOPT_AFTER,
            );
            let root = workspace.root().to_path_buf();
            let keep = session.id().to_owned();
            let _ = std::thread::Builder::new()
                .name("dial-cargo-target-prune".into())
                .spawn(move || {
                    crate::tools::exec::cargo_target::prune_stale(
                        &root,
                        &keep,
                        crate::tools::exec::cargo_target::STALE_AFTER,
                    )
                });
        }
        let skills = skills::discover(&layout);
        let parts = PromptParts {
            soul: crate::agent::window::context::soul(&layout),
            overrides: crate::agent::window::context::system_overrides(&layout),
            context: crate::agent::window::context::discover(
                &layout,
                config.experimental.context_overlap_dedup,
                &config.foreign_sources,
                &config.context_files.extra,
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
        let always =
            crate::agent::safety::approval::load(&layout.state_dir.join("permissions.toml"))?;
        let context_rule_catalog = (config.compaction.experimental_context_management
            && config.portable_rules.enabled)
            .then(|| {
                Arc::new(crate::agent::safety::rules::Catalog::discover(
                    &layout,
                    workspace.root(),
                    &config.portable_rules,
                ))
            });
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
            assistance: crate::agent::model::assistance::State::restore(&session.control_state()),
            modes: modes::State::restore(&session.control_state()),
            workflow_contracts: Default::default(),
            layout: Some(layout),
            session,
            workspace,
            ext: Registry::standard(config, {
                let mut native = crate::computer::tools(config);
                native.extend(crate::weights::tool::tools(config));
                native
            }),
            background_notices: Vec::new(),
            exit_bound_jobs_noticed: Default::default(),
            hook_context: Vec::new(),
            peer_state: Default::default(),
            monitor_state: Default::default(),
            shared_runs: Default::default(),
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
            training_capture: crate::agent::memory::training::Capture::default(),
            logging: true,
            verifications: Vec::new(),
            behavioral_checks: Vec::new(),
            step_checks: StepChecks::default(),
            portable_callback_intercepted: false,
            edit_preview_intercepted: false,
            pending_batch_approvals: Vec::new(),
            pending_check: None,
            verify_batch: Default::default(),
            cur_step: 0,
            last_prompt_tokens: 0,
            context_meter: crate::agent::window::context_meter::ContextMeter::default(),
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
            tool_routing: Default::default(),
            compaction_retry_carry: None,
            lean_tools: false,
            local_escalation: Default::default(),
            local_session: Default::default(),
            metrics: AgentMetrics::default(),
            usage_scope: llm::usage::Scope::default().next_turn(config.harness.max_steps)?,
            cache_warm: None,
            preparation: Default::default(),
            compaction_runtime:
                crate::agent::compact::compaction_runtime::CompactionRuntime::default(),
            memory_controller: crate::agent::memory::memory_controller::Controller::default(),
            usage_cost_base: 0.0,
            session_cost_warn: None,
            last_auto_thinking: None,
            reasoning_recovery: crate::agent::workflow::reasoning::Recovery::default(),
            live_tasks: crate::agent::workers::live_tasks::LiveTasks::default(),
            side_questions: Default::default(),
            superseded: crate::agent::workflow::evidence::Supersession::default(),
            legacy_memory_migrated,
            accepted_live_steering: Vec::new(),
            pending_tool_steering: Vec::new(),
            undrafted_steering: Vec::new(),
            request_prefix: Default::default(),
            workflow_metrics: Default::default(),
            efficiency_metrics: Default::default(),
            prefix_resets: Default::default(),
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
            agent
                .ext
                .register(crate::tools::knowledge::provenance::tool(
                    memory.clone(),
                    agent.session.id().to_owned(),
                ));
        }
        if let Some(cache) = &agent.codebase {
            agent.ext.register(crate::tools::knowledge::codebase::tool(
                cache.clone(),
                agent.session.id().to_owned(),
            ));
        }
        if let Some(history) = &agent.history {
            agent
                .ext
                .register(crate::tools::knowledge::history::tool(history.clone()));
        }
        agent.restore_control()?;
        agent.record_main_model_route("Main backend initialized for this process from the supplied session selection; earlier launch-selection provenance unavailable (not recorded)");
        agent.initialize_coordination();
        agent.rebuild_system();
        Ok(agent)
    }

    /// Hermetic agent for sealed evaluation and self-play: no user memory, skills, context files or
    /// persistence of its own. With a `store` it records trajectories (self-play only).
    pub(in crate::agent) fn hermetic(
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
            exit_bound_jobs_noticed: Default::default(),
            hook_context: Vec::new(),
            peer_state: Default::default(),
            monitor_state: Default::default(),
            shared_runs: Default::default(),
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
            training_capture: crate::agent::memory::training::Capture::default(),
            logging: true,
            verifications: Vec::new(),
            behavioral_checks: Vec::new(),
            step_checks: StepChecks::default(),
            portable_callback_intercepted: false,
            edit_preview_intercepted: false,
            pending_batch_approvals: Vec::new(),
            pending_check: None,
            verify_batch: Default::default(),
            cur_step: 0,
            last_prompt_tokens: 0,
            context_meter: crate::agent::window::context_meter::ContextMeter::default(),
            reported_messages: 0,
            version,
            workflow_metrics: Default::default(),
            efficiency_metrics: Default::default(),
            prefix_resets: Default::default(),
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
            tool_routing: Default::default(),
            compaction_retry_carry: None,
            lean_tools: false,
            local_escalation: Default::default(),
            local_session: Default::default(),
            metrics: AgentMetrics::default(),
            usage_scope: llm::usage::Scope::default().next_turn(config.harness.max_steps)?,
            cache_warm: None,
            preparation: Default::default(),
            compaction_runtime:
                crate::agent::compact::compaction_runtime::CompactionRuntime::default(),
            memory_controller: crate::agent::memory::memory_controller::Controller::default(),
            usage_cost_base: 0.0,
            session_cost_warn: None,
            last_auto_thinking: None,
            reasoning_recovery: crate::agent::workflow::reasoning::Recovery::default(),
            live_tasks: crate::agent::workers::live_tasks::LiveTasks::default(),
            workflow_contracts: Default::default(),
            side_questions: Default::default(),
            superseded: crate::agent::workflow::evidence::Supersession::default(),
            legacy_memory_migrated,
            accepted_live_steering: Vec::new(),
            pending_tool_steering: Vec::new(),
            undrafted_steering: Vec::new(),
            request_prefix: Default::default(),
        };
        agent.rebuild_system();
        Ok(agent)
    }

    /// Deterministic pointer to enabled skills whose names or descriptions match the user's words.
    pub(in crate::agent) fn skill_hint(&self, user_text: &str) -> Option<String> {
        if self.modes.skillful == Some(false) {
            return None;
        }
        let layout = self.layout.as_ref()?;
        let visible: Vec<_> = skills::discover(layout)
            .into_iter()
            .filter(|skill| self.modes.launch.skill_visible(&skill.name))
            .collect();
        // The hint names the route this request actually has: the `skill` tool itself, or
        // `tool_load` to add it. With neither offered there is nothing to act on.
        let offered = self.offered();
        let has = |name: &str| offered.iter().any(|tool| tool == name);
        let skill_offered = has("skill");
        if !skill_offered && !has("tool_load") {
            return None;
        }
        skills::match_hint(&visible, user_text, skill_offered)
    }

    pub(in crate::agent) fn rebuild_system(&mut self) {
        let offered = self.offered();
        // The interceptor names a dedicated tool only when this request offers it. The small and
        // lean local tiers refuse edits that add parser errors (the edit tool's `allow_syntax_errors`).
        let syntax_block = matches!(
            self.local_tier(),
            Some(
                crate::agent::model::local_profile::Tier::Small
                    | crate::agent::model::local_profile::Tier::Lean
            )
        );
        self.workspace = self
            .workspace
            .clone()
            .with_bash_interceptor(
                self.config.bash_interceptor.clone(),
                self.interceptor_tools(&offered),
            )
            .with_syntax_block(syntax_block);
        let stable = self.stable_tool_offer() && self.parts.system_template.is_none();
        let mut policy_tools = if stable {
            self.stable_core_tools()
        } else {
            offered.clone()
        };
        // Guidance that names a consolidated member stays while its successor is offered.
        let members: Vec<String> = policy_tools
            .iter()
            .flat_map(|name| self.ext.folded_members(name))
            .map(str::to_owned)
            .collect();
        policy_tools.extend(members);
        let discoverable = if stable {
            Vec::new()
        } else {
            self.discoverable_tools(&offered)
        };
        // A window-overflow fallback needs the lean instructions as well as lean schemas.
        let lean = self.lean_tools || self.lean_local() || self.local_escalated();
        let built = crate::agent::window::prompt::system_messages(
            crate::agent::window::prompt::Inputs {
                cwd: self.workspace.root(),
                runtimes: {
                    let mut runtimes = String::new();
                    // Reported whether or not eval is offered: without eval the shell is the only
                    // place helpers can run, and that is when a wrong-language helper costs most.
                    if !self.hermetic && self.tool_eligible("bash") {
                        runtimes.push_str(&tools::exec::shell::interpreter_guidance());
                        runtimes.push_str(&tools::fs::system::session_inventory().compact());
                    }
                    if !self.hermetic && self.tool_eligible("eval") {
                        runtimes.push_str(&tools::exec::eval::runtime_guidance(
                            self.workspace.root(),
                            self.config,
                        ));
                        if self.config.eval.portable_helpers {
                            runtimes.push_str("- Counterexample search in eval: `dial.search(prop, {gen})` and `dial.compare(f, g, {gen})` try edge cases and seeded random inputs from `dial.gen` (int, float, string, array, tuple, record, pick, oneOf, bool) and shrink a failure; compare a tricky implementation (a regex, a parser) against a plain reference.\n");
                        }
                    }
                    runtimes
                },
                harness: &self.version.harness,
                edit_mode: self.edit_mode(),
                soul: self.parts.soul.clone(),
                overrides: self.parts.overrides.clone(),
                context: &self.parts.context,
                skills: if (self.minimal_tools() || lean)
                    && !offered.iter().any(|tool| tool == "skill")
                {
                    String::new()
                } else {
                    with_skill_loader(&self.parts.skills, &policy_tools)
                },
                // Unattended runs defer `memory`; its guidance waits until the tool is loaded.
                memory: if self.modes.launch.unattended
                    && !offered
                        .iter()
                        .any(|tool| tool == crate::storage::memory_backend::TOOL_NAME)
                {
                    String::new()
                } else {
                    self.parts.memory.clone()
                },
                tools: &policy_tools,
                discoverable: &discoverable,
                template: self.parts.system_template.as_ref(),
                lean,
                context_overflow: self.lean_tools,
                concise_guidance: self.config.efficiency.enabled && !self.hermetic,
                prefer_subagents: self.config.task.prefer_subagents,
                one_call: self.local_one_call(),
                worked_example: self.local_one_call(),
                reasoning_strategy: if self.hermetic {
                    Default::default()
                } else {
                    self.config.experimental.reasoning_strategy
                },
            },
        );
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
            if !stable {
                system.push_str(&capability_prose(&offered, &discoverable));
            }
            if self.catalog().iter().any(|name| name == "mcp_servers") {
                // Trust and approval boundaries are stated once in the tool policy; this adds
                // only what the MCP catalog index does not: configuration is not a connection.
                system.push_str("\nConfigured MCP servers are not connection evidence; mcp_servers reports actual runtime status and registered tools.");
            }
        }
        // Plan mode, the external-thinking instruction and still-connecting MCP servers change
        // during a session, so they travel as a request-tail notice (`mode_notice`), keeping this
        // prefix constant.
        // Deferred or disallowed servers do not add instructions to unrelated requests. Loading
        // a tool rebuilds this prefix and restores its server's guidance before the next call.
        if let Some(servers) =
            crate::agent::window::prompt::mcp_instructions(self.ext.mcp_instructions_for(&offered))
        {
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
        self.agents_messages =
            crate::agent::window::prompt::agents_md_messages(&self.parts.context);
        self.system_messages = messages;
    }

    /// Session modes that can change between requests: plan mode, the external-thinking
    /// instruction and MCP servers still connecting. They are restated from current state on each
    /// request as a transient tail notice rather than written into the cached system prefix, so a
    /// toggle never rewrites that prefix and replay, resume and compaction need no extra record.
    pub(in crate::agent) fn mode_notice(&self) -> Option<String> {
        let mut lines = Vec::new();
        if self.plan_mode {
            lines.push("Plan mode is on: explore with read-only tools and produce a concrete plan; don't change files or run code yet. Call exit_plan with the complete plan to ask the user for approval before implementing.".to_owned());
        }
        if let Some(instruction) = self.external_thinking_instruction() {
            lines.push(instruction.to_owned());
        }
        let stable = self.stable_tool_offer() && self.parts.system_template.is_none();
        if !self.hermetic
            && !stable
            && let Some(servers) = self.ext.mcp_connecting()
        {
            lines.push(format!("MCP servers still connecting: {}. Their tools are not listed yet; tool_search and mcp_servers wait for them.", servers.join(", ")));
        }
        (!lines.is_empty()).then(|| {
            format!(
                "<dial-notice kind=\"session_mode\">\n{}\n</dial-notice>",
                lines.join("\n")
            )
        })
    }

    /// Load, compile and validate a custom system-prompt template (`--system-prompt-template`).
    /// A template that fails to render leaves the previous prompt in place and is an error.
    pub fn set_system_prompt_template(&mut self, path: &Path) -> Result<()> {
        let template = crate::agent::window::prompt_template::compile(
            &crate::agent::window::prompt_template::load_file(path)?,
        )?;
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
    pub(in crate::agent) fn edit_mode(&self) -> crate::config::types::EditMode {
        if self.hermetic {
            return self.config.base_edit_mode(&self.version.harness);
        }
        self.escalated_edit_mode()
            .unwrap_or_else(|| self.config.resolved_edit_mode(&self.version.harness))
    }

    /// Pick up a version the improvement loop admitted since this session started (between turns only,
    /// so a turn never changes harness mid-flight).
    pub(in crate::agent) fn refresh_version(
        &mut self,
        ui: &mut dyn Ui,
        prompt: &str,
    ) -> Result<()> {
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
            ui.diagnostic("Migrated legacy split memory tool selection to the single memory tool for this runtime; admitted history is unchanged.");
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
