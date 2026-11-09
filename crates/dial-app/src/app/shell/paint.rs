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
use crate::ui::transcript::{Retire, TranscriptCtx};
use crate::ui::{Rows, tasks};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

/// Live inputs of the `cost` segment while a turn holds the agent: the shared ledger and its
/// observed totals when the turn began. Spend observed since then is this turn's running cost
/// (it can include background work that settles meanwhile); the exact per-turn figure replaces
/// it when the turn ends.
pub(in crate::app) struct LiveCost {
    ledger: crate::llm::usage::Scope,
    ledger_start: f64,
    session_start: Option<f64>,
}

impl LiveCost {
    pub(in crate::app) fn start(agent: &Agent<'_>) -> Self {
        let ledger = agent.usage_ledger();
        Self {
            ledger_start: ledger.observed_cost().unwrap_or(0.0),
            ledger,
            session_start: agent.session_cost(),
        }
    }

    /// `(turn, session)` observed USD so far; `None` when nothing priced was observed.
    pub(in crate::app) fn observe(&self) -> (Option<f64>, Option<f64>) {
        let turn = self
            .ledger
            .observed_cost()
            .map(|total| total - self.ledger_start)
            .filter(|spent| spent.is_finite() && *spent > 0.0);
        let session = match (self.session_start, turn) {
            (start, Some(spent)) => Some(start.unwrap_or(0.0) + spent),
            (start, None) => start,
        };
        (turn, session)
    }
}

/// Keep the launch screen and status bar on the same selection, even before an agent exists.
pub(super) fn update_model_status(
    status: &mut crate::ui::status::StatusData,
    home: &mut Option<crate::ui::welcome::Home>,
    config: &crate::config::types::Config,
) {
    let selection = config.selection();
    status.assistance = Some(config.assistance.enabled);
    if status.provider != selection.provider
        || status.model.trim_end_matches(" [plan]")
            != selection
                .display_name()
                .trim_start_matches(&format!("{}/", selection.provider))
    {
        status.inference.clear();
        status.main_model_explanation = None;
    }
    status.provider = selection.provider.clone();
    status.model = selection
        .display_name()
        .trim_start_matches(&format!("{}/", selection.provider))
        .to_owned();
    // `auto` keeps no profile level: each user turn's effort is judged from its prompt.
    status.thinking = if config.thinking_auto {
        "auto".to_owned()
    } else {
        reasoning_profile(config)
            .ok()
            .flatten()
            .as_ref()
            .map(thinking_label)
            .unwrap_or_default()
    };
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

/// Live view of a streaming `/btw` answer: its thread, then the newest non-empty lines of text
/// (at most `max` rows in all). The tail is kept because the newest words are what the user watches.
fn side_rows(
    thread: &str,
    streamed: &str,
    cols: usize,
    theme: &crate::ui::theme::Theme,
    max: usize,
) -> Vec<ratatui::text::Line<'static>> {
    const TAIL_LINES: usize = 3;
    if max == 0 {
        return Vec::new();
    }
    let header = format!("/btw {}", crate::ui::text::one_line(thread));
    let mut rows = vec![ratatui::text::Line::from(ratatui::text::Span::styled(
        crate::ui::text::ellipsize(&header, cols, theme.ascii()),
        theme.fg(crate::ui::theme::Tok::Muted),
    ))];
    let budget = (max - 1).min(TAIL_LINES);
    let tail: Vec<String> = streamed
        .lines()
        .rev()
        .map(crate::ui::text::sanitize)
        .filter(|line| !line.trim().is_empty())
        .take(budget)
        .collect();
    for line in tail.into_iter().rev() {
        rows.push(ratatui::text::Line::from(ratatui::text::Span::raw(
            crate::ui::text::ellipsize_start(&line, cols, theme.ascii()),
        )));
    }
    rows
}

/// Longest session title the terminal title carries, in cells.
const TAB_SCOPE_WIDTH: usize = 40;

/// The terminal title: `dial · <project>`, the session's scope clipped to [`TAB_SCOPE_WIDTH`],
/// then the state circle. Keep the circle last so left-truncated, unselected tabs still show it.
pub(super) fn tab_title(base: &str, scope: &str, marker: &str) -> String {
    let mut title = base.to_owned();
    let scope = crate::ui::text::clip(scope, TAB_SCOPE_WIDTH);
    if !scope.is_empty() {
        title.push_str(" · ");
        title.push_str(&scope);
    }
    if !marker.is_empty() {
        title.push(' ');
        title.push_str(marker);
    }
    title
}

