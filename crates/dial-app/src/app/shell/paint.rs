//! Repainting the shell: transcript tail, composer, status line and worker rows.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::display::{local_time, short};
use crate::app::overlay::ActiveOverlay;
use crate::app::selection::{reasoning_profile, thinking_label};
use crate::app::shell::Shell;
use crate::config::types::Approval;
use crate::ui::composer::ComposerCtx;
use crate::ui::keys::Action;
use crate::ui::status::{Lora, Phase};
use crate::ui::term::Viewport;
use crate::ui::term_state::{Activity, Progress, Signals};
use crate::ui::transcript::TranscriptCtx;
use crate::ui::{Rows, tasks};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

/// Keep the launch screen and status bar on the same selection, even before an agent exists.
pub(super) fn update_model_status(
    status: &mut crate::ui::status::StatusData,
    home: &mut Option<crate::ui::welcome::Home>,
    config: &crate::config::types::Config,
) {
    let selection = config.selection();
    if status.provider != selection.provider
        || status.model.trim_end_matches(" [plan]")
            != selection
                .display_name()
                .trim_start_matches(&format!("{}/", selection.provider))
    {
        status.inference.clear();
    }
    status.provider = selection.provider.clone();
    status.model = selection
        .display_name()
        .trim_start_matches(&format!("{}/", selection.provider))
        .to_owned();
    status.thinking = reasoning_profile(config)
        .ok()
        .flatten()
        .as_ref()
        .map(thinking_label)
        .unwrap_or_default();
    if let Some(home) = home {
        home.info.provider.clone_from(&status.provider);
        home.info.model.clone_from(&status.model);
    }
}

/// What the terminal should show outside the cells for this paint (see `ui::term_state`); `None`
/// when terminal integration is off.
pub(super) fn terminal_signals(
    ui: &crate::llm::model_config::UiConfig,
    status: &crate::ui::status::StatusData,
    after_turn: Activity,
    awaiting_user: bool,
) -> Option<Signals> {
    if !ui.terminal_integration {
        return None;
    }
    let (activity, progress) = if awaiting_user {
        (Activity::Waiting, Progress::Paused)
    } else if status.busy() {
        let progress = status
            .prompt_fraction()
            .map_or(Progress::Indeterminate, |fraction| {
                Progress::Percent((fraction * 100.0).round() as u8)
            });
        (Activity::Working, progress)
    } else {
        (after_turn, Progress::None)
    };
    let mut vars = vec![
        ("dialState", activity.name().to_owned()),
        ("dialModel", status.model.clone()),
    ];
    if let Some(fraction) = status.context_fraction() {
        vars.push(("dialContext", format!("{:.0}%", fraction * 100.0)));
    }
    Some(Signals {
        activity,
        progress,
        cwd: (!status.project_path.is_empty()).then(|| status.project_path.clone().into()),
        vars,
    })
}

impl Shell {
    // ----- painting -------------------------------------------------------------------------

