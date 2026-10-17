//! Overlay state: the active overlay, its destinations, and the streaming job that feeds it.

use crate::app::overlay::auth::AuthJob;
use crate::config::types::Config;
use crate::llm::model_config::ModelSelection;
use crate::ui::keys::Keymap;
use crate::ui::overlay::compute::ComputeOverlay;
use crate::ui::overlay::models::ModelOverlay;
use crate::ui::overlay::palette::{ChoiceSpec, PaletteOverlay};
use crate::ui::overlay::providers::ProviderOverlay;
use crate::ui::overlay::settings::SettingsOverlay;
use crate::ui::overlay::shortcuts::ShortcutOverlay;
use crate::ui::overlay::{DrawCtx, OverlayEvent};
use crossterm::event::Event;
use std::time::Instant;

pub(in crate::app) mod apply;
pub(in crate::app) mod auth;
pub(in crate::app) mod compute;
pub(in crate::app) mod env;
pub(in crate::app) mod run;
pub(in crate::app) mod settings;

/// A background cycle or training run this shell started. Its finish is noted once.
pub(super) struct ImproveWatch {
    pub(super) state_dir: std::path::PathBuf,
    pub(super) started: u64,
    pub(super) checked: Instant,
}

pub(super) enum Which {
    Palette,
    Choice(Box<ChoiceSpec>),
    Compute,
    Improve,
    Jobs,
    Subagents,
    ModelDownloads,
    SubagentsActivity,
    SubagentViewer {
        id: String,
        entry: Option<usize>,
        from_hub: bool,
    },
    Extensions,
    Settings,
    LearningSettings,
    LearningModels,
    ModelSettings,
    ContextDefaults,
    Models,
    ModelRoles,
    Providers,
    Shortcuts,
    Git {
        root: std::path::PathBuf,
        revision: String,
    },
}

/// `subagent-open:<id>[#<entry>]` from the hub: the worker's transcript, scrolled to the entry.
fn subagent_viewer_destination(action: &str) -> Option<Which> {
    let rest = action.strip_prefix("subagent-open:")?;
    let (id, entry) = match rest.split_once('#') {
        Some((id, entry)) => (id, entry.parse().ok()),
        None => (rest, None),
    };
    Some(Which::SubagentViewer {
        id: id.to_owned(),
        entry,
        from_hub: true,
    })
}

/// Overlay navigation uses the same destinations while idle and during work.
pub(super) fn overlay_destination(action: &str) -> Option<Which> {
    match action {
        "open:models" => Some(Which::Models),
        "open:roles" => Some(Which::ModelRoles),
        "open:providers" => Some(Which::Providers),
        "open:settings" => Some(Which::Settings),
        "open:model-settings" => Some(Which::ModelSettings),
        "open:context-defaults" => Some(Which::ContextDefaults),
        "open:shortcuts" => Some(Which::Shortcuts),
        "open:compute" => Some(Which::Compute),
        "open:improve" => Some(Which::Improve),
        "open:improve-settings" => Some(Which::LearningSettings),
        "open:improve-models" => Some(Which::LearningModels),
        "open:extensions" => Some(Which::Extensions),
        "subagent-hub" => Some(Which::Subagents),
        "subagent-hub:activity" => Some(Which::SubagentsActivity),
        _ => subagent_viewer_destination(action),
    }
}

/// Work navigation is identical while idle and while a turn is running.
pub(super) fn work_panel(name: &str, args: &str) -> Option<Which> {
    if !args.is_empty() {
        return None;
    }
    match name {
        "workers" | "agents" | "jobs" | "hub" => Some(Which::Subagents),
        "processes" | "bg" => Some(Which::Jobs),
        "compute" => Some(Which::Compute),
        _ => None,
    }
}

#[derive(Default)]
pub(super) struct OverlayOutcome {
    pub(super) verify_lora: Option<ModelSelection>,
    /// The effective config after the last successful change made inside the overlay.
    pub(super) latest: Option<Config>,
    /// A request from inside the overlay to open another one (`open:models`, `open:providers`, …).
    pub(super) next: Option<String>,
}

pub(super) enum ActiveOverlay {
    Palette(PaletteOverlay),
    Compute(ComputeOverlay),
    Improve(Box<crate::ui::overlay::improve::ImproveOverlay>),
    Extensions(Box<crate::ui::overlay::extensions::ExtensionsOverlay>),
    Settings(Box<SettingsOverlay>),
    ModelSettings(Box<crate::ui::overlay::model_settings::ModelSettingsOverlay>),
    Models(Box<ModelOverlay>),
    Providers(ProviderOverlay),
    Shortcuts(ShortcutOverlay),
    Git(Box<crate::ui::overlay::git::GitOverlay>),
    Subagents(Box<crate::ui::subagent_hub::SubagentHub>),
    SubagentViewer(Box<crate::ui::overlay::subagent_viewer::SubagentViewer>),
    Jobs(Box<crate::ui::overlay::jobs::JobsOverlay>),
}

