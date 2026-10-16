//! `/models local`: a read-only dashboard of downloaded MLX and GGUF models. The snapshot is read once
//! when the panel opens (`cli::local_models::collect`, the same data as `dial models status --json`).
//! Nothing here loads a model, starts Python, or uses the network; unavailable values show as unknown.
use super::{
    DrawCtx, OverlayEvent,
    settings::{filled, fit, panel, selected_fill},
};
use crate::cli::local_models::{self, Dashboard, ModelRow};
use crate::llm::local_fit::Fit;
use crate::ui::{
    chrome::{self, Section},
    keys::{Action, Keymap},
    text,
    theme::{Theme, Tok},
};
use crossterm::event::{Event, KeyEventKind};
use ratatui::{
    Frame,
    style::Style,
    text::{Line, Span},
};

/// Lines reserved below the list for the selected model's details (title plus six fields).
const DETAIL_LINES: usize = 8;

pub struct LocalModelsOverlay {
    dashboard: Dashboard,
    cursor: usize,
    message: String,
}

impl LocalModelsOverlay {
    pub fn new(dashboard: Dashboard) -> Self {
        Self {
            dashboard,
            cursor: 0,
            message: String::new(),
        }
    }

    pub fn banner(&mut self, message: String) {
        self.message = text::one_line(&message);
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if key.kind == KeyEventKind::Release {
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectCancel, key) {
            return OverlayEvent::Close;
        }
        let count = self.dashboard.models.len();
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(count.saturating_sub(1));
        }
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let extra = format!("{} downloaded · read-only", self.dashboard.models.len());
        panel(
            frame,
            ctx.theme,
            Section::Learn,
            "Local models",
            &extra,
            |width, height| self.compose(usize::from(width), usize::from(height), ctx.theme),
        );
    }

    fn compose(&self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        let dashboard = &self.dashboard;
        let memory = match dashboard.memory_bytes {
            Some(bytes) => format!(
                "{:.1} GiB {}",
                bytes as f64 / (1u64 << 30) as f64,
                dashboard.memory_source
            ),
            None => "unknown".into(),
        };
        let mut out = vec![
            filled(
                vec![Span::styled(
                    format!(" Memory {memory} · stats {}", dashboard.stats),
                    theme.fg(Tok::Muted),
                )],
                width,
                Style::default(),
            ),
            filled(vec![], width, Style::default()),
        ];
        if dashboard.models.is_empty() {
            out.push(filled(
                vec![Span::styled(
                    " No downloaded MLX or GGUF models found.",
                    theme.fg(Tok::Muted),
                )],
                width,
                Style::default(),
            ));
        } else {
            let list_height = height.saturating_sub(out.len() + DETAIL_LINES + 2);
            let start = if self.cursor < list_height {
                0
            } else {
                self.cursor + 1 - list_height
            };
            for (index, row) in dashboard
                .models
                .iter()
                .enumerate()
                .skip(start)
                .take(list_height)
            {
                let fill = if index == self.cursor {
                    selected_fill(theme)
                } else {
                    Style::default()
                };
                out.push(filled(list_spans(row, theme), width, fill));
            }
            out.push(filled(vec![], width, Style::default()));
            if let Some(row) = dashboard.models.get(self.cursor) {
                out.push(filled(
                    vec![Span::styled(
                        format!(" {}", text::one_line(&row.label)),
                        theme.fg(Tok::Accent),
                    )],
                    width,
                    Style::default(),
                ));
                for (name, value) in detail_fields(row) {
                    let token = if value.starts_with("unknown") {
                        Tok::Muted
                    } else {
                        Tok::Text
                    };
                    out.push(filled(
                        vec![Span::styled(
                            format!("   {name:<8} {}", text::one_line(&value)),
                            theme.fg(token),
                        )],
                        width,
                        Style::default(),
                    ));
                }
            }
        }
        while out.len() + 2 < height {
            out.push(filled(vec![], width, Style::default()));
        }
        out.push(filled(
            vec![Span::styled(
                if self.message.is_empty() {
                    String::new()
                } else {
                    format!(" {}", self.message)
                },
                theme.fg(Tok::Warning),
            )],
            width,
            Style::default(),
        ));
        let mut spans = vec![Span::raw(" ")];
        spans.extend(chrome::key_hint_spans(
            theme,
            &[("↑↓", "select"), ("Esc", "back")],
        ));
        out.push(Line::from(fit(spans, width)));
        out
    }
}

