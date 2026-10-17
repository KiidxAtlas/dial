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
                self.role_chat(main, "small", llm::usage::Purpose::Retrieval, &chat, ui)
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
                // `assistance.local.side_models = "reuse_main"`: stay on the loaded main model.
                && !self.local_side_reuses_main(role)
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
        let text = self.role_chat(main, "coach", llm::usage::Purpose::Repair, &chat, ui)?;
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
            Err(error) => crate::telemetry::request_failed(
                self.config,
                self.hermetic,
                self.session.id(),
                backend.model_label(),
                started.elapsed(),
                error,
            ),
        }
    }
    pub(super) fn role_chat(
        &mut self,
        main: &mut Backend,
        role: &str,
        purpose: llm::usage::Purpose,
        request: &Chat,
        ui: &mut dyn Ui,
    ) -> Result<String> {
        let timeout_secs = self.config.worker_timeout_secs;
        main.require_session_identity()?;
        let namespace = match &self.active_side_namespace {
            Some(namespace) => namespace.clone(),
            None => compaction_runtime::side_request_namespace(self.session.id())?,
        };
        let previous_cache = main.swap_cache_namespace(&namespace, &self.auth_config)?;
        let previous_scope =
            main.set_usage_scope(Some(self.usage_scope.worker().for_purpose(purpose)));
        let previous_profile = main.swap_request_profile(None);
        let result = self.role_chat_scoped(
            main,
            role,
            purpose,
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
        purpose: llm::usage::Purpose,
        request: &Chat,
        ui: &mut dyn Ui,
        namespace: &str,
        main_profile: Option<&llm::profiles::ModelProfile>,
        timeout_secs: u64,
    ) -> Result<String> {
        let selection = self.role_selection(role).cloned();
        let Some(selection) = selection else {
            if (request.max_tokens > 0 || main_profile.is_some())
                && let Some(profile) =
                    main.bounded_request_profile(main_profile, request.max_tokens)
            {
                main.next_request_profile(crate::llm::profiles::request_tiers_for(
                    Some(profile),
                    &self.auth_config.selection(),
                    self.auth_config.effective_service_tier(),
                    self.auth_config.extended_context,
                )?);
            }
            self.prepare_local_side_request(main);
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
            return accept_auxiliary_reply(main, role, reply);
        };
        self.observe_local_side_request(role);
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
        backend.set_usage_scope(Some(self.usage_scope.worker().for_purpose(purpose)));
        let previous_profile = backend.swap_request_profile(None);
        if (request.max_tokens > 0 || previous_profile.is_some())
            && let Some(profile) =
                backend.bounded_request_profile(previous_profile.as_ref(), request.max_tokens)
        {
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
        let result = result.and_then(|reply| accept_auxiliary_reply(&backend, role, reply));
        backend.release();
        backend.restore_cache_namespace(previous_cache);
        let _ = backend.swap_request_profile(previous_profile);
        self.role_backends.insert(role.to_owned(), backend);
        ui.role("main", &self.config.selection().display_name());
        result
    }
}

/// Auxiliary replies must be complete, tool-free text. A received reply that is rejected remains
/// physical usage, so it is recorded as a discarded attempt under the purpose that requested it.
fn accept_auxiliary_reply(backend: &Backend, role: &str, reply: llm::Reply) -> Result<String> {
    if reply.finish_reason == "length" || !reply.tool_calls.is_empty() || reply.dropped_tool_call()
    {
        backend.record_discarded_response()?;
        anyhow::bail!(
            "{role} response was truncated or contained tool calls; no auxiliary result was accepted"
        );
    }
    Ok(reply.text)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_summary_role_chatgpt_does_not_inject_output_limits() {
        for assigned in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            llm::chatgpt::test_account(home.path(), false);
            let output = json!({"type":"message","role":"assistant","status":"completed",
                "content":[{"type":"output_text","text":"summary","annotations":[]}]});
            let server = llm::fake::serve(vec![llm::fake::Script::Sse(format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type":"response.output_text.delta","delta":"summary"}),
                json!({"type":"response.completed","response":{"status":"completed","output":[output],"usage":{"input_tokens":10,"output_tokens":2}}})
            ))]);
            let profile = llm::profiles::ModelProfile {
                provider: "chatgpt".into(),
                model: "account-model".into(),
                ..Default::default()
            };
            let mut config = Config {
                provider: Some("chatgpt".into()),
                model: "account-model".into(),
                user_dir: home.path().to_path_buf(),
                model_profile: Some("plan".into()),
                ..Default::default()
            };
            config.model_profiles.insert("plan".into(), profile.clone());
            if assigned {
                config
                    .model_roles
                    .insert("small".into(), config.selection());
            }
            let backend = || {
                let mut backend = Backend::new(
                    ProviderKind::ChatGpt {
                        model: "account-model".into(),
                        user_dir: home.path().to_path_buf(),
                    },
                    Default::default(),
                    5,
                )
                .with_session_id("role-compaction")
                .with_profile(profile.clone());
                backend.chatgpt_url = Some(format!("{}/codex/responses", server.url));
                backend
            };
            let version = Version {
                id: 0,
                parent: None,
                kind: "config".into(),
                harness: config.harness.clone(),
                adapter: None,
                status: "active".into(),
            };
            let mut agent =
                Agent::hermetic(&config, backend(), &version, workspace.path(), None, None)
                    .unwrap();
            if assigned {
                agent.role_backends.insert("small".into(), backend());
            }
            let mut main = agent.backend.take().unwrap();
            let request = Chat {
                messages: vec![json!({"role":"user","content":"Summarize."})],
                tools: vec![],
                temperature: 0.0,
                max_tokens: 64,
                seed: None,
            };
            assert_eq!(
                agent
                    .role_chat(
                        &mut main,
                        "small",
                        llm::usage::Purpose::Compaction,
                        &request,
                        &mut NullUi,
                    )
                    .unwrap(),
                "summary"
            );
            let requests = server.requests();
            assert_eq!(requests.len(), 1);
            let body = llm::fake::body(&requests[0]);
            for field in ["max_tokens", "max_output_tokens", "temperature", "top_p"] {
                assert!(
                    body.get(field).is_none(),
                    "assigned={assigned}, {field}: {body}"
                );
            }
            assert!(main.model_profile().unwrap().max_tokens.is_none());
            assert!(config.active_model_profile().unwrap().max_tokens.is_none());
        }
    }

    #[test]
    fn auxiliary_output_budget_preserves_endpoint_caps_and_selected_profiles() {
        let backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            Default::default(),
            5,
        )
        .with_profile(llm::profiles::ModelProfile {
            provider: "openai".into(),
            model: "test".into(),
            max_tokens: Some(1024),
            ..Default::default()
        });
        let selected = llm::profiles::ModelProfile {
            max_tokens: Some(32),
            ..backend.model_profile().unwrap().clone()
        };
        assert_eq!(
            backend
                .bounded_request_profile(None, 64)
                .unwrap()
                .max_tokens,
            Some(64)
        );
        assert_eq!(
            backend
                .bounded_request_profile(Some(&selected), 64)
                .unwrap()
                .max_tokens,
            Some(32)
        );
        assert_eq!(
            backend.bounded_request_profile(None, 0).unwrap().max_tokens,
            Some(1024)
        );
        assert_eq!(backend.model_profile().unwrap().max_tokens, Some(1024));
    }
}
