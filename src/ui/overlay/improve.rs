//! `/improve`: one tabbed panel for self-improvement: status, jobs, versions and setup. The controller
//! builds a [`View`] from the learning store and job files; this panel only lays it out and turns
//! keys into events. Stop and rollback ask for Enter first, then run the matching `/improve` command,
//! which keeps its own checks.
use super::{
    DrawCtx, OverlayEvent,
    compute::tab_line,
    settings::{filled, fit, panel, selected_fill},
};
use crate::ui::{
    chrome::{self, Section},
    keys::{Action, Keymap},
    text,
    theme::{Sym, Theme, Tok},
};
use crossterm::event::{Event, KeyEventKind};
use ratatui::{
    Frame,
    style::Style,
    text::{Line, Span},
};
use std::time::{Duration, Instant};

const TABS: [&str; 4] = ["Status", "Jobs", "Versions", "Setup"];
/// While a job runs, the controller re-reads the snapshot at this interval.
pub const REFRESH: Duration = Duration::from_secs(2);
/// Rows of the Setup tab: the panel name and what it changes.
const SETUP: [(&str, &str); 2] = [
    ("Settings", "Enabled, schedule, budgets and teaching"),
    ("Models", "learning models and their roles"),
];

/// What the panel shows, computed by the controller from the store and job files.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct View {
    /// `[improve] enabled`, the Enabled switch in settings.
    pub enabled: bool,
    /// Plain-words readiness: what holds learning back, or that a cycle is eligible.
    pub readiness: String,
    /// Plain-words state of the latest cycle.
    pub state: String,
    pub live_version: Option<i64>,
    /// Approved turns recorded for this model, and the number training needs.
    pub approved: usize,
    pub needed: usize,
    /// Stage and timing lines of a cycle that is running; empty when none is.
    pub progress: Vec<String>,
    /// One line per step of the latest finished cycle.
    pub last_cycle: Vec<String>,
    pub jobs: Vec<JobRow>,
    pub versions: Vec<VersionRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JobRow {
    /// Model state id, the argument of `/improve stop`.
    pub id: String,
    pub label: String,
    pub active: bool,
    pub stopping: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VersionRow {
    pub id: i64,
    /// One compact line: id, status, kind and parent.
    pub text: String,
    pub live: bool,
    /// The parent of the live version: the only one `/improve rollback` can restore.
    pub previous: bool,
}

/// A destructive choice waiting for Enter.
enum Ask {
    Stop(String),
    Rollback(i64),
}

impl Ask {
    fn prompt(&self) -> String {
        match self {
            Self::Stop(id) => format!(
                "Stop job {id}? It halts at the next safe point, the unfinished cycle is discarded, and improvement stays paused for this model until it is resumed."
            ),
            Self::Rollback(id) => format!(
                "Make v{id} live again? The newer version stays recorded, and the change applies from the next turn."
            ),
        }
    }

    /// The `/improve` command that performs the choice. Rollback carries `--yes` because the panel
    /// has already asked.
    fn event(&self) -> OverlayEvent {
        OverlayEvent::Action(match self {
            Self::Stop(id) => format!("command:/improve stop {id}"),
            Self::Rollback(id) => format!("command:/improve rollback {id} --yes"),
        })
    }
}

enum Notice {
    Info(String),
    Done(String),
    Failed(String),
}

pub struct ImproveOverlay {
    view: View,
    tab: usize,
    cursor: usize,
    ask: Option<Ask>,
    notice: Option<Notice>,
    refreshed: Instant,
}

impl ImproveOverlay {
    pub fn new(view: View) -> Self {
        Self {
            view,
            tab: 0,
            cursor: 0,
            ask: None,
            notice: None,
            refreshed: Instant::now(),
        }
    }

    /// Replaces the snapshot. The cursor stays inside the list when it shrinks.
    pub fn update(&mut self, view: View) {
        self.view = view;
        self.refreshed = Instant::now();
        self.cursor = self.cursor.min(self.rows().saturating_sub(1));
    }

    /// True when a job is active and the snapshot is due for a re-read.
    pub fn wants_refresh(&self) -> bool {
        self.view.jobs.iter().any(|job| job.active) && self.refreshed.elapsed() >= REFRESH
    }

    pub fn banner(&mut self, message: String, is_error: bool) {
        let message = text::one_line(&message);
        self.notice = Some(if is_error {
            Notice::Failed(message)
        } else {
            Notice::Done(message)
        });
    }

    fn rows(&self) -> usize {
        match self.tab {
            1 => self.view.jobs.len(),
            2 => self.view.versions.len(),
            3 => SETUP.len(),
            _ => 0,
        }
    }

    fn switch(&mut self, step: usize) {
        self.tab = (self.tab + step) % TABS.len();
        self.cursor = 0;
        self.notice = None;
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if key.kind == KeyEventKind::Release {
            return OverlayEvent::None;
        }
        if let Some(ask) = self.ask.take() {
            if keys.matches(Action::SelectConfirm, key) {
                return ask.event();
            }
            // Esc drops the question; any other key leaves it open.
            if !keys.matches(Action::SelectCancel, key) {
                self.ask = Some(ask);
            }
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectCancel, key) {
            return OverlayEvent::Close;
        }
        if keys.matches(Action::TabNext, key) || keys.matches(Action::SectionNext, key) {
            self.switch(1);
        } else if keys.matches(Action::TabPrev, key) || keys.matches(Action::SectionPrev, key) {
            self.switch(TABS.len() - 1);
        } else if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(self.rows().saturating_sub(1));
        } else if keys.matches(Action::SelectConfirm, key) {
            return self.activate();
        }
        OverlayEvent::None
    }

    fn activate(&mut self) -> OverlayEvent {
        match self.tab {
            1 => {
                let Some(job) = self.view.jobs.get(self.cursor) else {
                    return OverlayEvent::None;
                };
                self.notice = Some(if job.stopping {
                    Notice::Info(format!("A stop is already requested for {}.", job.id))
                } else if job.active {
                    self.ask = Some(Ask::Stop(job.id.clone()));
                    return OverlayEvent::None;
                } else {
                    Notice::Info("That job is idle; there is nothing to stop.".into())
                });
            }
            2 => {
                let Some(version) = self.view.versions.get(self.cursor) else {
                    return OverlayEvent::None;
                };
                if version.live {
                    self.notice = Some(Notice::Info(format!("v{} is already live.", version.id)));
                } else if version.previous {
                    self.ask = Some(Ask::Rollback(version.id));
                } else {
                    self.notice = Some(Notice::Info(
                        "Only the previous version can be restored from here. Roll back one version at a time.".into(),
                    ));
                }
            }
            3 => {
                let target = if self.cursor == 0 {
                    "open:improve-settings"
                } else {
                    "open:improve-models"
                };
                return OverlayEvent::Action(target.into());
            }
            _ => {}
        }
        OverlayEvent::None
    }

    /// Body rows and the index of the selected one. Pointer and state colors are applied here.
    fn body(&self, theme: &Theme) -> (Vec<(String, Tok)>, Option<usize>) {
        let pointer = |selected: bool| {
            if selected {
                theme.sym(Sym::Pointer).to_owned()
            } else {
                " ".to_owned()
            }
        };
        let mut rows: Vec<(String, Tok)> = Vec::new();
        let mut selected = None;
        match self.tab {
            0 => {
                let view = &self.view;
                rows.push((
                    if view.enabled {
                        "Automatic improvement: on".into()
                    } else {
                        "Automatic improvement: off (Enabled is off in /improve settings)".into()
                    },
                    if view.enabled { Tok::Success } else { Tok::Warning },
                ));
                rows.push((format!("Readiness: {}", view.readiness), Tok::Text));
                rows.push((
                    format!(
                        "Approved examples: {} of {} needed for training",
                        view.approved, view.needed
                    ),
                    Tok::Text,
                ));
                rows.push((
                    match view.live_version {
                        Some(id) => format!("Live version: v{id}"),
                        None => "Live version: none yet".into(),
                    },
                    Tok::Text,
                ));
                rows.push((format!("State: {}", view.state), Tok::Muted));
                for line in &view.progress {
                    rows.push((format!("  {line}"), Tok::Accent));
                }
                rows.push((String::new(), Tok::Muted));
                rows.push((" Last cycle".into(), Tok::Muted));
                if view.last_cycle.is_empty() {
                    rows.push(("  No cycle has finished yet.".into(), Tok::Muted));
                }
                for line in &view.last_cycle {
                    rows.push((format!("  {line}"), Tok::Muted));
                }
            }
            1 => {
                if self.view.jobs.is_empty() {
                    rows.push(("No improvement jobs in this project.".into(), Tok::Muted));
                }
                for (index, job) in self.view.jobs.iter().enumerate() {
                    let is_selected = index == self.cursor;
                    if is_selected {
                        selected = Some(rows.len());
                    }
                    rows.push((
                        format!(
                            " {} {} {}",
                            pointer(is_selected),
                            if job.active { "●" } else { "○" },
                            job.label
                        ),
                        if job.active {
                            Tok::Success
                        } else if is_selected {
                            Tok::Accent
                        } else {
                            Tok::Text
                        },
                    ));
                }
            }
            2 => {
                if self.view.versions.is_empty() {
                    rows.push(("No versions recorded yet.".into(), Tok::Muted));
                }
                for (index, version) in self.view.versions.iter().enumerate() {
                    let is_selected = index == self.cursor;
                    if is_selected {
                        selected = Some(rows.len());
                    }
                    rows.push((
                        format!(
                            " {} {} {}",
                            pointer(is_selected),
                            if version.live { "●" } else { "○" },
                            version.text
                        ),
                        if version.live {
                            Tok::Success
                        } else if is_selected {
                            Tok::Accent
                        } else {
                            Tok::Text
                        },
                    ));
                }
            }
            _ => {
                for (index, (name, detail)) in SETUP.iter().enumerate() {
                    let is_selected = index == self.cursor;
                    if is_selected {
                        selected = Some(rows.len());
                    }
                    rows.push((
                        format!(" {} {name} — {detail}", pointer(is_selected)),
                        if is_selected { Tok::Accent } else { Tok::Text },
                    ));
                }
            }
        }
        (rows, selected)
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        panel(
            frame,
            ctx.theme,
            Section::Learn,
            "Improve",
            "this model",
            |width, height| {
                let w = usize::from(width);
                let h = usize::from(height);
                let (rows, selected) = self.body(ctx.theme);
                let mut output: Vec<Line<'static>> = vec![tab_line(ctx.theme, &TABS, self.tab, w)];
                let available = h.saturating_sub(output.len() + 2);
                let scroll = selected.map_or(0, |row| row.saturating_sub(available.saturating_sub(1)));
                for (index, (row, token)) in rows.into_iter().enumerate().skip(scroll).take(available) {
                    let fill = if self.ask.is_none() && selected == Some(index) {
                        selected_fill(ctx.theme)
                    } else {
                        Style::default()
                    };
                    output.push(filled(
                        vec![Span::styled(format!(" {}", text::one_line(&row)), ctx.theme.fg(token))],
                        w,
                        fill,
                    ));
                }
                let notice = match (&self.ask, &self.notice) {
                    (Some(ask), _) => Some((ask.prompt(), Tok::Warning, None)),
                    (None, Some(Notice::Done(message))) => Some((message.clone(), Tok::Success, Some(Sym::Check))),
                    (None, Some(Notice::Failed(message))) => Some((message.clone(), Tok::Error, Some(Sym::Cross2))),
                    (None, Some(Notice::Info(message))) => Some((message.clone(), Tok::Muted, None)),
                    (None, None) => None,
                };
                if let Some((message, token, mark)) = notice
                    && output.len() < h
                {
                    let prefix = mark.map_or(String::new(), |sym| format!("{} ", ctx.theme.sym(sym)));
                    output.push(filled(
                        vec![Span::styled(format!(" {prefix}{}", text::one_line(&message)), ctx.theme.fg(token))],
                        w,
                        Style::default(),
                    ));
                }
                if output.len() < h {
                    let keys = ctx.keys;
                    let hints: Vec<(String, String)> = if self.ask.is_some() {
                        vec![
                            (keys.label(Action::SelectConfirm), "confirm".into()),
                            (keys.label(Action::SelectCancel), "keep it".into()),
                        ]
                    } else {
                        let act = match self.tab {
                            1 => "stop",
                            2 => "roll back",
                            3 => "open",
                            _ => "",
                        };
                        let mut hints = vec![
                            (
                                format!("{}/{}", keys.label(Action::TabPrev), keys.label(Action::TabNext)),
                                "tab".into(),
                            ),
                            (
                                format!("{}/{}", keys.label(Action::SelectUp), keys.label(Action::SelectDown)),
                                "select".into(),
                            ),
                        ];
                        if !act.is_empty() {
                            hints.push((keys.label(Action::SelectConfirm), act.into()));
                        }
                        hints.push((keys.label(Action::SelectCancel), "close".into()));
                        hints
                    };
                    let hints: Vec<(&str, &str)> = hints.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(chrome::key_hint_spans(ctx.theme, &hints));
                    output.push(Line::from(fit(spans, w)));
                }
                output
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::settings::testkit::*;
    use super::*;
    use crossterm::event::KeyCode;

    fn view() -> View {
        View {
            enabled: true,
            readiness: "eligible at the next scheduling check".into(),
            state: "idle".into(),
            live_version: Some(3),
            approved: 18,
            needed: 24,
            last_cycle: vec!["self-play · 24 cases".into(), "evaluation · kept v3".into()],
            jobs: vec![
                JobRow {
                    id: "abcd".into(),
                    label: "project · training · local-model".into(),
                    active: true,
                    stopping: false,
                },
                JobRow {
                    id: "ef01".into(),
                    label: "other · idle · local-model".into(),
                    active: false,
                    stopping: false,
                },
            ],
            versions: vec![
                VersionRow {
                    id: 3,
                    text: "v3 · active · lora · from v2".into(),
                    live: true,
                    previous: false,
                },
                VersionRow {
                    id: 2,
                    text: "v2 · retired · lora · from v1".into(),
                    live: false,
                    previous: true,
                },
                VersionRow {
                    id: 1,
                    text: "v1 · retired · base".into(),
                    live: false,
                    previous: false,
                },
            ],
            ..View::default()
        }
    }

    fn press(overlay: &mut ImproveOverlay, code: KeyCode) -> OverlayEvent {
        overlay.handle(&key(code), &Keymap::defaults())
    }

    #[test]
    fn tabs_cycle_and_escape_closes_the_panel() {
        let mut overlay = ImproveOverlay::new(view());
        press(&mut overlay, KeyCode::Right);
        assert_eq!(overlay.tab, 1);
        press(&mut overlay, KeyCode::Left);
        press(&mut overlay, KeyCode::Left);
        assert_eq!(overlay.tab, 3);
        assert!(matches!(press(&mut overlay, KeyCode::Esc), OverlayEvent::Close));
    }

    #[test]
    fn stop_asks_for_enter_and_escape_keeps_the_job() {
        let mut overlay = ImproveOverlay::new(view());
        press(&mut overlay, KeyCode::Right);
        assert!(matches!(press(&mut overlay, KeyCode::Enter), OverlayEvent::None));
        assert!(overlay.ask.is_some(), "Enter asks first");
        assert!(matches!(press(&mut overlay, KeyCode::Esc), OverlayEvent::None));
        assert!(overlay.ask.is_none(), "Esc drops the question without closing");
        press(&mut overlay, KeyCode::Enter);
        let OverlayEvent::Action(action) = press(&mut overlay, KeyCode::Enter) else {
            panic!("confirmed stop sends a command");
        };
        assert_eq!(action, "command:/improve stop abcd");
    }

    #[test]
    fn idle_jobs_have_nothing_to_stop() {
        let mut overlay = ImproveOverlay::new(view());
        press(&mut overlay, KeyCode::Right);
        press(&mut overlay, KeyCode::Down);
        press(&mut overlay, KeyCode::Enter);
        assert!(overlay.ask.is_none());
        assert!(overlay.screen_notice().contains("nothing to stop"));
    }

    #[test]
    fn only_the_previous_version_can_be_rolled_back_after_confirmation() {
        let mut overlay = ImproveOverlay::new(view());
        press(&mut overlay, KeyCode::Right);
        press(&mut overlay, KeyCode::Right);
        press(&mut overlay, KeyCode::Enter);
        assert!(overlay.ask.is_none(), "the live version is not a rollback target");
        press(&mut overlay, KeyCode::Down);
        press(&mut overlay, KeyCode::Enter);
        assert!(overlay.ask.is_some());
        let OverlayEvent::Action(action) = press(&mut overlay, KeyCode::Enter) else {
            panic!("confirmed rollback sends a command");
        };
        assert_eq!(action, "command:/improve rollback 2 --yes");
        press(&mut overlay, KeyCode::Down);
        press(&mut overlay, KeyCode::Enter);
        assert!(overlay.ask.is_none(), "an older version cannot be restored from here");
    }

    #[test]
    fn setup_rows_open_the_existing_settings_and_models_views() {
        let mut overlay = ImproveOverlay::new(view());
        for _ in 0..3 {
            press(&mut overlay, KeyCode::Right);
        }
        let OverlayEvent::Action(settings) = press(&mut overlay, KeyCode::Enter) else {
            panic!("settings row opens a view");
        };
        assert_eq!(settings, "open:improve-settings");
        press(&mut overlay, KeyCode::Down);
        let OverlayEvent::Action(models) = press(&mut overlay, KeyCode::Enter) else {
            panic!("models row opens a view");
        };
        assert_eq!(models, "open:improve-models");
    }

    #[test]
    fn status_and_lists_render_plain_lines_without_internal_keys() {
        let rig = Rig::new();
        let mut overlay = ImproveOverlay::new(view());
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let status = screen(&render(100, 24, |frame| overlay.draw(frame, &ctx)));
        assert!(status.contains("Approved examples: 18 of 24 needed for training"), "{status}");
        assert!(status.contains("Live version: v3"), "{status}");
        assert!(status.contains("self-play · 24 cases"), "{status}");
        assert!(!status.contains("[improve]") && !status.contains('{'), "{status}");
        press(&mut overlay, KeyCode::Right);
        press(&mut overlay, KeyCode::Right);
        let versions = screen(&render(100, 24, |frame| overlay.draw(frame, &ctx)));
        assert!(versions.contains("v2 · retired · lora · from v1"), "{versions}");
        let rows = versions.lines().filter(|line| line.contains("· retired ·")).count();
        assert_eq!(rows, 2, "one line per version: {versions}");
    }

    #[test]
    fn refresh_is_requested_only_while_a_job_runs() {
        let mut overlay = ImproveOverlay::new(view());
        overlay.refreshed = Instant::now() - REFRESH;
        assert!(overlay.wants_refresh());
        overlay.view.jobs.iter_mut().for_each(|job| job.active = false);
        assert!(!overlay.wants_refresh());
    }

    #[test]
    fn small_panels_render_without_panics() {
        let rig = Rig::new();
        let mut overlay = ImproveOverlay::new(view());
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        for width in [1, 12, 40, 120] {
            for height in [1, 4, 12, 30] {
                render(width, height, |frame| overlay.draw(frame, &ctx));
            }
        }
    }
}

impl ImproveOverlay {
    /// The text of the notice line, for tests and the footer.
    #[cfg(test)]
    fn screen_notice(&self) -> String {
        match &self.notice {
            Some(Notice::Done(m) | Notice::Failed(m) | Notice::Info(m)) => m.clone(),
            None => String::new(),
        }
    }
}