/// Rows the chrome between the transcript and the composer may take on a `rows`-tall terminal
/// whose composer takes `composer` rows: at most two fifths of the height, and never so many that
/// fewer than three rows remain for the transcript.
pub(super) fn aux_budget(rows: usize, composer: usize) -> usize {
    rows.saturating_sub(composer + 3).min(rows * 2 / 5)
}

impl Shell {
    // ----- painting -------------------------------------------------------------------------

    /// The user submitted input on the home screen: whatever it reports from now on ends the home
    /// at the next paint, so a command's answer is never hidden behind the launch screen.
    pub(super) fn mark_home_input(&mut self) {
        if self.home.is_some() && self.home_mark.is_none() {
            self.home_mark = Some(self.transcript.block_count());
        }
    }

    /// Names the tab after the session's scope (see [`tab_title`]); an empty title clears it.
    pub(super) fn set_scope(&mut self, session_title: &str) {
        self.tab_scope = crate::ui::text::one_line(session_title.trim());
    }

    pub(in crate::app) fn paint(&mut self, panel: Option<Rows>) -> Result<()> {
        self.status.controls_pending = self.pending_config.is_some();
        self.composer.set_command_context(self.command_context());
        if panel.is_some() && self.busy_overlay.is_some() {
            // Approvals and questions paint inline, even when a turn has a menu open. Yield the
            // alternate screen without discarding the menu; its next step reopens and redraws it.
            self.term.close_overlay()?;
        }
        if self.term.output_backlogged() {
            self.term.defer_paint();
            return Ok(());
        }
        let files = if self.ui.session_rail {
            self.git.files(Path::new(&self.status.project_path))
        } else {
            Vec::new()
        };
        // Workers spawned by a `task` call (a parent node) live in that call's card; the pinned
        // worker block above the composer lists the rest (background and scheduled workers).
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
        // The conversation has started (a message, a resumed or switched session, or a command
        // whose report would otherwise land in scrollback behind the full-height home): the home
        // steps aside for its one-line summary, which leads the conversation in scrollback.
        let reported = self
            .home_mark
            .is_some_and(|mark| self.transcript.block_count() > mark);
        if self.home.is_some() && (self.transcript.has_user() || reported) {
            let home = self.home.take().expect("checked");
            self.home_mark = None;
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
            tab_scope,
            todo,
            work_tab,
            home,
            ..
        } = self;
        let theme = themes.current();
        let expand_key = keys.label(Action::ToolsExpand);
        transcript.set_thinking_key(&keys.label(Action::ThinkingToggle));
        let tctx = TranscriptCtx {
            theme,
            ui,
            tick: status.tick,
            tools_expanded: *tools_expanded,
            expand_key: &expand_key,
        };
        // Every paint retires a bounded slice of history, busy or idle, so a resumed transcript drains
        // over successive frames instead of in one write.
        let retire = Retire::frame(Instant::now());
        let (settled, images) = transcript.retire_settled(cols, &tctx, retire);
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
                // Short terminals keep the transcript: the rail, pins and work card need room.
                let tall = rows as usize >= crate::ui::worker_pins::MIN_TERMINAL_ROWS;
                // One budget for every row between the transcript and the composer, spent in
                // priority order: what needs attention, the plan, workers, then the rail.
                let mut budget = aux_budget(rows as usize, view.rows.len());
                let mut attention = Vec::new();
                let notices = [
                    self.release_notice.chrome_notice(),
                    self.runtime_updates.notice(),
                ];
                // Notices join their parts with `·`; the ASCII preset keeps its own separator.
                let dot = format!(" {} ", theme.sym(crate::ui::theme::Sym::Dot));
                for notice in notices.into_iter().flatten() {
                    if attention.len() >= budget {
                        break;
                    }
                    let notice = notice.replace(" · ", &dot);
                    attention.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                        crate::ui::text::ellipsize(&notice, cols as usize, theme.ascii()),
                        theme.fg(crate::ui::theme::Tok::Muted),
                    )));
                }
                if let Some((thread, streamed)) = self.side.streamed() {
                    let room = budget.saturating_sub(attention.len());
                    attention.extend(side_rows(thread, streamed, cols as usize, theme, room));
                }
                budget = budget.saturating_sub(attention.len());
                let tab_key = keys.label(Action::WorkTab);
                let cycle_key = keys.label(Action::ApprovalCycle);
                // Wide terminals show the plan as a tabbed card (the work panel); narrower or
                // shorter ones keep the compact focus summary.
                let panel = if tall {
                    crate::ui::work_panel::rows(
                        todo,
                        &crate::ui::work_panel::Facts {
                            approval: &status.approval,
                            sandbox: &status.sandbox,
                            tab_key: &tab_key,
                            cycle_key: &cycle_key,
                        },
                        *work_tab,
                        cols as usize,
                        budget.min((rows as usize / 2).min(14)),
                        theme,
                        spinner,
                    )
                } else {
                    Vec::new()
                };
                let plan = if panel.is_empty() {
                    tasks::rows(todo, cols as usize, theme, spinner, cap.min(budget))
                } else {
                    panel
                };
                budget = budget.saturating_sub(plan.len());
                let worker_cap = match ui.pinned_workers {
                    crate::llm::model_config::PinnedWorkers::Full => {
                        (rows as usize / 3).clamp(2, 12)
                    }
                    _ => (rows as usize / 5).clamp(2, 6),
                };
                // An attached `/watch` pane shows workers in full; the block would repeat it.
                let workers = if tall && !self.live_pane.attached() {
                    crate::ui::worker_pins::rows(
                        &self.workers,
                        cols as usize,
                        worker_cap.min(budget),
                        ui.pinned_workers,
                        ui.worker_live_preview,
                        status.tick,
                        theme,
                    )
                } else {
                    Vec::new()
                };
                budget = budget.saturating_sub(workers.len());
                let mut bottom = if ui.session_rail && tall && budget > 0 {
                    crate::ui::rail::session(
                        &self.session_id,
                        title,
                        &files,
                        cols as usize,
                        (rows as usize / 8).clamp(1, 4).min(budget),
                        theme,
                    )
                } else {
                    Vec::new()
                };
                // Drawn top to bottom: rail, workers, attention, then the plan by the composer.
                bottom.extend(workers);
                bottom.extend(attention);
                bottom.extend(plan);
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
        let marker = signals
            .as_ref()
            .filter(|_| !theme.ascii())
            .map_or("", |signals| signals.activity.marker());
        let tab = tab_title(title, tab_scope, marker);
        term.paint(&Viewport {
            rows: &live,
            cursor: cursor.map(|(row, col)| (above + row, col)),
            title: Some(tab.as_str()),
            signals: signals.as_ref(),
        })
    }

    /// Re-reads the worker board (throttled to about once a second) and queues finish notices.
    /// Reads session files, so this runs from the event loop, never from draw. While the worker
    /// hub or viewer is open the notices stay queued (those views already show the change) and
    /// are delivered once it closes, so nothing raised meanwhile is lost. Notices about the worker
    /// the viewer is showing are dropped instead: the user is already watching it.
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
        let held = self
            .busy_overlay
            .as_ref()
            .and_then(|busy| match &busy.overlay {
                ActiveOverlay::Subagents(_) => Some(None),
                ActiveOverlay::SubagentViewer(viewer) => Some(Some(viewer.id().to_owned())),
                _ => None,
            });
        if let Some(viewing) = held {
            if let Some(id) = viewing {
                self.workers.drop_notices_for(&id);
            }
            return;
        }
        let hub_key = self.keys.label(Action::SubagentsHub);
        let open_key = self.keys.label(Action::SubagentsRecent);
        for line in self.workers.take_notices_with(&hub_key, &open_key) {
            self.note("Workers", line);
        }
    }

    pub(super) fn refresh_status(&mut self, app: &App, agent: &Agent<'_>) {
        self.status.inbox_unread = self.inbox.unread();
        self.refresh_workers(app);
        self.adapter_monitor = agent.adapter_monitor();
        self.request_policy = agent.session().control_state();
        update_model_status(
            &mut self.status,
            &mut self.home,
            agent.active_model_config(),
        );
        self.status.main_model_explanation = agent.main_model_explanation().map(str::to_owned);
        self.status.planning = Some(agent.plan_mode());
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
        status.loaded_context_window = agent.loaded_context_window();
        status.compaction_reserve = Some(
            agent
                .effective_config()
                .compaction
                .reserve(status.context_window),
        );
        status.approval = format!("{:?}", agent.approval()).to_lowercase();
        self.pty_approval = if agent.plan_mode() {
            Approval::Readonly
        } else {
            agent.approval()
        };
        status.sandbox = format!("{:?}", app.config.sandbox.mode).to_lowercase();
        // Between turns the segment shows the last turn's exact ledger figure.
        status.turn_cost = agent.usage_ledger().observed_turn_cost();
        status.session_cost = agent.session_cost();
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
