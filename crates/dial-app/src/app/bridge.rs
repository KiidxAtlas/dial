//! `Bridge`: the `Ui` adapter that carries agent events into the shell.

use crate::app::core::App;
use crate::app::shell::{PAINT_EVERY, Shell};
use crate::runtime::tool_output::ToolOutput;
use crate::ui::composer::ComposerEvent;
use crate::ui::keys::Action;
use crossterm::event::{Event, KeyEventKind, poll, read};
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
        if self.failed || (!force && self.last_paint.elapsed() < PAINT_EVERY) {
            return;
        }
        self.last_paint = Instant::now();
        self.pump();
        self.shell.refresh_resources(self.app, false, true);
        self.shell.refresh_workers(self.app);
        self.shell.status.elapsed_secs = self.started.elapsed().as_secs_f64();
        if let Some(monitor) = &self.shell.adapter_monitor {
            self.shell.status.lora = self
                .shell
                .learning
                .cache
                .value
                .2
                .with_runtime(&monitor.lock());
        }
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
        if paint_result.is_err() {
            self.failed = true;
            self.interrupt.store(true, Ordering::SeqCst);
        }
    }

    /// Immediate feedback for an interrupt: the turn only stops at its next safe point, so name
    /// the tool it is waiting on. Activity stays on this label until the turn settles.
    fn show_stopping(&mut self) {
        let activity = match self.pending.last() {
            Some((_, name, _)) => format!("stopping after {name}"),
            None => "stopping".to_owned(),
        };
        // Set directly: `set_activity` holds the label once the interrupt flag is up.
        self.shell.status.activity = activity;
        self.repaint(true);
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
        // A submitted command owns the next idle dispatch. Leave subsequent input in the
        // terminal until the active operation has unwound, rather than overwriting it.
        if self.shell.startup.is_some() {
            return;
        }
        // Lands a finished push-to-talk transcript in the draft while the turn keeps running.
        self.shell.poll_voice();
        // Background jobs (usage, sign-in, skill installs) report without waiting for the turn.
        self.shell.pump_jobs_during_turn(self.app);
        // Hosted guests reach a running turn only through here: an abort stops it, prompts wait.
        for notice in crate::collab::room::pump() {
            self.shell.note("Collab", notice);
        }
        if crate::collab::room::take_aborts() {
            self.interrupt.store(true, Ordering::SeqCst);
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
                let cancel = matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release
                    && self.shell.keys.matches(Action::Interrupt, key)
                    && !self.shell.keys.matches(Action::SelectCancel, key));
                let result = if cancel {
                    if let Some(state) = &mut self.shell.busy_overlay {
                        state.next = None;
                    }
                    self.shell.finish_busy_overlay(self.app)
                } else {
                    self.shell.step_busy_overlay(self.app, Some(event))
                };
                match result {
                    Ok(stop) if stop || cancel => {
                        self.interrupt.store(true, Ordering::SeqCst);
                        return;
                    }
                    Ok(_) => {}
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
                                self.interrupt.store(true, Ordering::SeqCst);
                                return;
                            }
                        }
                        ComposerEvent::Interrupt | ComposerEvent::Exit => {
                            self.interrupt.store(true, Ordering::SeqCst);
                            self.show_stopping();
                        }
                        ComposerEvent::Submit(text) => {
                            if shell.route_busy_command(self.app, text) {
                                self.interrupt.store(true, Ordering::SeqCst);
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
                        ComposerEvent::Action(Action::SubagentsHub) => {
                            shell.route_busy_command(self.app, "/agents".into());
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
                        ComposerEvent::Action(Action::DisplayReset) => {
                            shell.deferred_display_reset = true;
                            shell.note("Display", "display reset after the current turn.");
                        }
                        ComposerEvent::Action(Action::PasteImage) => {
                            if let Err(error) = shell.composer.paste_clipboard_image() {
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
