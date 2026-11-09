//! `Bridge`: the `Ui` adapter that carries agent events into the shell.

use crate::app::core::App;
use crate::app::shell::Shell;
use crate::app::shell::frame_pacing::Phase;
use crate::runtime::tool_output::ToolOutput;
use crate::ui::composer::ComposerEvent;
use crate::ui::input::{poll, read};
use crate::ui::keys::Action;
use crossterm::event::{Event, KeyEventKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// The agent <-> screen bridge
// ---------------------------------------------------------------------------

pub(super) struct Bridge<'a> {
    pub(super) app: &'a App,
    pub(super) shell: &'a mut Shell,
    pub(super) interrupt: Arc<AtomicBool>,
    pub(super) started: Instant,
    pub(super) last_paint: Instant,
    pub(super) failed: bool,
    /// (id, tool name, started) of tool calls awaiting their result, oldest first.
    pub(super) pending: Vec<(String, String, Instant)>,
    pub(super) next_id: u64,
}

impl Bridge<'_> {
    /// A successful `write` is previewed by the lines it put on disk: the result text is written
    /// for the model and carries no content. Capped so a huge file never reaches the transcript.
    pub(super) fn attach_write_preview(&mut self, id: &str, output: &ToolOutput) {
        const PREVIEW_BYTES: u64 = 64 * 1024;
        if output.is_error {
            return;
        }
        let Some((name, summary)) = self.shell.transcript.tool_call_summary(id) else {
            return;
        };
        let path = summary.split_whitespace().next().unwrap_or_default();
        if name != "write" || path.is_empty() {
            return;
        }
        let full = self.app.layout.project_root.join(path);
        let Ok(file) = std::fs::File::open(&full) else {
            return;
        };
        let mut content = String::new();
        use std::io::Read as _;
        if file
            .take(PREVIEW_BYTES)
            .read_to_string(&mut content)
            .is_ok()
        {
            self.shell.transcript.tool_input(
                id,
                serde_json::json!({ "path": path, "content": content }).to_string(),
            );
        }
    }

    pub(super) fn repaint(&mut self, force: bool) {
        if self.failed {
            return;
        }
        // Input is drained on every streaming callback, not only on paint ticks, so keys stay
        // responsive while output streams past the paint throttle.
        let input = Instant::now();
        self.pump();
        self.shell
            .frame_pacing
            .record(Phase::Input, input.elapsed());
        // A backlogged terminal defers composition entirely instead of composing a frame that would
        // queue behind unsent output. Composition reads current state at paint time, so the next
        // unforced repaint after the terminal drains paints everything; forced repaints still paint.
        if !force
            && (self.shell.term.output_backlogged()
                || self.last_paint.elapsed() < self.shell.frame_pacing.interval())
        {
            return;
        }
        let frame = Instant::now();
        self.last_paint = frame;
        let refresh = Instant::now();
        self.shell.refresh_resources(self.app, false, true);
        self.shell.refresh_workers(self.app);
        self.shell
            .frame_pacing
            .record(Phase::Refresh, refresh.elapsed());
        self.shell.status.elapsed_secs = self.started.elapsed().as_secs_f64();
        if let Some(live) = &self.shell.live_cost {
            (self.shell.status.turn_cost, self.shell.status.session_cost) = live.observe();
        }
        if let Some(monitor) = &self.shell.adapter_monitor {
            self.shell.status.lora = self
                .shell
                .learning
                .cache
                .value
                .2
                .with_runtime(&monitor.lock());
        }
        let paint = Instant::now();
        let paint_result = if self.shell.busy_overlay.is_some() {
            self.shell
                .step_busy_overlay(self.app, None)
                .map(|interrupt| {
                    if interrupt {
                        self.interrupt.store(true, Ordering::SeqCst);
                    }
                })
        } else {
            self.shell.paint(None)
        };
        self.shell
            .frame_pacing
            .record(Phase::Paint, paint.elapsed());
        self.shell.frame_pacing.record_frame(frame.elapsed());
        if paint_result.is_err() {
            self.failed = true;
            self.interrupt.store(true, Ordering::SeqCst);
        }
    }

    /// Immediate feedback for an interrupt: the turn only stops at its next safe point, so name
    /// the tool it is waiting on, and why when the stop was not a plain stop key. Activity stays
    /// on this label until the turn settles (`set_activity` holds it once the flag is up).
    fn mark_stopping(&mut self, reason: Option<&str>) {
        let mut activity = match self.pending.last() {
            Some((_, name, _)) => format!("stopping after {name}"),
            None => "stopping".to_owned(),
        };
        if let Some(reason) = reason {
            activity.push_str(" · ");
            activity.push_str(reason);
        }
        self.shell.status.activity = activity;
        // The next repaint paints even when throttled. Painting from here would re-enter `pump`.
        self.last_paint = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(self.started);
    }

    /// Raise the interrupt and show it at once. Safe outside `pump` only.
    pub(super) fn show_stopping(&mut self, reason: Option<&str>) {
        self.interrupt.store(true, Ordering::SeqCst);
        self.mark_stopping(reason);
        self.repaint(true);
    }

    /// Raise the interrupt from inside `pump`; the enclosing repaint paints the label.
    fn stop_from_pump(&mut self, reason: Option<&str>) {
        self.interrupt.store(true, Ordering::SeqCst);
        self.mark_stopping(reason);
    }

    /// While a submitted command waits for the turn to unwind, keep reading so a repeated stop
    /// key still lands; every other event is held, in order, for the idle loop that runs next.
    fn hold_input_while_stopping(&mut self) {
        for _ in 0..64 {
            if !poll(Duration::ZERO).unwrap_or(false) {
                break;
            }
            let Ok(event) = read() else { break };
            match &event {
                Event::Key(key) if crate::app::shell::prompts::stop_key(&self.shell.keys, key) => {
                    self.stop_from_pump(None);
                }
                Event::Resize(..) => self.shell.term.on_resize(),
                _ => self.shell.held.push_back(event),
            }
        }
    }

    pub(super) fn set_activity(&mut self, activity: &str) {
        if self.interrupt.load(Ordering::SeqCst) {
            return;
        }
        if self.shell.status.activity != activity {
            self.shell.status.activity = activity.to_owned();
        }
    }

    /// Drain pending keys so Ctrl-C/Esc reach the agent and the next draft stays editable.
    pub(super) fn pump(&mut self) {
        if crate::ui::term::termination_requested() {
            self.failed = true;
            self.interrupt.store(true, Ordering::SeqCst);
            return;
        }
        // A submitted command owns the next idle dispatch. Hold subsequent input for the idle
        // loop rather than overwriting it, but keep honouring the stop key during a slow stop.
        if self.shell.startup.is_some() {
            self.hold_input_while_stopping();
            return;
        }
        // Finished speech uses the same after-turn queue as a typed follow-up.
        self.shell.poll_voice(self.app);
        // Background jobs (usage, sign-in, skill installs) report without waiting for the turn.
        self.shell.pump_jobs_during_turn(self.app);
        // Hosted guests reach a running turn only through here: an abort stops it, prompts wait.
        for notice in crate::collab::room::pump() {
            self.shell.note("Collab", notice);
        }
        if crate::collab::room::take_aborts() {
            self.stop_from_pump(Some("stopped by a collab guest"));
        }
        for _ in 0..64 {
            if !poll(Duration::ZERO).unwrap_or(false) {
                break;
            }
            let Ok(event) = read() else { break };
            if let Event::Key(key) = &event
                && self.shell.consume_voice_edge(key)
            {
                continue;
            }
            if self.shell.busy_overlay.is_some() {
                // The first stop key in an open panel only closes the panel; the turn keeps
                // running. A second press (with no panel open) stops the turn.
                let close = matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release
                    && self.shell.keys.matches(Action::Interrupt, key)
                    && !self.shell.keys.matches(Action::SelectCancel, key));
                let result = if close {
                    if let Some(state) = &mut self.shell.busy_overlay {
                        state.next = None;
                    }
                    self.shell.finish_busy_overlay(self.app)
                } else {
                    self.shell.step_busy_overlay(self.app, Some(event))
                };
                match result {
                    Ok(true) => {
                        self.stop_from_pump(Some("then runs the panel's choice"));
                        return;
                    }
                    Ok(false) => {}
                    Err(_) => {
                        self.failed = true;
                        self.interrupt.store(true, Ordering::SeqCst);
                        return;
                    }
                }
                continue;
            }
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let shell = &mut *self.shell;
                    match shell.composer_key(key) {
                        ComposerEvent::Action(Action::CommandPalette) => {
                            if shell.route_busy_command(self.app, "/palette".into()) {
                                self.stop_from_pump(Some("then runs your command"));
                                return;
                            }
                        }
                        ComposerEvent::Action(Action::StatusFocus) => {
                            shell.route_busy_command(self.app, "/controls".into());
                        }
                        ComposerEvent::Interrupt | ComposerEvent::Exit => {
                            self.stop_from_pump(None);
                        }
                        ComposerEvent::Submit(text) => {
                            if shell.route_busy_command(self.app, text) {
                                self.stop_from_pump(Some("then sends your message"));
                                return;
                            }
                            continue;
                        }
                        ComposerEvent::Action(Action::ModelSelect) => {
                            shell.route_busy_command(self.app, "/model".into());
                        }
                        ComposerEvent::Action(Action::ModelRoles) => {
                            shell.route_busy_command(self.app, "/roles".into());
                        }
                        ComposerEvent::Action(Action::ModelFavoriteNext) => {
                            shell.route_busy_command(self.app, "/model next".into());
                        }
                        ComposerEvent::Action(Action::ToolsExpand) => {
                            shell.tools_expanded = !shell.tools_expanded;
                            shell.display_reset();
                        }
                        ComposerEvent::Action(Action::ToolsToggle) => {
                            shell.ui.show_tools = !shell.ui.show_tools
                        }
                        ComposerEvent::Action(Action::ThinkingToggle) => {
                            shell.ui.show_thinking = !shell.ui.show_thinking
                        }
                        ComposerEvent::Action(Action::StatusExpand) => {
                            shell.status.toggle_status_expanded();
                        }
                        ComposerEvent::Action(Action::WorkTab) => {
                            shell.work_tab = shell.work_tab.next();
                        }
                        ComposerEvent::Action(Action::SubagentsHub) => {
                            shell.route_busy_command(self.app, "/agents".into());
                        }
                        ComposerEvent::Action(Action::SubagentsRecent) => {
                            shell.busy_recent_worker(self.app);
                        }
                        ComposerEvent::Action(Action::RateGood | Action::RateBad) => {
                            shell.rating_unavailable_during_turn();
                        }
                        ComposerEvent::Action(Action::Settings) => {
                            shell.route_busy_command(self.app, "/settings".into());
                        }
                        ComposerEvent::Action(Action::ProviderSetup) => {
                            shell.route_busy_command(self.app, "/providers".into());
                        }
                        ComposerEvent::Action(Action::EditExternal) => {
                            shell.deferred_editor = true;
                            shell.note("Editor", "External editor opens after the current turn.");
                        }
                        ComposerEvent::Action(Action::ApprovalCycle) => {
                            shell.deferred_approval_cycle = true;
                            shell.note("Approval", "approval mode changes after the current turn.");
                        }
                        ComposerEvent::Action(Action::DisplayReset) => shell.display_reset(),
                        ComposerEvent::Action(Action::PasteImage) => {
                            if let Err(error) = shell.composer.start_clipboard_image() {
                                shell.note(
                                    "Image",
                                    format!("clipboard image unavailable: {error:#}"),
                                );
                            }
                        }
                        ComposerEvent::Action(Action::VoiceInput) => shell.voice_press(self.app),
                        _ => {}
                    }
                }
                Event::Paste(text) => self.shell.composer_paste(&text),
                Event::Resize(..) => self.shell.term.on_resize(),
                _ => {}
            }
        }
    }
}
