//! Submitting composer text: user shell escapes, loop scheduling and history.

use crate::agent::{self, Agent, Ui};
use crate::app::bridge::Bridge;
use crate::app::core::App;
use crate::app::drive::Step;
use crate::app::overlay::Which;
use crate::app::shell::{PAINT_EVERY, Shell};
use crate::config::settings_store;
use crate::runtime::tool_output::ToolOutput;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

impl Shell {
    pub(super) fn save_history(&mut self, app: &App) {
        if let Err(error) = settings_store::save_history(
            &app.layout,
            &self.composer.history(),
            self.ui.history_limit,
        ) {
            self.note(
                "History",
                format!("could not save input history: {error:#}"),
            );
        }
    }

    /// `Alt+Shift+A`: the transcript of the most recently active running worker. Repeated presses
    /// cycle through the running workers in roster order. With none running, it cycles through all
    /// workers, and with no workers it opens the hub.
    pub(super) fn open_recent_worker(&mut self, app: &App, agent: &mut Agent<'_>) -> Result<Step> {
        let running: Vec<&str> = self
            .workers
            .agents()
            .iter()
            .filter(|a| !a.status.is_finished())
            .map(|a| a.id.as_str())
            .collect();
        let all: Vec<&str> = self
            .workers
            .agents()
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        let pool = if running.is_empty() { all } else { running };
        let id = crate::ui::worker_pins::next_recent(&pool, self.recent_worker.as_deref());
        let Some(id) = id else {
            return self.open(app, agent, Which::Subagents);
        };
        self.recent_worker = Some(id.clone());
        self.open(
            app,
            agent,
            Which::SubagentViewer {
                id,
                entry: None,
                from_hub: false,
            },
        )
    }

    // ----- submitting -----------------------------------------------------------------------

    pub(super) fn submit(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        text: String,
    ) -> Result<Step> {
        let trimmed = text.trim().to_owned();
        if trimmed.is_empty() {
            return Ok(Step::Done);
        }
        self.save_history(app);
        if let Err(error) = crate::storage::archive::record_prompt(
            &app.layout.user_dir,
            &agent.layout().unwrap_or(&app.layout).project_root,
            Some(agent.session().id()),
            &trimmed,
        ) {
            self.note(
                "History",
                format!("could not record prompt history: {error:#}"),
            );
        }
        if trimmed.starts_with('/') {
            return self.command(app, agent, &trimmed);
        }
        let (shell, include) = if let Some(command) = trimmed.strip_prefix("!!") {
            (Some(command), false)
        } else {
            (trimmed.strip_prefix('!'), true)
        };
        if let Some(command) = shell.map(str::trim).filter(|c| !c.is_empty()) {
            return self.run_user_shell(app, agent, command, include);
        }
        self.run_turn(app, agent, &trimmed)
    }

