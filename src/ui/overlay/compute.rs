//! Live compute, organized by models, jobs, services and tools.
use super::{
    DrawCtx, OverlayEvent,
    settings::{filled, fit, panel, selected_fill, typed_char},
};
use crate::ui::{
    chrome::{self, Section},
    keys::{Action, Keymap},
    text,
    theme::{Sym, Theme, Tok},
};
use crossterm::event::Event;
use ratatui::{
    Frame,
    style::Style,
    text::{Line, Span},
};
use serde_json::{Value, json};

const TABS: [&str; 5] = ["All", "Models", "Background", "Services", "Tools"];
fn model_label(value: &Value) -> &str {
    let model = value
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown");
    if model.starts_with('/') {
        model.rsplit('/').next().unwrap_or(model)
    } else {
        model
    }
}

/// A row of the compute list: a section heading, or a text row in its color.
enum ListRow {
    Heading(String),
    Text(String, Tok),
}

/// The tab strip in the shared tab style ([`chrome::tab_style`]). ASCII keeps `[tab]`.
pub(super) fn tab_line(theme: &Theme, tabs: &[&str], active: usize, width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let name = if i == active && theme.ascii() {
            format!("[{tab}]")
        } else {
            (*tab).to_owned()
        };
        spans.push(Span::styled(name, chrome::tab_style(theme, i == active)));
    }
    Line::from(fit(spans, width))
}

