//! Tool call authorization and dispatch, code callbacks, custom tools and parallel tool groups.
use crate::config;

use crate::agent::*;

pub(in crate::agent::run) mod search_index;
#[cfg(test)]
mod search_tests;

/// The request with its history replaced by a contract projection, or `None` when the projection
/// changed nothing. Unchanged requests stay borrowed, so the common path copies no history.
pub(in crate::agent) fn projected_request(
    request: &Chat,
    projection: Option<Vec<Value>>,
) -> Option<Chat> {
    projection.map(|messages| Chat {
        messages,
        tools: request.tools.clone(),
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        seed: request.seed,
    })
}

pub(in crate::agent) fn callback_image(
    workspace: &Workspace,
    call: &ToolCall,
    output: &mut ToolOutput,
    supplied: Option<Value>,
) -> Result<Option<(Value, Vec<u8>, &'static str)>> {
    if output.is_error {
        return Ok(None);
    }
    // `github` file_read attaches PNG bytes as `data.artifact`, the same shape browser screenshots use.
    let reference = if call.name == "browser" || call.name == "computer" || call.name == "github" {
        crate::storage::artifact::capture_image(workspace.artifacts(), output)?
    } else if call.name == "read"
        && let Some(path) = call.arguments["path"]
            .as_str()
            .filter(|path| llm::images::is_image(Path::new(path)))
    {
        Some(match supplied {
            Some(reference) => reference,
            None => {
                let absolute = workspace.resolve(path).map_err(anyhow::Error::msg)?;
                let relative = absolute.strip_prefix(workspace.root())?;
                llm::images::attachment(workspace.root(), relative)?
            }
        })
    } else {
        None
    };
    reference
        .map(|reference| {
            let (bytes, mime) = llm::images::verified_bytes(&reference)?;
            Ok((reference, bytes, mime))
        })
        .transpose()
}

pub(in crate::agent) fn callback_image_payload(
    output: &mut ToolOutput,
    image: Option<(Value, Vec<u8>, &'static str)>,
) -> Result<()> {
    if let Some((reference, bytes, mime)) = image {
        use base64::Engine;
        let data = output.data.get_or_insert_with(|| json!({}));
        let object = data
            .as_object_mut()
            .context("image callback data must be an object")?;
        object.insert(
            "images".into(),
            json!([{
                "type":"image",
                "mimeType":mime,
                "data":base64::engine::general_purpose::STANDARD.encode(bytes),
                "reference":reference
            }]),
        );
    }
    Ok(())
}

pub(in crate::agent) fn interrupted_tool() -> ToolOutput {
    crate::tools::exec::execution::before_start()
}

/// Stdin for an operator-configured tool. With `parameters` the program receives the whole
/// arguments object after its `required` properties are checked; without them, a single `input`.
pub(in crate::agent) fn custom_tool_stdin(
    tool: &crate::config::types::ToolConfig,
    args: &Value,
) -> Result<String> {
    let Some(schema) = &tool.parameters else {
        let input = args.get("input").and_then(Value::as_str).unwrap_or("");
        return Ok(json!({"input": input}).to_string());
    };
    for key in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        ensure!(
            crate::tools::present(args, key),
            "missing required argument `{key}`"
        );
    }
    Ok(args.to_string())
}

pub(in crate::agent) fn custom_tool_output(
    tool: &crate::config::types::ToolConfig,
    args: &Value,
    cancel: &AtomicBool,
) -> ToolOutput {
    let input = match custom_tool_stdin(tool, args) {
        Ok(input) => input,
        Err(error) => {
            return ToolOutput::err(format!("{error:#}"))
                .with_effects(crate::tools::surface::outcome::Effects::None);
        }
    };
    match worker::run_tool_command(&tool.program, &tool.args, &input, tool.timeout_secs, cancel) {
        Ok(text) => ToolOutput {
            text,
            is_error: false,
            data: None,
            execution: Default::default(),
        },
        Err(error) => {
            let effects = if error.downcast_ref::<worker::ToolEffectsUnknown>().is_some() {
                crate::tools::surface::outcome::Effects::Unknown
            } else {
                crate::tools::surface::outcome::Effects::None
            };
            ToolOutput::err(format!("{error:#}")).with_effects(effects)
        }
    }
}

