//! Foreground tool owner: keep the UI responsive while authorized native execution runs.
//! Callbacks return to this owner for admission; crate::tools::exec::execution owns execution and certainty.
use crate::config;

use crate::agent::*;

impl Agent<'_> {
    pub(in crate::agent) fn speculation_key(&self, name: &str, args: &Value) -> Option<String> {
        if self.hermetic
            || !self.effective_config().experimental.tool_speculation
            || name != "ast_search"
            || self.ext.origin(name) != Some(crate::tools::surface::catalog::Origin::Native)
            || crate::agent::safety::approval::denied_by_rule(
                &self.always,
                &ToolCall {
                    id: String::new(),
                    name: name.into(),
                    arguments: args.clone(),
                    malformed_arguments: false,
                },
                self.workspace.root(),
            )
        {
            return None;
        }
        let plugins = self.hook_plugins();
        if crate::agent::safety::hooks::wanted(
            self.tool_hooks(),
            plugins.as_deref(),
            "pre_tool_use",
        ) {
            return None;
        }
        let tool = self.ext.get(name)?;
        if tool.needs_approval(args)
            || tool.supports_tool_dispatch()
            || tool.may_modify_workspace(args)
        {
            return None;
        }
        use sha2::Digest as _;
        let authority = json!({"session":self.session.id(),"step":self.cur_step,"root":self.workspace.root(),
            "approval":self.approval,"plan":self.plan_mode,"rules":self.always,
            "sandbox":format!("{:?}",self.workspace.sandbox_policy()),"revision":tool.contract_revision(),"schema":tool.schema()});
        Some(format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(&authority).ok()?)
        ))
    }
    pub(in crate::agent) fn speculative_pending(&self, call: &ToolCall) -> bool {
        self.speculation_key(&call.name, &call.arguments)
            .is_some_and(|key| self.workspace.speculation.pending(&key, &call.arguments))
    }
    pub(in crate::agent) fn warm_speculative_read(&mut self, call: &ToolCall) -> bool {
        let Some(key) = self.speculation_key(&call.name, &call.arguments) else {
            return false;
        };
        if self.authorize_call(call, &mut NullUi).is_err() {
            return false;
        }
        let owner = self.cancellation.turn(self.interrupt.clone());
        self.workspace.speculation.start(
            &self.workspace,
            self.effective_config().clone(),
            &self.ext,
            call.arguments.clone(),
            key,
            owner,
        )
    }
    /// Blocking I/O tools run off the controller thread so the inline UI can repaint and process input.
    pub(in crate::agent) fn run_background_tool(
        &mut self,
        name: &str,
        args: &Value,
        ui: &mut dyn Ui,
        mut backend: Option<&mut Backend>,
    ) -> Option<ToolOutput> {
        self.portable_callback_intercepted = false;
        if self.hermetic {
            return None;
        }
        let builtin = config::tools::BUILTIN.contains(&name);
        let extension = self.ext.get(name).cloned();
        let registry = self.ext.clone();
        let cancellation = extension.clone();
        let external = self.config.tools.get(name).cloned();
        if !builtin && extension.is_none() && external.is_none() {
            return None;
        }
        if let Some(key) = self.speculation_key(name, args)
            && let Some(mut output) = self.workspace.speculation.take(&self.workspace, args, &key)
        {
            let data = output.data.get_or_insert_with(|| json!({}));
            if let Some(data) = data.as_object_mut() {
                data.insert(
                    "speculation".into(),
                    json!({"reused":true,"stats":self.workspace.speculation.stats()}),
                );
            }
            return Some(output);
        }
        let workspace = self.workspace.clone();
        let config = self.effective_config().clone();
        let arguments = args.clone();
        let name = name.to_owned();
        let callback_owner = name.clone();
        let guarded_edit = matches!(name.as_str(), "edit" | "write");
        let original_call = (cancellation.is_some() || guarded_edit).then(|| ToolCall {
            id: String::new(),
            name: callback_owner.clone(),
            arguments: args.clone(),
            malformed_arguments: false,
        });
        let turn_cancel = self.cancellation.turn(self.interrupt.clone());
        let mutation_cancel = turn_cancel.mutation();
        let cancel = mutation_cancel.interrupt.flag();
        let worker_cancel = mutation_cancel;
        let signals = crate::tools::exec::signals::CallSignals::new();
        let worker_signals = signals.clone();
        let interruptible = extension
            .as_ref()
            .is_some_and(|tool| tool.interruptible(args));
        let mut steering = Vec::new();
        let active = Arc::new(AtomicBool::new(false));
        let started = active.clone();
        let serial = !matches!(name.as_str(), "edit" | "write")
            && !extension.as_ref().is_some_and(|tool| {
                tool.supports_tool_dispatch() || tool.manages_workspace_serialization()
            })
            && !crate::agent::safety::approval::parallel_read(&self.ext, &name, &arguments);
        enum Event {
            Output(crate::tools::integrations::ext::ToolEvent),
            Callback(
                crate::tools::integrations::ext::ToolCallback,
                mpsc::SyncSender<ToolOutput>,
            ),
            Mutation(
                String,
                Vec<PathBuf>,
                bool,
                mpsc::SyncSender<std::result::Result<(), String>>,
            ),
            Confirm(String, mpsc::SyncSender<bool>),
            Repair(
                tools::EditRepairRequest,
                mpsc::SyncSender<std::result::Result<String, String>>,
            ),
            Done(ToolOutput),
        }
        let (tx, rx) = mpsc::sync_channel(64);
        let worker = match thread::Builder::new()
            .name("dial-tool".to_owned())
            .spawn(move || {
                let output = crate::tools::exec::execution::run(&cancel, || crate::tools::exec::execution::mutation_scope(&worker_cancel, || crate::tools::exec::signals::scope(&worker_signals, || {
                    let _serial = if serial {
                        Some(workspace.serial_operation())
                    } else {
                        None
                    };
                    if cancel.load(Ordering::SeqCst) {
                        return interrupted_tool();
                    }
                    started.store(true, Ordering::SeqCst);
                    let emit = |event| {
                        let _ = tx.send(Event::Output(event));
                    };
                    if matches!(name.as_str(), "bash" | "read") {
                        return crate::tools::exec::execution::native(
                            &ToolContext { workspace: &workspace, config: &config, mutation_authorizer: None, confirm: None },
                            &registry, &name, &arguments, &emit, &cancel,
                        ).expect("native tool");
                    }
                        let authorize_mutation = |requested: &str, paths: &[PathBuf], unbounded: bool| {
                            if cancel.load(Ordering::SeqCst) { return Err("native effect cancelled before authorization".into()); }
                            let (reply, result) = mpsc::sync_channel(1);
                            tx.send(Event::Mutation(requested.into(), paths.to_vec(), unbounded, reply))
                                .map_err(|_| "native mutation controller disconnected".to_owned())?;
                            loop {
                                match result.recv_timeout(Duration::from_millis(80)) {
                                    Ok(result) => return result,
                                    Err(RecvTimeoutError::Timeout) if cancel.load(Ordering::SeqCst) => {
                                        return Err("native effect cancelled while waiting for authorization".into());
                                    }
                                    Err(RecvTimeoutError::Timeout) => {}
                                    Err(RecvTimeoutError::Disconnected) => {
                                        return Err("native mutation authorization ended without a result".into());
                                    }
                                }
                            }
                        };
                        let confirm = |summary: &str| -> bool {
                            if cancel.load(Ordering::SeqCst) { return false; }
                            let (reply, result) = mpsc::sync_channel(1);
                            if tx.send(Event::Confirm(summary.into(), reply)).is_err() {
                                return false;
                            }
                            loop {
                                match result.recv_timeout(Duration::from_millis(80)) {
                                    Ok(answer) => return answer,
                                    Err(RecvTimeoutError::Timeout) if cancel.load(Ordering::SeqCst) => return false,
                                    Err(RecvTimeoutError::Timeout) => {}
                                    Err(RecvTimeoutError::Disconnected) => return false,
                                }
                            }
                        };
                    if matches!(name.as_str(), "edit" | "write") {
                        let repair = |request: &tools::EditRepairRequest| {
                            if cancel.load(Ordering::SeqCst) { return Err("edit repair cancelled".into()); }
                            let (reply, result) = mpsc::sync_channel(1);
                            tx.send(Event::Repair(request.clone(), reply))
                                .map_err(|_| "edit repair controller disconnected".to_owned())?;
                            loop {
                                match result.recv_timeout(Duration::from_millis(80)) {
                                    Ok(result) => return result,
                                    Err(RecvTimeoutError::Timeout) if cancel.load(Ordering::SeqCst) => {
                                        return Err("edit repair cancelled while waiting for the owner".into());
                                    }
                                    Err(RecvTimeoutError::Timeout) => {}
                                    Err(RecvTimeoutError::Disconnected) => return Err("edit repair ended without a result".into()),
                                }
                            }
                        };
                        let context = ToolContext { workspace: &workspace, config: &config, mutation_authorizer: Some(&authorize_mutation), confirm: Some(&confirm) };
                        return if name == "edit" {
                            tools::edit_with_context(&context, &arguments, Some(&repair), &cancel)
                        } else {
                            tools::write_with_context(&context, &arguments, Some(&repair), &cancel)
                        };
                    }
                    if builtin {
                        return crate::tools::exec::execution::native(&ToolContext { workspace: &workspace, config: &config, mutation_authorizer: Some(&authorize_mutation), confirm: Some(&confirm) }, &registry, &name, &arguments, &emit, &cancel).expect("native tool");
                    }
                    if let Some(tool) = extension {
                        let mut dispatch = |request| {
                            if cancel.load(Ordering::SeqCst) {
                                return interrupted_tool();
                            }
                            let (reply, result) = mpsc::sync_channel(1);
                            if tx.send(Event::Callback(request, reply)).is_err() {
                                return ToolOutput::err("code-mode controller disconnected");
                            }
                            loop {
                                match result.recv_timeout(Duration::from_millis(80)) {
                                    Ok(output) => return output,
                                    Err(RecvTimeoutError::Timeout) if cancel.load(Ordering::SeqCst) => {
                                        return ToolOutput::err("callback interrupted after dispatch")
                                            .with_status(tools::ToolStatus::Aborted)
                                            .with_effects(crate::tools::surface::outcome::Effects::Unknown);
                                    }
                                    Err(RecvTimeoutError::Timeout) => {}
                                    Err(RecvTimeoutError::Disconnected) => {
                                        return ToolOutput::err("code-mode callback ended without a result").with_effects(crate::tools::surface::outcome::Effects::Unknown);
                                    }
                                }
                            }
                        };
                        return crate::tools::exec::execution::with_dispatch(&registry, tool.as_ref(),
                            &ToolContext {
                                workspace: &workspace,
                                config: &config,
                                mutation_authorizer: Some(&authorize_mutation),
                                confirm: Some(&confirm),
                            },
                            &arguments,
                            &emit,
                            &cancel,
                            &mut dispatch,
                        );
                    }
                    let tool = external.expect("external tool was checked");
                    custom_tool_output(&tool, &arguments, &cancel)
                })));
                started.store(false, Ordering::SeqCst);
                let _ = tx.send(Event::Done(output));
            }) {
            Ok(worker) => worker,
            Err(e) => {
                return Some(ToolOutput { execution: Default::default(),
                    data: None,
                    text: format!("starting tool: {e}"),
                    is_error: true,
                });
            }
        };
        let mut callback_count = 0usize;
        let supports_callbacks = cancellation
            .as_ref()
            .is_some_and(|tool| tool.supports_tool_dispatch());
        let native_authority = cancellation.is_some() || guarded_edit;
        let callback_catalog = if native_authority && self.config.portable_rules.enabled {
            self.layout
                .as_ref()
                .map(|layout| {
                    crate::agent::safety::rules::Catalog::discover(layout, self.workspace.root())
                })
                .unwrap_or_default()
        } else {
            crate::agent::safety::rules::Catalog::default()
        };
        for warning in &callback_catalog.warnings {
            ui.notice(warning);
        }
        let mut callback_state = native_authority
            .then(|| crate::agent::safety::rules::State::restore(&self.session.control_state()));
        let mut callback_regenerations = callback_state
            .as_ref()
            .map(crate::agent::safety::rules::State::regenerations)
            .unwrap_or(0);
        let mut callback_gate = callback_state.as_mut().map(|state| {
            crate::agent::safety::rules::CallbackGate::new(
                crate::agent::safety::rules::CallbackRules {
                    catalog: &callback_catalog,
                    state,
                    agent: "main",
                    regenerations: &mut callback_regenerations,
                    max_regenerations: self.config.portable_rules.max_regenerations,
                },
            )
        });
        let output = loop {
            turn_cancel.sync();
            let queued = ui.take_steering();
            if !queued.is_empty() {
                signals.request_steering();
                signals.interrupt_waits();
                steering.extend(queued);
                if interruptible && let Some(tool) = &cancellation {
                    tool.cancel_operation();
                }
            }

            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(Event::Output(event)) => {
                    if matches!(
                        event.kind.as_str(),
                        "background_output" | "background_progress"
                    ) {
                        ui.background_output(
                            &event.name,
                            &event.text,
                            event.kind == "background_progress",
                        );
                    } else if event.kind == "output" {
                        ui.tool_output(&event.name, &event.text);
                    } else if event.kind == "progress" {
                        ui.tool_progress(&event.name, &event.text);
                    } else {
                        ui.background_notice(&format!(
                            "{}: {}{}",
                            event.name,
                            event.text,
                            event
                                .artifact
                                .map(|uri| format!("\n[Complete output: {uri}]"))
                                .unwrap_or_default()
                        ));
                    }
                }
                Ok(Event::Callback(request, reply)) => {
                    callback_count += 1;
                    let output = if !supports_callbacks {
                        ToolOutput::err("this native tool has no code-mode callback capability")
                    } else if callback_count > 128 {
                        ToolOutput::err("code-mode callback limit exceeded (128 per cell)")
                    } else if let (Some(backend), Some(gate)) =
                        (backend.as_deref_mut(), callback_gate.as_mut())
                    {
                        self.code_callback(backend, request, &callback_owner, ui, gate)
                    } else {
                        ToolOutput::err(
                            "code-mode callbacks require the agent execution controller",
                        )
                    };
                    self.portable_callback_intercepted |= callback_gate
                        .as_ref()
                        .is_some_and(|gate| gate.blocked().is_some());
                    let _ = reply.send(output);
                }
                Ok(Event::Repair(request, reply)) => {
                    let result = if self.interrupt.load(Ordering::SeqCst) {
                        Err("edit repair cancelled".into())
                    } else if !guarded_edit || !self.config.edit.auto_repair.enabled {
                        Err("edit repair is not enabled for this execution owner".into())
                    } else if let Some(main) = backend.as_deref_mut() {
                        self.repair_edit(main, &request, ui)
                            .map_err(|error| format!("{error:#}"))
                    } else {
                        Err("edit repair requires the live model owner".into())
                    };
                    let _ = reply.send(result);
                }
                Ok(Event::Mutation(tool, paths, unbounded, reply)) => {
                    let result = (|| -> std::result::Result<(), String> {
                        if self.interrupt.load(Ordering::SeqCst) {
                            return Err("native effect cancelled before authorization".into());
                        }
                        if self.edit_preview_intercepted {
                            return Err("native effect refused after a completed edit preview failed in this assistant batch".into());
                        }
                        let source_read = guarded_edit && tool == "read";
                        if tool != callback_owner && !source_read {
                            return Err(
                                "native mutation authority does not match the active tool".into()
                            );
                        }
                        let gate = callback_gate
                            .as_mut()
                            .ok_or("native effect requires the owner rule controller")?;
                        if let Some(reason) = gate.blocked() {
                            return Err(reason.into());
                        }
                        let original = original_call
                            .as_ref()
                            .ok_or("native effect has no authorized original call")?;
                        if source_read {
                            // Source bytes are read-deny policy only; the owner edit's own target authorization and interception cover the write.
                            return crate::agent::safety::approval::authorize_source_read_paths(
                                &self.always,
                                self.workspace.root(),
                                self.workspace.additional_roots(),
                                original,
                                &tool,
                                &paths,
                                unbounded,
                            );
                        }
                        crate::agent::safety::approval::authorize_mutation_paths(
                            crate::agent::safety::approval::MutationPolicy {
                                rules: &self.always,
                                root: self.workspace.root(),
                                extra_roots: self.workspace.additional_roots(),
                                read_only: self.plan_mode
                                    || self.approval == crate::config::types::Approval::Readonly,
                            },
                            original,
                            &tool,
                            &paths,
                            unbounded,
                        )?;
                        let observed = crate::agent::safety::rules::mutation_call(
                            self.workspace.root(),
                            self.workspace.additional_roots(),
                            original,
                            &paths,
                            unbounded,
                        )?;
                        self.intercept_native_effect(gate, &observed, ui)
                    })();
                    let _ = reply.send(result);
                }
                Ok(Event::Confirm(summary, reply)) => {
                    let allowed = match original_call.as_ref() {
                        Some(original) => crate::agent::safety::approval::authorize_with_cancel(
                            self.approval,
                            &mut self.always,
                            self.workspace.root(),
                            original,
                            &summary,
                            ui,
                            self.interrupt.as_ref(),
                        )
                        .is_ok(),
                        None => false,
                    };
                    let _ = reply.send(allowed);
                }
                Ok(Event::Done(mut output)) => {
                    if self.edit_preview_intercepted {
                        output.is_error = true;
                        output.text.push_str("\n[An authorized edit preview failed; remaining native callbacks/effects were refused. Earlier completed results are retained.]");
                    }
                    if let Some(reason) = callback_gate.as_ref().and_then(|gate| gate.blocked()) {
                        output.is_error = true;
                        output.text.push_str(&format!("\n[{reason}]"));
                        self.portable_callback_intercepted = true;
                    }
                    break output;
                }
                Err(RecvTimeoutError::Timeout) => {
                    ui.heartbeat();
                    if self.interrupt.load(Ordering::SeqCst)
                        && active.load(Ordering::SeqCst)
                        && let Some(tool) = &cancellation
                    {
                        tool.cancel_operation();
                    }
                    self.pump_subagents(ui);
                }
                Err(_) => {
                    break ToolOutput {
                        execution: Default::default(),
                        data: None,
                        text: "tool worker ended without output".into(),
                        is_error: true,
                    };
                }
            }
        };
        let _ = worker.join();
        self.pending_tool_steering.extend(steering);
        Some(output)
    }
}