/// Section heading of the item list: ` ── TOOLS ───────`, label in secondary, rule muted.
pub(super) fn section_heading(theme: &Theme, name: &str, width: usize) -> Line<'static> {
    let rule = theme.sym(Sym::BoxH);
    let head = format!(" {rule}{rule} ");
    let title = chrome::label(theme, name);
    let pad = width.saturating_sub(text::width(&head) + text::width(&title.content) + 1);
    Line::from(fit(
        vec![
            Span::styled(head, theme.fg(Tok::BorderMuted)),
            title,
            Span::styled(format!(" {}", rule.repeat(pad)), theme.fg(Tok::BorderMuted)),
        ],
        width,
    ))
}
fn memory(value: &Value) -> String {
    let bytes = value.as_u64().unwrap_or(0) as f64;
    if bytes >= 1_000_000_000.0 {
        format!("{:.2} GB", bytes / 1_000_000_000.0)
    } else {
        format!("{:.3} GB", bytes / 1_000_000_000.0)
    }
}
#[derive(Clone)]
enum Target {
    Control(&'static str),
    Job(String),
    Process(Value),
    /// Non-actionable row summarising collapsed idle jobs.
    Summary,
}
impl Target {
    fn identity(&self) -> String {
        match self {
            Self::Control(action) => (*action).into(),
            Self::Job(state) => state.clone(),
            Self::Process(process) => process["identity"].as_str().unwrap_or("").into(),
            Self::Summary => String::new(),
        }
    }
}
struct Item {
    section: &'static str,
    label: String,
    active: bool,
    target: Target,
}
/// A destructive request waiting for Enter: the control to send and the consequence to name.
struct Pending {
    action: String,
    prompt: String,
}
pub struct ComputeOverlay {
    snapshot: Value,
    tab: usize,
    cursor: usize,
    /// Cursor of the list the item menu was opened from; Esc returns to it.
    saved_cursor: usize,
    menu: Option<Target>,
    confirm: Option<Pending>,
    /// Result of the last control: text and whether it failed.
    message: Option<(String, bool)>,
}
impl ComputeOverlay {
    pub fn new() -> Self {
        Self {
            snapshot: Value::Null,
            tab: 0,
            cursor: 0,
            saved_cursor: 0,
            menu: None,
            confirm: None,
            message: None,
        }
    }
    pub fn update(&mut self, snapshot: Value) {
        let selected = self
            .items()
            .get(self.cursor)
            .map(|item| item.target.identity());
        self.snapshot = snapshot;
        let items = self.items();
        if self.menu.is_none() {
            self.cursor = selected
                .and_then(|id| items.iter().position(|item| item.target.identity() == id))
                .unwrap_or(self.cursor.min(items.len().saturating_sub(1)));
        }
    }
    pub fn banner(&mut self, message: String, is_error: bool) {
        self.message = Some((text::one_line(&message), is_error));
    }
    fn jobs(&self) -> &[Value] {
        self.snapshot["jobs"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    fn processes(&self) -> &[Value] {
        self.snapshot["processes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    fn items(&self) -> Vec<Item> {
        let mut items = Vec::new();
        if self.tab == 0 || self.tab == 2 {
            for (action, label) in [
                ("pause", "Pause all background work"),
                ("resume", "Resume background work"),
                ("start_current", "Start improvement for current model"),
            ] {
                items.push(Item {
                    section: "Controls",
                    label: label.into(),
                    active: false,
                    target: Target::Control(action),
                });
            }
        }
        for (tab, kind, section) in [
            (1, "models", "Local models"),
            (2, "background", "Background jobs"),
            (3, "services", "Services"),
            (4, "tools", "Tools & subprocesses"),
        ] {
            if self.tab != 0 && self.tab != tab {
                continue;
            }
            if tab == 2 {
                let mut idle = 0usize;
                for job in self.jobs() {
                    let Some(state) = job["state"].as_str() else {
                        continue;
                    };
                    let phase = if job["control_busy"] == true {
                        if job["stop_requested"] == true {
                            "stopping"
                        } else {
                            job["progress"]["stage"]
                                .as_str()
                                .or(job["scheduler"]["state"].as_str())
                                .unwrap_or("running / waiting")
                        }
                    } else if job["stop_requested"] == true {
                        "stopped"
                    } else {
                        "idle"
                    };
                    if phase == "idle" && job["active"] != true {
                        idle += 1;
                        continue;
                    }
                    let project = job["project"]
                        .as_str()
                        .unwrap_or("")
                        .rsplit('/')
                        .find(|part| !part.is_empty())
                        .unwrap_or("unknown");
                    items.push(Item {
                        section,
                        label: format!("{project} · {phase} · {}", model_label(&job["model"])),
                        active: job["active"] == true,
                        target: Target::Job(state.into()),
                    });
                }
                if idle > 0 {
                    items.push(Item {
                        section,
                        label: format!("{idle} idle job{}", if idle == 1 { "" } else { "s" }),
                        active: false,
                        target: Target::Summary,
                    });
                }
            } else {
                for process in self
                    .processes()
                    .iter()
                    .filter(|process| process["kind"] == kind)
                {
                    items.push(Item {
                        section,
                        label: format!(
                            "{} · PID {} · {} · {} · CPU {:.1}%{}",
                            model_label(&process["model"]),
                            process["pid"],
                            process["phase"].as_str().unwrap_or("running"),
                            memory(&process["memory_bytes"]),
                            process["cpu_percent"].as_f64().unwrap_or(0.0),
                            if process["orphaned"] == true {
                                " · orphaned"
                            } else {
                                ""
                            }
                        ),
                        active: true,
                        target: Target::Process(process.clone()),
                    });
                }
            }
        }
        items
    }
    fn event(action: &str, state: Option<&str>) -> OverlayEvent {
        OverlayEvent::Action(format!(
            "compute:{}",
            json!({"action":action,"state":state})
        ))
    }
    /// Removing a job or stopping a process waits for Enter: both end work in progress.
    fn request(target: &Target) -> Option<Pending> {
        match target {
            Target::Job(state) => Some(Pending {
                action: format!("compute:{}", json!({"action":"remove","state":state})),
                prompt: "Remove this job? Automatic scheduling for it stops; its saved model data is kept.".into(),
            }),
            Target::Process(process) => Some(Pending {
                action: format!(
                    "compute:{}",
                    json!({"action":"stop_process","pid":process["pid"],"identity":process["identity"]})
                ),
                prompt: "Stop this process? Work it is doing is lost; saved models reload on next use.".into(),
            }),
            Target::Control(_) | Target::Summary => None,
        }
    }
    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if key.kind == crossterm::event::KeyEventKind::Release {
            return OverlayEvent::None;
        }
        if let Some(pending) = self.confirm.take() {
            if keys.matches(Action::SelectConfirm, key) {
                return OverlayEvent::Action(pending.action);
            }
            // Esc drops the question and keeps the job running; other keys leave it open.
            if !keys.matches(Action::SelectCancel, key) {
                self.confirm = Some(pending);
            }
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectCancel, key) {
            if self.menu.take().is_some() {
                self.cursor = self.saved_cursor;
                return OverlayEvent::None;
            }
            return OverlayEvent::Close;
        }
        if self.menu.is_none()
            && (keys.matches(Action::TabNext, key)
                || keys.matches(Action::TabPrev, key)
                || keys.matches(Action::SectionNext, key)
                || keys.matches(Action::SectionPrev, key))
        {
            let back = keys.matches(Action::TabPrev, key) || keys.matches(Action::SectionPrev, key);
            self.tab = (self.tab + if back { TABS.len() - 1 } else { 1 }) % TABS.len();
            self.cursor = 0;
            return OverlayEvent::None;
        }
        let items = self.items();
        let count = match &self.menu {
            Some(Target::Job(_)) => 3,
            Some(_) => 1,
            None => items.len(),
        };
        if typed_char(key) == Some('x') {
            if key.kind == crossterm::event::KeyEventKind::Repeat {
                return OverlayEvent::None;
            }
            let target = self
                .menu
                .clone()
                .or_else(|| items.get(self.cursor).map(|item| item.target.clone()));
            self.confirm = target.as_ref().and_then(Self::request);
        } else if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(count.saturating_sub(1));
        } else if keys.matches(Action::SelectConfirm, key) {
            if let Some(target) = self.menu.clone() {
                if let Target::Job(state) = &target {
                    // Index 2 discards an unfinished cycle, so it asks first.
                    if self.cursor == 2 {
                        self.confirm = Some(Pending {
                            action: format!(
                                "compute:{}",
                                json!({"action":"stop_now","state":state})
                            ),
                            prompt: "Stop now? The unfinished cycle is discarded and starts over next time.".into(),
                        });
                        return OverlayEvent::None;
                    }
                    return Self::event(["start", "stop"][self.cursor.min(1)], Some(state));
                }
                self.confirm = Self::request(&target);
                return OverlayEvent::None;
            }
            if let Some(item) = items
                .get(self.cursor)
                .filter(|item| !matches!(item.target, Target::Summary))
            {
                if let Target::Control(action) = item.target {
                    return Self::event(action, None);
                }
                self.saved_cursor = self.cursor;
                self.menu = Some(item.target.clone());
                self.cursor = 0;
            }
        }
        OverlayEvent::None
    }
    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        panel(
            frame,
            ctx.theme,
            Section::Sandbox,
            "Compute & background work",
            "all projects",
            |width, height| {
                let w = usize::from(width);
                let h = usize::from(height);
                let owner = &self.snapshot["owner"];
                let live = self.processes().len();
                let dial_memory = self.processes().iter().fold(
                    self.snapshot["session_memory_bytes"].as_u64().unwrap_or(0),
                    |total, process| {
                        total.saturating_add(process["memory_bytes"].as_u64().unwrap_or(0))
                    },
                );
                let occupied = self.snapshot["busy"] == true || live > 0;
                let mut header: Vec<(String, Tok)> = vec![
                    (
                        if self.snapshot.is_null() {
                            "Reading live compute status…".into()
                        } else {
                            format!(
                                "Local compute: {} · {live} processes · background {}",
                                if occupied { "occupied" } else { "available" },
                                if self.snapshot["background_paused"] == true {
                                    "paused"
                                } else {
                                    "allowed"
                                }
                            )
                        },
                        if occupied { Tok::Success } else { Tok::Muted },
                    ),
                    (
                        format!("Memory: Dial {}", memory(&json!(dial_memory))),
                        if dial_memory > 0 {
                            Tok::Success
                        } else {
                            Tok::Muted
                        },
                    ),
                    (
                        if self
                            .processes()
                            .iter()
                            .any(|p| p["memory_metric"] == "footprint")
                        {
                            "macOS memory footprint, including compressed/GPU allocations; RSS fallback.".into()
                        } else {
                            "Process resident memory (RSS); shared pages may be counted in multiple processes.".into()
                        },
                        Tok::Muted,
                    ),
                ];
                if self.snapshot["busy"] == true {
                    header.push((
                        if owner.is_object() {
                            format!(
                                "Owner: {} · PID {} · {}",
                                owner["kind"].as_str().unwrap_or("unknown"),
                                owner["pid"],
                                model_label(&owner["model"])
                            )
                        } else {
                            "Owner: unregistered Dial process".into()
                        },
                        Tok::Success,
                    ));
                }
                if let Some(error) = self.snapshot["process_error"].as_str() {
                    header.push((format!("Processes: {error}"), Tok::Warning));
                }
                let mut rows: Vec<ListRow> = Vec::new();
                let mut selected_row = 0;
                if let Some(target) = &self.menu {
                    match target {
                        Target::Job(state) => {
                            let job = self
                                .jobs()
                                .iter()
                                .find(|job| job["state"].as_str() == Some(state));
                            header.push((
                                format!(
                                    "Job: {}",
                                    job.map(|job| model_label(&job["model"]))
                                        .unwrap_or("no longer available")
                                ),
                                Tok::Text,
                            ));
                            if let Some(job) = job {
                                header.push((
                                    format!(
                                        "Project: {} · PID {}",
                                        job["project"].as_str().unwrap_or("unknown"),
                                        job["scheduler"]["pid"]
                                    ),
                                    Tok::Muted,
                                ));
                                if let Some(stage) = job["progress"]["stage"].as_str() {
                                    header.push((
                                        format!("Phase: {stage}"),
                                        if job["active"] == true {
                                            Tok::Success
                                        } else {
                                            Tok::Muted
                                        },
                                    ));
                                }
                                if let (Some(iteration), Some(total)) = (
                                    job["training"]["iteration"].as_u64(),
                                    job["training"]["total"].as_u64(),
                                ) {
                                    header.push((
                                        format!(
                                            "Training: {} {iteration}/{total}",
                                            job["training"]["phase"].as_str().unwrap_or("step")
                                        ),
                                        Tok::Muted,
                                    ));
                                }
                                if let Some(memory) = job["training"]["peak_memory_gb"].as_f64() {
                                    header.push((
                                        format!("Peak MLX memory: {memory:.2} GB"),
                                        Tok::Muted,
                                    ));
                                }
                            }
                            for (index, label) in [
                                "Start / resume cycle",
                                "Stop job safely",
                                "Stop job now (discards unfinished cycle)",
                            ]
                            .iter()
                            .enumerate()
                            {
                                rows.push(ListRow::Text(
                                    format!(
                                        " {} {label}",
                                        if index == self.cursor {
                                            ctx.theme.sym(crate::ui::theme::Sym::Pointer)
                                        } else {
                                            " "
                                        }
                                    ),
                                    if index == self.cursor {
                                        Tok::Accent
                                    } else {
                                        Tok::Text
                                    },
                                ));
                            }
                            selected_row = self.cursor;
                        }
                        Target::Process(selected) => {
                            let live = self
                                .processes()
                                .iter()
                                .find(|p| p["identity"] == selected["identity"]);
                            let process = live.unwrap_or(selected);
                            header.push((
                                format!(
                                    "{} · PID {} · {} · CPU {:.1}%",
                                    model_label(&process["model"]),
                                    process["pid"],
                                    memory(&process["memory_bytes"]),
                                    process["cpu_percent"].as_f64().unwrap_or(0.0)
                                ),
                                if live.is_some() {
                                    Tok::Success
                                } else {
                                    Tok::Muted
                                },
                            ));
                            if live.is_none() {
                                header.push(("Process exited.".into(), Tok::Muted));
                            }
                            if process["memory_metric"] == "footprint" {
                                header.push((
                                    format!(
                                        "Memory footprint: {} · Resident (RSS): {}",
                                        memory(&process["memory_bytes"]),
                                        memory(&process["resident_memory_bytes"])
                                    ),
                                    Tok::Muted,
                                ));
                            }
                            header.push((
                                "Saved models are preserved; the process reloads on next use."
                                    .into(),
                                Tok::Muted,
                            ));
                            rows.push(ListRow::Text(
                                " > Stop / unload process".into(),
                                Tok::Accent,
                            ));
                        }
                        Target::Control(_) => {}
                        Target::Summary => {}
                    }
                } else {
                    let items = self.items();
                    self.cursor = self.cursor.min(items.len().saturating_sub(1));
                    let mut section = "";
                    for (index, item) in items.iter().enumerate() {
                        if section != item.section {
                            section = item.section;
                            rows.push(ListRow::Heading(section.to_owned()));
                        }
                        if index == self.cursor {
                            selected_row = rows.len();
                        }
                        rows.push(ListRow::Text(
                            format!(
                                " {} {}{}",
                                if index == self.cursor {
                                    ctx.theme.sym(crate::ui::theme::Sym::Pointer)
                                } else {
                                    " "
                                },
                                if item.active { "● " } else { "○ " },
                                item.label
                            ),
                            if item.active {
                                Tok::Success
                            } else if index == self.cursor {
                                Tok::Accent
                            } else {
                                Tok::Text
                            },
                        ));
                    }
                    if items.is_empty() {
                        rows.push(ListRow::Text(
                            "No live items in this section.".into(),
                            Tok::Muted,
                        ));
                    }
                }
                let header_limit = h.saturating_sub(5);
                let mut output: Vec<Line<'static>> = vec![tab_line(ctx.theme, &TABS, self.tab, w)];
                output.extend(header.into_iter().take(header_limit).map(|(line, token)| {
                    filled(
                        vec![Span::styled(
                            format!(" {}", text::one_line(&line)),
                            ctx.theme.fg(token),
                        )],
                        w,
                        Style::default(),
                    )
                }));
                let available = h.saturating_sub(output.len() + 2);
                let scroll = selected_row.saturating_sub(available.saturating_sub(1));
                for (i, row) in rows.into_iter().enumerate().skip(scroll).take(available) {
                    let fill = if self.menu.is_none() && i == selected_row {
                        selected_fill(ctx.theme)
                    } else {
                        Style::default()
                    };
                    output.push(match row {
                        ListRow::Heading(name) => section_heading(ctx.theme, &name, w),
                        ListRow::Text(line, token) => filled(
                            vec![Span::styled(
                                format!(" {}", text::one_line(&line)),
                                ctx.theme.fg(token),
                            )],
                            w,
                            fill,
                        ),
                    });
                }
                let notice = match (&self.confirm, &self.message) {
                    (Some(pending), _) => Some(filled(
                        vec![Span::styled(
                            format!(" {}", text::one_line(&pending.prompt)),
                            ctx.theme.fg(Tok::Warning),
                        )],
                        w,
                        Style::default(),
                    )),
                    (None, Some((message, true))) => Some(filled(
                        vec![Span::styled(
                            format!(" {} {message}", ctx.theme.sym(Sym::Cross2)),
                            ctx.theme.fg(Tok::Error),
                        )],
                        w,
                        Style::default(),
                    )),
                    (None, Some((message, false))) => Some(filled(
                        vec![Span::styled(
                            format!(" {} {message}", ctx.theme.sym(Sym::Check)),
                            ctx.theme.fg(Tok::Success),
                        )],
                        w,
                        Style::default(),
                    )),
                    (None, None) => None,
                };
                if let Some(line) = notice.filter(|_| output.len() < h) {
                    output.push(line);
                }
                if output.len() < h {
                    let confirm = ctx.keys.label(Action::SelectConfirm);
                    let cancel = ctx.keys.label(Action::SelectCancel);
                    let hints: Vec<(&str, &str)> = if self.confirm.is_some() {
                        vec![(confirm.as_str(), "confirm"), (cancel.as_str(), "cancel")]
                    } else {
                        vec![
                            ("←→/Tab", "section"),
                            ("↑↓", "select"),
                            (confirm.as_str(), "details"),
                            ("x", "remove/stop"),
                            // Esc steps back out of an item's menu, and closes from the list.
                            (
                                cancel.as_str(),
                                if self.menu.is_some() { "back" } else { "close" },
                            ),
                        ]
                    };
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(chrome::key_hint_spans(ctx.theme, &hints));
                    output.push(Line::from(fit(spans, w)));
                }
                output
            },
        );
    }
}
impl Default for ComputeOverlay {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
