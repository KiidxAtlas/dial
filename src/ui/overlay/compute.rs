//! Shared local compute: live ownership, background jobs and direct operator controls.
use super::{
    DrawCtx, OverlayEvent,
    settings::{filled, panel, title_line},
};
use crate::ui::{
    keys::{Action, Keymap},
    text,
    theme::{Sym, Tok},
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    style::Style,
    text::{Line, Span},
};
use serde_json::{Value, json};

fn model_label(value: &Value) -> &str {
    let model = value.as_str().unwrap_or("unknown");
    if model.starts_with('/') {
        model.rsplit('/').next().unwrap_or(model)
    } else {
        model
    }
}

pub struct ComputeOverlay {
    snapshot: Value,
    cursor: usize,
    menu: Option<String>,
    message: String,
}
impl ComputeOverlay {
    pub fn new() -> Self {
        Self {
            snapshot: Value::Null,
            cursor: 0,
            menu: None,
            message: "Loading live compute status…".into(),
        }
    }
    pub fn update(&mut self, snapshot: Value) {
        self.snapshot = snapshot;
        if self.message.starts_with("Loading") {
            self.message.clear();
        }
    }
    pub fn banner(&mut self, message: String) {
        self.message = text::one_line(&message);
    }
    fn jobs(&self) -> &[Value] {
        self.snapshot["jobs"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    fn event(&self, action: &str, state: Option<&str>) -> OverlayEvent {
        OverlayEvent::Action(format!(
            "compute:{}",
            json!({"action":action,"state":state})
        ))
    }
    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if key.kind == crossterm::event::KeyEventKind::Release {
            return OverlayEvent::None;
        }
        if key.code == KeyCode::Esc {
            if self.menu.take().is_some() {
                return OverlayEvent::None;
            }
            return OverlayEvent::Close;
        }
        let count = if self.menu.is_some() {
            3
        } else {
            self.jobs().len() + 3
        };
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(count.saturating_sub(1));
        } else if keys.matches(Action::SelectConfirm, key) {
            if let Some(state) = &self.menu {
                return self.event(
                    ["start", "stop", "stop_now"][self.cursor.min(2)],
                    Some(state),
                );
            }
            return match self.cursor {
                0 => self.event("pause", None),
                1 => self.event("resume", None),
                2 => self.event("start_current", None),
                index => {
                    self.menu = self
                        .jobs()
                        .get(index - 3)
                        .and_then(|job| job["state"].as_str())
                        .map(str::to_owned);
                    self.cursor = 0;
                    OverlayEvent::None
                }
            };
        }
        OverlayEvent::None
    }
    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let title = title_line(
            ctx.theme,
            Sym::IconSettings,
            "Compute & background work",
            "all projects",
        );
        panel(frame, ctx.theme, title, |width, height| {
            let w = usize::from(width);
            let h = usize::from(height);
            let owner = &self.snapshot["owner"];
            let mut lines = vec![if self.snapshot.is_null() {
                "Reading live compute status…".into()
            } else {
                format!(
                    "Local compute: {} · background {}",
                    if self.snapshot["busy"] == true {
                        "occupied"
                    } else {
                        "available"
                    },
                    if self.snapshot["background_paused"] == true {
                        "paused"
                    } else {
                        "enabled"
                    }
                )
            }];
            lines.push(if self.snapshot["busy"] != true {
                "Coding requests have priority over background work.".into()
            } else if owner.is_object() {
                format!(
                    "Owner: {} · PID {} · {}",
                    owner["kind"].as_str().unwrap_or("unknown"),
                    owner["pid"],
                    model_label(&owner["model"])
                )
            } else {
                "Owner: older/unregistered Dial process; inspect running jobs below.".into()
            });
            if let Some(project) = owner["project"].as_str() {
                lines.push(format!("Project: {project}"));
            }
            let rows = if let Some(state) = &self.menu {
                let job = self
                    .jobs()
                    .iter()
                    .find(|job| job["state"].as_str() == Some(state));
                lines.push(format!(
                    "Job: {}",
                    job.and_then(|job| job["model"].as_str())
                        .unwrap_or("no longer available")
                ));
                if let Some(job) = job {
                    lines.push(format!(
                        "Project: {}",
                        job["project"].as_str().unwrap_or("unknown")
                    ));
                    if let Some(stage) = job["progress"]["stage"].as_str() {
                        lines.push(format!("Phase: {stage}"));
                    }
                    if let (Some(iteration), Some(total)) = (
                        job["training"]["iteration"].as_u64(),
                        job["training"]["total"].as_u64(),
                    ) {
                        lines.push(format!(
                            "Training: {} {iteration}/{total}",
                            job["training"]["phase"].as_str().unwrap_or("step")
                        ));
                    }
                    if let Some(memory) = job["training"]["peak_memory_gb"].as_f64() {
                        lines.push(format!("Peak MLX memory: {memory:.2} GB"));
                    }
                }
                vec![
                    "Start / resume cycle".into(),
                    "Stop job safely".into(),
                    "Stop job now (discards unfinished cycle)".into(),
                ]
            } else {
                let mut rows = vec![
                    "Pause all background work".into(),
                    "Resume background work".into(),
                    "Start improvement for current model".into(),
                ];
                rows.extend(self.jobs().iter().map(|job| {
                    format!(
                        "{} · PID {} · {} · {}",
                        text::clip(model_label(&job["model"]), (w / 3).max(10)),
                        job["scheduler"]["pid"],
                        if job["control_busy"] == true {
                            job["progress"]["stage"]
                                .as_str()
                                .unwrap_or("running / waiting")
                        } else if job["stop_requested"] == true {
                            "stopped"
                        } else {
                            "idle"
                        },
                        job["project"].as_str().unwrap_or("")
                    )
                }));
                rows
            };
            self.cursor = self.cursor.min(rows.len().saturating_sub(1));
            let mut output: Vec<Line<'static>> = lines
                .into_iter()
                .take(h.saturating_sub(4))
                .map(|line| {
                    filled(
                        vec![Span::styled(
                            text::one_line(&line),
                            ctx.theme.fg(Tok::Muted),
                        )],
                        w,
                        Style::default(),
                    )
                })
                .collect();
            let available = h.saturating_sub(output.len() + 2);
            let scroll = self.cursor.saturating_sub(available.saturating_sub(1));
            for (index, row) in rows.iter().enumerate().skip(scroll).take(available) {
                output.push(filled(
                    vec![Span::styled(
                        format!(" {} {row}", if index == self.cursor { ">" } else { " " }),
                        ctx.theme.fg(if index == self.cursor {
                            Tok::Accent
                        } else {
                            Tok::Text
                        }),
                    )],
                    w,
                    Style::default(),
                ));
            }
            if output.len() < h {
                output.push(filled(
                    vec![Span::styled(self.message.clone(), ctx.theme.fg(Tok::Muted))],
                    w,
                    Style::default(),
                ));
            }
            if output.len() < h {
                output.push(filled(
                    vec![Span::styled(
                        "↑↓ choose · Enter act · Esc back/close",
                        ctx.theme.fg(Tok::Dim),
                    )],
                    w,
                    Style::default(),
                ));
            }
            output
        });
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
    #[test]
    fn menu_emits_explicit_controls_and_job_identity() {
        let keys = Keymap::defaults();
        let mut overlay = ComputeOverlay::new();
        overlay.update(json!({"jobs":[{"state":"/private/job","model":"test"}]}));
        let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
            panic!("missing pause action")
        };
        assert!(action.contains("pause"));
        overlay.cursor = 3;
        overlay.handle(&key(KeyCode::Enter), &keys);
        overlay.cursor = 1;
        overlay.update(json!({"jobs":[{"state":"/different/job","model":"changed"}]}));
        let OverlayEvent::Action(action) = overlay.handle(&key(KeyCode::Enter), &keys) else {
            panic!("missing stop action")
        };
        assert!(action.contains("stop") && action.contains("/private/job"));
    }
    #[test]
    fn small_panels_render_without_panics() {
        let rig = Rig::new();
        let mut overlay = ComputeOverlay::new();
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
    fn owners_and_controls_are_visible_in_the_panel() {
        let rig = Rig::new();
        let mut overlay = ComputeOverlay::new();
        overlay.update(json!({"busy":true,"owner":{"kind":"background","pid":42,"model":"local-model","project":"/project"},"jobs":[]}));
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let shown = rows(&render(120, 30, |frame| overlay.draw(frame, &ctx))).join("\n");
        for expected in [
            "Compute & background work",
            "Owner: background",
            "PID 42",
            "Pause all background",
            "Resume background",
            "Start improvement",
        ] {
            assert!(shown.contains(expected), "missing {expected}: {shown}");
        }
    }

    #[test]
    fn selected_job_shows_training_progress_and_memory() {
        let rig = Rig::new();
        let mut overlay = ComputeOverlay::new();
        overlay.update(json!({"jobs":[{"state":"/job","model":"local","progress":{"stage":"training"},"training":{"phase":"sft","iteration":4,"total":32,"peak_memory_gb":1.25}}]}));
        overlay.menu = Some("/job".into());
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
}
