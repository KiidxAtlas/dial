//! Foreground tool owner: keep the UI responsive while authorized native execution runs.
//! Callbacks return to this owner for admission; crate::tools::exec::execution owns execution and certainty.
use crate::config;

use crate::agent::*;

impl Agent<'_> {
    pub(in crate::agent) fn speculation_key(&self, name: &str, args: &Value) -> Option<String> {
        if self.hermetic
            || !self.effective_config().experimental.tool_speculation
            || !matches!(name, "ast_search" | "read" | "grep")
            || (!matches!(name, "read" | "grep")
                && self.ext.origin(name) != Some(crate::tools::surface::catalog::Origin::Native))
            || (matches!(name, "read" | "grep") && self.ext.get(name).is_some())
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
        let tool = self.ext.get(name);
        if tool.is_some_and(|tool| {
            tool.needs_approval(args)
                || tool.supports_tool_dispatch()
                || tool.may_modify_workspace(args)
        }) {
            return None;
        }
        use sha2::Digest as _;
        let authority = json!({"name":name,"session":self.session.id(),"step":self.cur_step,"root":self.workspace.root(),
            "approval":self.approval,"plan":self.plan_mode,"rules":self.always,
            "sandbox":format!("{:?}",self.workspace.sandbox_policy()),"revision":tool.map(|tool| tool.contract_revision()),"schema":tool.map(|tool| tool.schema()),"builtin_schema":crate::tools::schemas(&[name], self.workspace.edit_mode()).ok()});
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
        self.workspace.speculation.start_named(
            &self.workspace,
            self.effective_config().clone(),
            &self.ext,
            call,
            key,
            owner,
        )
    }
    /// `/image generate` and `/image edit`: one user-requested `image_gen` call outside a model
    /// turn. It refreshes image availability like a turn boundary, then takes the same approval,
    /// plan-mode and mutation authority a model call gets. `output` must be a new
    /// workspace-relative file; `source`, when given, is the workspace image to edit.
    ///
    /// # Errors
    /// Returns an unavailable image route or a refused approval; generation failures are the
    /// returned error output.
    pub fn generate_image(
        &mut self,
        prompt: &str,
        output: &str,
        source: Option<&str>,
        ui: &mut dyn Ui,
    ) -> Result<ToolOutput> {
        let name = crate::tools::net::image_gen::TOOL;
        ensure!(
            !self.hermetic,
            "image generation is unavailable in hermetic runs"
        );
        self.ext.sync_image_generation(&self.auth_config)?;
        ensure!(
            self.ext.get(name).is_some(),
            "image_gen is not enabled; set image.auto = true or add \"image_gen\" to integrations"
        );
        let mut arguments = json!({"prompt": prompt, "output": output});
        if let Some(source) = source {
            arguments["image"] = json!(source);
        }
        let call = ToolCall {
            id: String::new(),
            name: name.into(),
            arguments,
            malformed_arguments: false,
        };
        if let Err(denial) = self.authorize_call(&call, ui) {
            bail!("{}", denial.message());
        }
        ui.tool_call(name, output);
        let result = self
            .run_background_tool(name, &call.arguments, ui, None)
            .context("image_gen is not registered")?;
        ui.tool_result(name, &result);
        Ok(result)
    }
    /// Blocking I/O tools run off the controller thread so the inline UI can repaint and process input.
    pub(in crate::agent) fn run_background_tool(
        &mut self,
        name: &str,
        args: &Value,
        ui: &mut dyn Ui,
        backend: Option<&mut Backend>,
    ) -> Option<ToolOutput> {
        let call = ToolCall {
            id: String::new(),
            name: name.to_owned(),
            arguments: args.clone(),
            malformed_arguments: false,
        };
        self.run_background_tools(std::slice::from_ref(&call), ui, backend)
            .map(|outputs| {
                outputs
                    .into_iter()
                    .next()
                    .expect("one result for one call")
                    .0
            })
    }

    /// Shared native worker/event owner. Multi-call admission belongs to dependency dispatch;
    /// each callback still returns to this live controller before its native effect.
    pub(in crate::agent) fn run_background_tools(
        &mut self,
        calls: &[ToolCall],
        ui: &mut dyn Ui,
        mut backend: Option<&mut Backend>,
    ) -> Option<Vec<(ToolOutput, f64)>> {
        self.portable_callback_intercepted = false;
        if self.hermetic {
            return None;
        }
        if calls.iter().any(|call| {
            !config::tools::BUILTIN.contains(&call.name.as_str())
                && !self.ext.contains(&call.name)
                && !self.config.tools.contains_key(&call.name)
        }) {
            return None;
        }
        if calls.len() == 1 {
            let call = &calls[0];
            if let Some(key) = self.speculation_key(&call.name, &call.arguments)
                && let Some(mut output) = self.workspace.speculation.take_named(
                    &self.workspace,
                    &call.name,
                    &call.arguments,
                    &key,
                )
            {
                let data = output.data.get_or_insert_with(|| json!({}));
                if let Some(data) = data.as_object_mut() {
                    data.insert(
                        "speculation".into(),
                        json!({"reused":true,"stats":self.workspace.speculation.stats()}),
                    );
                }
                return Some(vec![(output, 0.0)]);
            }
        }
        let resource_bounds = if calls.len() > 1 {
            let bounds = calls
                .iter()
                .map(|call| {
                    crate::agent::run::dependency_dispatch::footprint(
                        &self.workspace,
                        self.edit_mode(),
                        call,
                    )
                })
                .collect::<Option<Vec<_>>>();
            let Some(bounds) = bounds else {
                return Some(calls.iter().map(|_| (ToolOutput::err("Native dependency resources changed before execution; retry serially.")
                    .with_status(tools::ToolStatus::Blocked).with_effects(tools::surface::outcome::Effects::None), 0.0)).collect());
            };
            Some(bounds)
        } else {
            None
        };
        let turn_cancel = self.cancellation.turn(self.interrupt.clone());
        let mut steering = Vec::new();
        let mut input_signalled = false;
        let mut signals = Vec::new();
        let mut active_calls = Vec::new();
        let mut workers = Vec::new();
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
            Done(ToolOutput, f64),
        }
        let (tx, rx) = mpsc::sync_channel(64);
        struct IndexedSender<T> {
            index: usize,
            sender: mpsc::SyncSender<(usize, T)>,
        }
        impl<T> IndexedSender<T> {
            fn send(&self, event: T) -> std::result::Result<(), mpsc::SendError<(usize, T)>> {
                self.sender.send((self.index, event))
            }
        }
        let failed_start = tx.clone();
        for (index, call) in calls.iter().enumerate() {
            let name = call.name.clone();
            let arguments = call.arguments.clone();
            let builtin = config::tools::BUILTIN.contains(&name.as_str());
            let extension = self.ext.get(&name).cloned();
            let external = self.config.tools.get(&name).cloned();
            #[cfg(test)]
            let effect_probe = super::dependency_dispatch::tests::effect_probe();
            let workspace = self.workspace.clone();
            let config = self.effective_config().clone();
            let registry = self.ext.clone();
            let mutation_cancel = turn_cancel.mutation();
            let cancel = mutation_cancel.interrupt.flag();
            let worker_cancel = mutation_cancel;
            let worker_signals = crate::tools::exec::signals::CallSignals::new();
            signals.push(worker_signals.clone());
            let active = Arc::new(AtomicBool::new(false));
            active_calls.push(active.clone());
            let started = active;
            let serial = !matches!(name.as_str(), "edit" | "write")
                && !extension.as_ref().is_some_and(|tool| {
                    tool.supports_tool_dispatch() || tool.manages_workspace_serialization()
                })
                && !crate::agent::safety::approval::parallel_read(&self.ext, &name, &arguments);
            let tx = IndexedSender {
                index,
                sender: tx.clone(),
            };
            match thread::Builder::new()
            .name("dial-tool".to_owned())
            .spawn(move || {
                let began = Instant::now();
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
                            #[cfg(test)]
                            if let Some(probe) = &effect_probe { probe(paths)?; }
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
                let _ = tx.send(Event::Done(output, began.elapsed().as_secs_f64()));
            }) {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                let _ = failed_start.send((index, Event::Done(ToolOutput::err(format!("starting tool: {error}")), 0.0)));
            }
        };
        }
        drop(tx);
        drop(failed_start);
        let mut callback_counts = vec![0usize; calls.len()];
        let native_authority = calls.iter().any(|call| {
            self.ext.contains(&call.name) || matches!(call.name.as_str(), "edit" | "write")
        });
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
        let mut outputs = vec![None; calls.len()];
        while outputs.iter().any(Option::is_none) {
            turn_cancel.sync();
            let queued = ui.take_steering();
            if !queued.is_empty() {
                for signal in &signals {
                    signal.request_steering();
                    signal.interrupt_waits();
                }
                steering.extend(queued);
                for call in calls {
                    if let Some(tool) = self.ext.get(&call.name)
                        && tool.interruptible(&call.arguments)
                    {
                        tool.cancel_operation();
                    }
                }
            } else if !input_signalled && ui.pending_input() {
                // Cooperative only: waits end and long commands move to bg; nothing is killed.
                input_signalled = true;
                for signal in &signals {
                    signal.request_steering();
                    signal.interrupt_waits();
                }
            }

            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok((position, event)) => {
                    let original_call = Some(calls[position].clone());
                    let callback_owner = calls[position].name.clone();
                    let guarded_edit = matches!(callback_owner.as_str(), "edit" | "write");
                    let supports_callbacks = self
                        .ext
                        .get(&callback_owner)
                        .is_some_and(|tool| tool.supports_tool_dispatch());
                    match event {
                        Event::Output(event) => {
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
                        Event::Callback(request, reply) => {
                            callback_counts[position] += 1;
                            let output = if !supports_callbacks {
                                ToolOutput::err(
                                    "this native tool has no code-mode callback capability",
                                )
                            } else if callback_counts[position] > 128 {
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
                        Event::Repair(request, reply) => {
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
                        Event::Mutation(tool, paths, unbounded, reply) => {
                            let result = (|| -> std::result::Result<(), String> {
                                if self.interrupt.load(Ordering::SeqCst) {
                                    return Err(
                                        "native effect cancelled before authorization".into()
                                    );
                                }
                                if let Some(bounds) = &resource_bounds
                                    && !bounds[position].permits(&paths, unbounded)
                                {
                                    return Err("native effect exceeded its admitted dependency footprint; retry serially".into());
                                }
                                if self.edit_preview_intercepted {
                                    return Err("native effect refused after a completed edit preview failed in this assistant batch".into());
                                }
                                let source_read = guarded_edit && tool == "read";
                                if tool != callback_owner && !source_read {
                                    return Err(
                                        "native mutation authority does not match the active tool"
                                            .into(),
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
                                            || self.approval
                                                == crate::config::types::Approval::Readonly,
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
                        Event::Confirm(summary, reply) => {
                            let allowed = match original_call.as_ref() {
                                Some(original) => {
                                    crate::agent::safety::approval::authorize_with_cancel(
                                        self.approval,
                                        &mut self.always,
                                        self.workspace.root(),
                                        original,
                                        &summary,
                                        ui,
                                        self.interrupt.as_ref(),
                                    )
                                    .is_ok()
                                }
                                None => false,
                            };
                            let _ = reply.send(allowed);
                        }
                        Event::Done(mut output, seconds) => {
                            if self.edit_preview_intercepted {
                                output.is_error = true;
                                output.text.push_str("\n[An authorized edit preview failed; remaining native callbacks/effects were refused. Earlier completed results are retained.]");
                            }
                            if let Some(reason) =
                                callback_gate.as_ref().and_then(|gate| gate.blocked())
                            {
                                output.is_error = true;
                                output.text.push_str(&format!("\n[{reason}]"));
                                self.portable_callback_intercepted = true;
                            }
                            outputs[position] = Some((output, seconds));
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    ui.heartbeat();
                    if self.interrupt.load(Ordering::SeqCst) {
                        for (call, active) in calls.iter().zip(&active_calls) {
                            if active.load(Ordering::SeqCst)
                                && let Some(tool) = self.ext.get(&call.name)
                            {
                                tool.cancel_operation();
                            }
                        }
                    }
                    self.pump_subagents(ui);
                }
                Err(_) => {
                    for output in &mut outputs {
                        if output.is_none() {
                            *output = Some((
                                ToolOutput::err("tool worker ended without output")
                                    .with_effects(crate::tools::surface::outcome::Effects::Unknown),
                                0.0,
                            ));
                        }
                    }
                }
            }
        }
        for worker in workers {
            let _ = worker.join();
        }
        self.pending_tool_steering.extend(steering);
        Some(
            outputs
                .into_iter()
                .map(|output| output.expect("each owned call settled"))
                .collect(),
        )
    }
}