/// Run only a prevalidated consecutive group. No agent/session/UI state enters a worker.
pub(in crate::agent) fn parallel_tools(
    workspace: &Workspace,
    config: &Config,
    ext: &Registry,
    calls: &[ToolCall],
    interrupt: &AtomicBool,
    ui: &mut dyn Ui,
    pending_steering: &mut Vec<String>,
) -> Vec<(ToolOutput, f64)> {
    let signals = crate::tools::exec::signals::CallSignals::new();
    thread::scope(|scope| {
        let mut receivers = Vec::with_capacity(calls.len());
        let mut unique: HashMap<(&str, String), usize> = HashMap::new();
        let mut sources = Vec::with_capacity(calls.len());
        let mut remaining = Vec::new();
        let mut first_calls: Vec<usize> = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            let key = (
                call.name.as_str(),
                crate::agent::safety::guard::normalized_arguments(call).to_string(),
            );
            if let Some(&source) = unique.get(&key)
                && !["pre_tool_use", "post_tool_use"].iter().any(|event| {
                    crate::agent::safety::hooks::wanted_for_tool(
                        &config.hooks,
                        ext.plugins().as_deref(),
                        event,
                        &call.name,
                    )
                })
            {
                sources.push(source);
                remaining[source] += 1;
                ui.diagnostic(&format!(
                    "Batch reuse: {} shares the result of {}; no duplicate inspection runs.",
                    call.id, calls[first_calls[source]].id
                ));
                continue;
            }
            let source = receivers.len();
            unique.insert(key, source);
            sources.push(source);
            remaining.push(1usize);
            first_calls.push(index);
            let (tx, rx) = mpsc::sync_channel(1);
            receivers.push(rx);
            let signals = signals.clone();
            scope.spawn(move || {
                let started = Instant::now();
                let mut notices = Vec::new();
                let mut policy_reason = None;
                let output = crate::tools::exec::execution::run(interrupt, || crate::tools::exec::signals::scope(&signals, || {
                    if interrupt.load(Ordering::SeqCst) {
                        return interrupted_tool();
                    }
                    let plugins = ext.plugins();
                    match crate::agent::safety::hooks::pre_tool_with(&config.hooks, plugins.as_deref(), workspace.root(), &call.name, &call.arguments, interrupt, &mut |notice| notices.push(notice.to_owned())) {
                        Err(error) => {
                            policy_reason = Some(crate::agent::safety::approval::PolicyReason::HookVeto);
                            return ToolOutput::err(error).with_status(tools::ToolStatus::Blocked);
                        }
                        Ok(Some(args)) if args != call.arguments => {
                            policy_reason = Some(crate::agent::safety::approval::PolicyReason::HookVeto);
                            return ToolOutput::err("pre_tool_use changed a scheduled call; retry the revised arguments through the execution controller").with_status(tools::ToolStatus::Blocked);
                        }
                        _ => {}
                    }
                    let mut output = crate::tools::exec::execution::native(
                        &ToolContext { workspace, config, mutation_authorizer: None, confirm: None },
                        ext, &call.name, &call.arguments, &|_| {}, interrupt,
                    ).unwrap_or_else(|| ToolOutput::err("parallel tool unavailable"));
                    crate::agent::safety::hooks::post_tool_with(&config.hooks, plugins.as_deref(), workspace.root(), &call.name, &call.arguments, &mut output, interrupt, &mut |notice| notices.push(notice.to_owned()));
                    output
                }));
                let _ = tx.send(((output, started.elapsed().as_secs_f64()), notices, policy_reason));
            });
        }
        let mut results: Vec<_> = receivers
            .into_iter()
            .enumerate()
            .map(|(source, rx)| {
                Some(loop {
                    let steering = ui.take_steering();
                    if !steering.is_empty() {
                        signals.request_steering();
                        signals.interrupt_waits();
                        pending_steering.extend(steering);
                        for call in calls {
                            if let Some(tool) = ext
                                .get(&call.name)
                                .filter(|tool| tool.interruptible(&call.arguments))
                            {
                                tool.cancel_operation();
                            }
                        }
                    }
                    if interrupt.load(Ordering::SeqCst) {
                        signals.interrupt_waits();
                        for call in calls {
                            if let Some(tool) = ext.get(&call.name) {
                                tool.cancel_operation();
                            }
                        }
                    }
                    match rx.recv_timeout(Duration::from_millis(80)) {
                        Ok((result, notices, policy_reason)) => {
                            if let Some(reason) = policy_reason {
                                crate::agent::safety::approval::explain(
                                    ui,
                                    &calls[first_calls[source]],
                                    reason,
                                );
                            }
                            for notice in notices {
                                ui.notice(&notice);
                            }
                            break result;
                        }
                        Err(RecvTimeoutError::Timeout) => ui.heartbeat(),
                        Err(RecvTimeoutError::Disconnected) => {
                            break (ToolOutput::err("tool worker disconnected"), 0.0);
                        }
                    }
                })
            })
            .collect();
        sources
            .into_iter()
            .enumerate()
            .map(|(index, source)| {
                remaining[source] -= 1;
                let (output, seconds) = if remaining[source] == 0 {
                    results[source].take().expect("unconsumed batch result")
                } else {
                    results[source]
                        .as_ref()
                        .expect("shared batch result")
                        .clone()
                };
                (
                    output,
                    if index == first_calls[source] {
                        seconds
                    } else {
                        0.0
                    },
                )
            })
            .collect()
    })
}

