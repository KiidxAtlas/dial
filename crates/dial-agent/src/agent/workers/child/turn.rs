//! Child-local phases over the same history, budget and live authority.
use super::*;
use crate::agent::run::engine::{Engine, Tick};
use std::ops::ControlFlow;

/// The former drive closure's mutable captures. Live authorities remain uniquely borrowed;
/// request and reply facts cross phases by value rather than cloning authority or history.
pub(super) struct ChildTurn<'a> {
    env: Env<'a>,
    job: &'a Job<'a>,
    backend: &'a mut Backend,
    adapter: Option<&'a Path>,
    approver: &'a mut dyn Approver,
    rule_state: &'a mut crate::agent::safety::rules::State,
    children: &'a ChildBackends,
    names: Vec<String>,
    schemas: Vec<Value>,
    messages: Vec<Value>,
    steps: u32,
    budget: u32,
    schema_retries: u32,
    paired_tools: usize,
    max_tokens: u32,
    recovering_output: bool,
    emissions: crate::agent::run::guidance::Emissions,
    turn_cancel: crate::runtime::cancellation::TurnCancellation,
    reasoning_recovery: reasoning::Recovery,
    request_prefix: crate::agent::window::request_prefix::Tracker,
    superseded: crate::agent::workflow::evidence::Supersession,
    pending_coordination: std::cell::RefCell<Vec<String>>,
    overflow_retried: bool,
    compaction_failed: bool,
    compaction_retry_floor: Option<u64>,
    active_settings: Option<(
        crate::agent::workers::live_tasks::control::WorkerSettings,
        crate::config::types::Config,
    )>,
    cached_memory_prompt: Option<(String, String)>,
    inspections: crate::agent::workflow::efficiency::Inspections,
    rule_catalog: crate::agent::safety::rules::Catalog,
    rule_agent: String,
    rule_regenerations: u32,
}

struct PreparedStep {
    prepared_recall: Option<crate::storage::memory_backend::PreparedRecall>,
    summary_step: bool,
    text_only: bool,
    research_step: bool,
}
struct GeneratedReply {
    result: Result<llm::Reply>,
    rule_deferred: Vec<crate::agent::safety::rules::Hit>,
}
struct AdmittedRound {
    reply: llm::Reply,
    recovered_original: Option<Value>,
}
struct DispatchRound {
    reply: llm::Reply,
    narration_warning: Option<String>,
}
struct RoundSettlement {
    narration_warning: Option<String>,
    portable_batch_intercepted: bool,
    round_messages: Vec<String>,
    investigation_stop: Option<String>,
}