/// The labelled fields shown for the selected model.
fn detail_fields(row: &ModelRow) -> Vec<(&'static str, String)> {
    vec![
        ("speed", local_models::speed_text(&row.speed)),
        ("memory", local_models::memory_text(&row.memory)),
        ("version", local_models::version_text(&row.improvement)),
        ("gate", local_models::gate_text(&row.improvement)),
        ("waiting", local_models::waiting_text(&row.improvement)),
        ("source", format!("{} · {}", row.source, row.path)),
    ]
}

/// One list row: `label  [format]  median tok/s  fit  first waiting item`.
fn list_spans(row: &ModelRow, theme: &Theme) -> Vec<Span<'static>> {
    let speed = match row.speed.median_tok_per_s {
        Some(median) => format!("{median:.1} tok/s"),
        None => "tok/s unknown".into(),
    };
    let fit_label = match row.memory.fit {
        Fit::Fits => "fits",
        Fit::Tight => "tight",
        Fit::WontFit => "will not fit",
        Fit::Unknown => "fit unknown",
    };
    let waiting = match &row.improvement.waiting {
        None => "unknown".to_owned(),
        Some(items) => items.first().cloned().unwrap_or_else(|| "ready".into()),
    };
    vec![Span::styled(
        format!(
            " {}  [{}]  {speed}  {fit_label}  {waiting}",
            text::one_line(&row.label),
            row.format.unwrap_or("?")
        ),
        theme.fg(Tok::Text),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::local_models::{Dashboard, Improvement, Memory, ModelRow};
    use crate::stats::DecodeSpeed;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn row(label: &str) -> ModelRow {
        ModelRow {
            label: label.into(),
            path: format!("/models/{label}"),
            source: "test".into(),
            format: Some("MLX"),
            speed: DecodeSpeed::default(),
            memory: Memory {
                weights_bytes: None,
                resident_bytes: None,
                context_tokens: None,
                fit: Fit::Unknown,
            },
            improvement: Improvement {
                store: "no_store",
                store_dir: None,
                version: None,
                kind: None,
                adapter: None,
                last_gate: None,
                verified_turns: None,
                lora_min_examples: 24,
                new_turns: None,
                min_new_turns: 5,
                cooldown_secs: None,
                waiting: Some(vec!["no recorded turns for this model yet".into()]),
            },
        }
    }

    fn dashboard(models: Vec<ModelRow>) -> Dashboard {
        Dashboard {
            generated_ms: 0,
            memory_bytes: None,
            memory_source: "unknown",
            stats: "missing",
            models,
        }
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn arrows_move_the_selection_within_the_list_and_escape_closes() {
        let keys = Keymap::defaults();
        let mut overlay = LocalModelsOverlay::new(dashboard(vec![row("a"), row("b")]));
        overlay.handle(&key(KeyCode::Down), &keys);
        overlay.handle(&key(KeyCode::Down), &keys);
        assert_eq!(overlay.cursor, 1, "the cursor stops at the last model");
        overlay.handle(&key(KeyCode::Up), &keys);
        assert_eq!(overlay.cursor, 0);
        assert!(matches!(
            overlay.handle(&key(KeyCode::Esc), &keys),
            OverlayEvent::Close
        ));
    }

    #[test]
    fn unknown_sources_render_as_unknown_not_as_numbers() {
        let overlay = LocalModelsOverlay::new(dashboard(vec![row("mlx-model")]));
        let theme = Theme::dark();
        let shown = text_of(&overlay.compose(120, 30, &theme));
        assert!(shown.contains("stats missing"), "{shown}");
        assert!(
            shown.contains("mlx-model  [MLX]  tok/s unknown  fit unknown"),
            "{shown}"
        );
        assert!(
            shown.contains("speed    unknown (no stats database)"),
            "{shown}"
        );
        assert!(
            shown.contains("version  no learning store for this model"),
            "{shown}"
        );
        assert!(
            shown.contains("waiting  no recorded turns for this model yet"),
            "{shown}"
        );
    }

    #[test]
    fn an_empty_or_tiny_dashboard_draws_without_panicking() {
        let theme = Theme::dark();
        let empty = LocalModelsOverlay::new(dashboard(Vec::new()));
        assert!(text_of(&empty.compose(80, 24, &theme)).contains("No downloaded MLX or GGUF"));
        let tiny = LocalModelsOverlay::new(dashboard(vec![row("a"), row("b"), row("c")]));
        let lines = tiny.compose(40, 4, &theme);
        assert!(!lines.is_empty());
    }
}