impl ActiveOverlay {
    pub(super) fn draw(&mut self, frame: &mut ratatui::Frame, ctx: &DrawCtx) {
        match self {
            Self::Palette(o) => o.draw(frame, ctx),
            Self::Settings(o) => o.draw(frame, ctx),
            Self::ModelSettings(o) => o.draw(frame, ctx),
            Self::Compute(o) => o.draw(frame, ctx),
            Self::Improve(o) => o.draw(frame, ctx),
            Self::Extensions(o) => o.draw(frame, ctx),
            Self::Models(o) => o.draw(frame, ctx),
            Self::Providers(o) => o.draw(frame, ctx),
            Self::Shortcuts(o) => o.draw(frame, ctx),
            Self::Git(o) => o.draw(frame, ctx),
            Self::Subagents(o) => o.draw(frame, ctx),
            Self::SubagentViewer(o) => o.draw(frame, ctx),
            Self::Jobs(o) => o.draw(frame, ctx),
        }
    }

    /// Native image placements for the frame just drawn. Only the git overlay previews images.
    fn native_placements(&self) -> Vec<crate::ui::images::Placement> {
        match self {
            Self::Git(o) => o.native_placements(),
            _ => Vec::new(),
        }
    }

    pub(super) fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if let Event::Mouse(mouse) = event
            && !matches!(self, Self::Git(_) | Self::SubagentViewer(_))
        {
            return self.mouse(*mouse, keys);
        }
        match self {
            Self::Palette(o) => o.handle(event, keys),
            Self::Settings(o) => o.handle(event, keys),
            Self::ModelSettings(o) => o.event(event, keys),
            Self::Compute(o) => o.handle(event, keys),
            Self::Improve(o) => o.handle(event, keys),
            Self::Extensions(o) => o.handle(event, keys),
            Self::Models(o) => o.handle(event, keys),
            Self::Providers(o) => o.handle(event, keys),
            Self::Shortcuts(o) => o.handle(event, keys),
            Self::Subagents(o) => o.handle(event, keys),
            Self::Git(o) => o.handle(event, keys),
            Self::SubagentViewer(o) => o.handle(event, keys, std::time::Instant::now()),
            Self::Jobs(o) => o.handle(event, keys),
        }
    }

    /// Mouse input for panels without their own mouse handling (git has one). A left click selects
    /// the row drawn there in the palette and jobs; the wheel moves the selection like the arrow
    /// keys. Jobs ignore the wheel while attached, because arrow keys then belong to the job.
    fn mouse(&mut self, mouse: crossterm::event::MouseEvent, keys: &Keymap) -> OverlayEvent {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
        let arrow = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                return match self {
                    Self::Palette(o) => o.click(mouse.row),
                    Self::Jobs(o) => o.click(mouse.row),
                    Self::Subagents(o) => {
                        o.click(mouse.column, mouse.row, std::time::Instant::now())
                    }
                    _ => OverlayEvent::None,
                };
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = matches!(mouse.kind, MouseEventKind::ScrollDown);
                if let Self::Subagents(o) = self
                    && o.wheel_over_inspector(mouse.column, mouse.row, down)
                {
                    return OverlayEvent::None;
                }
                if down { KeyCode::Down } else { KeyCode::Up }
            }
            _ => return OverlayEvent::None,
        };
        if matches!(self, Self::Jobs(o) if o.attached().is_some()) {
            return OverlayEvent::None;
        }
        self.handle(&Event::Key(KeyEvent::new(arrow, KeyModifiers::NONE)), keys)
    }

    pub(super) fn banner(&mut self, message: String, is_error: bool) {
        match self {
            Self::Models(o) => o.set_status(message, is_error),
            Self::Compute(o) => o.banner(message, is_error),
            Self::Improve(o) => o.banner(message, is_error),
            Self::Extensions(o) => o.banner(message),
            Self::Providers(o) => o.set_status(message, is_error),
            Self::ModelSettings(o) => o.banner(message, is_error),
            Self::Subagents(o) => o.banner(message, is_error),
            Self::SubagentViewer(o) => o.set_notice(message),
            Self::Jobs(o) => o.banner(message, is_error),
            Self::Settings(o) => o.banner(message, is_error),
            Self::Palette(_) | Self::Shortcuts(_) | Self::Git(_) => {}
        }
    }
}

/// Starts `work` as the background job in `slot`. Progress lines it reports reach the transcript as
/// the main loop drains them. Returns false, leaving `slot` alone, when another job is still running.
pub(crate) fn start_streaming_job(
    slot: &mut Option<AuthJob>,
    label: &'static str,
    work: impl FnOnce(
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        &mut dyn FnMut(&str),
    ) -> anyhow::Result<String>
    + Send
    + 'static,
) -> bool {
    if slot.is_some() {
        return false;
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let (line_sender, line_receiver) = std::sync::mpsc::channel::<String>();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    std::thread::spawn(move || {
        let mut progress = |line: &str| {
            let _ = line_sender.send(line.to_owned());
        };
        let _ = sender.send(work(worker_cancel, &mut progress));
    });
    *slot = Some(AuthJob {
        label,
        receiver,
        cancel,
        lines: Some(line_receiver),
        model: None,
    });
    true
}