impl<'a> ChildTurn<'a> {
    pub(super) fn prepare(
        env: Env<'a>,
        job: &'a Job<'a>,
        backend: &'a mut Backend,
        adapter: Option<&'a Path>,
        approver: &'a mut dyn Approver,
        rule_state: &'a mut crate::agent::safety::rules::State,
        children: &'a ChildBackends,
    ) -> std::result::Result<Self, String> {
        let mut names = toolset(job.spec, &env.harness.tools, env.ext);
        if env.config.task.can_spawn_at(job.depth)
            && env.harness.tools.iter().any(|name| name == "task")
            && !matches!(
                crate::agent::workers::subagents::spawn_policy(env.config, job),
                crate::agent::base::agent_defs::Spawns::None
            )
        {
            names.push("task".into());
        }
        names.retain(|name| {
            !matches!(
                name.as_str(),
                "context_notes" | "new_context" | "ask" | "checkpoint" | "rewind"
            )
        });
        if env.config.compaction.experimental_context_management {
            names.extend(
                ["context_notes", "new_context", "session_search"]
                    .into_iter()
                    .map(str::to_owned),
            );
        }
        if env.config.compaction.snapcompact.overlay
            && job.spec.name != "seance"
            && !env.harness.tools.is_empty()
            && env
                .config
                .active_model_profile()
                .is_none_or(|profile| profile.capabilities.tools != Some(false))
        {
            names.push("session_search".into());
        }
        if env.config.ask.enabled && env.harness.tools.iter().any(|name| name == "ask") {
            names.push("ask".into());
        }
        // A selected runtime serves the canonical memory tools itself. Without one, provenance's own
        // registered recall/retain stay available to the child like any other extension tool.
        names.retain(|name| {
            !crate::storage::memory_backend::TOOL_NAMES.contains(&name.as_str())
                || (children.memory.is_none()
                    && env.ext.contains(name)
                    && (job.spec.mutates || !matches!(name.as_str(), "retain" | "learn")))
        });
        if let Some(memory) = &children.memory {
            // Mirror the root offer: the runtime's tools unless an operator pinned the harness list.
            let pinned = env.config.harness.tools != config::tools::default_tools();
            names.extend(
                memory
                    .offered_names()
                    .into_iter()
                    .filter(|name| {
                        crate::storage::memory_backend::TOOL_NAMES.contains(name)
                            && (job.spec.mutates || !matches!(*name, "retain" | "learn"))
                            && (!pinned || env.harness.tools.iter().any(|loaded| loaded == name))
                    })
                    .map(str::to_owned),
            );
        }
        let checkpoint = env.config.checkpoint.enabled
            && job.allowed_tools.is_some_and(|tools| {
                tools.iter().any(|name| {
                    crate::agent::workflow::exploration::TOOL_NAMES.contains(&name.as_str())
                })
            });
        if checkpoint {
            names.extend(
                crate::agent::workflow::exploration::TOOL_NAMES
                    .into_iter()
                    .map(str::to_owned),
            );
            names.push("session_search".into());
        }
        let rule_root = env.workspace.root();
        let rule_catalog = if env.config.portable_rules.enabled {
            env.layout
                .map(|layout| {
                    crate::agent::safety::rules::Catalog::discover(
                        layout,
                        rule_root,
                        &env.config.portable_rules,
                    )
                })
                .unwrap_or_default()
        } else {
            crate::agent::safety::rules::Catalog::default()
        };
        for warning in &rule_catalog.warnings {
            approver.notice(warning);
        }
        let rule_agent = if job.kind.eq_ignore_ascii_case("main") {
            "sub".to_owned()
        } else {
            job.kind.to_lowercase()
        };
        rule_state.begin(rule_root, &job.prompt);
        let rule_regenerations = 0u32;
        if let Some(allowed) = job.allowed_tools {
            names.retain(|name| allowed.contains(name) || name == "artifact_read");
        }
        if checkpoint {
            for name in crate::agent::workflow::exploration::TOOL_NAMES {
                if !names.iter().any(|offered| offered == name) {
                    names.push(name.into());
                }
            }
        }
        if let Some(allowed) = env
            .config
            .active_model_profile()
            .and_then(|profile| profile.tools.as_ref())
        {
            names.retain(|name| allowed.contains(name));
        }
        if !["checkpoint", "rewind", "session_search"]
            .iter()
            .all(|required| names.iter().any(|name| name == required))
        {
            names.retain(|name| {
                !crate::agent::workflow::exploration::TOOL_NAMES.contains(&name.as_str())
            });
        }
        drop_root_bound(&mut names, env.ext, env.workspace.root());
        names.sort();
        names.dedup();
        if approver.live_coordination()
            && !names.is_empty()
            && env
                .config
                .active_model_profile()
                .and_then(|p| p.tools.as_ref())
                .is_none_or(|tools| tools.iter().any(|n| n == "message"))
        {
            names.push("message".into());
        }
        if let Some(allowed) = &job.resume_tools {
            names.retain(|name| allowed.contains(name));
        }
        if let Some(sink) = &children.transcript
            && let Err(error) =
                sink.execution_contract(&names, &env.config.selection().display_name())
        {
            return Err(format!("saving worker execution contract: {error:#}"));
        }
        let schemas = match schemas(env, &names) {
            Ok(schemas) => schemas,
            Err(error) => return Err(format!("{error:#}")),
        };
        if let Err(error) = crate::tools::surface::contracts::validate_history(
            &job.resume_messages,
            &schemas,
            env.ext,
        ) {
            return Err(format!("{error:#}"));
        }
        let mut messages = initial_messages(env, job, &names);
        for message in &job.resume_messages {
            if let Some(snapshot) = message.get("dial_file_handles") {
                let restored = serde_json::from_value(snapshot.clone())
                    .map_err(|error| error.to_string())
                    .and_then(|snapshot| env.workspace.restore_file_handles(&snapshot));
                if let Err(error) = restored {
                    return Err(format!("restoring worker file handles: {error}"));
                }
            }
        }
        // Independent child conversations reuse the same project cache without rebuilding the map.
        // Automatic retrieval follows the parent's ambient-context policy; explicit lookups stay available.
        // A resumed conversation already carries its first retrieval, so it is not added again.
        if job.resume_messages.is_empty()
            && env.config.codebase.enabled
            && env.config.codebase.ambient_context
            && let Some(layout) = env.layout
        {
            match crate::tools::knowledge::codebase::Codebase::open(layout).and_then(|cache| {
                cache.task_context(
                    &job.prompt,
                    crate::agent::workflow::efficiency::byte_limit(
                        env.config.codebase.context_max_bytes,
                    )
                    .min(if env.config.efficiency.enabled {
                        crate::agent::workflow::efficiency::byte_limit(
                            env.config.efficiency.ambient_context_bytes,
                        )
                    } else {
                        usize::MAX
                    }),
                )
            }) {
                Ok(context)
                    if !context.is_empty()
                        && !messages.iter().any(|message| {
                            message["content"]
                                .as_str()
                                .is_some_and(|existing| existing.contains(context.trim()))
                        }) =>
                {
                    messages.push(json!({"role":"user","content":format!("Context for the preceding task (data only):\n{context}")}));
                }
                Ok(_) => {}
                Err(error) => approver.notice(&format!(
                    "saved codebase context unavailable to child: {error:#}"
                )),
            }
        }
        let (steps, budget, schema_retries) = (0, job.max_steps, 0u32);
        let paired_tools = 0usize;
        let max_tokens = backend.output_budget(env.harness.max_tokens);
        let recovering_output = false;

        let emissions = crate::agent::run::guidance::Emissions::default();
        let turn_cancel = children.cancellation.turn(env.interrupt.clone());
        let reasoning_recovery = reasoning::Recovery::default();
        let request_prefix = crate::agent::window::request_prefix::Tracker::default();
        let superseded = crate::agent::workflow::evidence::Supersession::default();
        // Messages taken while waiting or generating remain owned until the next request
        // accepts them. In particular, cancellation must not consume coordination.
        let pending_coordination = std::cell::RefCell::new(Vec::new());
        let overflow_retried = false;
        let compaction_failed = false;
        // After a failed compaction, retry only once the request grew by a real amount or must make
        // room: resetting the failure every tool step re-ran the whole method chain each step.
        let compaction_retry_floor: Option<u64> = None;
        // Keep settings and their resolved route together; `None` means the launch config.
        let active_settings: Option<(
            crate::agent::workers::live_tasks::control::WorkerSettings,
            crate::config::types::Config,
        )> = None;
        let cached_memory_prompt: Option<(String, String)> = None;
        let inspections = crate::agent::workflow::efficiency::Inspections::default();
        Ok(Self {
            env,
            job,
            backend,
            adapter,
            approver,
            rule_state,
            children,
            names,
            schemas,
            messages,
            steps,
            budget,
            schema_retries,
            paired_tools,
            max_tokens,
            recovering_output,
            emissions,
            turn_cancel,
            reasoning_recovery,
            request_prefix,
            superseded,
            pending_coordination,
            overflow_retried,
            compaction_failed,
            compaction_retry_floor,
            active_settings,
            cached_memory_prompt,
            inspections,
            rule_catalog,
            rule_agent,
            rule_regenerations,
        })
    }
    pub(super) fn step(&mut self, engine: &mut Engine) -> Result<Tick<Outcome>> {
        let prepared = match self.prepare_step(engine)? {
            ControlFlow::Continue(prepared) => prepared,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let request = match self.prepare_request(engine, &prepared)? {
            ControlFlow::Continue(request) => request,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let generated = match self.request_reply(request, &prepared)? {
            ControlFlow::Continue(generated) => generated,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let reply = match self.recover_reply(engine, generated)? {
            ControlFlow::Continue(reply) => reply,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let admitted = match self.accept_reply(engine, prepared, reply)? {
            ControlFlow::Continue(admitted) => admitted,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let admitted = match self.prepare_round(engine, admitted)? {
            ControlFlow::Continue(admitted) => admitted,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        let settlement = match self.dispatch_round(engine, admitted)? {
            ControlFlow::Continue(settlement) => settlement,
            ControlFlow::Break(tick) => return Ok(tick),
        };
        match self.settle_round(engine, settlement)? {
            ControlFlow::Break(tick) => Ok(tick),
            ControlFlow::Continue(()) => Ok(Tick::Again),
        }
    }
    fn prepare_step(
        &mut self,
        engine: &mut Engine,
    ) -> Result<ControlFlow<Tick<Outcome>, PreparedStep>> {
        let env = self.env;
        let job = self.job;
        let backend = &mut *self.backend;
        let adapter = self.adapter;
        let approver = &mut *self.approver;
        let rule_state = &mut *self.rule_state;
        let children = self.children;
        let rule_root = env.workspace.root();
        if self.budget != 0 && self.steps >= self.budget {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Exhausted,
            ));
        }

        rule_state.clear_callback();
        self.pending_coordination
            .borrow_mut()
            .extend(approver.take_messages());
        if append_coordination(
            &mut self.messages,
            std::mem::take(&mut *self.pending_coordination.borrow_mut()),
        ) {
            self.paired_tools = 0;
            rule_state.begin(
                rule_root,
                self.messages
                    .last()
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or(""),
            );

            self.reasoning_recovery = Default::default();
            engine.transition(crate::agent::run::engine::RecoveryTransition::Coordination);
            engine.recovery.reset();
        }
        if env.interrupt.load(Ordering::SeqCst) {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::interrupted(self.steps)),
            ));
        }
        if let Err(error) = child_cost_boundary(env) {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                    self.steps,
                    format!("{error:#}"),
                )),
            ));
        }
        match serde_json::to_value(&*rule_state) {
            Ok(policy) => children.journal.borrow_mut().rule_policy = policy,
            Err(error) => {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("binding actual child rules: {error}"),
                    )),
                ));
            }
        }
        for message in &mut self.messages {
            crate::storage::media::retain_message(message, &children.image_store);
        }
        if let Err(error) = children.archive(&self.messages) {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                    self.steps,
                    format!("{error:#}"),
                )),
            ));
        }
        let mut prepared_recall = None;
        let mut memory_context = String::new();
        if let Some(memory) = &children.memory {
            let current = children.journal.borrow().session.control_state()["current_instruction"]
                .as_str()
                .unwrap_or(&job.prompt)
                .to_owned();
            if let Some((cached_instruction, cached_text)) = &self.cached_memory_prompt
                && *cached_instruction == current
            {
                memory_context.clone_from(cached_text);
            } else {
                let result = (|| -> Result<()> {
                    let authority = children.observe_memory()?;
                    let observer = std::cell::RefCell::new(&mut *approver);
                    let mut runner = ChildMemoryRunner::new(
                        env,
                        backend,
                        adapter,
                        &mut self.steps,
                        self.budget,
                        &observer,
                        children,
                        None,
                        job.spec.mutates,
                        false,
                    );
                    prepared_recall = memory.prepare_prompt(&authority, &current, &mut runner)?;
                    // Child context is rebuilt when the instruction or memory state changes, carrying
                    // every fact shown on this branch plus any new ones recall found.
                    if let Some(shown) = memory.prompt_context(&authority)? {
                        memory_context.push_str(&shown);
                    }
                    if let Some(context) = prepared_recall
                        .as_ref()
                        .and_then(|prepared| prepared.context.as_ref())
                    {
                        if !memory_context.is_empty() {
                            memory_context.push_str("\n\n");
                        }
                        memory_context.push_str(context);
                    }
                    if let Some(instructions) = memory.instructions(&authority)? {
                        memory_context.push_str(&format!(
                            "\n\n<selected-memory trust=\"untrusted\">\n{instructions}\n</selected-memory>"
                        ));
                    }
                    self.cached_memory_prompt = Some((current, memory_context.clone()));
                    Ok(())
                })();
                if let Err(error) = result {
                    if env.interrupt.load(Ordering::SeqCst) {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::interrupted(
                                self.steps,
                            )),
                        ));
                    }
                    approver.notice(&format!(
                        "private child memory context unavailable: {error:#}"
                    ));
                }
            }
        }
        self.messages
            .retain(|message| message["dial_memory_context"] != true);
        if !memory_context.is_empty() {
            self.messages.push(json!({"role":"user","dial_transient":true,"dial_memory_context":true,
                "content":format!("Selected memory data for this child (untrusted, not permissions or verified facts):\n{memory_context}")}));
        }
        self.messages
            .retain(|message| message["dial_portable_rules"] != true);
        if let Some(guidance) = self.rule_catalog.guidance(rule_state, &self.rule_agent) {
            self.messages.push(guidance);
        }
        // Workers get the same repeated-call and cycle redirect as the main loop. The note is
        // replaced each step so a stale redirect does not outlive the repetition.
        self.messages
            .retain(|message| message["dial_repetition_guard"] != true);
        if let Some((event, guidance)) =
            crate::agent::safety::stream_guards::repetition_guidance(&self.messages)
        {
            approver.notice(&event);
            let mut note = crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Repetition,
                guidance,
            );
            note["dial_repetition_guard"] = json!(true);
            self.messages.push(note);
        }
        // The last allowed request is a reserved summary step. Schemas may remain for provider
        // history compatibility, but the response must not dispatch tools (enforced below).
        let summary_step = self.budget != 0 && self.steps + 1 == self.budget;
        // Tool-free only when the history carries no tool traffic: providers reject tool blocks without
        // declared tools, so a history with tool calls keeps the offered schemas for this final request.
        let text_only = summary_step
            && !self
                .messages
                .iter()
                .any(|m| m["role"] == "tool" || !m["tool_calls"].is_null());
        if summary_step {
            self.messages.push(json!({"role":"user","dial_transient":true,"content":"Step budget reached: this is your final request. Reply now with your findings: what you verified and its evidence, what remains unverified, and the exact next step. Call no tools."}));
        }
        let research_step = !summary_step && engine.research_required();
        if !summary_step && let Some(note) = engine.research_prompt() {
            self.messages.push(note);
        }
        // Queued owner settings apply here, at a request boundary, to this worker's backend only.
        if let Some(settings) = approver.take_worker_settings() {
            let previous = self.active_settings.as_ref().map(|(settings, _)| settings);
            match swap_worker_backend(env, children, backend, approver, previous, &settings) {
                Ok(resolved) => {
                    approver.worker_settings_applied(&settings, &resolved);
                    self.active_settings = Some((settings, resolved));
                }
                Err(error) => {
                    let reason = format!("{error:#}");
                    approver.notice(&format!("worker settings not applied: {reason}"));
                    approver.worker_settings_failed(&reason);
                }
            }
        }
        // The same request-tail assembly as the parent: one advisory note by priority, then one
        // merged harness message for this request's plain notes.
        let notes_start = crate::agent::run::guidance::tail_start(&self.messages);
        crate::agent::run::guidance::assemble(&mut self.messages, notes_start, &mut self.emissions);
        Ok(ControlFlow::Continue(PreparedStep {
            prepared_recall,
            summary_step,
            text_only,
            research_step,
        }))
    }
    fn prepare_request(
        &mut self,
        engine: &mut Engine,
        prepared: &PreparedStep,
    ) -> Result<ControlFlow<Tick<Outcome>, Chat>> {
        let env = self.env;
        let job = self.job;
        let backend = &mut *self.backend;
        let adapter = self.adapter;
        let approver = &mut *self.approver;
        let rule_state = &mut *self.rule_state;
        let children = self.children;
        let text_only = prepared.text_only;
        let mut request = Chat {
            messages: std::mem::take(&mut self.messages),
            tools: if text_only {
                Vec::new()
            } else {
                std::mem::take(&mut self.schemas)
            },
            temperature: env.harness.temperature,
            max_tokens: self.max_tokens,
            seed: None,
        };
        let experimental = env.config.compaction.experimental_context_management
            && ["context_notes", "new_context", "session_search"]
                .iter()
                .all(|name| self.names.iter().any(|offered| offered == name));
        children.project(&mut request, job, rule_state, experimental);
        let selection = env.config.selection();
        let identity = crate::config::compaction::CompactionIdentity {
            provider: &selection.provider,
            model: &selection.model,
            agent: Some(job.kind),
        };
        let mut estimate = crate::agent::compact::compaction::request_tokens_with_policy(
            &request,
            &env.config.compaction,
            identity,
        );
        if experimental {
            let threshold = match env
                .config
                .compaction
                .effective_threshold(identity, child_window(env, backend))
            {
                Ok(threshold) => threshold,
                Err(error) => {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps,
                            format!("{error:#}"),
                        )),
                    ));
                }
            };
            let mut journal = children.journal.borrow_mut();
            let ChildJournal {
                session, reminder, ..
            } = &mut *journal;
            if let Some(guidance) = crate::agent::window::context_notes::pressure_guidance(
                session,
                estimate,
                threshold,
                crate::config::compaction::speculation_lead(threshold),
                reminder,
            ) {
                request.messages.push(guidance);
                estimate = crate::agent::compact::compaction::request_tokens_with_policy(
                    &request,
                    &env.config.compaction,
                    identity,
                );
            }
        }
        let window = child_window(env, backend);
        let needs_room = estimate
            > crate::agent::compact::compaction::prompt_budget(
                &env.config.compaction,
                window,
                backend.output_budget(self.max_tokens),
            );
        let should_compact =
            match env
                .config
                .compaction
                .should_compact(identity, window, estimate, self.steps > 0)
            {
                Ok(value) => {
                    value
                        || (needs_room
                            && env.config.compaction.enabled
                            && env.config.compaction.strategy
                                != crate::config::compaction::CompactionStrategy::Off)
                }
                Err(error) => {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps,
                            format!("{error:#}"),
                        )),
                    ));
                }
            };
        if should_compact && env.config.compaction.experimental_context_management && !experimental
        {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                    self.steps,
                    "Private child context maintenance requires all permitted notebook, new_context and raw session_search controls; no ordinary summary was substituted.",
                )),
            ));
        }
        if !self.compaction_failed
            && should_compact
            && (needs_room
                || self
                    .compaction_retry_floor
                    .is_none_or(|floor| estimate >= floor))
            && !env.config.compaction.experimental_context_management
        {
            let reason = if needs_room {
                crate::config::compaction::CompactionReason::Overflow
            } else if self.steps == 0 {
                crate::config::compaction::CompactionReason::Threshold
            } else {
                crate::config::compaction::CompactionReason::MidTurn
            };
            match compact_child(
                env,
                job,
                ChildRequestCtx {
                    backend,
                    adapter,
                    steps: &mut self.steps,
                    budget: self.budget,
                    approver,
                },
                &mut request,
                reason,
                children,
            ) {
                Ok(true) => {
                    self.compaction_retry_floor = None;
                    // The compacted context no longer holds the results duplicate memory cited.
                    engine.evidence_compacted();
                }
                Ok(false) => {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps,
                            "Context compacted; child continuation is paused by compaction policy.",
                        )),
                    ));
                }
                Err(error) => {
                    if children.journal.borrow().failed {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                self.steps,
                                format!("{error:#}"),
                            )),
                        ));
                    }
                    self.compaction_failed = true;
                    self.compaction_retry_floor = Some(
                        estimate
                            .saturating_add(crate::config::compaction::SPECULATION_LEAD_MIN_TOKENS),
                    );
                    if env.interrupt.load(Ordering::SeqCst) {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::interrupted(
                                self.steps,
                            )),
                        ));
                    }
                    approver.notice(&format!("child context compaction unavailable: {error:#}"));
                }
            }
        }

        if should_compact && experimental && !children.journal.borrow().exploration.busy() {
            match rollover_child(env, job, ChildRequestCtx { backend, adapter, steps: &mut self.steps, budget: self.budget, approver }, &mut request, rule_state, children, None) {
                Ok(true) => approver.notice("Private child notes-backed context window committed; original records remain searchable until this task ends."),
                Ok(false) => return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Finished(Outcome::failed(self.steps, "Private child context reset committed; continuation paused by compaction policy or the retained protected context exceeds its continuation band.")))),
                Err(_) if env.interrupt.load(Ordering::SeqCst) => return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Finished(Outcome::interrupted(self.steps)))),
                Err(error) => return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Finished(Outcome::failed(self.steps, format!("{error:#}"))))),
            }
        }
        let projected = crate::agent::compact::compaction::request_tokens_with_policy(
            &request,
            &env.config.compaction,
            identity,
        );
        if projected
            > crate::agent::compact::compaction::prompt_budget(
                &env.config.compaction,
                window,
                backend.output_budget(self.max_tokens),
            )
        {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                    self.steps,
                    "The exact child request and response reserve exceed its context window; compaction cannot safely make room.",
                )),
            ));
        }
        if self.budget != 0 && self.steps >= self.budget {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                    self.steps,
                    "used all child model requests while compacting",
                )),
            ));
        }
        Ok(ControlFlow::Continue(request))
    }
    fn request_reply(
        &mut self,
        request: Chat,
        prepared: &PreparedStep,
    ) -> Result<ControlFlow<Tick<Outcome>, GeneratedReply>> {
        let env = self.env;
        let backend = &mut *self.backend;
        let adapter = self.adapter;
        let approver = &mut *self.approver;
        let rule_state = &mut *self.rule_state;
        let children = self.children;
        let rule_root = env.workspace.root();
        let text_only = prepared.text_only;
        let research_step = prepared.research_step;
        let observer = std::cell::RefCell::new(&mut *approver);
        let check_admission = || {
            self.pending_coordination
                .borrow_mut()
                .extend(observer.borrow_mut().take_messages());
            if env.interrupt.load(Ordering::SeqCst)
                || !self.pending_coordination.borrow().is_empty()
            {
                Err(Interrupted)
            } else {
                Ok(())
            }
        };
        let _provider_slot = match crate::agent::model::provider_slots::acquire_checked(
            env.config,
            check_admission,
            || {
                observer
                    .borrow_mut()
                    .notice(crate::agent::model::provider_slots::QUEUED_NOTICE)
            },
        ) {
            Ok(slot) => slot,
            Err(_) if env.interrupt.load(Ordering::SeqCst) => {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::interrupted(self.steps)),
                ));
            }
            Err(_) => {
                self.messages = request.messages;
                if !text_only {
                    self.schemas = request.tools;
                }
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
        };
        // Request hooks are synchronous and can themselves wait. Revalidate after
        // that boundary, before spending a request or dispatching stale context.
        // Project a request copy: the child's retained history and immutable archive keep full
        // paths, original observations and handle metadata across later requests/maintenance.
        let mut model_request = request.clone();
        crate::agent::compact::snapcompact::views::project(&mut model_request.messages);
        crate::agent::window::file_handles::project(env.workspace, &mut model_request.messages);
        let estimate_before =
            crate::llm::wire::estimate_parts(&model_request.messages, &model_request.tools);
        let window = child_window(env, backend);
        let policy = crate::agent::workflow::evidence::Policy {
            warm_tokens: if env.config.efficiency.enabled || env.config.compaction.preserve_prefix {
                0
            } else {
                10_000
            },
            pressure: estimate_before
                > u64::from(window) * u64::from(env.config.efficiency.cache_pressure_percent) / 100,
            protect_tokens: (window as usize / 5).max(4_000),
            supersede_reads: env.config.compaction.supersede_reads,
            drop_useless: env.config.compaction.drop_useless,
        };
        crate::agent::workflow::evidence::project(
            &mut model_request.messages,
            &mut self.superseded,
            &policy,
        );
        let provider_rewrite = match child_provider_request(&env, &model_request, &observer) {
            Ok(rewrite) => rewrite,
            Err(error) => {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("{error:#}"),
                    )),
                ));
            }
        };
        if check_admission().is_err() {
            self.messages = request.messages;
            if !text_only {
                self.schemas = request.tools;
            }
            return Ok(ControlFlow::Break(
                if env.interrupt.load(Ordering::SeqCst) {
                    crate::agent::run::engine::Tick::Finished(Outcome::interrupted(self.steps))
                } else {
                    crate::agent::run::engine::Tick::Again
                },
            ));
        }
        // Hold the provider slot only for inference, never tools or nested work.
        // Coordination cancels this scope only, not the task or hook/tool interrupt.
        self.steps += 1;
        observer.borrow_mut().progress(self.steps, "processing", "");
        let generation_scope = self.turn_cancel.inference();
        let generation_cancel = generation_scope.flag();
        let mut rule_monitor = self
            .rule_catalog
            .monitor(rule_root, rule_state, &self.rule_agent);
        let provider_request = provider_rewrite.as_ref().unwrap_or(&model_request);
        let prefix = self
            .request_prefix
            .observe(&env.config.selection().display_name(), provider_request);
        if prefix.invalidation != crate::agent::window::request_prefix::Invalidation::None {
            observer
                .borrow_mut()
                .notice(&format!("Request prefix: {}", prefix.invalidation));
        }
        let started = std::time::Instant::now();
        if let Some(stream) = &children.transcript {
            stream.assistant_stream(None)?;
        }
        let mut rule_ui = RuleUi {
            observer: &observer,
            steps: self.steps,
            stream: children.transcript.clone(),
            stream_error: None,
            registry: env.ext,
        };
        let mut stream_ui = crate::agent::ui::state_ui::StateUi::with_registry(
            &mut rule_ui,
            children.journal.borrow().session.runtime(),
            None,
            env.ext.clone(),
        );
        // Workers share the main loop's host-default policy, using the route actually bound
        // above rather than the launch route after an owner-requested model switch.
        let config = self
            .active_settings
            .as_ref()
            .map_or(env.config, |(_, config)| config);
        let mut profile = backend.model_profile().cloned();
        crate::agent::model::local_profile::host_default_sampling(
            config,
            config,
            false,
            &mut profile,
            env.harness.temperature,
        );
        crate::agent::model::local_profile::host_default_output_limit(
            config,
            false,
            &mut profile,
            if self.recovering_output {
                self.max_tokens
            } else {
                env.harness.max_tokens
            },
        );
        backend.next_request_profile(profile);
        let result = crate::agent::chat_with_ui_inner(
            backend,
            adapter,
            provider_request,
            crate::agent::ChatOptions {
                timeout: env.config.worker_timeout_secs,
                render_text: !research_step,
                context_tokens: None,
                policy: Some(&env.config.reasoning_guard),
                recovery: None,
            },
            generation_cancel.clone(),
            crate::agent::ChatSinks {
                ui: &mut stream_ui,
                on_idle: &mut |_| {
                    generation_scope.sync();
                    self.pending_coordination
                        .borrow_mut()
                        .extend(observer.borrow_mut().take_messages());
                    if !self.pending_coordination.borrow().is_empty() {
                        generation_cancel.store(true, Ordering::SeqCst);
                    }
                },
            },
            Some(&mut rule_monitor),
        );
        // Matches whose interruptMode let this reply proceed; journaled once the reply is accepted.
        let rule_deferred = std::mem::take(&mut rule_monitor.deferred);
        stream_ui.finish(result.is_err());
        if let Some(error) = rule_ui.stream_error.take() {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(self.steps, error)),
            ));
        }
        if let Err(error) = children
            .journal
            .borrow()
            .session
            .runtime()
            .check_persistence()
        {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(self.steps, error)),
            ));
        }
        drop(_provider_slot);
        // The provider ledger owns transport usage. Cost is charged once even
        // when coordination or guidance discards the accepted response.
        if let Ok(reply) = &result
            && let Some(cost) = backend.reply_cost(reply)
        {
            approver.add_cost(cost);
        }
        record_child_request(env, &children.namespace, backend, &result, started);
        self.messages = request.messages;
        crate::agent::run::guidance::retire_delivered(&mut self.messages, result.is_ok());
        if !text_only {
            self.schemas = request.tools;
        }
        Ok(ControlFlow::Continue(GeneratedReply {
            result,
            rule_deferred,
        }))
    }
    fn recover_reply(
        &mut self,
        engine: &mut Engine,
        generated: GeneratedReply,
    ) -> Result<ControlFlow<Tick<Outcome>, llm::Reply>> {
        let env = self.env;
        let job = self.job;
        let backend = &mut *self.backend;
        let adapter = self.adapter;
        let approver = &mut *self.approver;
        let rule_state = &mut *self.rule_state;
        let children = self.children;
        let rule_root = env.workspace.root();
        let GeneratedReply {
            result,
            rule_deferred,
        } = generated;
        let experimental = env.config.compaction.experimental_context_management
            && ["context_notes", "new_context", "session_search"]
                .iter()
                .all(|name| self.names.iter().any(|offered| offered == name));
        self.pending_coordination
            .borrow_mut()
            .extend(approver.take_messages());
        if append_coordination(
            &mut self.messages,
            std::mem::take(&mut *self.pending_coordination.borrow_mut()),
        ) && !env.interrupt.load(Ordering::SeqCst)
        {
            self.paired_tools = 0;
            if let Some(stream) = &children.transcript {
                stream.assistant_stream(None)?;
            }
            if result.is_ok()
                && let Err(error) = backend.record_discarded_response()
            {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("accounting discarded child response: {error:#}"),
                    )),
                ));
            }
            rule_state.begin(
                rule_root,
                self.messages
                    .last()
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or(""),
            );
            if self.budget != 0 {
                self.budget = self.budget.saturating_add(1);
            }

            self.reasoning_recovery = Default::default();
            engine.transition(crate::agent::run::engine::RecoveryTransition::Coordination);
            approver.notice(
                "New coordination received; discarded the old response before executing tools.",
            );
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        let rule_hit = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<crate::agent::safety::rules::Hit>())
            .cloned();
        let scoped_paths = result.as_ref().ok().and_then(|reply| {
            self.rule_catalog.pending_paths(
                rule_root,
                rule_state,
                &self.rule_agent,
                &reply.tool_calls,
            )
        });
        if !env.interrupt.load(Ordering::SeqCst) && (rule_hit.is_some() || scoped_paths.is_some()) {
            if let Some(stream) = &children.transcript {
                stream.assistant_stream(None)?;
            }
            if result.is_ok()
                && let Err(error) = backend.record_discarded_response()
            {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("accounting discarded child guidance response: {error:#}"),
                    )),
                ));
            }
            if self.rule_regenerations >= env.config.portable_rules.max_regenerations {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        "portable guidance regeneration limit reached; stale tools were not executed",
                    )),
                ));
            }
            self.rule_regenerations += 1;
            rule_state.set_regenerations(self.rule_regenerations);
            if let Some(hit) = rule_hit {
                if let Err(error) = rule_state.remember(&hit, &self.rule_agent) {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps, error,
                        )),
                    ));
                }
                approver.notice(&hit.to_string());
            }
            if let Some(paths) = scoped_paths {
                if let Err(error) = rule_state.activate(paths) {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps, error,
                        )),
                    ));
                }
                approver.notice("Discarded scoped tool attempt; delivering applicable guidance before any tool action.");
            }
            if self.budget != 0 {
                self.budget = self.budget.saturating_add(1);
            }
            let _ = engine.setback(Setback::GuidanceRegeneration);
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        let reply = match result {
            Ok(reply) => {
                if let Err(error) =
                    admit_child_rule_hits(rule_state, &self.rule_agent, &rule_deferred, approver)
                {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps, error,
                        )),
                    ));
                }
                reply
            }
            Err(error)
                if error.downcast_ref::<reasoning::GateHit>().is_some()
                    && !env.interrupt.load(Ordering::SeqCst) =>
            {
                self.recovering_output = true;
                let _ = engine.setback(Setback::StreamStall);
                match self.reasoning_recovery.intercept(&error.to_string()) {
                    Ok(message) => {
                        approver.notice(&message);
                        self.messages.push(json!({"role":"user","content":message}));
                    }
                    Err(message) => {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                self.steps, message,
                            )),
                        ));
                    }
                }
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            Err(error)
                if error.downcast_ref::<Interrupted>().is_some()
                    || env.interrupt.load(Ordering::SeqCst) =>
            {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::interrupted(self.steps)),
                ));
            }
            Err(error)
                if crate::agent::looks_like_overflow(&error)
                    && !self.overflow_retried
                    && env.config.compaction.enabled
                    && env.config.compaction.strategy
                        != crate::config::compaction::CompactionStrategy::Off =>
            {
                self.overflow_retried = true;
                if let crate::agent::run::turn::progress::Rung::Stop(reason) =
                    engine.setback(Setback::Overflow)
                {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps, reason,
                        )),
                    ));
                }
                let mut retry = Chat {
                    messages: std::mem::take(&mut self.messages),
                    tools: std::mem::take(&mut self.schemas),
                    temperature: env.harness.temperature,
                    max_tokens: self.max_tokens,
                    seed: None,
                };
                if env.config.compaction.experimental_context_management {
                    if !experimental {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                self.steps,
                                "Private child overflow cannot reset context: its real notebook/new_context/raw-history capabilities are not all permitted. No ordinary summary was substituted.",
                            )),
                        ));
                    }
                    match rollover_child(
                        env,
                        job,
                        ChildRequestCtx {
                            backend,
                            adapter,
                            steps: &mut self.steps,
                            budget: self.budget,
                            approver,
                        },
                        &mut retry,
                        rule_state,
                        children,
                        None,
                    ) {
                        Ok(true) => {
                            self.messages = retry.messages;
                            self.schemas = retry.tools;
                            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
                        }
                        Ok(false) => {
                            return Ok(ControlFlow::Break(
                                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                    self.steps,
                                    "Private child overflow reset committed; continuation paused by compaction policy or the retained protected context exceeds its continuation band.",
                                )),
                            ));
                        }
                        Err(_) if env.interrupt.load(Ordering::SeqCst) => {
                            return Ok(ControlFlow::Break(
                                crate::agent::run::engine::Tick::Finished(Outcome::interrupted(
                                    self.steps,
                                )),
                            ));
                        }
                        Err(error) => {
                            return Ok(ControlFlow::Break(
                                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                    self.steps,
                                    format!("{error:#}"),
                                )),
                            ));
                        }
                    }
                }
                match compact_child(
                    env,
                    job,
                    ChildRequestCtx {
                        backend,
                        adapter,
                        steps: &mut self.steps,
                        budget: self.budget,
                        approver,
                    },
                    &mut retry,
                    crate::config::compaction::CompactionReason::Overflow,
                    children,
                ) {
                    Ok(true) => {
                        self.messages = retry.messages;
                        self.schemas = retry.tools;
                        return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
                    }
                    Ok(false) => {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                self.steps,
                                "Context compacted; child continuation is paused by compaction policy.",
                            )),
                        ));
                    }
                    Err(_error) if env.interrupt.load(Ordering::SeqCst) => {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::interrupted(
                                self.steps,
                            )),
                        ));
                    }
                    Err(error) => {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                self.steps,
                                format!("{error:#}"),
                            )),
                        ));
                    }
                }
            }
            Err(error) => {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::provider_failed(
                        self.steps,
                        &error,
                        env.config.retry.fallback_on,
                    )),
                ));
            }
        };
        Ok(ControlFlow::Continue(reply))
    }
    fn accept_reply(
        &mut self,
        engine: &mut Engine,
        prepared: PreparedStep,
        mut reply: llm::Reply,
    ) -> Result<ControlFlow<Tick<Outcome>, AdmittedRound>> {
        let env = self.env;
        let job = self.job;
        let backend = &mut *self.backend;
        let adapter = self.adapter;
        let approver = &mut *self.approver;
        let rule_state = &mut *self.rule_state;
        let children = self.children;
        let rule_root = env.workspace.root();
        let PreparedStep {
            mut prepared_recall,
            summary_step,
            research_step,
            ..
        } = prepared;
        if let Some(prepared) = prepared_recall.take() {
            // `false` means a newer branch or recall superseded this one: nothing to publish.
            let _ = prepared.commit(&children.memory_authority());
        }
        // Schemas retained for protocol history are not permission to act on the reserved
        // summary request. Stop before recovery, inline-edit conversion or tool admission.
        if summary_step
            && (!reply.tool_calls.is_empty()
                || reply.dropped_tool_call()
                || looks_like_text_call(&reply.text, &self.names)
                || (env.workspace.edit_mode() == crate::config::types::EditMode::Sloppy
                    && env.workspace.edit_settings().recover_inline_edits
                    && self.names.iter().any(|name| name == "edit")
                    && tools::inline_edit_payload(&reply.text).is_some()))
        {
            let mut reason = format!(
                "used all {} steps without a final answer; tools requested on the reserved summary step were not executed",
                job.max_steps
            );
            let findings = if reply.text.trim().is_empty() {
                last_written_findings(&self.messages)
            } else {
                Some(reply.text.as_str())
            };
            if let Some(text) = findings {
                reason.push_str("; last findings (unverified):\n");
                reason.push_str(&clip_evidence(text, 4096));
            }
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::incomplete(self.steps, reason)),
            ));
        }
        if research_step {
            use crate::agent::workflow::research::Synthesis;
            let message = match engine.synthesize(&reply) {
                Synthesis::Accepted => {
                    // Internal checkpoint records must not become child transcript answers.
                    let archived = children.archive(&self.messages).and_then(|()| {
                        let mut journal = children.journal.borrow_mut();
                        let mut control = journal.session.control_state();
                        control["research_checkpoint"] =
                            json!({"response":reply.text,"verified":false});
                        journal.session.set_control_state(control)
                    });
                    if let Err(error) = archived {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                self.steps,
                                format!("archiving research synthesis: {error:#}"),
                            )),
                        ));
                    }
                    approver.notice("Research checkpoint recorded as unverified findings; following through on the next action.");
                    None
                }
                Synthesis::Retry(message) => Some(message),
                Synthesis::Incomplete(message) => {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::incomplete(
                            self.steps, message,
                        )),
                    ));
                }
            };
            if let Some(message) = message {
                self.messages
                    .push(json!({"role":"user","dial_transient":true,"content":message}));
            }
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        let length_facts = crate::agent::run::reply_triage::LengthFacts {
            reasoning_guard: env.config.reasoning_guard.enabled,
            // Workers have no todo plan tool to gate on.
            plan_gate: false,
            requested_tokens: self.max_tokens,
            window: child_window(env, backend),
            prompt_tokens: reply.prompt_tokens.unwrap_or(0),
        };
        if let Some(truncation) = crate::agent::run::reply_triage::truncation(
            engine,
            &mut self.reasoning_recovery,
            &reply,
            &length_facts,
        ) {
            self.recovering_output = true;
            let _ = engine.setback(Setback::Truncation);
            use crate::agent::run::reply_triage::Truncation;
            match truncation {
                Truncation::Reasoning { message, .. } => {
                    approver.notice(&message);
                    self.messages.push(json!({"role":"user","content":message}));
                }
                Truncation::Raise(raised) => self.max_tokens = raised,
                Truncation::Continue { partial, message } => {
                    if let Some(text) = partial {
                        self.messages
                            .push(json!({"role":"assistant","content":text}));
                    }
                    self.messages.push(json!({"role":"user","content":message}));
                }
                Truncation::Stop(reason) => {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::incomplete(
                            self.steps, reason,
                        )),
                    ));
                }
                Truncation::ReasoningExhausted(reason) => {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps, reason,
                        )),
                    ));
                }
            }
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        engine.length_recovered(&reply);
        if !reply.dropped_tool_call()
            && (!reply.text.trim().is_empty() || !reply.tool_calls.is_empty())
        {
            self.recovering_output = false;
        }
        let mut recovered_original = None;
        if reply.finish_reason == "stop"
            && reply.tool_calls.is_empty()
            && env.workspace.edit_mode() == crate::config::types::EditMode::Sloppy
            && env.workspace.edit_settings().recover_inline_edits
            && self.names.iter().any(|name| name == "edit")
            && let Some((_, arguments)) = tools::inline_edit_payload(&reply.text)
        {
            recovered_original = Some(llm::assistant_message(&reply));
            // The transport emitted ordinary text, not this controller-recovered tool use.
            // Keep its genuine signed carrier privately; never replay an orphan synthetic call.
            reply.provider_content.clear();
            reply.tool_calls.push(llm::ToolCall {
                id: format!("child-inline-edit-{}", self.steps),
                name: "edit".into(),
                arguments,
                malformed_arguments: false,
            });
            reply.text.clear();
        }
        // Classified after inline-edit recovery, as in the foreground loop, so a recovered edit
        // never counts as a form failure.
        let form_failure = crate::agent::run::reply_triage::FormFailure::of(&reply, &self.names);
        if let Err(reason) = engine.recovery.form(form_failure.is_some()) {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::incomplete(self.steps, reason)),
            ));
        }
        if let Some(failure) = form_failure {
            let _ = engine.setback(Setback::UnusableReply);
            if failure == crate::agent::run::reply_triage::FormFailure::TextCall {
                self.messages
                    .push(json!({"role":"assistant","content":reply.text}));
            }
            self.messages
                .push(json!({"role":"user","content":failure.correction()}));
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        match engine
            .recovery
            .stopped(&reply.text, !reply.tool_calls.is_empty())
        {
            crate::agent::run::turn_recovery::StopAction::Accept => {}
            crate::agent::run::turn_recovery::StopAction::Continue { reason, message } => {
                let _ = engine.setback(Setback::NoAnswer);
                approver.notice(&format!("Recovering: {reason}."));
                if !reply.text.trim().is_empty() {
                    self.messages.push(llm::assistant_message(&reply));
                }
                self.messages
                    .push(json!({"role":"user","content":message,"dial_transient":true}));
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            crate::agent::run::turn_recovery::StopAction::Incomplete(reason) => {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::incomplete(
                        self.steps, reason,
                    )),
                ));
            }
        }
        if reply.tool_calls.is_empty() {
            let steering = approver.take_messages();
            if !steering.is_empty() {
                self.messages.push(llm::assistant_message(&reply));
                append_coordination(&mut self.messages, steering);
                self.paired_tools = 0;
                rule_state.begin(rule_root, "");

                self.reasoning_recovery = Default::default();
                engine.transition(crate::agent::run::engine::RecoveryTransition::Coordination);
                engine.recovery.reset();
                if self.budget != 0 {
                    self.budget = self.budget.saturating_add(1);
                }
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            self.messages.push(llm::assistant_message(&reply));
            for message in &mut self.messages {
                crate::storage::media::retain_message(message, &children.image_store);
            }
            if let Err(error) = children.archive(&self.messages) {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("{error:#}"),
                    )),
                ));
            }
            if children.journal.borrow().exploration.busy() {
                self.messages.push(json!({"role":"user","content":"Finish the active investigation by calling rewind with findings before a final answer. Actual effects and the request budget are not rolled back."}));
                return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
            }
            match finish(job, &reply.text) {
                Ok((answer, data)) => {
                    let reported = if job.schema.is_none() {
                        crate::agent::verify::completion::declared_outcome(&answer)
                            .unwrap_or(crate::runtime::Phase::Completed)
                    } else {
                        crate::runtime::Phase::Completed
                    };
                    let phase = children.journal.borrow();
                    let capture = reported == crate::runtime::Phase::Completed
                        && env.config.memory.autolearn
                        && env.config.memory.autolearn_auto_continue
                        && self.paired_tools >= env.config.memory.autolearn_min_tool_calls
                        && reply.finish_reason == "stop"
                        && !reply.text.trim().is_empty()
                        && !env.plan_mode
                        && !summary_step
                        && job.spec.mutates
                        && self.names.iter().any(|name| name == "learn")
                        && !phase.exploration.busy()
                        && !phase.exploration.completed
                        && phase.pending.is_none();
                    drop(phase);
                    if capture {
                        self.paired_tools = 0;
                        let preview_abort = std::cell::Cell::new(false);
                        let result = capture_child_memory(
                            env,
                            job,
                            &mut self.messages,
                            RerankContext {
                                backend: &mut *backend,
                                adapter,
                                steps: &mut self.steps,
                                budget: self.budget,
                                children,
                                preview_abort: &preview_abort,
                                capture: true,
                            },
                            approver,
                            crate::agent::safety::rules::CallbackRules {
                                catalog: &self.rule_catalog,
                                state: rule_state,
                                agent: &self.rule_agent,
                                regenerations: &mut self.rule_regenerations,
                                max_regenerations: env.config.portable_rules.max_regenerations,
                            },
                            self.max_tokens,
                        );
                        if let Err(error) = result {
                            if env.interrupt.load(Ordering::SeqCst) {
                                return Ok(ControlFlow::Break(
                                    crate::agent::run::engine::Tick::Finished(
                                        Outcome::interrupted(self.steps),
                                    ),
                                ));
                            }
                            if children.journal.borrow().failed {
                                return Ok(ControlFlow::Break(
                                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                        self.steps,
                                        format!("{error:#}"),
                                    )),
                                ));
                            }
                            approver.notice(&format!(
                                "private child autolearn capture did not complete: {error:#}"
                            ));
                        }
                        let mut incoming =
                            std::mem::take(&mut *children.capture_messages.borrow_mut());
                        incoming.extend(approver.take_messages());
                        if append_coordination(&mut self.messages, incoming) {
                            if self.budget != 0 {
                                self.budget = self.budget.saturating_add(1);
                            }
                            rule_state.begin(rule_root, "");

                            self.reasoning_recovery = Default::default();
                            engine.transition(
                                crate::agent::run::engine::RecoveryTransition::Coordination,
                            );
                            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
                        }
                    }
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome {
                            status: if reported == crate::runtime::Phase::Completed {
                                Status::done(answer)
                            } else {
                                Status {
                                    phase: reported,
                                    text: answer,
                                }
                            },
                            data,
                            isolation: None,
                            acceptance: Default::default(),
                            portable_rules: None,
                            steps: self.steps,
                            retryable_provider: false,
                            model_route: None,
                            inspections: Default::default(),
                            conversation: None,
                        }),
                    ));
                }
                Err(why) if self.schema_retries < env.config.task.max_schema_retries => {
                    self.schema_retries += 1;
                    let _ = engine.setback(Setback::UnusableReply);
                    // Retries extend a finite work budget so schema correction cannot
                    // consume the model requests reserved for the assigned task.
                    if self.budget != 0 {
                        self.budget = self.budget.saturating_add(1);
                    }
                    self.messages.push(json!({"role":"user","content":format!("Your answer was rejected: {why}. Reply again with only a JSON value that validates against the schema. Schema correction attempt {} of {}.", self.schema_retries, env.config.task.max_schema_retries)}));
                }
                Err(why) => {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps,
                            format!("the answer does not match output_schema: {why}"),
                        )),
                    ));
                }
            }
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        Ok(ControlFlow::Continue(AdmittedRound {
            reply,
            recovered_original,
        }))
    }
    fn prepare_round(
        &mut self,
        engine: &mut Engine,
        admitted: AdmittedRound,
    ) -> Result<ControlFlow<Tick<Outcome>, DispatchRound>> {
        let env = self.env;
        let backend = &mut *self.backend;
        let approver = &mut *self.approver;
        let rule_state = &mut *self.rule_state;
        let children = self.children;
        let rule_root = env.workspace.root();
        let AdmittedRound {
            mut reply,
            recovered_original,
        } = admitted;
        // Children get the same argument compatibility, but never discover capabilities outside
        // their assigned tool subset or widen read-only permissions.
        for call in &mut reply.tool_calls {
            if call.malformed_arguments {
                continue;
            }
            call.name = crate::agent::run::tool_compat::canonical_name(&call.name, &self.names)
                .into_owned();
            if let Some(parameters) = self
                .schemas
                .iter()
                .find(|s| s["function"]["name"] == call.name)
                .and_then(|s| s.pointer("/function/parameters"))
            {
                crate::agent::run::tool_compat::normalize(
                    &call.name,
                    parameters,
                    &mut call.arguments,
                );
                if let Err(error) = crate::agent::window::file_handles::expand(env.workspace, call)
                {
                    call.malformed_arguments = true;
                    approver.notice(&format!("File reference refused: {error}"));
                }
            }
        }
        let mut normalized_rules =
            self.rule_catalog
                .monitor(rule_root, rule_state, &self.rule_agent);
        let normalized_hit = normalized_rules.completed(&reply);
        if let Some(failure) = normalized_rules.failure {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(self.steps, failure)),
            ));
        }
        let scoped_paths = self.rule_catalog.pending_paths(
            rule_root,
            rule_state,
            &self.rule_agent,
            &reply.tool_calls,
        );
        if normalized_hit.is_some() || scoped_paths.is_some() {
            if let Err(error) = backend.record_discarded_response() {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("accounting discarded normalized child response: {error:#}"),
                    )),
                ));
            }
            if self.rule_regenerations >= env.config.portable_rules.max_regenerations {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        "portable guidance regeneration limit reached; stale tools were not executed",
                    )),
                ));
            }
            self.rule_regenerations += 1;
            rule_state.set_regenerations(self.rule_regenerations);
            if let Some(hit) = normalized_hit {
                if let Err(error) = rule_state.remember(&hit, &self.rule_agent) {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps, error,
                        )),
                    ));
                }
                approver.notice(&hit.to_string());
            }
            if let Some(paths) = scoped_paths
                && let Err(error) = rule_state.activate(paths)
            {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(self.steps, error)),
                ));
            }
            if self.budget != 0 {
                self.budget = self.budget.saturating_add(1);
            }
            approver.notice("Discarded normalized tool attempt; delivering applicable portable guidance before execution.");
            let _ = engine.setback(Setback::GuidanceRegeneration);
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        if let Err(error) = admit_child_rule_hits(
            rule_state,
            &self.rule_agent,
            &normalized_rules.deferred,
            approver,
        ) {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(self.steps, error)),
            ));
        }
        if let Err(error) =
            self.rule_catalog
                .observe_paths(rule_root, rule_state, &reply.tool_calls)
        {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(self.steps, error)),
            ));
        }
        match serde_json::to_value(&*rule_state) {
            Ok(policy) => children.journal.borrow_mut().rule_policy = policy,
            Err(error) => {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("binding admitted child rules: {error}"),
                    )),
                ));
            }
        }
        let narration_warning = engine.narration.observe(&reply);
        let mut assistant =
            crate::tools::surface::contracts::assistant_message(&reply, &self.schemas, env.ext);
        crate::agent::window::file_handles::capture_calls(env.workspace, &mut assistant);
        if let Err(error) =
            children.archive_assistant(&self.messages, &assistant, recovered_original)
        {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                    self.steps,
                    format!("archiving actual child assistant: {error:#}"),
                )),
            ));
        }
        self.messages.push(assistant);
        Ok(ControlFlow::Continue(DispatchRound {
            reply,
            narration_warning,
        }))
    }
    fn dispatch_round(
        &mut self,
        engine: &mut Engine,
        admitted: DispatchRound,
    ) -> Result<ControlFlow<Tick<Outcome>, RoundSettlement>> {
        let env = self.env;
        let job = self.job;
        let backend = &mut *self.backend;
        let adapter = self.adapter;
        let approver = &mut *self.approver;
        let rule_state = &mut *self.rule_state;
        let children = self.children;
        let DispatchRound {
            reply,
            narration_warning,
        } = admitted;
        let _speculative_batch = env.workspace.speculation.batch();
        let mut parallel = std::collections::VecDeque::new();
        let mut prerequisites =
            crate::agent::run::dependency_dispatch::BatchPrerequisites::default();
        let mut investigation_stop = None;
        let mut round_messages = Vec::new();
        let mut portable_batch_intercepted = false;
        let preview_abort = std::cell::Cell::new(false);
        for (index, call) in reply.tool_calls.iter().enumerate() {
            if parallel.is_empty()
                && let Some(reason) = engine.pending_refusal()
            {
                self.messages
                    .push(crate::tools::surface::outcome::skipped(&call.id, reason));
                continue;
            }
            if env.interrupt.load(Ordering::SeqCst) {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::interrupted(self.steps)),
                ));
            }
            if portable_batch_intercepted && parallel.is_empty() {
                let reason = "This call was not executed: portable guidance intercepted an earlier callback in this assistant batch. Regenerate with the staged guidance before further tool actions.";
                self.messages.push(llm::tool_message(&call.id, reason));
                approver.notice(reason);
                crate::telemetry::tool_with_effects(
                    env.config,
                    false,
                    &children.namespace,
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                    crate::tools::surface::outcome::Effects::None,
                );
                continue;
            }
            if preview_abort.get() && parallel.is_empty() {
                self.messages.push(llm::tool_message(&call.id, "This call was not executed: an authorized completed edit preview failed earlier in this assistant batch. Replan before further tool actions."));
                crate::telemetry::tool_with_effects(
                    env.config,
                    false,
                    &children.namespace,
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                    crate::tools::surface::outcome::Effects::None,
                );
                continue;
            }
            if parallel.is_empty() && {
                let journal = children.journal.borrow();
                journal.pending.is_some() || journal.exploration.requested_rewind()
            } {
                self.messages.push(llm::tool_message(&call.id, "This call was not executed: a private context transition was requested earlier in this batch. Only completed results are retained; regenerate after the owned boundary."));
                crate::telemetry::tool_with_effects(
                    env.config,
                    false,
                    &children.namespace,
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                    crate::tools::surface::outcome::Effects::None,
                );
                continue;
            }
            round_messages.extend(
                approver
                    .take_messages()
                    .into_iter()
                    .filter(|text| !text.trim().is_empty()),
            );
            if !round_messages.is_empty() && parallel.is_empty() {
                self.messages.push(llm::tool_message(&call.id, "This call was not executed: a new coordination message arrived before it started. Re-evaluate this action with the new message and the existing task constraints."));
                crate::telemetry::tool_with_effects(
                    env.config,
                    false,
                    &children.namespace,
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                    crate::tools::surface::outcome::Effects::None,
                );
                continue;
            }
            if parallel.is_empty()
                && let Some(output) =
                    prerequisites.rejection(env.workspace, env.workspace.edit_mode(), env.ext, call)
            {
                self.messages
                    .push(crate::tools::surface::outcome::message(&call.id, &output));
                crate::telemetry::tool_with_effects(
                    env.config,
                    false,
                    &children.namespace,
                    &call.name,
                    0.0,
                    tools::ToolStatus::Skipped,
                    tools::surface::outcome::Effects::None,
                );
                continue;
            }
            if !reply.text_calls_lifted && env.config.experimental.tool_speculation {
                for later in reply.tool_calls.iter().skip(index + 1).take(2) {
                    if call_form_error(later, &self.schemas, &self.names).is_some()
                        || engine.rejection_at(env.workspace, later).is_some()
                    {
                        continue;
                    }
                    let Some(key) = child_speculation_key(env, job, later, &children.namespace)
                    else {
                        continue;
                    };
                    if approver
                        .authorize_with_cancel(later, &summarize(later), false, env.interrupt)
                        .is_ok()
                    {
                        env.workspace.speculation.start_named(
                            env.workspace,
                            env.config.clone(),
                            env.ext,
                            later,
                            key,
                            children.cancellation.turn(env.interrupt.clone()),
                        );
                    }
                }
            }
            if parallel.is_empty()
                && !reply.text_calls_lifted
                && !child_speculation_key(env, job, call, &children.namespace)
                    .is_some_and(|key| env.workspace.speculation.pending(&key, &call.arguments))
                && !crate::agent::safety::hooks::wanted(&env.config.hooks, "pre_tool_use")
            {
                let count = reply.tool_calls[index..]
                    .iter()
                    .take(env.config.task.max_concurrency)
                    .take_while(|call| {
                        approval::parallel_read(env.ext, &call.name, &call.arguments)
                            && call_form_error(call, &self.schemas, &self.names).is_none()
                            && approver
                                .authorize_with_cancel(call, &summarize(call), false, env.interrupt)
                                .is_ok()
                            && engine.rejection_at(env.workspace, call).is_none()
                    })
                    .count();
                if count > 0 {
                    // Parallel reads skip `call_tool`; RuleUi reports each call's start and
                    // completion as parallel_tools emits them, so the parent's agent tree counts
                    // and names them like any other call.
                    let observer = std::cell::RefCell::new(&mut *approver);
                    parallel = crate::agent::parallel_tools(
                        crate::agent::ParallelToolContext {
                            workspace: env.workspace,
                            config: env.config,
                            ext: env.ext,
                            interrupt: env.interrupt,
                            ui: &mut RuleUi {
                                observer: &observer,
                                steps: self.steps,
                                stream: children.transcript.clone(),
                                stream_error: None,
                                registry: env.ext,
                            },
                            pending_steering: &mut round_messages,
                        },
                        &reply.tool_calls[index..index + count],
                        None,
                    )
                    .into();
                }
            }
            let rejection = engine.rejection_at(env.workspace, call);
            let blocked = rejection.is_some() && parallel.is_empty();
            let started = std::time::Instant::now();
            let mut seconds = None;
            let mut output = match parallel.pop_front() {
                Some((output, elapsed)) => {
                    seconds = Some(elapsed);
                    output
                }
                None if rejection.is_some() => crate::runtime::tool_output::ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: rejection.unwrap(),
                    is_error: true,
                }
                .with_status(tools::ToolStatus::Blocked)
                .with_effects(tools::surface::outcome::Effects::None),
                None => {
                    let callback_gate =
                        std::sync::Mutex::new(crate::agent::safety::rules::CallbackGate::new(
                            crate::agent::safety::rules::CallbackRules {
                                catalog: &self.rule_catalog,
                                state: rule_state,
                                agent: &self.rule_agent,
                                regenerations: &mut self.rule_regenerations,
                                max_regenerations: env.config.portable_rules.max_regenerations,
                            },
                        ));
                    let live = children.transcript.clone();
                    if let Some(stream) = &live {
                        stream.tool_started(&call.id, &call.name);
                    }
                    let output = call_tool(
                        env,
                        job,
                        ToolSurface {
                            schemas: &self.schemas,
                            names: &self.names,
                            provenance: approval::CallProvenance::from_reply(&reply),
                        },
                        call,
                        approver,
                        Some(RerankContext {
                            backend: &mut *backend,
                            adapter,
                            steps: &mut self.steps,
                            budget: self.budget,
                            children,
                            preview_abort: &preview_abort,
                            capture: false,
                        }),
                        Some(&callback_gate),
                    );
                    if let Some(stream) = &live {
                        stream.tool_finished(&call.id);
                    }
                    output
                }
            };
            // A controller retry/loop refusal did not attempt a native mutation.
            if !blocked {
                prerequisites.observe(env.workspace, env.workspace.edit_mode(), call, &output);
            }
            if matches!(
                call.name.as_str(),
                "recall" | "retain" | "memory_update" | "forget" | "reflect" | "learn"
            ) {
                self.cached_memory_prompt = None;
            }
            if !blocked && !output.is_error {
                self.inspections.record(call);
            }
            let tool_seconds = seconds.unwrap_or_else(|| started.elapsed().as_secs_f64());
            portable_batch_intercepted |= rule_state.callback_pending();
            if let Some(advice) = crate::agent::run::failure_guidance::shell_tool_recovery(
                call,
                &mut output,
                |name| {
                    crate::agent::run::failure_guidance::LaunchCapabilities::child_tool(
                        name,
                        &self.names,
                        env.config,
                        env.ext,
                    )
                },
            ) {
                output.text.push_str(&advice);
            }
            if matches!(call.name.as_str(), "bash" | "bg") {
                let capabilities = crate::agent::run::failure_guidance::LaunchCapabilities::child(
                    &self.names,
                    env.config,
                    env.ext,
                );
                if let Some(advice) = crate::agent::run::failure_guidance::shell_launch_recovery(
                    call,
                    &mut output,
                    &capabilities,
                ) {
                    output.text.push_str(&advice);
                }
            }
            let (duplicate, repeat_advice, stop) = engine.settle(
                env.workspace,
                call,
                &mut output,
                crate::agent::run::engine::Settlement {
                    // `call_tool` refuses the same form error before execution.
                    contract_failed: !blocked
                        && call_form_error(call, &self.schemas, &self.names).is_some(),
                    tool: env.ext.get(&call.name).map(|tool| tool.as_ref()),
                    ..Default::default()
                },
                |id| {
                    self.superseded.holds_full(id)
                        && self
                            .messages
                            .iter()
                            .any(|message| message["tool_call_id"].as_str() == Some(id))
                },
            );
            if let Some(reason) = stop {
                investigation_stop = Some(reason);
            }
            if output.is_error
                && let Some(advice) =
                    crate::learn::task_policy::recovery_hint(&call.name, &output.text)
            {
                output.text.push_str("\n\n");
                output.text.push_str(advice);
            }
            if let Some(advice) = repeat_advice {
                output.text.push_str("\n\n");
                output.text.push_str(&advice);
            }
            let image = if output.is_error {
                None
            } else {
                match crate::agent::run::dispatch::capture_output_image(
                    env.workspace,
                    call,
                    &mut output,
                ) {
                    Ok(image) => image,
                    Err(e) if call.name == "browser" || call.name == "computer" => {
                        return Ok(ControlFlow::Break(
                            crate::agent::run::engine::Tick::Finished(Outcome::failed(
                                self.steps,
                                format!("retaining screenshot: {e:#}"),
                            )),
                        ));
                    }
                    Err(e)
                        if call.name == "session_search"
                            && call.arguments["view"] == "layers"
                            && call.arguments["format"] == "image" =>
                    {
                        output = ToolOutput::err(format!(
                            "retaining Snapcompact view: {e:#}; use format:text"
                        ));
                        None
                    }
                    // The text result stands on its own; only the attachment is lost.
                    Err(e) => {
                        output
                            .text
                            .push_str(&format!("\n[Image attachment unavailable: {e:#}]"));
                        None
                    }
                }
            };
            // Shaped by the same helpers as the parent's results: this child's own window sets the
            // clip, and complete evidence is linked only when the visible body dropped content.
            let mut retain_notice = None;
            let artifact = match env.workspace.artifacts() {
                Some(store) => match store.retain(&output.text) {
                    Ok(uri) => Some(uri),
                    Err(e) => {
                        retain_notice = Some(format!(
                            "\n[Complete evidence could not be retained: {e:#}]"
                        ));
                        None
                    }
                },
                None => None,
            };
            let limit = crate::agent::workflow::efficiency::preview_limit(
                &env.config.efficiency,
                child_window(env, backend),
                &call.name,
                output.is_error,
                artifact.is_some(),
            );
            let original_observation = duplicate.is_none();
            let edit = crate::agent::workflow::efficiency::edit_result(
                &call.name,
                &output,
                artifact.is_some(),
            );
            let (mut body, dropped) = crate::agent::workflow::efficiency::visible_result(
                edit.as_deref().unwrap_or(&output.text),
                limit,
                duplicate,
            );
            let dropped = dropped || edit.is_some();
            if let Some(compact) = crate::agent::workflow::efficiency::compact_command(
                call,
                &output,
                artifact.as_deref(),
            ) {
                body = compact;
            }
            if body != output.text {
                let integrity = output.execution.output.get_or_insert_with(Default::default);
                integrity.mark_presentation_partial();
            }
            crate::telemetry::tool_output(
                env.config,
                false,
                &children.namespace,
                &call.name,
                tool_seconds,
                if blocked {
                    tools::ToolStatus::Blocked
                } else {
                    tools::ToolStatus::from_output(&output)
                },
                &output.execution,
            );
            if let Some(uri) = &artifact {
                crate::agent::workflow::efficiency::evidence_link(&mut body, uri, dropped);
            }
            if let Some(notice) = retain_notice {
                body.push_str(&notice);
            }
            let mut message = llm::tool_message(&call.id, &body);
            crate::tools::surface::outcome::annotate(&mut message, &output);
            crate::agent::compact::snapcompact::views::mark(&mut message, call, &output);
            message["is_error"] = json!(output.is_error);
            message["dial_evidence"] = json!({"artifact":artifact,"is_error":output.is_error});
            if call.name == "read" && !output.is_error && original_observation {
                let mut observation = call.arguments.clone();
                if let Some(args) = observation.as_object_mut() {
                    args.remove("i");
                }
                message["dial_evidence"]["observation"] = json!(observation.to_string());
            }
            if let Some(image) = image {
                message["dial_images"] = json!([image]);
            }
            let attach = match call.name.as_str() {
                "read" => call.arguments["path"].as_str(),
                crate::tools::net::image_gen::TOOL => call.arguments["output"].as_str(),
                _ => None,
            };
            if !output.is_error
                && let Some(path) = attach.filter(|path| llm::images::is_image(Path::new(path)))
            {
                let attachment = env
                    .workspace
                    .resolve(path)
                    .map_err(anyhow::Error::msg)
                    .and_then(|absolute| {
                        let relative = absolute
                            .strip_prefix(env.workspace.root())
                            .context("image resolves outside the project")?;
                        llm::images::attachment(env.workspace.root(), relative)
                    });
                match attachment {
                    Ok(image) => message["dial_images"] = json!([image]),
                    Err(error) => {
                        approver.notice(&format!("Image attachment unavailable: {error:#}"))
                    }
                }
            }
            if call.name == "ask"
                && let Some(data) = output.data.as_ref()
            {
                if let Some(images) = data.get("dial_images") {
                    message["dial_images"] = images.clone();
                }
                if let Some(result) = data.get("dial_question_result") {
                    message["dial_question_result"] = result.clone();
                }
            }
            if let Some(records) = output
                .data
                .as_ref()
                .and_then(|data| data["dial_question_results"].as_array())
            {
                crate::agent::workflow::questions::project_callbacks(
                    &mut message,
                    records.clone(),
                    tools::model_output_limit(child_window(env, backend)),
                );
            }
            if !output.is_error {
                if call.name == "new_context"
                    && children
                        .journal
                        .borrow()
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.tool_call_id.as_deref() == Some(&call.id))
                {
                    message["dial_context_result"] = json!(true);
                }
                if crate::agent::workflow::exploration::TOOL_NAMES.contains(&call.name.as_str()) {
                    message["dial_exploration_result"] = json!(true);
                }
            }
            if !output.is_error {
                let model = crate::agent::window::file_handles::model_result(
                    env.workspace,
                    call,
                    message["content"].as_str().unwrap_or(""),
                );
                if message["content"].as_str() != Some(model.as_str()) {
                    message["dial_model_content"] = json!(model);
                    message["dial_file_handles"] = json!(env.workspace.file_handle_snapshot());
                }
            }
            crate::storage::media::retain_message(&mut message, &children.image_store);
            if let Err(error) =
                children.archive_tool(&self.messages, &message, &output, call, env.ext)
            {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("archiving actual child tool result: {error:#}"),
                    )),
                ));
            }
            if !output.is_error
                && !output
                    .data
                    .as_ref()
                    .is_some_and(|data| data["is_error"] == true)
            {
                self.paired_tools = self.paired_tools.saturating_add(1);
            }
            self.messages.push(message);
        }
        Ok(ControlFlow::Continue(RoundSettlement {
            narration_warning,
            portable_batch_intercepted,
            round_messages,
            investigation_stop,
        }))
    }
    fn settle_round(
        &mut self,
        engine: &mut Engine,
        settlement: RoundSettlement,
    ) -> Result<ControlFlow<Tick<Outcome>, ()>> {
        let env = self.env;
        let job = self.job;
        let backend = &mut *self.backend;
        let adapter = self.adapter;
        let approver = &mut *self.approver;
        let rule_state = &mut *self.rule_state;
        let children = self.children;
        let rule_root = env.workspace.root();
        let RoundSettlement {
            narration_warning,
            portable_batch_intercepted,
            mut round_messages,
            investigation_stop,
        } = settlement;
        engine.research_round();
        if portable_batch_intercepted && rule_state.callback_retry() && self.budget != 0 {
            self.budget = self.budget.saturating_add(1);
        }
        round_messages.extend(approver.take_messages());
        let uncertain = engine.pending_refusal().is_some();
        let coordinated = append_coordination(&mut self.messages, round_messages);
        if coordinated {
            self.paired_tools = 0;
            rule_state.begin(rule_root, "");

            self.reasoning_recovery = Default::default();
            engine.transition(crate::agent::run::engine::RecoveryTransition::Coordination);
            if self.budget != 0 {
                self.budget = self.budget.saturating_add(1);
            }
        }
        for message in &mut self.messages {
            crate::storage::media::retain_message(message, &children.image_store);
        }
        if let Err(error) = children.archive(&self.messages) {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                    self.steps,
                    format!("{error:#}"),
                )),
            ));
        }
        if uncertain {
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Finished(Outcome::incomplete(self.steps, investigation_stop.unwrap_or_else(|| "Stopped after uncertain tool effects; inspect affected state before retrying.".into())))));
        }
        // Charge once before memory maintenance, hooks or any final publication can continue.
        if let Err(error) = engine.finish_tool_round() {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(
                    self.steps,
                    format!("{error:#}"),
                )),
            ));
        }
        let pending_memory = children.memory_pending.borrow_mut().take();
        if let Some(memory) = &children.memory
            && let Some(origin) = pending_memory.as_ref()
        {
            let result = (|| -> Result<()> {
                let authority = children.observe_memory()?;
                let observer = std::cell::RefCell::new(&mut *approver);
                let mut runner = ChildMemoryRunner::new(
                    env,
                    backend,
                    adapter,
                    &mut self.steps,
                    self.budget,
                    &observer,
                    children,
                    Some(origin),
                    job.spec.mutates,
                    false,
                );
                memory.maintain(&authority, &mut runner)
            })();
            if let Err(error) = result {
                if env.interrupt.load(Ordering::SeqCst) {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::interrupted(self.steps)),
                    ));
                }
                approver.notice(&format!(
                    "private child memory maintenance did not complete: {error:#}"
                ));
            }
        }
        let mut next = Chat {
            messages: std::mem::take(&mut self.messages),
            tools: std::mem::take(&mut self.schemas),
            temperature: env.harness.temperature,
            max_tokens: self.max_tokens,
            seed: None,
        };
        match serde_json::to_value(&*rule_state) {
            Ok(policy) => children.journal.borrow_mut().rule_policy = policy,
            Err(error) => {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("binding completed child rules: {error}"),
                    )),
                ));
            }
        }
        match settle_child_exploration(env, job, &next.tools, children, approver) {
            Ok(true) => {
                next = crate::agent::compact::compaction::replacement_request(
                    &next,
                    &child_history(children.journal.borrow().session.messages()),
                );
                if let Err(error) = children.refresh_binding(env, backend) {
                    return Ok(ControlFlow::Break(
                        crate::agent::run::engine::Tick::Finished(Outcome::failed(
                            self.steps,
                            format!("{error:#}"),
                        )),
                    ));
                }
                approver.notice("Private child exploration rewound to its real report branch. Actual workspace effects and usage remain; full abandoned source is recoverable with session_search history/all_branches during this task.");
            }
            Ok(false) => {}
            Err(error) => {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("{error:#}"),
                    )),
                ));
            }
        }
        let pending = children.journal.borrow().pending.clone();
        if let Some(signal) = pending {
            if let Err(error) = rollover_child(
                env,
                job,
                ChildRequestCtx {
                    backend,
                    adapter,
                    steps: &mut self.steps,
                    budget: self.budget,
                    approver,
                },
                &mut next,
                rule_state,
                children,
                Some(signal),
            ) {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        format!("{error:#}"),
                    )),
                ));
            }
            approver.notice("Private child notes-backed context boundary committed.");
        }
        self.messages = next.messages;
        self.schemas = next.tools;
        self.compaction_failed = false;
        if coordinated {
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        if portable_batch_intercepted {
            if !rule_state.callback_retry() {
                return Ok(ControlFlow::Break(
                    crate::agent::run::engine::Tick::Finished(Outcome::failed(
                        self.steps,
                        "portable guidance regeneration limit reached; callback and remaining unstarted batch actions were refused",
                    )),
                ));
            }
            return Ok(ControlFlow::Break(crate::agent::run::engine::Tick::Again));
        }
        if let Some(advice) = narration_warning {
            approver.notice(&advice);
            self.messages.push(crate::agent::run::guidance::note(
                crate::agent::run::guidance::Priority::Loop,
                advice,
            ));
        }
        if env.interrupt.load(Ordering::SeqCst) {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::interrupted(self.steps)),
            ));
        }
        if let Some(reason) = investigation_stop {
            return Ok(ControlFlow::Break(
                crate::agent::run::engine::Tick::Finished(Outcome::failed(self.steps, reason)),
            ));
        }
        Ok(ControlFlow::Continue(()))
    }
    pub(super) fn finish(self, driven: Result<Option<Outcome>>) -> Outcome {
        let job = self.job;
        let mut outcome = match driven {
            Ok(Some(outcome)) => outcome,
            Err(error) => Outcome::failed(self.steps, format!("{error:#}")),
            Ok(None) => {
                // Exhaustion still salvages the worker's last written findings.
                let reason = match last_written_findings(&self.messages) {
                    Some(text) => format!(
                        "used all {} steps without a final answer; last findings:\n{}",
                        job.max_steps,
                        clip_evidence(text, 4096)
                    ),
                    None => format!("used all {} steps without a final answer", job.max_steps),
                };
                Outcome::incomplete(self.steps, reason)
            }
        };
        outcome.inspections = self.inspections;
        // Moved out once: every finish path, including provider and budget exhaustion, keeps its history.
        outcome.conversation = Some(ChildConversation {
            messages: self.messages,
            tools: self.names,
        });
        outcome
    }
}
