//! Role-routed auxiliary requests: semantic find, edit repair and role chats.

use super::*;

impl<'a> Agent<'a> {
    pub(super) fn run_semantic_find(
        &mut self,
        main: &mut Backend,
        args: &Value,
        request: tools::navigation::SemanticRequest,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let result = (|| -> Result<ToolOutput> {
            let supplementary = tools::navigation::lsp_args(args).map_err(anyhow::Error::msg)?;
            let response = if request.has_candidates() {
                let chat = semantic_chat(&request, self.version.harness.max_tokens);
                self.role_chat(main, "small", &chat, ui)
                    .context("semantic find ranking failed")?
            } else {
                "{\"indices\":[]}".to_owned()
            };
            let mut output = ToolOutput {
                execution: Default::default(),
                data: None,
                text: request
                    .render_response(&response)
                    .map_err(anyhow::Error::msg)?,
                is_error: false,
            };
            if !request.has_candidates() {
                output.text.insert_str(
                    0,
                    "[No source candidates; semantic model was not invoked.]\n",
                );
            }
            if let Some(arguments) = supplementary {
                let metadata = self.run_background_tool("lsp", &arguments, ui, None).context(
                    "semantic find supplemental metadata requires the configured lsp integration",
                )?;
                append_find_supplement(&self.workspace, &mut output, metadata)?;
            }
            Ok(output)
        })();
        result.unwrap_or_else(|e| ToolOutput {
            execution: Default::default(),
            data: None,
            text: format!("{e:#}"),
            is_error: true,
        })
    }

    pub(super) fn role_selection(&self, role: &str) -> Option<&llm::model_config::ModelSelection> {
        self.config.model_roles.get(role).filter(|selection| {
            !(selection.base_url.is_none()
                && self.config.endpoint.is_none()
                && selection.model == self.config.model
                && selection.provider == "mlx"
                && self.config.provider.as_deref().is_none_or(|p| p == "mlx"))
        })
    }

