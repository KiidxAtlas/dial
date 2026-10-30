//! Bounded startup preparation through the existing idle owner, never a new user turn.
use crate::agent::*;
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(in crate::agent) struct State {
    identity: String,
    attempted: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    LocalPrefix,
    Lsp(String),
    Cargo,
}
impl Action {
    fn key(&self) -> String {
        match self {
            Self::LocalPrefix => "local-prefix".into(),
            Self::Lsp(path) => format!("lsp:{path}"),
            Self::Cargo => "cargo-check".into(),
        }
    }
    fn call(&self, timeout: u64) -> Option<ToolCall> {
        let (name, arguments) = match self {
            Self::LocalPrefix => return None,
            Self::Lsp(path) => ("lsp", json!({"action":"document_symbols","path":path})),
            Self::Cargo => (
                "bash",
                json!({"command":"cargo check --locked --offline --workspace","timeout":timeout}),
            ),
        };
        Some(ToolCall {
            id: format!("startup-preparation:{}", self.key()),
            name: name.into(),
            arguments,
            malformed_arguments: false,
        })
    }
}

impl Agent<'_> {
    fn preparation_identity(&self) -> String {
        let config = self.effective_config();
        format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    self.session.id(),
                    self.workspace.root(),
                    config.selection(),
                    &config.preparation,
                ))
                .unwrap_or_default()
            )
        )
    }

    fn preparation_action(&self, busy: bool) -> Option<Action> {
        if busy
            || self.hermetic
            || self.plan_mode
            || !self.session_lifecycle
            || self.interrupt.load(Ordering::SeqCst)
            || self.session.message_count() != 0
            || self.maintenance_has_live_work()
            || self.compaction_runtime.has_live_worker()
        {
            return None;
        }
        let config = &self.effective_config().preparation;
        if config.validate().is_err() {
            return None;
        }
        let same = self.preparation.identity == self.preparation_identity();
        let pending =
            |action: &Action| !same || !self.preparation.attempted.contains(&action.key());
        if config.local_prefix && pending(&Action::LocalPrefix) {
            return Some(Action::LocalPrefix);
        }
        for path in &config.lsp_paths {
            let action = Action::Lsp(path.clone());
            if pending(&action) {
                return Some(action);
            }
        }
        (config.cargo_check && pending(&Action::Cargo)).then_some(Action::Cargo)
    }

    pub(in crate::agent) fn preparation_local_pending(&self, busy: bool) -> bool {
        matches!(self.preparation_action(busy), Some(Action::LocalPrefix))
            && self.backend.as_ref().is_some_and(Backend::local_inference)
    }

    /// Run one action per idle boundary. The app/RPC owner already holds the local compute lease.
    /// Existing maintenance input control cancels the action as soon as foreground work arrives.
    pub(in crate::agent) fn prepare_session_idle(&mut self, ui: &mut dyn Ui) -> Result<bool> {
        let Some(action) = self.preparation_action(!ui.maintenance_ready()) else {
            return Ok(false);
        };
        let identity = self.preparation_identity();
        if self.preparation.identity != identity {
            self.preparation = State {
                identity,
                attempted: HashSet::new(),
            };
        }
        // Denials/failures are consumed too: idle ticks must not repeatedly ask or retry a failure.
        self.preparation.attempted.insert(action.key());
        let timeout = self.effective_config().preparation.timeout_secs;
        let guard = self.compaction_runtime.foreground_idle(timeout)?;
        let normal_interrupt = std::mem::replace(&mut self.interrupt, guard.cancel.clone());
        let body = self.session.runtime().start(
            "maintenance",
            &format!("Startup preparation: {}", action.key()),
            self.session.branch_leaf().map(str::to_owned),
        );
        let result = match &action {
            Action::LocalPrefix => self.prepare_local_prefix(ui, &normal_interrupt),
            _ => self.prepare_startup_tool(&action, timeout, ui),
        };
        let cancelled =
            guard.cancel.load(Ordering::SeqCst) || normal_interrupt.load(Ordering::SeqCst);
        self.interrupt = normal_interrupt;
        let (phase, detail) = match &result {
            Ok(detail) if !cancelled => (crate::runtime::Phase::Completed, detail.clone()),
            Ok(_) => (
                crate::runtime::Phase::Interrupted,
                "Preparation yielded to foreground input".into(),
            ),
            Err(error) if cancelled => (
                crate::runtime::Phase::Interrupted,
                format!("Preparation interrupted: {error:#}"),
            ),
            Err(error) => (
                crate::runtime::Phase::Failed,
                format!("Preparation not completed: {error:#}"),
            ),
        };
        body.settle(
            detail.clone(),
            Some(json!({"action":action.key(),"startup_only":true,"verification_credit":false})),
            phase,
        );
        ui.background_notice(&detail);
        drop(guard);
        // Preparation failure is observable, but never fails the next user turn or retries itself.
        Ok(true)
    }

    fn prepare_startup_tool(
        &mut self,
        action: &Action,
        timeout: u64,
        ui: &mut dyn Ui,
    ) -> Result<String> {
        match action {
            Action::Lsp(path) => {
                let resolved = self.workspace.resolve(path).map_err(anyhow::Error::msg)?;
                ensure!(
                    resolved.starts_with(self.workspace.root()) && resolved.is_file(),
                    "LSP preparation requires an existing workspace source file"
                );
                ensure!(
                    self.ext.get("lsp").is_some(),
                    "LSP is not available in this session"
                );
            }
            Action::Cargo => {
                ensure!(
                    self.workspace.root().join("Cargo.toml").is_file(),
                    "No workspace Cargo.toml; Rust preparation skipped"
                );
            }
            Action::LocalPrefix => bail!("local prefix warming is not a tool action"),
        }
        let call = action
            .call(timeout)
            .context("missing startup tool action")?;
        let schemas = self.schemas(std::slice::from_ref(&call.name))?;
        let names = vec![call.name.clone()];
        if let Some(error) = call_form_error(&call, &schemas, &names) {
            bail!("Startup preparation arguments were refused: {error:?}");
        }
        let mut backend = self.backend.take().context("session backend is busy")?;
        let output = self.dispatch(&mut backend, &call, ui);
        self.backend = Some(backend);
        ensure!(!output.is_error, "{}", output.text);
        ensure!(
            output.execution.effects != crate::runtime::execution::Effects::Unknown,
            "Preparation stopped with unknown effects; inspect the tool result before continuing"
        );
        if matches!(action, Action::Cargo) {
            if let Some(process) = &output.execution.process {
                ensure!(
                    process.exit_code == Some(0) && !process.cancelled && !process.timed_out,
                    "Startup cargo check did not finish successfully"
                );
            } else {
                return Ok("Startup cargo check launched in session background; inspect bg for its outcome; no verification credit claimed".into());
            }
        }
        Ok(format!(
            "Startup {} completed; no final-check evidence claimed",
            action.key()
        ))
    }

    fn prepare_local_prefix(
        &mut self,
        ui: &mut dyn Ui,
        normal_interrupt: &Arc<AtomicBool>,
    ) -> Result<String> {
        let mut backend = self.backend.take().context("session backend is busy")?;
        if !backend.local_inference() {
            self.backend = Some(backend);
            bail!("Local prefix warming skips hosted backends; no remote request was made");
        }
        if !backend.local_prefix_available() {
            self.backend = Some(backend);
            bail!("Local model is not installed; preparation did not download it");
        }
        let request = self.startup_prefix_request();
        let request = match request {
            Ok(request) => match self.provider_request_with(&request, ui) {
                Ok(Some(rewritten))
                    if rewritten.max_tokens == 1 && rewritten.tools == request.tools =>
                {
                    rewritten
                }
                Ok(Some(_)) => {
                    self.backend = Some(backend);
                    bail!("Startup prefix hook exceeded the one-token/tool-schema bound");
                }
                Ok(None) => request,
                Err(error) => {
                    self.backend = Some(backend);
                    return Err(error);
                }
            },
            Err(error) => {
                self.backend = Some(backend);
                return Err(error);
            }
        };
        let previous_scope = backend.set_usage_scope(Some(
            self.usage_scope.for_purpose(llm::usage::Purpose::CacheWarm),
        ));
        let cancel = self.interrupt.clone();
        let timeout = self.effective_config().preparation.timeout_secs;
        let started = Instant::now();
        let result = chat_with_ui(
            &mut backend,
            None,
            &request,
            ChatOptions {
                timeout,
                render_text: false,
                context_tokens: None,
                policy: None,
                recovery: None,
            },
            cancel.clone(),
            ui,
            &mut |ui| {
                if normal_interrupt.load(Ordering::SeqCst) || !ui.maintenance_ready() {
                    cancel.store(true, Ordering::SeqCst);
                }
            },
        );
        self.record_aux_request(&backend, &result, started);
        backend.set_usage_scope(previous_scope);
        self.backend = Some(backend);
        result?;
        Ok(
            "Local prefix warming request completed; output discarded, cache reuse not guaranteed"
                .into(),
        )
    }

    fn startup_prefix_request(&self) -> Result<Chat> {
        let mut messages = self.system_messages.clone();
        messages.extend(self.agents_messages.clone());
        messages
            .push(json!({"role":"user","content":"Warm this session prefix. Do not use tools."}));
        Ok(Chat {
            messages,
            tools: self.schemas(&self.offered())?,
            temperature: 0.0,
            max_tokens: 1,
            seed: None,
        })
    }
}

#[cfg(test)]
mod tests;
