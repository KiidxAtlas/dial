//! Appearance previews are projections of the open editor, never another settings store.
use super::super::DrawCtx;
use super::{Mode, Row, SettingsOverlay, choice_value, fit, hint_line, parse_input, pressed};
use crate::config::schema::SettingDef;
use crate::llm::model_config::PinnedWorkers;
use crate::ui::{
    keys::{Action, Keymap},
    text,
    theme::Tok,
};
use crossterm::event::{Event, KeyCode};
use ratatui::text::{Line, Span};
use toml::Value;

pub(super) fn is_appearance(def: &SettingDef) -> bool {
    def.search_location().0 == "Appearance"
}

impl SettingsOverlay {
    pub(super) fn appearance_selected(&self) -> bool {
        matches!(self.current_row(), Some(Row::Setting(i)) if is_appearance(&crate::config::schema::registry()[i]))
    }

    /// Switching surfaces retains the actual editor, cursor, filter and unconfirmed value.
    pub(super) fn appearance_key(&mut self, event: &Event, keys: &Keymap) -> bool {
        if !self.appearance_selected() {
            self.appearance_preview = false;
            return false;
        }
        let Event::Key(key) = event else {
            return self.appearance_preview;
        };
        if !pressed(key) {
            return false;
        }
        if key.code == KeyCode::F(4) && key.modifiers.is_empty() {
            self.appearance_preview = !self.appearance_preview;
            self.appearance_scroll = 0;
            return true;
        }
        if !self.appearance_preview {
            return false;
        }
        if keys.matches(Action::SelectConfirm, key) || keys.matches(Action::SelectCancel, key) {
            self.appearance_preview = false;
            return false;
        }
        match key.code {
            KeyCode::Down => self.appearance_scroll = self.appearance_scroll.saturating_add(1),
            KeyCode::Up => self.appearance_scroll = self.appearance_scroll.saturating_sub(1),
            KeyCode::PageDown => {
                self.appearance_scroll = self.appearance_scroll.saturating_add(self.page)
            }
            KeyCode::PageUp => {
                self.appearance_scroll = self.appearance_scroll.saturating_sub(self.page)
            }
            KeyCode::Home => self.appearance_scroll = 0,
            KeyCode::End => self.appearance_scroll = usize::MAX,
            _ => {}
        }
        true
    }

    pub(super) fn compose(&mut self, w: u16, h: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        if self.bulk_review.is_none() && self.appearance_selected() && self.appearance_preview {
            self.compose_appearance_preview(w, h, ctx)
        } else {
            self.compose_controls(w, h, ctx)
        }
    }

    fn compose_appearance_preview(&mut self, w: u16, h: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(w), usize::from(h));
        if h == 0 {
            return Vec::new();
        }
        let mut content = Vec::new();
        let status =
            (ctx.preview_status)(&self.preview_config(), w.min(usize::from(u16::MAX)) as u16);
        let status_text: String = status.spans.iter().map(|s| s.content.as_ref()).collect();
        for sample in std::iter::once(format!(" Preview status line: {status_text}"))
            .chain(self.appearance_samples())
        {
            content.extend(
                text::wrap_words(&sample, w.max(1))
                    .into_iter()
                    .map(Line::from),
            );
        }
        let room = h.saturating_sub(2);
        self.page = room.max(1);
        self.appearance_scroll = self
            .appearance_scroll
            .min(content.len().saturating_sub(room.max(1)));
        let mut lines = vec![Line::from(fit(
            vec![Span::styled(
                " Preview / F4 controls",
                ctx.theme.fg(Tok::Accent),
            )],
            w,
        ))];
        lines.extend(content.into_iter().skip(self.appearance_scroll).take(room));
        while lines.len() + 1 < h {
            lines.push(Line::default());
        }
        if h > 1 {
            let mut hints = self.footer(ctx.keys);
            hints.insert(1.min(hints.len()), "PgUp/PgDn scroll".into());
            lines.push(hint_line(ctx.theme, &hints, w));
        }
        lines
    }

    /// A valid, unconfirmed value is visible only to the sample renderer.
    pub(super) fn appearance_draft(&self) -> Option<(&'static SettingDef, Value)> {
        let (def, value) = match &self.mode {
            Mode::Pick {
                def,
                choices,
                cursor,
                filter,
            } if is_appearance(def) => {
                let visible = self.pick_visible(def, choices, &filter.text);
                if !visible.contains(cursor) {
                    return None;
                }
                (
                    *def,
                    choice_value(def.kind_for(&self.effective), choices.get(*cursor)?),
                )
            }
            Mode::Edit { def, editor } if is_appearance(def) => (
                *def,
                parse_input(def.kind_for(&self.effective), &editor.text).ok()?,
            ),
            Mode::List { def, editor } if is_appearance(def) => (*def, editor.value()),
            Mode::Entries { def, editor } if is_appearance(def) => (*def, editor.value().clone()),
            _ => return None,
        };
        Some((def, value))
    }

    pub(super) fn appearance_samples(&self) -> Vec<String> {
        let ui = self.preview_config();
        vec![
            format!(
                " Preview theme: {}{}",
                ui.theme,
                if self.appearance_draft().is_some() {
                    " (unsaved)"
                } else {
                    ""
                }
            ),
            if ui.show_tools {
                format!(
                    " Preview tools: read file - up to {} output lines",
                    ui.tool_output_lines
                )
            } else {
                " Preview tools: hidden; answer only".into()
            },
            if ui.motion {
                " Preview motion: on - panel reveals and launch animation".into()
            } else {
                " Preview motion: off - static panels and launch mark".into()
            },
            format!(
                " Preview transcript: answer{}{}",
                if ui.show_thinking {
                    " + reasoning text"
                } else {
                    " + reasoning summary"
                },
                if ui.show_turn_stats {
                    " + token/time stats"
                } else {
                    ""
                }
            ),
            match ui.pinned_workers {
                PinnedWorkers::Off => " Preview worker cards: hidden".into(),
                mode => format!(
                    " Preview worker cards: {} - Worker 1: running{}",
                    if mode == PinnedWorkers::Collapsed {
                        "up to 3"
                    } else {
                        "all that fit"
                    },
                    if ui.worker_live_preview {
                        " / read file (second row)"
                    } else {
                        ""
                    }
                ),
            },
            if ui.session_rail {
                " Preview session rail: workspace +12 -3 (includes existing work)".into()
            } else {
                " Preview session rail: hidden".into()
            },
            " Side pane: separate terminal view; no Appearance preference".into(),
        ]
    }
}

#[cfg(test)]
mod tests;