    pub(in crate::app) fn paint(&mut self, panel: Option<Rows>) -> Result<()> {
        let files = if self.ui.session_rail {
            self.git.files(Path::new(&self.status.project_path))
        } else {
            Vec::new()
        };
        // Workers spawned by a `task` call live in that call's card; the rail keeps the rest
        // (background and scheduled workers that no card owns).
        let snapshots: Vec<crate::runtime::Node> = self
            .runtime
            .bodies("worker")
            .iter()
            .filter(|b| b.read(|n| n.parent.is_some()))
            .map(|b| b.snapshot())
            .collect();
        if !snapshots.is_empty() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            self.transcript.sync_agents(&snapshots, now_ms);
        }
        self.status.workers = Some(self.workers.tally());
        self.status.tick = (self.started.elapsed().as_millis() / 83) as u64;
        self.status.clock = local_time("%H:%M");
        self.status.mode = self.composer.mode();
        self.status.usage = self.usage_status.poll();
        let (cols, rows) = self.term.size();
        // The conversation has started (a message, a resumed or switched session): the home
        // steps aside for its one-line summary, which leads the conversation in scrollback.
        if self.home.is_some() && self.transcript.has_user() {
            let home = self.home.take().expect("checked");
            self.term.commit(home.summary(cols, self.themes.current()));
        }
        let Shell {
            term,
            themes,
            keys,
            composer,
            transcript,
            status,
            ui,
            tools_expanded,
            title,
            todo,
            home,
            ..
        } = self;
        let theme = themes.current();
        let expand_key = keys.label(Action::ToolsExpand);
        let tctx = TranscriptCtx {
            theme,
            ui,
            tick: status.tick,
            tools_expanded: *tools_expanded,
            expand_key: &expand_key,
        };
        let (settled, images) = transcript.take_settled_with_images(cols, &tctx);
        if !settled.is_empty() {
            if let Some(protocol) = term.image_protocol() {
                term.commit_with_images(settled, images, protocol);
            } else {
                term.commit(settled);
            }
        }
        let cctx = ComposerCtx {
            theme,
            keys,
            ui,
            status,
        };
        let (bottom, cursor) = match panel {
            Some(rows) => (rows, None),
            None => {
                let input_width = if home.is_some() {
                    crate::ui::welcome::content_width(cols)
                } else {
                    cols
                };
                let view = if home.is_some() {
                    composer.home_view(input_width, &cctx)
                } else {
                    composer.view(input_width, &cctx)
                };
                let spinner = (status.phase == Phase::Working).then(|| theme.spinner(status.tick));
                let cap = (rows as usize / 3).min(10);
                let available = (rows as usize).saturating_sub(view.rows.len() + 3);
                let mut bottom = if ui.session_rail && available > 0 {
                    crate::ui::rail::session(
                        &self.session_id,
                        title,
                        &files,
                        cols as usize,
                        (rows as usize / 8).clamp(1, 4).min(available),
                        theme,
                    )
                } else {
                    Vec::new()
                };
                let worker_cap = match ui.pinned_workers {
                    crate::llm::model_config::PinnedWorkers::Full => {
                        (rows as usize / 3).clamp(2, 12)
                    }
                    _ => (rows as usize / 5).clamp(2, 6),
                }
                .min(available.saturating_sub(bottom.len()));
                if rows as usize >= crate::ui::worker_pins::MIN_TERMINAL_ROWS {
                    bottom.extend(crate::ui::worker_pins::rows(
                        &self.workers,
                        cols as usize,
                        worker_cap,
                        ui.pinned_workers,
                        ui.worker_live_preview,
                        status.tick,
                        theme,
                    ));
                }
                bottom.extend(tasks::rows(
                    todo,
                    cols as usize,
                    theme,
                    spinner,
                    cap.min(available.saturating_sub(bottom.len())),
                ));
                if let Some(home) = home {
                    let frame = home.frame(cols, rows.saturating_sub(1), theme, view, bottom);
                    (frame.rows, Some(frame.cursor))
                } else {
                    let above = bottom.len() as u16;
                    bottom.extend(view.rows);
                    (bottom, Some((view.cursor.0 + above, view.cursor.1)))
                }
            }
        };
        let room = (rows as usize).saturating_sub(bottom.len()).max(1);
        let (spilled, mut live) = transcript.live(cols, &tctx, room);
        if !spilled.is_empty() {
            term.commit(spilled);
        }
        let above = live.len() as u16;
        live.extend(bottom);
        let signals = terminal_signals(ui, status, self.term_activity, self.awaiting_user);
        term.paint(&Viewport {
            rows: &live,
            cursor: cursor.map(|(row, col)| (above + row, col)),
            title: Some(title.as_str()),
            signals: signals.as_ref(),
        })
    }

    /// Re-reads the worker board (throttled to about once a second) and queues finish notices.
    /// Reads session files, so this runs from the event loop, never from draw. Notices raised while
    /// the subagent hub or viewer is open are dropped: those views already show the change.
    pub(in crate::app) fn refresh_workers(&mut self, app: &App) {
        if self.session_id.is_empty() {
            return;
        }
        let refreshed = self.workers.refresh(
            Instant::now(),
            &app.layout,
            &self.session_id,
            &self.runtime,
            self.live_tasks.as_ref(),
        );
        if !refreshed {
            return;
        }
        let hub_key = self.keys.label(Action::SubagentsHub);
        let subagents_open = self.busy_overlay.as_ref().is_some_and(|busy| {
            matches!(
                busy.overlay,
                ActiveOverlay::Subagents(_) | ActiveOverlay::SubagentViewer(_)
            )
        });
        for line in self.workers.take_notices(&hub_key) {
            if !subagents_open {
                self.note("Workers", line);
            }
        }
    }

    pub(super) fn refresh_status(&mut self, app: &App, agent: &Agent<'_>) {
        self.refresh_workers(app);
        self.adapter_monitor = agent.adapter_monitor();
        self.request_policy = agent.session().control_state();
        update_model_status(&mut self.status, &mut self.home, agent.effective_config());
        let project_root = agent
            .layout()
            .map(|layout| layout.project_root.clone())
            .unwrap_or_else(|| app.layout.project_root.clone());
        let (branch, counts) = self.git.read(&project_root);
        // Learning status is loaded only by an explicit improvement view.
        let status = &mut self.status;
        status.project_path = project_root.display().to_string();
        if agent.plan_mode() {
            status.model.push_str(" [plan]");
        }
        status.branch = branch;
        status.git_counts = counts;
        status.session = short(agent.session().id());
        let (tokens, estimated) = agent.context_usage();
        status.prompt_tokens = (tokens > 0).then_some(tokens);
        status.context_estimated = estimated;
        status.context_window = agent.context_window();
        status.approval = format!("{:?}", agent.approval()).to_lowercase();
        self.pty_approval = if agent.plan_mode() {
            Approval::Readonly
        } else {
            agent.approval()
        };
        status.sandbox = format!("{:?}", app.config.sandbox.mode).to_lowercase();
        status.version = None;
        status.learning.clear();
        status.lora = Lora::Unknown;
        status.role.clear();
        if self.todo != *agent.todo() {
            self.todo = agent.todo().clone();
        }
    }

    /// Shift+Tab: cycles write → auto → readonly → ask → write and reports the new mode.
    pub(super) fn cycle_approval(&mut self, agent: &mut Agent<'_>) {
        let next = match agent.approval() {
            Approval::Write => Approval::Auto,
            Approval::Auto => Approval::Readonly,
            Approval::Readonly => Approval::Ask,
            Approval::Ask => Approval::Write,
        };
        agent.set_approval(next);
        self.note("Approval", format!("{next:?}").to_lowercase());
    }
}

#[cfg(test)]
mod tests;