    pub(super) fn completed_edit_preview_error(&self, reply: &llm::Reply) -> Option<String> {
        if !self.config.edit.streaming_abort || reply.finish_reason == "length" {
            return None;
        }
        let policy = if self.plan_mode {
            Approval::Readonly
        } else {
            self.approval
        };
        for call in &reply.tool_calls {
            if call.name != "edit" || call.malformed_arguments {
                continue;
            }
            let Ok(targets) = tools::editing_targets(self.edit_mode(), &call.arguments) else {
                continue;
            };
            let Ok(paths) = targets
                .iter()
                .map(|path| self.workspace.resolve(path))
                .collect::<std::result::Result<Vec<_>, _>>()
            else {
                continue;
            };
            if !approval::edit_preview_allowed(
                policy,
                &self.always,
                self.workspace.root(),
                call,
                &paths,
            ) {
                continue;
            }
            let preview = tools::preview_edit(&self.workspace, &call.arguments);
            if tools::preview_edit_should_abort(&preview) {
                return Some(preview.text);
            }
        }
        None
    }
    pub(super) fn authorized_edit_preview(&self, call: &ToolCall) -> Option<ToolOutput> {
        if !self.config.edit.streaming_abort || call.name != "edit" || call.malformed_arguments {
            return None;
        }
        let targets = tools::editing_targets(self.edit_mode(), &call.arguments).ok()?;
        let paths = targets
            .iter()
            .map(|path| self.workspace.resolve(path))
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()?;
        // The edit has already been approved. Keep explicit edit/read denials;
        // never prompt for or grant a source-read permission during preview.
        if !approval::edit_preview_allowed(
            Approval::Auto,
            &self.always,
            self.workspace.root(),
            call,
            &paths,
        ) {
            return None;
        }
        let preview = tools::preview_edit_streaming(&self.workspace, &call.arguments);
        tools::preview_edit_should_abort(&preview).then_some(preview)
    }
    pub(super) fn repair_edit(
        &mut self,
        main: &mut Backend,
        request: &tools::EditRepairRequest,
        ui: &mut dyn Ui,
    ) -> Result<String> {
        ensure!(
            !self.interrupt.load(Ordering::SeqCst),
            "edit repair cancelled"
        );
        let role_config = self.auth_config.for_role("coach");
        let selection = role_config.selection();
        let (window, output_budget) = if self.role_selection("coach").is_some() {
            if !self.role_backends.contains_key("coach") {
                self.role_backends
                    .insert("coach".into(), make_backend(&role_config)?);
            }
            let helper = self
                .role_backends
                .get("coach")
                .expect("inserted coach backend");
            (
                role_config.context_window.min(
                    helper
                        .model_profile()
                        .and_then(|profile| profile.context_window)
                        .unwrap_or(u32::MAX),
                ),
                helper.output_budget(self.version.harness.max_tokens),
            )
        } else {
            (
                self.context_window(),
                main.output_budget(self.version.harness.max_tokens),
            )
        };
        let mut chat = edit_runtime::repair_request(request, 1);
        let estimate = compaction::request_tokens_with_policy(
            &chat,
            &role_config.compaction,
            compaction::CompactionIdentity {
                provider: &selection.provider,
                model: &selection.model,
                agent: Some("coach"),
            },
        );
        ensure!(
            estimate < u64::from(window),
            "complete source repair exceeds the coach model window; no partial repair was requested"
        );
        chat.max_tokens = output_budget
            .min(u32::try_from(u64::from(window) - estimate).unwrap_or(u32::MAX))
            .max(1);
        let text = self.role_chat(main, "coach", &chat, ui)?;
        ensure!(
            !self.interrupt.load(Ordering::SeqCst),
            "edit repair cancelled"
        );
        edit_runtime::repair_response(&text).map_err(anyhow::Error::msg)
    }
    /// Stats/OTLP row for an auxiliary model call (role, maintenance, summary, memory), emitted
    /// exactly as the main turn path does so the same session is not undercounted.
    pub(super) fn record_aux_request(
        &self,
        backend: &Backend,
        reply: &Result<llm::Reply>,
        started: Instant,
    ) {
        match reply {
            Ok(reply) => crate::telemetry::request(
                self.config,
                self.hermetic,
                self.session.id(),
                backend.model_label(),
                reply,
                backend.reply_cost(reply),
                started.elapsed(),
            ),
            Err(_) => crate::telemetry::request_failed(
                self.config,
                self.hermetic,
                self.session.id(),
                backend.model_label(),
                started.elapsed(),
            ),
        }
    }
    pub(super) fn role_chat(
        &mut self,
        main: &mut Backend,
        role: &str,
        request: &Chat,
        ui: &mut dyn Ui,
    ) -> Result<String> {
        self.role_chat_timeout(main, role, request, self.config.worker_timeout_secs, ui)
    }
    pub(super) fn role_chat_timeout(
        &mut self,
        main: &mut Backend,
        role: &str,
        request: &Chat,
        timeout_secs: u64,
        ui: &mut dyn Ui,
    ) -> Result<String> {
        main.require_session_identity()?;
        let namespace = match &self.active_side_namespace {
            Some(namespace) => namespace.clone(),
            None => compaction_runtime::side_request_namespace(&self.session.id())?,
        };
        let previous_cache = main.swap_cache_namespace(&namespace, &self.auth_config)?;
        let previous_scope = main.set_usage_scope(Some(self.usage_scope.worker()));
        let previous_profile = main.swap_request_profile(None);
        let result = self.role_chat_scoped(
            main,
            role,
            request,
            ui,
            &namespace,
            previous_profile.as_ref(),
            timeout_secs,
        );
        main.restore_cache_namespace(previous_cache);
        main.set_usage_scope(previous_scope);
        let _ = main.swap_request_profile(previous_profile);
        result
    }
    pub(super) fn role_chat_scoped(
        &mut self,
        main: &mut Backend,
        role: &str,
        request: &Chat,
        ui: &mut dyn Ui,
        namespace: &str,
        main_profile: Option<&llm::profiles::ModelProfile>,
        timeout_secs: u64,
    ) -> Result<String> {
        let selection = self.role_selection(role).cloned();
        let Some(selection) = selection else {
            if (request.max_tokens > 0 || main_profile.is_some())
                && let Some(mut profile) = main_profile.or(main.compaction_profile()).cloned()
            {
                if request.max_tokens > 0 {
                    profile.max_tokens = Some(
                        profile
                            .max_tokens
                            .filter(|cap| *cap > 0)
                            .map_or(request.max_tokens, |cap| cap.min(request.max_tokens)),
                    );
                }
                main.next_request_profile(crate::llm::profiles::request_tiers_for(
                    Some(profile),
                    &self.auth_config.selection(),
                    self.auth_config.effective_service_tier(),
                    self.auth_config.extended_context,
                )?);
            }
            let rewritten = self.provider_request_with(request, ui)?;
            let request = rewritten.as_ref().unwrap_or(request);
            let started = Instant::now();
            let result = if self.hermetic {
                crate::learn::compute::background_call(self.config, main, |main, cancel| {
                    main.chat_with_control(
                        self.version.adapter.as_deref(),
                        request,
                        self.config.worker_timeout_secs,
                        None,
                        &mut |_| {},
                        cancel,
                    )
                })
            } else {
                let adapter = self.version.adapter.clone();
                let timeout = timeout_secs;
                let interrupt = self.interrupt.clone();
                chat_with_ui(
                    main,
                    adapter.as_deref(),
                    request,
                    ChatOptions {
                        timeout,
                        render_text: false,
                        context_tokens: None,
                        policy: None,
                        recovery: Some(&self.config.request_recovery),
                    },
                    interrupt,
                    ui,
                    &mut |ui| self.pump_subagents(ui),
                )
            };
            self.record_aux_request(main, &result, started);
            let reply = result?;
            if let Some(cost) = main.reply_cost(&reply) {
                self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost);
            }
            ensure!(
                reply.finish_reason != "length" && reply.tool_calls.is_empty(),
                "{role} response was truncated or contained tool calls; no auxiliary result was accepted"
            );
            return Ok(reply.text);
        };
        let role_config = self.auth_config.for_role(role);
        let mut backend = match self.role_backends.remove(role) {
            Some(backend) => backend,
            None => make_backend(&role_config)?,
        };
        let previous_cache = match backend.swap_cache_namespace(namespace, &role_config) {
            Ok(previous) => previous,
            Err(error) => {
                self.role_backends.insert(role.to_owned(), backend);
                return Err(error);
            }
        };
        backend.set_usage_scope(Some(self.usage_scope.worker()));
        let previous_profile = backend.swap_request_profile(None);
        if (request.max_tokens > 0 || previous_profile.is_some())
            && let Some(mut profile) = previous_profile
                .as_ref()
                .or(backend.compaction_profile())
                .cloned()
        {
            if request.max_tokens > 0 {
                profile.max_tokens = Some(
                    profile
                        .max_tokens
                        .filter(|cap| *cap > 0)
                        .map_or(request.max_tokens, |cap| cap.min(request.max_tokens)),
                );
            }
            backend.next_request_profile(crate::llm::profiles::request_tiers_for(
                Some(profile),
                &self.auth_config.selection(),
                self.auth_config.effective_service_tier(),
                self.auth_config.extended_context,
            )?);
        }
        main.release();
        ui.role(role, &selection.display_name());
        let rewritten = self.provider_request_with(request, ui)?;
        let request = rewritten.as_ref().unwrap_or(request);
        let started = Instant::now();
        let result = if self.hermetic {
            crate::learn::compute::background_call(self.config, &mut backend, |backend, cancel| {
                backend.chat_with_control(None, request, timeout_secs, None, &mut |_| {}, cancel)
            })
        } else {
            let timeout = timeout_secs;
            let interrupt = self.interrupt.clone();
            chat_with_ui(
                &mut backend,
                None,
                request,
                ChatOptions {
                    timeout,
                    render_text: false,
                    context_tokens: None,
                    policy: None,
                    recovery: Some(&self.config.request_recovery),
                },
                interrupt,
                ui,
                &mut |ui| self.pump_subagents(ui),
            )
        };
        self.record_aux_request(&backend, &result, started);
        if let Ok(reply) = &result
            && let Some(cost) = backend.reply_cost(reply)
        {
            self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost);
        }
        backend.release();
        backend.restore_cache_namespace(previous_cache);
        let _ = backend.swap_request_profile(previous_profile);
        self.role_backends.insert(role.to_owned(), backend);
        ui.role("main", &self.config.selection().display_name());
        result.and_then(|reply| {
            ensure!(
                reply.finish_reason != "length" && reply.tool_calls.is_empty(),
                "{role} response was truncated or contained tool calls; no auxiliary result was accepted"
            );
            Ok(reply.text)
        })
    }
}

pub(super) fn semantic_chat(request: &tools::navigation::SemanticRequest, max_tokens: u32) -> Chat {
    Chat {
        messages: vec![
            json!({"role":"system","content":request.system.as_str()}),
            json!({"role":"user","content":request.prompt.as_str()}),
        ],
        tools: vec![],
        temperature: 0.0,
        max_tokens,
        seed: None,
    }
}

pub(super) fn append_find_supplement(
    workspace: &Workspace,
    output: &mut ToolOutput,
    metadata: ToolOutput,
) -> Result<()> {
    output
        .text
        .push_str("\n[Supplementary real language-server metadata; outside model ranking]\n");
    output
        .text
        .push_str(&guard::clip_middle(&metadata.text, 4096));
    if metadata.text.len() > 4096 {
        let store = workspace
            .artifacts()
            .context("retaining supplementary metadata requires artifact storage")?;
        output.text.push_str(&format!(
            "\n[Complete language-server metadata: {}]\n",
            store.retain(&metadata.text)?
        ));
    }
    output.is_error |= metadata.is_error;
    Ok(())
}