    /// `!cmd`: the user's own shell command, run in the project under the configured sandbox. Its output
    /// joins the conversation so the model can refer to it.
    fn run_user_shell(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        command: &str,
        include: bool,
    ) -> Result<Step> {
        use crate::tools::{self, Workspace};
        let allowed = {
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
            agent.approve_user_shell(command, &mut bridge)
        };
        if !allowed {
            self.note("Shell", "command denied by approval policy");
            return Ok(Step::Done);
        }
        self.transcript
            .push_user(&format!("{}{command}", if include { "!" } else { "!!" }));
        let started = Instant::now();
        let layout = agent
            .layout()
            .context("agent has no project layout")?
            .clone();
        layout.ensure_project_outside_storage()?;
        let ws = Workspace::new(&layout.project_root)
            .and_then(|ws| {
                tools::apply_sandbox_config(
                    ws,
                    &app.config.sandbox,
                    Some((layout.state_dir.as_path(), layout.user_dir.as_path())),
                )
                .map(|ws| ws.with_shell_backend(app.config.shell.backend))
            })
            .and_then(|ws| {
                Ok(
                    ws.with_artifacts(crate::storage::artifact::ArtifactStore::new(
                        &layout.state_dir.join("artifacts"),
                    )?),
                )
            });
        let output = match ws {
            Ok(ws) => {
                app.interrupt.store(false, Ordering::SeqCst);
                self.composer.set_busy(true);
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
                bridge.tool_call("bash", command);
                let output = std::thread::scope(|scope| {
                    let (tx, rx) = std::sync::mpsc::sync_channel(16);
                    let cancel = app.interrupt.clone();
                    scope.spawn(move || {
                        let on_output = |chunk: &[u8]| {
                            let _ = tx.send(Ok(chunk.to_vec()));
                        };
                        let result = crate::tools::exec::shell::bash_tool_streaming(
                            &ws,
                            &json!({"command":command}),
                            120,
                            &on_output,
                            &cancel,
                        );
                        let output = match result {
                            Ok(output) => output,
                            Err(text) => ToolOutput {
                                execution: Default::default(),
                                data: None,
                                text,
                                is_error: true,
                            },
                        };
                        let _ = tx.send(Err(output));
                    });
                    loop {
                        match rx.recv_timeout(Duration::from_millis(80)) {
                            Ok(Ok(chunk)) => {
                                bridge.tool_output("bash", &String::from_utf8_lossy(&chunk))
                            }
                            Ok(Err(output)) => break output,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                // Quiet output still owns input: keys, paste and resize go through
                                // the same dispatcher a noisy command uses in `repaint`, so the
                                // draft, queue and configured interrupt behave identically.
                                bridge.pump();
                                bridge.heartbeat()
                            }
                            Err(_) => {
                                break ToolOutput {
                                    execution: Default::default(),
                                    data: None,
                                    text: "shell worker disconnected".into(),
                                    is_error: true,
                                };
                            }
                        }
                    }
                });
                bridge.tool_result("bash", &output);
                bridge.shell.composer.set_busy(false);
                output
            }
            Err(error) => ToolOutput {
                execution: Default::default(),
                data: None,
                text: format!("{error:#}"),
                is_error: true,
            },
        };
        self.transcript.finish_turn();
        if include {
            agent.session_mut().append(json!({"role": "user", "content": format!("I ran `{command}` myself:\n{}", agent::safety::guard::clip_middle(&output.text, 8000))}))?;
        }
        Ok(Step::Done)
    }

    /// Returns the loop prompt to re-submit once the armed iteration is due. A `--while` / `--until`
    /// condition runs on a worker and its verdict is picked up on a later tick. Reports a stop or a
    /// pause, and clears an interrupt flag that a cancelled condition may have set.
    pub(super) fn loop_due(
        &mut self,
        app: &App,
        agent: &Agent<'_>,
        dirty: &mut bool,
    ) -> Option<String> {
        use crate::agent::modes::loop_runner::{self, Due, Gate};
        // Esc cancels only a condition that is already running; otherwise the flag is stale.
        if !self.loop_mode.is_awaiting_condition() {
            app.interrupt.store(false, Ordering::SeqCst);
        }
        let cwd = agent.workspace().root().to_path_buf();
        let interrupt = app.interrupt.clone();
        let due = self.loop_mode.take_due(Instant::now(), |condition| {
            Gate::start(condition, &cwd, interrupt, loop_runner::CONDITION_TIMEOUT)
        });
        match due {
            Due::Idle => None,
            Due::Submit(prompt) => Some(prompt),
            Due::Stop(message) | Due::Paused(message) => {
                self.note("Loop", message);
                *dirty = true;
                None
            }
        }
    }

    /// Esc while idle suspends an enabled loop.
    pub(super) fn pause_loop(&mut self) {
        if self.loop_mode.is_enabled() {
            self.loop_mode.pause();
            self.note("Loop", "Loop paused. Send a prompt to resume.");
        }
    }

    /// A session change ends the loop: its prompt and budget belong to the old session.
    /// `disable` (not `pause`) so the next prompt typed in the new session is not captured as the loop prompt.
    pub(crate) fn stop_loop_for_session_change(&mut self) {
        // /undo and /outcome score the last turn of the session that ran it, never a session we left.
        self.last_trajectory = None;
        if self.loop_mode.disable() {
            self.note(
                "Loop",
                "Loop stopped: the session changed. Start it again with /loop.",
            );
        }
    }
}
