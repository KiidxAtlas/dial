//! Blocking waits: compute and local-model preparation, and the wait-state input loop.

use crate::agent::{Agent, Ui};
use crate::app::bridge::Bridge;
use crate::app::core::App;
use crate::app::discovery::ProbeLease;
use crate::app::learning::Learning;
use crate::app::overlay::auth::AuthJob;
use crate::app::overlay::run::WaitFlow;
use crate::app::overlay::start_streaming_job;
use crate::app::shell::{PAINT_EVERY, Shell, TICK};
use crate::learn::auto;
use crate::llm::model_config::ModelSelection;
use crate::storage::store::Store;
use crate::ui::composer::ComposerEvent;
use crate::ui::input::{poll, read};
use crate::ui::keys::Action;
use crate::ui::status::Phase;
use anyhow::{Result, ensure};
use crossterm::event::{Event, KeyEventKind};
use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

impl Shell {
    pub(in crate::app) fn wait_compute(&mut self, app: &App) -> Result<bool> {
        self.wait_compute_required(app, app.config.uses_local_compute(), false)
    }

    pub(super) fn wait_compute_required(
        &mut self,
        app: &App,
        required: bool,
        maintenance: bool,
    ) -> Result<bool> {
        if !app.config.compute_locks || !required || app.compute.borrow().is_some() {
            return Ok(true);
        }
        let _request = crate::learn::compute::Request::foreground(&app.layout.user_dir)?;
        let lock = auto::lock_file(&crate::config::paths::compute_lock_file(
            &app.layout.user_dir,
        ))?;
        let waiting_since = Instant::now();
        let elapsed_before_wait = self.status.elapsed_secs;
        let idle_input = self
            .maintenance_control
            .as_ref()
            .and_then(|control| control.input_time());
        loop {
            if maintenance
                && (self.startup.is_some()
                    || !self.deferred.is_empty()
                    || self.composer.has_queued_work()
                    || self
                        .maintenance_control
                        .as_ref()
                        .and_then(|control| control.input_time())
                        != idle_input)
            {
                return Ok(false);
            }
            match std::fs::File::try_lock_shared(&lock).map_err(std::io::Error::from) {
                Ok(()) => {
                    crate::learn::compute::register(
                        &app.layout.user_dir,
                        "session",
                        &app.layout.project_root,
                        &app.config.state_dir,
                        &app.config.model,
                    )?;
                    *app.compute.borrow_mut() = Some(lock);
                    return Ok(true);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            self.status.phase = Phase::WaitingCompute;
            self.status.elapsed_secs = elapsed_before_wait + waiting_since.elapsed().as_secs_f64();
            self.status.activity = "waiting for compute · Ctrl+, or /compute opens controls".into();
            self.poll_clipboard_image();
            self.paint_waiting(app)?;
            if poll(TICK)? {
                let event = read()?;
                match self.wait_event(app, event)? {
                    WaitFlow::Continue => {}
                    WaitFlow::Cancel => {
                        self.note(
                            "Cancelled",
                            "Stopped waiting for local compute: an improvement or training job holds it exclusively.",
                        );
                        return Ok(false);
                    }
                    WaitFlow::Stop => return Ok(false),
                }
            }
        }
    }

    /// Keep painting and accepting input while local preparation owns a subprocess. Typed text
    /// queues in the composer (the busy composer), and the configured interrupt cancels.
    fn wait_local_preparation(
        &mut self,
        app: &App,
        work: impl FnOnce(Arc<AtomicBool>, &mut dyn FnMut(&str)) -> Result<String> + Send + 'static,
    ) -> Result<bool> {
        let mut slot = None;
        start_streaming_job(&mut slot, "Local models", work);
        // Restore the caller's phase: a command that prepares then runs keeps its own phase.
        let prior = std::mem::replace(&mut self.status.phase, Phase::Setup);
        self.composer.set_busy(true);
        let outcome = self.preparation_events(app, &mut slot);
        self.composer.set_busy(false);
        self.status.phase = prior;
        outcome
    }

    fn preparation_events(&mut self, app: &App, slot: &mut Option<AuthJob>) -> Result<bool> {
        loop {
            for (label, line) in AuthJob::drain_lines(slot) {
                self.transcript.notice_merge(label, &line);
            }
            if let Some(job) = slot.as_ref() {
                match job.receiver.try_recv() {
                    Ok(result) => {
                        self.transcript.settle_job_notices();
                        result?;
                        return Ok(true);
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        anyhow::bail!("local preparation worker stopped unexpectedly")
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
            }
            self.status.activity = "preparing local model · Ctrl+C stops".into();
            self.poll_clipboard_image();
            self.paint_waiting(app)?;
            if poll(TICK)? {
                let event = read()?;
                let flow = self.wait_event(app, event)?;
                match flow {
                    WaitFlow::Continue => {}
                    WaitFlow::Cancel => {
                        *slot = None;
                        self.note("Local models", "Preparation cancelled; settings unchanged. Cached downloads can be reused.");
                        return Ok(false);
                    }
                    // A message sent now is queued in `startup` and runs once preparation stops.
                    WaitFlow::Stop => {
                        *slot = None;
                        return Ok(false);
                    }
                }
            }
        }
    }

    /// One terminal event during a blocking wait. An open panel gets it first, as in a turn:
    /// Escape or the first stop key closes the panel, and only a stop key with no panel open
    /// cancels the wait. Otherwise keys, paste and resize use the composer and command routing
    /// that turns use, so typed and queued work is kept rather than discarded.
    fn wait_event(&mut self, app: &App, event: Event) -> Result<WaitFlow> {
        if self.busy_overlay.is_some() {
            let close = matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release
                && self.keys.matches(Action::Interrupt, key)
                && !self.keys.matches(Action::SelectCancel, key));
            if close {
                if let Some(state) = &mut self.busy_overlay {
                    state.next = None;
                }
                self.finish_busy_overlay(app)?;
                return Ok(WaitFlow::Continue);
            }
            return Ok(if self.step_busy_overlay(app, Some(event))? {
                WaitFlow::Stop
            } else {
                WaitFlow::Continue
            });
        }
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.wait_key(app, key),
            Event::Paste(text) => {
                self.composer_paste(&text);
                Ok(WaitFlow::Continue)
            }
            Event::Resize(..) => {
                self.term.on_resize();
                Ok(WaitFlow::Continue)
            }
            _ => Ok(WaitFlow::Continue),
        }
    }

    fn wait_key(&mut self, app: &App, key: crossterm::event::KeyEvent) -> Result<WaitFlow> {
        // Compute admission can also run for idle maintenance, when the composer is not busy.
        // Escape dismisses local menus/search/choices; only the configured interrupt stops a wait.
        if self.keys.matches(Action::Interrupt, &key)
            && !self.composer.dismisses_interrupt(&key, &self.keys)
        {
            return Ok(WaitFlow::Cancel);
        }
        Ok(match self.composer_key(key) {
            ComposerEvent::Interrupt | ComposerEvent::Exit => WaitFlow::Cancel,
            ComposerEvent::Submit(text) => {
                if self.route_busy_command(app, text) {
                    WaitFlow::Stop
                } else {
                    WaitFlow::Continue
                }
            }
            ComposerEvent::Action(Action::CommandPalette) => self.wait_command(app, "/palette"),
            ComposerEvent::Action(Action::StatusFocus) => self.wait_command(app, "/controls"),
            ComposerEvent::Action(Action::ModelSelect) => self.wait_command(app, "/model"),
            ComposerEvent::Action(Action::ModelRoles) => self.wait_command(app, "/roles"),
            ComposerEvent::Action(Action::ProviderSetup) => self.wait_command(app, "/providers"),
            ComposerEvent::Action(Action::ModelFavoriteNext) => {
                self.wait_command(app, "/model next")
            }
            ComposerEvent::Action(Action::Settings) => self.wait_command(app, "/settings"),
            ComposerEvent::Action(Action::SubagentsHub) => self.wait_command(app, "/agents"),
            ComposerEvent::Action(Action::SubagentsRecent) => {
                self.busy_recent_worker(app);
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::RateGood | Action::RateBad) => {
                self.rating_unavailable_during_turn();
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::ToolsExpand) => {
                self.tools_expanded = !self.tools_expanded;
                self.display_reset();
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::ToolsToggle) => {
                self.ui.show_tools = !self.ui.show_tools;
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::ThinkingToggle) => {
                self.ui.show_thinking = !self.ui.show_thinking;
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::StatusExpand) => {
                self.status.toggle_status_expanded();
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::WorkTab) => {
                self.work_tab = self.work_tab.next();
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::DisplayReset) => {
                self.display_reset();
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::ApprovalCycle) => {
                self.deferred_approval_cycle = true;
                self.note("Approval", "approval mode changes after the current turn.");
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::VoiceInput) => {
                self.voice_press(app);
                WaitFlow::Continue
            }
            // One rule while Dial is busy: the external editor opens once the work finishes.
            ComposerEvent::Action(Action::EditExternal) => {
                self.deferred_editor = true;
                self.note("Editor", "External editor opens after the current turn.");
                WaitFlow::Continue
            }
            ComposerEvent::Action(Action::PasteImage) => {
                if let Err(error) = self.composer.start_clipboard_image() {
                    self.note("Image", format!("clipboard image unavailable: {error:#}"));
                }
                WaitFlow::Continue
            }
            _ => WaitFlow::Continue,
        })
    }

    fn wait_command(&mut self, app: &App, command: &str) -> WaitFlow {
        if self.route_busy_command(app, command.to_owned()) {
            WaitFlow::Stop
        } else {
            WaitFlow::Continue
        }
    }

    /// Repaint during a blocking wait: an open panel draws itself, otherwise the transcript.
    fn paint_waiting(&mut self, app: &App) -> Result<()> {
        if self.busy_overlay.is_some() {
            self.step_busy_overlay(app, None)?;
            Ok(())
        } else {
            self.paint(None)
        }
    }

    /// GGUF models run on Dial's supervised `llama-server`. When none is found, offer the same
    /// one-time install the MLX runtime gets, into Dial's own folder.
    fn prepare_llama_cpp(&mut self, app: &App) -> Result<bool> {
        if crate::llm::local_format::llama_server().is_some() {
            return Ok(true);
        }
        let choice = self.ask(
            "Set up llama.cpp",
            "GGUF models run on llama.cpp's llama-server, which is not installed. Dial installs a pinned, checksum-verified build into its own folder (no admin rights needed). Downloads need network access.",
            &[("Install llama.cpp", 'y'), ("Cancel", 'n')],
        );
        if choice != Some(0) {
            return Ok(false);
        }
        self.note("Local models", "installing llama.cpp…");
        let layout = app.layout.clone();
        self.wait_local_preparation(app, move |cancel, progress| {
            crate::setup::deps::run::install_and_save(&layout, "llama-cpp", &cancel, progress)?;
            Ok(String::new())
        })
    }

    pub(super) fn prepare_local(&mut self, app: &App) -> Result<bool> {
        if !app.config.runs_dial_mlx() {
            return Ok(true);
        }
        if crate::llm::local_format::serves_with_llama_cpp(&app.config.model) {
            return self.prepare_llama_cpp(app);
        }
        if !app.config.python.is_file() || !app.config.worker.is_file() {
            let choice = self.ask("Set up local models",
                "Installs a private Python environment with uv under your Dial home directory. Downloads need network access.",
                &[("Install runtime", 'y'), ("Cancel", 'n')]);
            if choice != Some(0) {
                return Ok(false);
            }
            self.note("Local models", "installing the local runtime…");
            let layout = app.layout.clone();
            if !self.wait_local_preparation(app, move |cancel, progress| {
                let progress = RefCell::new(progress);
                crate::setup::ensure_runtime_with(&layout, &cancel, &|line| {
                    progress.borrow_mut()(line)
                })?;
                Ok(String::new())
            })? {
                return Ok(false);
            }
            ensure!(
                app.config.python.is_file() && app.config.worker.is_file(),
                "custom Python/worker paths are missing; update them or use the default local setup paths"
            );
        }
        if !Path::new(&app.config.model).exists() {
            let needs_pin = app
                .store
                .as_ref()
                .map(|s| s.get_meta("serving_capabilities"))
                .transpose()?
                .flatten()
                .is_none();
            if needs_pin {
                self.note("Local models", "resolving and checking the local model…");
                let config = app.config.clone();
                let capabilities = Arc::new(Mutex::new(None));
                let prepared = capabilities.clone();
                if !self.wait_local_preparation(app, move |cancel, progress| {
                    let progress = RefCell::new(progress);
                    crate::llm::download::ensure_local_model_with(
                        &config.python,
                        &config.model,
                        &crate::llm::download::Choice::Auto,
                        &cancel,
                        &|line| progress.borrow_mut()(line),
                    )?;
                    let caps = crate::learn::worker::capabilities_controlled(&config, &cancel)?;
                    ensure!(
                        caps["inference"] == true,
                        "local model is not supported by this runtime: {}",
                        caps["reason"]
                    );
                    *prepared.lock().unwrap_or_else(|e| e.into_inner()) = Some(caps);
                    Ok(String::new())
                })? {
                    return Ok(false);
                }
                if let Some(store) = &app.store
                    && let Some(caps) = capabilities
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                {
                    store.set_meta("serving_capabilities", &caps.to_string())?;
                }
            }
        }
        Ok(true)
    }

    // ----- overlays -------------------------------------------------------------------------

    pub(in crate::app) fn verify_lora(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        selection: &ModelSelection,
    ) -> Result<()> {
        if selection.provider != "mlx"
            || selection.base_url.is_some()
            || !Path::new(&selection.model).is_absolute()
            || !Path::new(&selection.model).is_dir()
        {
            self.note(
                "LoRA verification",
                "Select a downloaded local MLX checkpoint: LoRA verification needs MLX weights. Remote endpoints and GGUF files cannot be probed here.",
            );
            return Ok(());
        }
        if !app.release_idle_compute(agent) {
            self.note(
                "LoRA verification",
                "Wait for running child agents to finish, then retry F6.",
            );
            return Ok(());
        }
        let _lease = if app.config.compute_locks {
            let lock = auto::lock_file(&crate::config::paths::compute_lock_file(
                &app.layout.user_dir,
            ))?;
            if let Err(error) = std::fs::File::try_lock_shared(&lock).map_err(std::io::Error::from)
            {
                self.note("LoRA verification", format!("An improvement or training job holds local compute; retry F6 when it finishes. {error}"));
                return Ok(());
            }
            Some(ProbeLease(lock))
        } else {
            None
        };
        let mut config = app.config.with_selection(selection);
        config.state_dir = config.learning_dir(&app.layout);
        app.interrupt.store(false, Ordering::SeqCst);
        self.note("LoRA verification", format!("Loading {} to verify adapter gradients and save/reload with a tiny temporary update. {} stops.", selection.display_name(), self.keys.label(Action::Interrupt)));
        let started = Instant::now();
        let result = std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let cancel = app.interrupt.clone();
            let config = &config;
            scope.spawn(move || {
                let _ = tx.send(crate::learn::worker::capabilities_controlled(
                    config, &cancel,
                ));
            });
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started,
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            bridge.set_activity("verifying LoRA support");
            loop {
                bridge.heartbeat();
                match rx.recv_timeout(Duration::from_millis(80)) {
                    Ok(result) => break result,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => {
                        break Err(anyhow::anyhow!("capability worker ended without a result"));
                    }
                }
            }
        });
        let cancelled = app.interrupt.swap(false, Ordering::SeqCst);
        match result {
            Ok(caps) if !cancelled && caps["inference"] != true => {
                self.note("LoRA verification", format!("Could not load the model to verify adapter targets: {}. The previous result was preserved.", caps["reason"].as_str().unwrap_or("model loading failed")));
            }
            Ok(caps) if !cancelled => {
                let store = Store::open(
                    &config.state_dir,
                    &config.model,
                    &config.use_case,
                    &config.harness,
                )?;
                store.set_meta("serving_capabilities", &caps.to_string())?;
                self.note(
                    "LoRA verification",
                    format!(
                        "{}: {}\n{}",
                        selection.display_name(),
                        if caps["lora"] == true {
                            "supported by Dial's trainer"
                        } else {
                            "unsupported by Dial's trainer"
                        },
                        caps["reason"].as_str().unwrap_or("No reason returned")
                    ),
                );
                self.learning = Learning::default();
                self.catalog.refresh(app);
            }
            _ if cancelled => self.note(
                "LoRA verification",
                "Cancelled; the previous result was preserved.",
            ),
            Err(error) => self.note(
                "LoRA verification",
                format!("Could not verify: {error:#}. The previous result was preserved."),
            ),
            _ => {}
        }
        drop(_lease);
        self.status.activity.clear();
        self.refresh_status(app, agent);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
