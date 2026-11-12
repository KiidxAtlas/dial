//! Activity navigation reuses the existing panels and their refresh/action owners.
use super::{ActiveOverlay, Which};
use crate::ui::chrome::{self, Section};
use crate::ui::keys::{Action, Keymap};
use crate::ui::overlay::palette::{ChoiceItem, ChoiceSpec, PaletteOverlay};
use crate::ui::overlay::{DrawCtx, OverlayEvent, panel_keys};
use crate::ui::{text, theme::Theme};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum ActivityTab {
    Attention,
    Workers,
    Processes,
    Downloads,
    Compute,
    Schedule,
    Queue,
}

impl ActivityTab {
    /// The tabs that Tab/Shift+Tab (and the arrow keys) cycle through. Downloads is not one of
    /// them: it is the Models panel's Get models view, a different panel with its own keys, so it
    /// opens only when asked for (`/activity downloads` or an attention item), never by stepping
    /// sideways from Workers.
    pub(super) const ALL: [Self; 6] = [
        Self::Attention,
        Self::Workers,
        Self::Processes,
        Self::Compute,
        Self::Schedule,
        Self::Queue,
    ];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Attention => "attention",
            Self::Workers => "workers",
            Self::Processes => "processes",
            Self::Downloads => "downloads",
            Self::Compute => "compute",
            Self::Schedule => "schedule",
            Self::Queue => "queue",
        }
    }

    /// The tab's label in the title row.
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Attention => "Attention",
            Self::Workers => "Workers",
            Self::Processes => "Processes",
            Self::Downloads => "Downloads",
            Self::Compute => "Compute",
            Self::Schedule => "Schedule",
            Self::Queue => "Queue",
        }
    }

    pub(super) fn parse(args: &str) -> Option<Self> {
        match args {
            "" | "workers" | "jobs" | "agents" | "hub" => Some(Self::Workers),
            "attention" | "needs-attention" => Some(Self::Attention),
            "processes" | "bg" => Some(Self::Processes),
            "downloads" => Some(Self::Downloads),
            "compute" | "waits" => Some(Self::Compute),
            "schedule" => Some(Self::Schedule),
            "queue" => Some(Self::Queue),
            _ => None,
        }
    }

    pub(super) fn next(self, backwards: bool) -> Self {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0);
        Self::ALL[(index + if backwards { Self::ALL.len() - 1 } else { 1 }) % Self::ALL.len()]
    }

    pub(in crate::app) fn destination(self) -> Which {
        match self {
            Self::Workers => Which::Subagents,
            Self::Processes => Which::Jobs,
            Self::Downloads => Which::ModelDownloads,
            Self::Compute => Which::Compute,
            Self::Attention | Self::Schedule | Self::Queue => Which::Activity(self),
        }
    }
}

pub(in crate::app) struct ActivityOverlay {
    pub(super) tab: ActivityTab,
    pub(super) panel: Box<ActiveOverlay>,
    keys: panel_keys::PanelKeys,
    schedule: Option<std::sync::mpsc::Receiver<ChoiceSpec>>,
    attention: crate::ui::subagent_model::HubCache,
    attention_session: String,
    attention_refreshed: Option<std::time::Instant>,
}

impl ActivityOverlay {
    pub(in crate::app) fn new(tab: ActivityTab, panel: ActiveOverlay) -> Self {
        Self {
            tab,
            panel: Box::new(panel),
            keys: Default::default(),
            schedule: None,
            attention: Default::default(),
            attention_session: String::new(),
            attention_refreshed: None,
        }
    }

    pub(in crate::app) fn with_session(mut self, id: &str) -> Self {
        self.attention_session = id.to_owned();
        self
    }

