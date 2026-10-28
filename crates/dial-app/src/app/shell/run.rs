//! The idle event loop that turns keystrokes, ticks and agent events into shell work.

use crate::agent::{Agent, Ui};
use crate::app::bridge::Bridge;
use crate::app::core::App;
use crate::app::display::local_time;
use crate::app::drive::Step;
use crate::app::overlay::auth::AuthJob;
use crate::app::shell::plugin_effects::Drained;
use crate::app::shell::{PAINT_EVERY, STATUS_TTL, Shell, TICK};
use crate::ui::composer::ComposerEvent;
use crate::ui::input::{poll, read};
use crate::ui::status::Phase;
use crate::ui::statusline;
use crate::ui::welcome;
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

impl Shell {
    // ----- idle loop ----------------------------------------------------------------------

    pub(in crate::app) fn run(&mut self, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
        self.maintenance_control = Some(agent.background_compaction_control());
        self.background_control = agent.background_control();
        // Restored workers exist before the first foreground turn binds its controls.
        self.live_tasks = Some(agent.live_task_control());
        let mut dirty = true;
        let mut previous_status = None;
        loop {
            // Actions pressed during a turn run at this boundary, once the turn has unwound.
            if std::mem::take(&mut self.deferred_editor) {
                self.edit_externally();
            }
            if std::mem::take(&mut self.deferred_approval_cycle) {
                self.cycle_approval(agent);
            }
            dirty |= self.refresh_resources(app, false, false);
            // Large @file rankings finish on a worker; land them without waiting for a keystroke.
            dirty |= self.composer.poll_autocomplete();
            // Plugin header, status, footer and title, plus autocomplete answers, once per pass.
            dirty |= self.refresh_plugin_chrome(agent);
            if let Some(notice) = self.composer.take_word_notice() {
                self.note("Word completion", notice);
            }
            for (label, failure) in [
                ("Git status", self.git.cache.take_failure()),
                ("Git files", self.git.files.take_failure()),
                ("Learning status", self.learning.cache.take_failure()),
            ] {
                if let Some(failure) = failure {
                    self.note(label, failure);
                    dirty = true;
                }
            }
            for notice in crate::collab::room::pump()
                .into_iter()
                .chain(crate::llm::oauth::take_notice())
            {
                self.note("Notice", notice);
                dirty = true;
            }
            // Plugin actions queued during the turn apply here, each behind its own approval.
            match self.drain_plugin_effects(app, agent) {
                Drained::Nothing => {}
                Drained::Changed => dirty = true,
                Drained::Rebuild(step) => return Ok(step),
            }
            if let Some(config) = self.pending_config.take() {
                if let step @ Step::Rebuild(_) = self.config_changed(app, agent, config) {
                    return Ok(step);
                }
                dirty = true;
            }
            if std::mem::take(&mut self.pending_resource_rebuild) {
                return Ok(Step::Rebuild(Box::new(app.config.clone())));
            }
            if let Some(selection) = self.pending_verification.take() {
                self.verify_lora(app, agent, &selection)?;
                dirty = true;
            }
            if let Some(payload) = self.pending_resume.take() {
                self.resume_worker(app, agent, &payload)?;
                dirty = true;
            }
            for steer in crate::collab::room::take_steering() {
                match steer {
                    crate::collab::host::Steer::Prompt(text) => {
                        let step = self.run_turn(app, agent, &text)?;
                        if !matches!(step, Step::Done) {
                            return Ok(step);
                        }
                        dirty = true;
                    }
                    // Aborts reach a running turn through `Bridge::pump`; while idle there is nothing to stop.
                    crate::collab::host::Steer::Abort => {}
                }
            }
            if let Some(prompt) = self.loop_due(app, agent, &mut dirty) {
                let step = self.run_turn(app, agent, &prompt)?;
                if !matches!(step, Step::Done) {
                    return Ok(step);
                }
                dirty = true;
            }
            if self.poll_clipboard_image() {
                dirty = true;
            }
            crate::app::commands::commit::poll(self);
            self.poll_improve_watch();
            if crate::app::commands::extensions_browser::poll(self, app, agent) {
                dirty = true;
            }
            for note in agent.finished_tan_jobs() {
                self.note("Tan", note);
                dirty = true;
            }
            if self.poll_voice() {
                dirty = true;
            }
            for (label, line) in AuthJob::drain_lines(&mut self.auth_job) {
                self.transcript.notice_merge(label, &line);
                dirty = true;
            }
            // A job cancelled elsewhere (Esc, sign-out, overlay) must not leave its panel pending.
            if self.auth_job.is_none() {
                self.transcript.settle_job_notices();
            }
            if let Some((provider, result)) = AuthJob::collect(&mut self.auth_job, app) {
                if provider == "Model" && result.is_ok() {
                    self.pending_config = Some(app.reload()?);
                }
                let step = match result {
                    Err(error) if provider == "Smithery" => {
                        crate::app::commands::smithery::browser_failed(self, app, agent, &error)?
                    }
                    // An embedding setup finished while memory is not semantic: report it, then
                    // offer the switch with a single key (the offer does nothing when semantic).
                    Ok(message)
                        if provider == "Setup"
                            && message.starts_with(crate::setup::embeddings::SUMMARY_PREFIX) =>
                    {
                        self.transcript.notice_merge(provider, &message);
                        self.transcript.settle_job_notices();
                        self.command(
                            app,
                            agent,
                            &format!(
                                "/setup memory {}",
                                crate::setup::embeddings::OFFER_SEMANTIC_ARG
                            ),
                        )?
                    }
                    Ok(message) if provider == "Smithery" => {
                        self.note(provider, message);
                        crate::app::commands::smithery::resume(self, app, agent)?
                    }
                    result => {
                        self.report_job_result(provider, result);
                        Step::Done
                    }
                };
                self.transcript.settle_job_notices();
                self.catalog.refresh(app);
                dirty = true;
                if !matches!(step, Step::Done) {
                    return Ok(step);
                }
            }
            {
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                agent.pump_subagents(&mut bridge);
                if bridge.failed {
                    anyhow::bail!("terminal draw failed");
                }
            }
            if self.startup.is_none() {
                self.startup = self
                    .deferred
                    .pop_front()
                    .or_else(|| self.composer.take_follow_up());
            }
            let maintenance_busy = self.startup.is_some()
                || !self.deferred.is_empty()
                || self.composer.has_queued_work();
            if agent.foreground_idle_pending(maintenance_busy)?
                && !self.wait_compute_required(app, true, true)?
            {
                agent.note_compaction_input();
                continue;
            }
            {
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                match agent.maintenance_tick(&mut bridge, maintenance_busy) {
                    Ok(Some(_)) => dirty = true,
                    Ok(None) => {}
                    Err(error) => bridge
                        .background_notice(&format!("Idle maintenance did not commit: {error:#}")),
                }
                if let Some(notice) = agent.memory_reachability_notice() {
                    bridge.background_notice(&notice);
                }
                if bridge.failed {
                    anyhow::bail!("terminal draw failed");
                }
            }
            let handoff = self.yield_compute
                || ((crate::learn::compute::background_pending(&app.layout.user_dir)?
                    || crate::learn::compute::foreground_pending(&app.layout.user_dir)?)
                    && app.compute.borrow().is_some());
            // Keep resident models loaded between turns so the next turn starts without reloading.
            // Unload them only when queued training, verification or another session asks for
            // compute; running child agents keep their models and compute lease.
            if self.startup.is_none() && handoff && app.release_idle_compute(agent) {
                self.yield_compute = false;
                self.note("Compute", "Local models unloaded for queued work. This chat stays open; the next turn reloads the model.");
                dirty = true;
            }
            self.refresh_status(app, agent);
            self.status.phase = Phase::Idle;
            self.status.model_progress = None;
            self.status.model_silence_secs = 0;
            self.status.clock = local_time("%H:%M");
            let status = statusline::preview_row(
                &self.ui,
                &self.status,
                self.themes.current(),
                self.term.size().0,
            );
            if self.transcript.has_pending_reveal() {
                self.transcript.advance_reveal(&self.ui);
            }
            let frame_pending = self.term.needs_repaint()
                || self.transcript.has_settled_backlog()
                || self.transcript.has_pending_reveal()
                || self.side.busy();
            if self.busy_overlay.is_some()
                || dirty
                || frame_pending
                || previous_status.as_ref() != Some(&status)
            {
                if self.busy_overlay.is_some() {
                    self.step_busy_overlay(app, None)?;
                } else {
                    self.paint(None)?;
                }
            }
            previous_status = Some(status);
            if let Some(text) = self.startup.take() {
                dirty = true;
                match self.submit(app, agent, text)? {
                    Step::Done => continue,
                    other => return Ok(other),
                }
            }
            let (left, right) = statusline::segments(&self.ui);
            let status_pending = self.git.cache.worker.is_some()
                || self.learning.cache.worker.is_some()
                || self.auth_job.is_some()
                || self.commit_job.is_some()
                || self.catalog.worker.is_some()
                || self.pending_favorite.is_some()
                || self.busy_overlay.is_some()
                || self.composer.autocomplete_pending()
                || frame_pending;
            let timeout = if status_pending {
                TICK
            } else if left.iter().chain(&right).any(|s| s == "time") {
                Duration::from_secs(1)
            } else {
                STATUS_TTL
            };
            let timeout = agent
                .compaction_wakeup()
                .map_or(timeout, |maintenance| timeout.min(maintenance));
            let timeout = self.loop_mode.next_due().map_or(timeout, |due| {
                timeout.min(due.saturating_duration_since(Instant::now()))
            });
            let timeout = if self.loop_mode.is_awaiting_condition() {
                timeout.min(crate::agent::modes::loop_runner::CONDITION_POLL)
            } else {
                timeout
            };
            let timeout = if self.composer.clipboard_image_pending() {
                timeout.min(TICK)
            } else {
                timeout
            };
            let timeout = if self.voice_input.is_some() {
                timeout.min(Duration::from_millis(100))
            } else {
                timeout
            };
            let animating = self.home.as_ref().is_some_and(welcome::Home::animating);
            let timeout = if animating {
                timeout.min(crate::ui::fx::FRAME)
            } else {
                timeout
            };
            if self.held.is_empty() && !poll(timeout)? {
                dirty = animating;
                if self.pending_favorite.is_some() {
                    self.catalog.collect();
                    let effective = &app.config;
                    let pending = self.pending_favorite.as_ref().unwrap();
                    if !pending.current(effective, self.scope, self.catalog.request) {
                        self.pending_favorite = None;
                        self.note("Favorites", "Pending switch cancelled because the model, favorites, scope or discovery request changed.");
                        dirty = true;
                    } else if self.catalog.get().is_some() {
                        self.pending_favorite = None;
                        match self.command(app, agent, "/model next")? {
                            Step::Done => dirty = true,
                            other => return Ok(other),
                        }
                    }
                }
                continue;
            }
            dirty = true;
            let event = match self.held.pop_front() {
                Some(event) => event,
                None => read()?,
            };
            if let Event::Key(key) = &event
                && self.consume_voice_edge(key)
            {
                continue;
            }
            if self.busy_overlay.is_some() {
                self.step_busy_overlay(app, Some(event))?;
                continue;
            }
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    // The user is back: the tab stops showing how the last turn ended.
                    self.term_activity = crate::ui::term_state::Activity::Idle;
                    if self.pending_favorite.is_some()
                        && (key.code == KeyCode::Esc
                            || (key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)))
                    {
                        agent.note_compaction_input();
                        self.pending_favorite = None;
                        self.note(
                            "Favorites",
                            "Pending switch cancelled; discovery may still populate the picker.",
                        );
                        continue;
                    }
                    #[cfg(unix)]
                    if key.code == KeyCode::Char('z')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if let Err(error) = self.term.suspend_process() {
                            self.note("Suspend", format!("cannot suspend the terminal: {error:#}"));
                        }
                        continue;
                    }
                    if let Some(picked) = self.home_key(&key) {
                        if let Some(command) = picked {
                            match self.submit(app, agent, command)? {
                                Step::Done => {}
                                other => return Ok(other),
                            }
                        }
                        continue;
                    }
                    let event = self.composer_key(key);
                    let step = match event {
                        ComposerEvent::None => Step::Done,
                        ComposerEvent::Interrupt => {
                            self.pause_loop();
                            self.stop_speech();
                            Step::Done
                        }
                        ComposerEvent::Exit => Step::Quit,
                        ComposerEvent::Submit(text) => self.submit(app, agent, text)?,
                        ComposerEvent::Action(action) => {
                            let result = self.on_action(action, app, agent);
                            agent.note_compaction_input();
                            result?
                        }
                    };
                    if !matches!(step, Step::Done) {
                        return Ok(step);
                    }
                }
                Event::Paste(text) => self.composer_paste(&text),
                Event::Resize(..) => self.term.on_resize(),
                _ => {}
            }
        }
    }
}