impl<'a> Agent<'a> {
    pub(in crate::agent) fn authorize_call(
        &mut self,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> std::result::Result<(), crate::agent::safety::approval::Denial> {
        let local_memory_write = call.name == "learn"
            && self.memory_runtime.as_ref().is_some_and(|memory| {
                memory.id() == crate::config::memory_backend::MemoryBackendId::Local
            });
        let result = if crate::agent::safety::approval::denied_by_rule(
            &self.always,
            call,
            self.workspace.root(),
        ) {
            crate::agent::safety::approval::explain_rule(
                ui,
                call,
                self.workspace.root(),
                &self.always,
                crate::agent::safety::approval::Effect::Deny,
            );
            Err(crate::agent::safety::approval::Denial::Rule)
        } else if self.plan_mode
            && (local_memory_write
                || !crate::agent::safety::approval::is_read_only(
                    &self.ext,
                    &call.name,
                    &call.arguments,
                ))
        {
            crate::agent::safety::approval::explain(
                ui,
                call,
                crate::agent::safety::approval::PolicyReason::PlanMode,
            );
            Err(crate::agent::safety::approval::Denial::ReadOnly)
        } else if local_memory_write
            || crate::agent::safety::approval::needs_approval(&self.ext, call)
        {
            crate::agent::safety::approval::authorize_with_cancel(
                self.approval,
                &mut self.always,
                self.workspace.root(),
                call,
                &describe(call),
                ui,
                self.interrupt.as_ref(),
            )
            .map(|persist| {
                if persist && let Err(error) = self.persist_permissions() {
                    ui.notice(&format!(
                        "The rule applies to this session but could not be saved: {error:#}"
                    ));
                }
            })
        } else {
            crate::agent::safety::approval::explain(
                ui,
                call,
                crate::agent::safety::approval::PolicyReason::SafeCall,
            );
            Ok(())
        };
        if let Err(denial) = &result {
            ui.notice(denial.message());
        }
        result
    }

    /// Operator hooks apply to live agents only; evaluation runs must stay hermetic.
    pub(in crate::agent) fn tool_hooks(&self) -> &[crate::config::types::HookConfig] {
        if self.hermetic {
            &[]
        } else {
            &self.config.hooks
        }
    }

    /// The plugin host lifecycle hooks dispatch through. Hermetic runs get none, like `tool_hooks`.
    pub(in crate::agent) fn hook_plugins(
        &self,
    ) -> Option<Arc<crate::tools::integrations::plugin::Host>> {
        if self.hermetic {
            None
        } else {
            self.ext.plugins()
        }
    }

    /// `before_provider_request` for one model call made by this agent. Hook notices reach `ui`;
    /// `None` means send the borrowed request unchanged.
    pub(in crate::agent) fn provider_request_with(
        &self,
        request: &Chat,
        ui: &mut dyn Ui,
    ) -> Result<Option<Chat>> {
        let projected = projected_request(
            request,
            crate::tools::surface::contracts::project_history(
                &request.messages,
                &request.tools,
                &self.ext,
            )?,
        );
        let mut notices = Vec::new();
        let rewritten = crate::agent::safety::hooks::provider_request(
            self.tool_hooks(),
            self.hook_plugins().as_deref(),
            self.workspace.root(),
            projected.as_ref().unwrap_or(request),
            self.interrupt.as_ref(),
            &mut |notice| notices.push(notice.to_owned()),
        )?;
        for notice in &notices {
            ui.notice(notice);
        }
        Ok(rewritten.or(projected))
    }

    pub(in crate::agent) fn dispatch(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let output = self.dispatch_call(backend, call, ui);
        modes::lifecycle::Lifecycle::dispatched(self, call, &output);
        output
    }

    pub(in crate::agent) fn dispatch_call(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        if self
            .ext
            .get(&call.name)
            .is_some_and(|tool| tool.supports_tool_dispatch())
        {
            self.callback_question_results.clear();
        }
        if call.name == "finish" {
            self.step_checks.finish_accepted = false;
        }
        let plugins = self.hook_plugins();
        let mut rewritten = None;
        if call.name != "exit_plan"
            && crate::agent::safety::hooks::wanted(
                self.tool_hooks(),
                plugins.as_deref(),
                "pre_tool_use",
            )
        {
            let root = self.workspace.root().to_path_buf();
            let configured_hooks = self.tool_hooks();
            let interrupt = self.interrupt.as_ref();
            let mut notices = Vec::new();
            let (result, context) = crate::agent::safety::hooks::with_heartbeat(ui, || {
                crate::agent::safety::hooks::pre_tool_noted(
                    configured_hooks,
                    plugins.as_deref(),
                    &root,
                    &call.name,
                    &call.arguments,
                    interrupt,
                    &mut |notice| notices.push(notice.to_owned()),
                )
            });
            notices.iter().for_each(|notice| {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Hook,
                    notice,
                ))
            });
            self.queue_hook_context(context);
            match result {
                Err(reason) => {
                    crate::agent::safety::approval::explain(
                        ui,
                        call,
                        crate::agent::safety::approval::PolicyReason::HookVeto,
                    );
                    if let Err(error) = self.record(
                        self.cur_step,
                        "tool_denied",
                        json!({"name":call.name,"hook":true}),
                    ) {
                        ui.notice(&format!("tool denial was not journaled: {error:#}"));
                    }
                    return ToolOutput {
                        execution: Default::default(),
                        data: None,
                        text: reason,
                        is_error: true,
                    };
                }
                Ok(arguments) => rewritten = arguments,
            }
        }
        let arguments_changed = rewritten
            .as_ref()
            .is_some_and(|arguments| arguments != &call.arguments);
        // A rewritten call still runs through schemas and approvals inside `dispatch_inner`.
        let effective = match rewritten {
            Some(arguments) => ToolCall {
                arguments,
                ..call.clone()
            },
            None => call.clone(),
        };
        let call = &effective;
        let mut approval_denied = false;
        let mut out =
            self.dispatch_inner(backend, call, ui, &mut approval_denied, arguments_changed);
        if call.name != "exit_plan"
            && !approval_denied
            && crate::agent::safety::hooks::wanted(
                self.tool_hooks(),
                plugins.as_deref(),
                "post_tool_use",
            )
        {
            let root = self.workspace.root().to_path_buf();
            let configured_hooks = self.tool_hooks();
            let interrupt = self.interrupt.as_ref();
            let mut notices = Vec::new();
            let context = crate::agent::safety::hooks::with_heartbeat(ui, || {
                crate::agent::safety::hooks::post_tool_with(
                    configured_hooks,
                    plugins.as_deref(),
                    &root,
                    &call.name,
                    &call.arguments,
                    &mut out,
                    interrupt,
                    &mut |notice| notices.push(notice.to_owned()),
                )
            });
            notices.iter().for_each(|notice| {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Hook,
                    notice,
                ))
            });
            self.queue_hook_context(context);
        }
        out
    }

    pub(in crate::agent) fn dispatch_inner(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        ui: &mut dyn Ui,
        approval_denied: &mut bool,
        arguments_changed: bool,
    ) -> ToolOutput {
        let denied = |why: &str| {
            ToolOutput {
                execution: Default::default(),
                data: None,
                text: why.to_owned(),
                is_error: true,
            }
            .with_status(tools::ToolStatus::Blocked)
            .with_effects(crate::tools::surface::outcome::Effects::None)
        };
        if self.edit_preview_intercepted {
            return denied(
                "This callback was not executed: an earlier authorized edit preview failed in this assistant batch.",
            );
        }
        // Unchanged calls already passed the request contract at admission. Only a
        // hook rewrite invalidates that evidence; authority is always checked below.
        if arguments_changed {
            let names = std::slice::from_ref(&call.name);
            let schemas = match self.schemas(names) {
                Ok(schemas) => schemas,
                Err(error) => return denied(&format!("resolving tool schema: {error:#}")),
            };
            if let Some((_, why)) = call_form_error(call, &schemas, names) {
                return denied(&format!("invalid tool call: {why}"));
            }
        }
        if self.repro_first_fires(call, crate::config::types::ReproFirst::Gate) {
            return denied(crate::agent::verify::repro_first::GATE);
        }
        if call.name == "request_directory" {
            return self.request_directory(call, ui);
        }
        if call.name == "exit_plan" && self.plan_mode {
            let Some(plan) = call.arguments["plan"]
                .as_str()
                .filter(|p| !p.trim().is_empty())
            else {
                return denied("exit_plan requires a nonempty plan");
            };
            self.latest_plan = Some(plan.to_owned());
            if self.plan_yolo_auto_approves() {
                return match self.plan_yolo_approve(plan, ui) {
                    Ok(()) => ToolOutput { execution: Default::default(),  data: None,
                        text: "Plan approved; implementation is now permitted subject to normal tool approvals.".into(), is_error: false },
                    Err(error) => denied(&format!("Plan-yolo approval failed: {error:#}")),
                };
            }
            match ui.approve("exit_plan", plan) {
                Decision::Allow | Decision::AllowAlways | Decision::AllowProject => {
                    self.accept_plan(plan, ui);
                    return ToolOutput { execution: Default::default(),  data: None,
                        text: "Plan approved; implementation is now permitted subject to normal tool approvals.".into(), is_error: false };
                }
                Decision::Deny => {
                    return denied(&format!(
                        "Plan rejected; remain in plan mode. {}",
                        ui.approval_reason()
                            .unwrap_or_else(|| "Ask the user what to revise.".into())
                    ));
                }
            }
        }
        if let Err(denial) = self.authorize_call(call, ui) {
            *approval_denied = true;
            if let Err(error) = self.record(self.cur_step, "tool_denied", json!({"name":call.name}))
            {
                ui.notice(&format!("tool denial was not journaled: {error:#}"));
            }
            return denied(denial.message());
        }
        if self.interrupt.load(Ordering::SeqCst) {
            return interrupted_tool();
        }
        if let Some(preview) = self.authorized_edit_preview(call) {
            self.edit_preview_intercepted = true;
            return preview;
        }
        let args = &call.arguments;
        let name = call.name.as_str();
        if crate::agent::workflow::exploration::TOOL_NAMES.contains(&name) {
            return self.dispatch_exploration(call);
        }
        if self.exploration.busy() && matches!(name, "new_context" | "finish") {
            return denied(
                "An investigation checkpoint is active or pending. Complete rewind before new_context or finish; actual effects, notebook and unfinished work remain intact.",
            );
        }
        if name == "ask" {
            let request = match crate::agent::workflow::questions::prepare(
                args,
                self.config,
                self.plan_mode,
                self.session.id(),
            ) {
                Ok(request) => request,
                Err(error) => return ToolOutput::err(error),
            };
            let outcome = ui.ask_questions(&request, self.interrupt.as_ref());
            return crate::agent::workflow::questions::output(
                &request,
                outcome,
                self.interrupt.as_ref(),
            );
        }
        if crate::agent::window::context_notes::TOOL_NAMES.contains(&name) {
            let owner = self.session.id().to_owned();
            let enabled = self.config.compaction.experimental_context_management;
            if name == "context_notes" {
                let output = crate::agent::window::context_notes::execute_notes(
                    &mut self.session,
                    args,
                    &owner,
                    enabled,
                    &self.interrupt,
                );
                if let Some(memory) = &self.memory_runtime {
                    memory.resume(memory_authority(&self.session, 0));
                }
                return output;
            }
            return match crate::agent::window::context_notes::request_new_context(
                &self.session,
                args,
                &owner,
                &call.id,
                enabled,
                &self.interrupt,
            ) {
                Ok((output, request)) => {
                    self.pending_context_rollover = Some(request);
                    output
                }
                Err(error) => ToolOutput::err(error.to_string()),
            };
        }
        if name == "session_search"
            && (self.experimental_context_owner() || self.exploration_owner())
        {
            return session::session_search_tool::call_owned(
                self.layout.as_ref(),
                &self.session,
                args,
            );
        }
        if name == "read"
            && !self.hermetic
            && let Some(path) = args["path"].as_str()
            && let Some(reference) =
                llm::images::authorized_user_reference(&self.session.messages(), Path::new(path))
        {
            if args["offset"].as_u64().is_some() {
                return denied("images do not support line offsets");
            }
            return match llm::images::verified_bytes(&reference) {
                Ok((bytes, _)) => ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: format!(
                        "Image: {path} ({} original bytes; explicitly supplied by the user; attached to the next model request when vision is available)",
                        bytes.len()
                    ),
                    is_error: false,
                },
                Err(error) => denied(&format!("Cannot read supplied image: {error:#}")),
            };
        }
        if name == "experience" {
            return self.experience_tool(backend, call, ui);
        }
        if name == "edit_preview" {
            return self.preview_tool(args, ui);
        }
        if name == "find" {
            match crate::tools::code::navigation::semantic_request(&self.workspace, args) {
                Ok(Some(request)) => return self.run_semantic_find(backend, args, request, ui),
                Ok(None) => {}
                Err(text) => {
                    return ToolOutput {
                        execution: Default::default(),
                        data: None,
                        text,
                        is_error: true,
                    };
                }
            }
        }
        if self
            .memory_runtime
            .as_ref()
            .is_some_and(|memory| memory.offered_names().contains(&name))
        {
            backend.release();
            return match self.selected_memory_tool(call, ui) {
                Ok(value) => ToolOutput {
                    execution: Default::default(),
                    text: value.to_string(),
                    data: Some(value),
                    is_error: false,
                },
                Err(error) => ToolOutput::err(format!("{error:#}")),
            };
        }
        if let Some(out) = self.run_background_tool(name, args, ui, Some(backend)) {
            if name == "bash" {
                self.note_verification(call, &out, ui);
            }
            return out;
        }
        if name == "tts" {
            let env = crate::voice::VoiceEnv::from_config(self.config);
            return match crate::voice::tool::execute(&env, args, &self.interrupt) {
                Ok(text) => ToolOutput::structured(json!({"spoken":true,"result":text})),
                Err(error) => ToolOutput::err(format!("{error:#}")),
            };
        }
        if config::tools::BUILTIN.contains(&name) {
            let workspace = &self.workspace;
            let out = blocking_with_heartbeat(ui, || tools::execute(workspace, name, args, 120))
                .unwrap_or_else(|error| ToolOutput::err(format!("{error:#}")));
            if name == "bash" {
                self.note_verification(call, &out, ui);
            }
            return out;
        }
        if name == crate::security::tool::NAME && self.ext.get(name).is_some() {
            let session_id = self.session.id().to_owned();
            let event = |event: &str, is_error| LifecycleEvent {
                event: event.into(),
                session_id: Some(session_id.clone()),
                turn_id: None,
                tool_name: Some(call.name.clone()),
                is_error,
            };
            self.ext.fire_lifecycle(&event("tool_started", None));
            let output = self.run_security_scan(backend, args, ui);
            self.ext
                .fire_lifecycle(&event("tool_finished", Some(output.is_error)));
            return output;
        }
        if let Some(tool) = self.ext.get(name) {
            let event = |event: &str, is_error| LifecycleEvent {
                event: event.into(),
                session_id: Some(self.session.id().to_owned()),
                turn_id: None,
                tool_name: Some(call.name.clone()),
                is_error,
            };
            self.ext.fire_lifecycle(&event("tool_started", None));
            // Network, LSP and MCP calls can take seconds; the UI keeps responding meanwhile.
            let workspace = &self.workspace;
            let config = self.effective_config();
            let output = blocking_with_heartbeat(ui, || {
                tool.call(
                    &ToolContext {
                        workspace,
                        config,
                        mutation_authorizer: None,
                        confirm: None,
                    },
                    args,
                )
            })
            .unwrap_or_else(|error| ToolOutput::err(format!("{error:#}")));
            self.ext
                .fire_lifecycle(&event("tool_finished", Some(output.is_error)));
            return output;
        }
        match name {
            "todo" => {
                let mut next = self.todo.clone();
                let output = next.call(args);
                if output.is_error || next == self.todo {
                    return output;
                }
                let mut state = self.session.control_state();
                state["todo"] = next.snapshot();
                if let Err(error) = self.session.set_control_state(state) {
                    return denied(&format!("could not persist todos: {error:#}"));
                }
                self.todo = next;
                ui.todo(&self.todo);
                output
            }
            "goal" => match self.goal_tool(args) {
                Ok(text) => ToolOutput {
                    execution: Default::default(),
                    text,
                    is_error: false,
                    data: None,
                },
                Err(error) => denied(&error),
            },
            "check" => self.check_tool(args, ui),
            "revert_edit" => self.revert_edit_tool(call),
            "finish" => {
                let finish = match crate::agent::verify::completion::Finish::parse(args) {
                    Ok(finish) => finish,
                    Err(error) => return denied(&error),
                };
                if finish.outcome == TurnOutcome::NeedsInput {
                    let question = finish.question.as_deref().unwrap_or_default();
                    return match ui.ask_user(&finish.summary, question, &finish.options) {
                        Some(answer) if !answer.trim().is_empty() => ToolOutput {
                            execution: Default::default(),
                            data: None,
                            text: format!("User response: {answer}"),
                            is_error: false,
                        },
                        _ if !ui.interactive_input() => {
                            self.step_checks.finish_accepted = true;
                            ToolOutput {
                                execution: Default::default(),
                                data: None,
                                text: finish.text,
                                is_error: false,
                            }
                        }
                        _ => denied(
                            "The input request was cancelled; continue without it or ask again interactively.",
                        ),
                    };
                }
                if finish.outcome == TurnOutcome::Completed
                    && let Err(reason) = self.validate_completion(ui)
                {
                    return denied(&reason);
                }

                self.step_checks.finish_accepted = true;
                ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: "Turn outcome accepted.".into(),
                    is_error: false,
                }
            }
            "tool_search" => {
                let query = match crate::tools::req_str(args, "query") {
                    Ok(query) => query,
                    Err(error) => return denied(&error),
                };
                let mut descriptions = HashMap::new();
                let candidates = self
                    .catalog()
                    .into_iter()
                    .filter_map(|name| {
                        if !self.tool_eligible(&name) {
                            return None;
                        }
                        let schema = self.schemas(std::slice::from_ref(&name)).ok()?.pop()?;
                        let description = schema["function"]["description"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned();
                        let indexed = search_index::text(&schema);
                        descriptions.insert(name.clone(), description);
                        Some((name, indexed))
                    })
                    .collect();
                let matches = crate::agent::model::assistance::discover(query, candidates);
                let activated: Vec<String> = matches
                    .iter()
                    .filter(|entry| entry.2)
                    .map(|entry| entry.0.clone())
                    .collect();
                if let Err(error) = self.activate_tools(&activated) {
                    return denied(&format!("persisting discovered tools: {error:#}"));
                }
                let offered = self.offered();
                let matches: Vec<Value> = matches.into_iter().map(|(name, _, _)| {
                    let description = descriptions.remove(&name).unwrap_or_default();
                    json!({"enabled_next_round":offered.contains(&name),"name":name,"description":description})
                }).collect();
                ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: json!(matches).to_string(),
                    is_error: false,
                }
            }
            "tool_load" => {
                let requested: Vec<String> = args["names"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let unload: Vec<String> = args["unload"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let catalog = self.catalog();
                if (requested.is_empty() && unload.is_empty())
                    || requested
                        .iter()
                        .chain(&unload)
                        .any(|n| !catalog.contains(n))
                {
                    return denied("tool_load contains an unavailable tool; use tool_search first");
                }
                if let Some(name) = requested.iter().find(|name| !self.tool_eligible(name)) {
                    return denied(&format!(
                        "tool {name} is unavailable under the current mode or tool allowlist"
                    ));
                }
                let mut enabled = self.enabled_tools.clone();
                enabled.extend(requested.iter().cloned());
                for name in &unload {
                    enabled.remove(name);
                }
                let mut discovered = self.discovered_tools();
                discovered.retain(|name| !unload.contains(name) && !enabled.contains(name));
                let mut state = self.session.control_state();
                let mut sorted: Vec<_> = enabled.iter().cloned().collect();
                sorted.sort();
                state["enabled_tools"] = json!(sorted);
                state["discovered_tools"] = json!(discovered);
                if let Err(e) = self.session.set_control_state(state) {
                    return denied(&format!("persisting enabled tools: {e:#}"));
                }
                self.enabled_tools = enabled;
                self.tool_routing.unload(&unload);
                self.rebuild_system();
                ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: format!(
                        "Next round: enabled [{}]; unloaded [{}]",
                        requested.join(", "),
                        unload.join(", ")
                    ),
                    is_error: false,
                }
            }
            "artifact_read" => match self.workspace.artifacts() {
                Some(store) => {
                    let output = store.call(args);
                    if !output.is_error {
                        self.metrics.artifacts_retrieved += 1;
                        if self.session.control_state()["truncated_artifacts"]
                            .as_array()
                            .is_some_and(|uris| uris.contains(&args["uri"]))
                        {
                            self.metrics.truncated_recoveries += 1;
                        }
                    }
                    output
                }
                None => denied("artifacts are unavailable in hermetic runs"),
            },
            "working_state" => {
                let mut state = self.session.control_state();
                let working = match crate::agent::workflow::working_state::apply(
                    &state["working_state"],
                    args,
                ) {
                    Ok(working) => working,
                    Err(error) => return denied(&format!("{error:#}")),
                };
                if args["action"] != "view" {
                    state["working_state"] = working.clone();
                    if let Err(e) = self.session.set_control_state(state) {
                        return denied(&format!("{e:#}"));
                    }
                }
                ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: working.to_string(),
                    is_error: false,
                }
            }
            "skill" | "skill_manage" | "session_search" => match &self.layout {
                None => denied("unavailable here"),
                Some(layout) => match name {
                    "skill"
                        if args
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|skill| !self.modes.launch.skill_visible(skill)) =>
                    {
                        crate::runtime::tool_output::ToolOutput {
                            execution: Default::default(),
                            data: None,
                            text: "skill is withheld by the --skills launch allowlist".into(),
                            is_error: true,
                        }
                    }
                    "skill" => skills::skill_tool::call(layout, args),
                    "skill_manage" => skills::manage_tool::call(layout, args),
                    _ if crate::tools::present(args, "history")
                        || crate::tools::present(args, "entry_id")
                        || crate::tools::present(args, "all_branches") =>
                    {
                        denied(
                            "raw session history requires the experimental durable session owner",
                        )
                    }
                    _ => session::session_search_tool::call(layout, args),
                },
            },
            "task" => self.run_task(backend, args, ui),
            "message" => {
                crate::agent::workers::live_tasks::output(self.live_tasks.message_call(args))
            }
            "peers" => self.peer_coordination(args),
            "solve" => self.run_solve(backend, args, ui),
            "escalate" => {
                backend.release();
                let env = crate::agent::workers::subagents::Env {
                    config: &self.auth_config,
                    workspace: &self.workspace,
                    ext: &self.ext,
                    layout: self.layout.as_ref(),
                    harness: &self.version.harness,
                    interrupt: &self.interrupt,
                    usage: Some(&self.usage_scope),
                    context_cap: Some(self.context_window()),
                    agents_messages: &self.agents_messages,
                    plan_mode: self.plan_mode,
                    memory: self.memory_runtime.as_ref(),
                };
                crate::agent::workflow::solve::escalate(
                    env,
                    args,
                    &mut self.session_cost,
                    ui,
                    self.session.id(),
                    self.hermetic,
                )
            }
            other => match self.config.tools.get(other) {
                Some(tool) => custom_tool_output(tool, args, &self.interrupt),
                None => denied(&format!("unknown tool: {other}")),
            },
        }
    }
    pub(in crate::agent) fn intercept_native_effect(
        &mut self,
        gate: &mut crate::agent::safety::rules::CallbackGate<'_>,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> std::result::Result<(), String> {
        let intercepted = gate.intercept(self.workspace.root(), call);
        let state_changed = gate.take_state_changed();
        self.portable_callback_intercepted |= gate.blocked().is_some();
        if intercepted.is_some() || state_changed {
            let mut control = self.session.control_state();
            gate.state().save(&mut control);
            if let Err(error) = self.session.set_control_state(control) {
                self.portable_callback_intercepted = true;
                return Err(gate.journaling_failed(format!(
                    "journaling portable native-effect state before execution: {error:#}; this effect and remaining native callbacks were refused"
                )));
            }
        }
        if let Some(reason) = intercepted {
            ui.notice(&reason);
            return Err(reason);
        }
        Ok(())
    }
    pub(in crate::agent) fn code_callback(
        &mut self,
        backend: &mut Backend,
        request: crate::tools::integrations::ext::ToolCallback,
        owner: &str,
        ui: &mut dyn Ui,
        gate: &mut crate::agent::safety::rules::CallbackGate<'_>,
    ) -> ToolOutput {
        if self.interrupt.load(Ordering::SeqCst) {
            return interrupted_tool();
        }
        if let Some(reason) = gate.blocked() {
            return ToolOutput::err(reason);
        }
        let mut names = self.offered();
        names.retain(|name| {
            !matches!(
                name.as_str(),
                "finish" | "exit_plan" | "checkpoint" | "rewind"
            ) && !self
                .ext
                .get(name)
                .is_some_and(|tool| tool.supports_tool_dispatch())
        });
        let (name, arguments) = match request {
            crate::tools::integrations::ext::ToolCallback::Schemas => {
                return match self.schemas(&names) {
                    Ok(schemas) => ToolOutput {
                        execution: Default::default(),
                        data: Some(Value::Array(schemas)),
                        text: String::new(),
                        is_error: false,
                    },
                    Err(error) => {
                        ToolOutput::err(format!("listing authorized code-mode tools: {error:#}"))
                    }
                };
            }
            crate::tools::integrations::ext::ToolCallback::Call { name, arguments } => {
                (name, arguments)
            }
        };
        if !names.contains(&name) {
            return ToolOutput::err(format!(
                "tool {name:?} is not available in this code-mode scope"
            ));
        }
        let schemas = match self.schemas(std::slice::from_ref(&name)) {
            Ok(schemas) => schemas,
            Err(error) => {
                return ToolOutput::err(format!("resolving code-mode tool schema: {error:#}"));
            }
        };
        let call = ToolCall {
            id: String::new(),
            name,
            arguments,
            malformed_arguments: false,
        };
        if let Some((_, why)) = call_form_error(&call, &schemas, &names) {
            return ToolOutput::err(format!("invalid code-mode tool call: {why}"));
        }
        if let Err(reason) = self.intercept_native_effect(gate, &call, ui) {
            return ToolOutput::err(reason);
        }
        if let Err(error) = self.record(
            self.cur_step,
            "code_tool_call",
            json!({"parent":owner,"name":call.name,"input":call.arguments,"contract":tools::surface::contracts::identities(&schemas,&self.ext).get(&call.name)}),
        ) {
            return ToolOutput::err(format!(
                "recording code-mode call before execution: {error:#}"
            ));
        }
        ui.tool_call_with_args(&call.id, &call.name, &summarize(&call), &call.arguments);
        if let Err(error) = self.session.flush_runtime() {
            let output = ToolOutput::err(format!(
                "journaling code-mode call before execution: {error:#}"
            ));
            ui.tool_result_with_id(&call.id, &call.name, &output);
            return output;
        }
        let started = Instant::now();
        let mut output = self.dispatch(backend, &call, ui);
        let supplied = if call.name == "read" {
            call.arguments["path"].as_str().and_then(|path| {
                llm::images::authorized_user_reference(&self.session.messages(), Path::new(path))
            })
        } else {
            None
        };
        let image = match callback_image(&self.workspace, &call, &mut output, supplied) {
            Ok(image) => image,
            Err(error) => {
                output.is_error = true;
                output.text.push_str(&format!(
                    "\n[Preparing callback image evidence failed: {error:#}]"
                ));
                None
            }
        };
        if let Some((reference, _, _)) = &image {
            ui.image(reference);
        }
        ui.tool_timing(&call.name, started.elapsed().as_secs_f64());
        ui.tool_result_with_id(&call.id, &call.name, &output);
        if call.name == "ask" {
            self.callback_question_results
                .push(json!({"parent":owner,"arguments":call.arguments,
                "text":output.text,"data":output.data,"is_error":output.is_error}));
        }
        // Persist compact image references, not duplicate base64 pixels, in the
        // execution ledger before returning the richer payload to native code.
        if let Err(error) = self.session.flush_runtime().and_then(|_| {
            self.record(
                self.cur_step,
                "code_tool_result",
                json!({"parent":owner,"name":call.name,"is_error":output.is_error,"output":output.text,"data":output.data}),
            )
        }) {
            output.execution.effects = crate::tools::surface::outcome::Effects::Unknown;
            output.is_error = true;
            output.text.push_str(&format!("\n[Recording completed callback evidence failed; inspect state before retrying: {error:#}]"));
        }
        if let Err(error) = callback_image_payload(&mut output, image) {
            output.is_error = true;
            output.text.push_str(&format!(
                "\n[Encoding callback image payload failed: {error:#}]"
            ));
        }
        output
    }

    /// Bash jobs owned by this session that must settle before completion can be accepted.
    pub(in crate::agent) fn awaited_background_jobs(&self) -> Vec<String> {
        let mut jobs = Vec::new();
        for name in self.ext.names() {
            if let Some(tool) = self.ext.get(name) {
                jobs.extend(tool.awaited_work());
            }
        }
        jobs
    }

    /// Keep unfinished bash work visible without blocking the controller or user steering.
    pub(in crate::agent) fn await_background_jobs(&mut self, ui: &mut dyn Ui) -> bool {
        self.pump_subagents(ui);
        let jobs = self.awaited_background_jobs();
        let workers = self.live_tasks.running_ids();
        if !self.interrupt.load(Ordering::Relaxed) {
            if !jobs.is_empty() {
                self.background_notices.push(format!(
                    "[Background work is still running]\n{}\nContinue independent work first. If nothing useful remains, tell the user what is pending and use a bounded `bg wait`. Do not report these jobs as completed; inspect their results or stop jobs you no longer need.",
                    jobs.join("\n")
                ));
            }
            // Live workers are awaited like background jobs: the turn waits for their results rather than ending.
            if !workers.is_empty() {
                self.background_notices.push(format!(
                    "[Live workers are still running]\n{}\nWait for their results with `task wait`, or continue independent work first. Their findings arrive as they finish; do not report these workers as completed before their results are in.",
                    workers.join("\n")
                ));
            }
        }
        // Finished worker results are queued for the next request: the turn continues to incorporate them.
        !self.background_notices.is_empty() || !self.live_tasks.undelivered().is_empty()
    }
}

#[cfg(test)]
mod tests;