    pub(in crate::app) fn attention_due(&self) -> bool {
        self.tab == ActivityTab::Attention
            && self
                .attention_refreshed
                .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(1))
    }

    // Producer-owned snapshots are passed together so refresh never acquires a second job owner.
    pub(in crate::app) fn refresh_attention(
        &mut self,
        layout: &crate::config::paths::Layout,
        runtime: &crate::runtime::Tree,
        live: Option<&crate::agent::LiveTaskControl>,
        jobs: &[crate::ui::overlay::jobs::JobRow],
        context: AttentionContext<'_>,
    ) {
        let workers = self
            .attention
            .snapshot(layout, &self.attention_session, runtime, live);
        if let ActiveOverlay::Palette(panel) = &mut *self.panel {
            panel.refresh_choices(attention_choices_with_context(
                &workers,
                jobs,
                context.status,
                context.todo,
                context.download_failure,
                context.setup_failure,
            ));
        }
        self.attention_refreshed = Some(std::time::Instant::now());
    }

    /// The schedule store has the same owner as `/schedule list`; disk reads stay off-thread.
    pub(in crate::app) fn load_schedule(&mut self, layout: crate::config::paths::Layout) {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut spec = schedule_choices();
            match crate::schedule::Store::new(&layout).and_then(|store| {
                let state = store.last_runs();
                Ok((store.load()?, state))
            }) {
                Ok((entries, state)) => {
                    for entry in entries {
                        spec.items.push(ChoiceItem {
                            value: format!("log {}", entry.id),
                            label: entry.id.clone(),
                            detail: crate::schedule::describe(
                                &entry,
                                state.get(&entry.id).copied(),
                            ),
                            current: false,
                        });
                    }
                }
                Err(error) => spec.summary = format!("Could not read schedules: {error:#}"),
            }
            let _ = send.send(spec);
        });
        self.schedule = Some(receive);
    }

    pub(super) fn tick(&mut self) {
        if let Some(spec) = self
            .schedule
            .as_ref()
            .and_then(|receive| receive.try_recv().ok())
        {
            *self.panel = ActiveOverlay::Palette(PaletteOverlay::choice(spec));
            self.schedule = None;
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        if self.keys.draw(frame, ctx, "Activity", "Ctrl+Tab  tabs inside the selected panel\nAttached terminals keep Tab; detach to switch activity tabs.", true, false) {
            return;
        }
        self.panel.draw(frame, ctx);
        // The Activity title row replaces the reused panel's own title row, on the same surface.
        let area = frame.area();
        if area.height > 0 {
            frame.render_widget(
                Paragraph::new(title_row(
                    ctx.theme,
                    ctx.keys,
                    self.tab,
                    usize::from(area.width),
                )),
                Rect { height: 1, ..area },
            );
        }
    }

    pub(super) fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let attached =
            matches!(&*self.panel, ActiveOverlay::Jobs(panel) if panel.attached().is_some());
        if !attached && self.keys.handle_key(event, true) {
            return OverlayEvent::None;
        }
        if let Event::Key(key) = event
            && key.kind != KeyEventKind::Release
            && !attached
        {
            // Ctrl+Tab preserves access to tab bars inside reused panels.
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            {
                let mut key = *key;
                key.modifiers.remove(KeyModifiers::CONTROL);
                return self.panel.handle(&Event::Key(key), keys);
            }
            let previous = panel_keys::matches(keys, Action::TabPrev, key);
            if previous || panel_keys::matches(keys, Action::TabNext, key) {
                return OverlayEvent::Action(format!(
                    "open:activity:{}",
                    self.tab.next(previous).name()
                ));
            }
        }
        self.panel.handle(event, keys)
    }
}

/// The Activity title row, `width` cells on the panel surface: ` Activity   Attention  Workers …`
/// with the active tab in the shared tab style, and the key that switches tabs at the right edge
/// when it fits. A row too narrow for every tab names only the active one and its position, and
/// the narrowest rows drop the panel name before the active tab.
pub(super) fn title_row(
    theme: &Theme,
    keys: &Keymap,
    active: ActivityTab,
    width: usize,
) -> Line<'static> {
    let spans_width = |spans: &[Span<'static>]| -> usize {
        spans.iter().map(|span| text::width(&span.content)).sum()
    };
    let mut spans = vec![Span::raw(" ")];
    spans.extend(chrome::fig_label(theme, Section::Dial, "Activity"));
    spans.push(Span::raw("   "));
    let lead = spans.clone();
    for (index, tab) in ActivityTab::ALL.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            tab.title(),
            chrome::tab_style(theme, tab == active),
        ));
    }
    if spans_width(&spans) + 1 > width {
        let position = ActivityTab::ALL
            .iter()
            .position(|tab| *tab == active)
            .map(|index| format!(" {}/{}", index + 1, ActivityTab::ALL.len()))
            .unwrap_or_default();
        spans = lead;
        spans.push(Span::styled(active.title(), chrome::tab_style(theme, true)));
        spans.push(Span::styled(
            position,
            theme.fg(crate::ui::theme::Tok::Muted),
        ));
        // Where even that is too wide, the active tab outranks the panel name and its gap.
        let name_end = 1 + chrome::fig_label(theme, Section::Dial, "Activity").len() + 1;
        if spans_width(&spans) > width {
            spans.drain(1..name_end);
        }
    }
    let switch = format!(
        "{}/{}",
        keys.label(Action::TabPrev),
        keys.label(Action::TabNext)
    );
    let mut hint = chrome::key_hint_spans(theme, &[(switch.as_str(), "tabs")]);
    hint.push(Span::raw(" "));
    let used = spans_width(&spans);
    let hint_w = spans_width(&hint);
    if used + hint_w + 2 <= width {
        spans.push(Span::raw(" ".repeat(width - used - hint_w)));
        spans.extend(hint);
    } else {
        spans = crate::ui::markdown::truncate_spans(
            spans,
            width,
            theme.sym(crate::ui::theme::Sym::Ellipsis),
        );
        let used = spans_width(&spans);
        spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    }
    Line::from(chrome::on_surface(theme, spans))
}

