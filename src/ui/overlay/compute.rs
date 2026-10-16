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
    style::{Modifier, Style},
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

/// The tab strip: the active tab is amber, bold and underlined; the others are secondary. ASCII keeps `[tab]`.
pub(super) fn tab_line(theme: &Theme, tabs: &[&str], active: usize, width: usize) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(if i == active {
            let name = if theme.ascii() {
                format!("[{tab}]")
            } else {
                (*tab).to_owned()
            };
            Span::styled(
                name,
                theme
                    .fg(Tok::Accent)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else {
            Span::styled((*tab).to_owned(), theme.fg(Tok::Muted))
        });
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
                    let hints: &[(&str, &str)] = if self.confirm.is_some() {
                        &[("Enter", "confirm"), ("Esc", "keep running")]
                    } else {
                        &[
                            ("←→/Tab", "section"),
                            ("↑↓", "select"),
                            ("Enter", "details"),
                            ("x", "remove/stop"),
                            ("Esc", "back"),
                        ]
                    };
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(chrome::key_hint_spans(ctx.theme, hints));
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
mod tests {
    use super::super::settings::testkit::*;
    use super::*;
    use crossterm::event::KeyCode;
    fn fixture() -> Value {
        json!({"busy":true,"owner":{"kind":"background","pid":42,"model":"local-model"},
        "jobs":[{"state":"/job","model":"local","project":"/project","active":true,"progress":{"stage":"training"},"training":{"phase":"sft","iteration":4,"total":32,"peak_memory_gb":1.25}}],
        "processes":[{"pid":70,"identity":"model70","kind":"models","model":"big-model","phase":"loading","memory_bytes":36507222016_u64,"cpu_percent":80.0},
        {"pid":71,"identity":"service71","kind":"services","model":"Laya","phase":"running"}, {"pid":72,"identity":"tool72","kind":"tools","model":"python","phase":"running"}]})
    }
    #[test]
    fn tabs_filter_sections_and_x_targets_the_selected_process() {
        let keys = Keymap::defaults();
        let mut overlay = ComputeOverlay::new();
        overlay.update(fixture());
        assert_eq!(overlay.items().len(), 7);
        overlay.handle(&key(KeyCode::Right), &keys);
        assert_eq!(overlay.tab, 1);
        assert_eq!(overlay.items().len(), 1);
        assert!(matches!(
            overlay.handle(&key(KeyCode::Char('x')), &keys),
            OverlayEvent::None
        ));
        let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
            panic!("missing stop");
        };
        let payload: Value =
            serde_json::from_str(action.strip_prefix("compute:").unwrap()).unwrap();
        assert_eq!(payload["action"], "stop_process");
        assert_eq!(payload["pid"], 70);
        assert_eq!(payload["identity"], "model70");
        overlay.handle(&key(KeyCode::Right), &keys);
        assert_eq!(overlay.items().len(), 4);
        overlay.cursor = 3;
        assert!(matches!(
            overlay.handle(&key(KeyCode::Char('x')), &keys),
            OverlayEvent::None
        ));
        let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
            panic!("missing remove");
        };
        assert!(action.contains("remove") && action.contains("/job"));
        overlay.handle(&key(KeyCode::Right), &keys);
        assert_eq!(overlay.items()[0].section, "Services");
        overlay.handle(&key(KeyCode::Right), &keys);
        assert_eq!(overlay.items()[0].section, "Tools & subprocesses");
        overlay.handle(&key(KeyCode::Right), &keys);
        assert_eq!(overlay.tab, 0);
    }
    #[test]
    fn menu_keeps_identity_when_the_inventory_changes() {
        let keys = Keymap::defaults();
        let mut overlay = ComputeOverlay::new();
        overlay.update(fixture());
        overlay.tab = 2;
        overlay.cursor = 3;
        overlay.handle(&key(KeyCode::Enter), &keys);
        overlay.cursor = 1;
        overlay.update(json!({"jobs":[{"state":"/different/job","model":"changed"}]}));
        let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
            panic!("missing stop");
        };
        assert!(
            action.contains("stop")
                && action.contains("/job")
                && !action.contains("/different/job")
        );
    }
    #[test]
    fn refresh_keeps_process_selection_and_repeated_x_does_not_stop_another_item() {
        let keys = Keymap::defaults();
        let mut overlay = ComputeOverlay::new();
        let mut status = fixture();
        status["processes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"pid":80,"identity":"model80","kind":"models","model":"second"}));
        overlay.update(status.clone());
        overlay.tab = 1;
        overlay.cursor = 1;
        status["processes"].as_array_mut().unwrap().reverse();
        overlay.update(status);
        assert!(matches!(
            overlay.handle(&key(KeyCode::Char('x')), &keys),
            OverlayEvent::None
        ));
        let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
            panic!("missing stop");
        };
        assert!(action.contains("model80"));
        let repeat = Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Repeat,
        ));
        assert!(matches!(overlay.handle(&repeat, &keys), OverlayEvent::None));
    }
    #[test]
    fn active_rows_are_green_even_when_selected_and_show_large_memory() {
        let rig = Rig::new();
        let mut overlay = ComputeOverlay::new();
        overlay.update(fixture());
        overlay.cursor = 3;
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let buffer = render(150, 35, |frame| overlay.draw(frame, &ctx));
        let shown_rows = rows(&buffer);
        let shown = shown_rows.join("\n");
        for expected in [
            "All",
            "Models",
            "Background",
            "Services",
            "Tools",
            "36.51 GB",
            "loading",
            "Owner: background",
            "x remove/stop",
        ] {
            assert!(shown.contains(expected), "missing {expected}: {shown}");
        }
        let y = shown_rows
            .iter()
            .position(|row| row.contains("big-model"))
            .unwrap() as u16;
        // Use the label cell rather than its byte offset (the row contains Unicode glyphs).
        let cell = (0..buffer.area.width)
            .find(|&x| buffer[(x, y)].symbol() == "b")
            .unwrap();
        assert_eq!(buffer[(cell, y)].fg, rig.theme.fg(Tok::Success).fg.unwrap());
    }
    #[test]
    fn memory_totals_include_all_process_footprint_once_in_every_tab() {
        let rig = Rig::new();
        let mut overlay = ComputeOverlay::new();
        overlay.update(json!({"session_memory_bytes":100_000_000_u64,
            "processes":[
                {"pid":70,"identity":"model70","kind":"models","model":"model",
                    "memory_bytes":2_000_000_000_u64,"memory_metric":"footprint"},
                {"pid":71,"identity":"laya71","kind":"services","model":"Laya",
                    "memory_bytes":38_500_000_000_u64,
                    "resident_memory_bytes":150_000_000_u64,"memory_metric":"footprint"}
            ],"jobs":[{"state":"/job","model":"model","active":true}]}));
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        for tab in 0..TABS.len() {
            overlay.tab = tab;
            let shown = rows(&render(150, 35, |frame| overlay.draw(frame, &ctx))).join("\n");
            assert!(shown.contains("Memory: Dial 40.60 GB"), "{shown}");
            assert!(shown.contains("compressed/GPU allocations"));
        }
        overlay.tab = 3;
        overlay.handle(&key(KeyCode::Enter), &rig.keys);
        let shown = rows(&render(150, 35, |frame| overlay.draw(frame, &ctx))).join("\n");
        assert!(shown.contains("Memory footprint: 38.50 GB · Resident (RSS): 0.150 GB"));
    }
    #[test]
    fn selected_job_shows_training_details() {
        let rig = Rig::new();
        let mut overlay = ComputeOverlay::new();
        overlay.update(fixture());
        overlay.menu = Some(Target::Job("/job".into()));
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let shown = rows(&render(120, 30, |frame| overlay.draw(frame, &ctx))).join("\n");
        assert!(shown.contains("Training: sft 4/32"));
        assert!(shown.contains("Peak MLX memory: 1.25 GB"));
        assert!(shown.contains("Stop job now"));
    }
    #[test]
    fn small_panels_render_without_panics() {
        let rig = Rig::new();
        let mut overlay = ComputeOverlay::new();
        overlay.update(fixture());
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

    #[test]
    fn esc_from_an_item_menu_restores_the_list_cursor_and_stop_now_asks_first() {
        let keys = Keymap::defaults();
        let mut overlay = ComputeOverlay::new();
        overlay.update(fixture());
        overlay.tab = 2;
        overlay.cursor = 3;
        overlay.handle(&key(KeyCode::Enter), &keys);
        overlay.handle(&key(KeyCode::Esc), &keys);
        assert_eq!(overlay.cursor, 3, "Esc returns to the item it was opened from");
        overlay.handle(&key(KeyCode::Enter), &keys);
        overlay.handle(&key(KeyCode::Down), &keys);
        overlay.handle(&key(KeyCode::Down), &keys);
        assert!(matches!(overlay.handle(&key(KeyCode::Enter), &keys), OverlayEvent::None));
        assert!(overlay.confirm.is_some(), "stop now waits for Enter");
        assert!(matches!(overlay.handle(&key(KeyCode::Esc), &keys), OverlayEvent::None));
        assert!(overlay.confirm.is_none(), "Esc keeps the job running");
    }

    #[test]
    fn banners_carry_success_or_error_tone() {
        let rig = Rig::new();
        let mut overlay = ComputeOverlay::new();
        overlay.update(fixture());
        overlay.banner("stop failed".into(), true);
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let buffer = render(100, 30, |frame| overlay.draw(frame, &ctx));
        assert_eq!(
            style_of(&buffer, "stop failed").and_then(|style| style.fg),
            rig.theme.fg(Tok::Error).fg
        );
    }
}