pub(in crate::app) fn schedule_choices() -> ChoiceSpec {
    ChoiceSpec {
        title: "Schedule".into(),
        command: "schedule".into(),
        summary: "Recurring and file-triggered runs. Enter opens a schedule's log. Add with /schedule add; remove with /schedule remove ID.".into(),
        items: vec![
            ChoiceItem { value: "list".into(), label: "List schedules".into(), detail: "Show the current schedule and last-run status".into(), current: false },
            ChoiceItem { value: "run".into(), label: "Run due schedules once".into(), detail: "Uses the existing background schedule runner".into(), current: false },
        ],
        ..Default::default()
    }
}

pub(in crate::app) fn queue_choices(items: &[(String, bool)]) -> ChoiceSpec {
    ChoiceSpec {
        title: "Queued messages".into(),
        command: "queue".into(),
        verb: "edit",
        remove: Some("remove"),
        summary: "Enter edits one message; Delete twice removes only that message. An existing composer draft is kept. Queue changes made elsewhere appear when reopened.".into(),
        items: items.iter().enumerate().map(|(index, (message, follow_up))| ChoiceItem {
            value: (index + 1).to_string(),
            label: format!("{} {}", index + 1, if *follow_up { "follow-up" } else { "steering" }),
            detail: crate::ui::text::one_line(message),
            current: false,
        }).collect(),
        ..Default::default()
    }
}

#[derive(Default)]
pub(in crate::app) struct AttentionContext<'a> {
    pub status: Option<&'a crate::ui::status::StatusData>,
    pub todo: Option<&'a crate::agent::workflow::todo::TodoList>,
    pub download_failure: Option<&'a crate::ui::overlay::models::DownloadFailure>,
    pub setup_failure: Option<&'a super::auth::SetupFailure>,
}

/// Initial aggregate before the shell's observed context arrives on refresh.
pub(in crate::app) fn attention_choices(
    workers: &crate::ui::subagent_model::HubSnapshot,
    jobs: &[crate::ui::overlay::jobs::JobRow],
) -> ChoiceSpec {
    attention_choices_with_context(workers, jobs, None, None, None, None)
}

/// Read-only projection of existing producer snapshots; absence is not success or failure.
pub(in crate::app) fn attention_choices_with_context(
    workers: &crate::ui::subagent_model::HubSnapshot,
    jobs: &[crate::ui::overlay::jobs::JobRow],
    status: Option<&crate::ui::status::StatusData>,
    todo: Option<&crate::agent::workflow::todo::TodoList>,
    download_failure: Option<&crate::ui::overlay::models::DownloadFailure>,
    setup_failure: Option<&super::auth::SetupFailure>,
) -> ChoiceSpec {
    let mut items: Vec<ChoiceItem> = workers
        .agents
        .iter()
        .filter(|worker| worker.needs_attention())
        .map(|worker| ChoiceItem {
            value: "activity workers".into(),
            label: format!("Worker · {} · {}", worker.name, worker.status.label()),
            detail: worker
                .config
                .as_ref()
                .and_then(|config| config.failure.clone())
                .or_else(|| worker.current.clone())
                .unwrap_or_else(|| worker.task.clone()),
            current: false,
        })
        .collect();
    items.extend(
        jobs.iter()
            .filter(|job| job.needs_attention())
            .map(|job| ChoiceItem {
                value: "activity processes".into(),
                label: format!("Process · {} · {}", job.name, job.status),
                detail: job.command.clone(),
                current: false,
            }),
    );
    if let Some(todo) = todo {
        items.extend(
            todo.tasks()
                .iter()
                .enumerate()
                .filter(|(_, task)| task.status == crate::agent::workflow::todo::Status::Blocked)
                .map(|(index, task)| ChoiceItem {
                    value: format!("todo show {}", index + 1),
                    label: format!("Todo · {} · blocked", index + 1),
                    detail: task.label(index + 1),
                    current: false,
                }),
        );
    }
    if status.is_some_and(|status| status.phase == crate::ui::status::Phase::WaitingCompute) {
        items.push(ChoiceItem {
            value: "activity compute".into(),
            label: "Main answer · waiting for local compute".into(),
            detail: "Observed compute wait. Inspect the Compute owner before choosing any action; opening it does not release compute or retry the answer.".into(),
            current: false,
        });
    }
    if let Some(failure) = download_failure {
        items.push(ChoiceItem {
            value: "activity downloads".into(),
            label: format!("Download · {} · last failure", failure.repo),
            detail: failure.detail.clone(),
            current: false,
        });
    }
    if let Some(failure) = setup_failure {
        items.push(ChoiceItem {
            value: "settings".into(),
            label: format!("Setup · {} · last failure", failure.label),
            detail: failure.detail.clone(),
            current: false,
        });
    }
    ChoiceSpec {
        title: "Attention needed".into(),
        summary: "Observed worker requests/failures, non-success process exits, blocked todos, main compute waits and historical download/setup failures. Enter opens the existing owner for inspection, never retries work. Main questions/approvals stay in their modal input owner. Missing snapshots do not mean all clear.".into(),
        items,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
