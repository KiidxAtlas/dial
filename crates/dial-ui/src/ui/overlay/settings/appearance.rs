//! Appearance previews are projections of the open editor, never another settings store.
use super::super::DrawCtx;
use super::super::panel_keys;
use super::super::widgets::pair_label;
use super::{
    Mode, Row, SettingsOverlay, choice_value, fit, local_function_key, local_function_label,
    parse_input, pressed,
};
use crate::config::schema::SettingDef;
use crate::llm::model_config::PinnedWorkers;
use crate::ui::{
    keys::{Action, Keymap},
    text,
    theme::Tok,
};
use crossterm::event::Event;
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
        if local_function_key(key, keys, 4, Action::ProviderSetup) {
            self.appearance_preview = !self.appearance_preview;
            self.appearance_scroll = 0;
            return true;
        }
        if !self.appearance_preview {
            return false;
        }
        if panel_keys::matches(keys, Action::SelectConfirm, key)
            || panel_keys::matches(keys, Action::SelectCancel, key)
        {
            self.appearance_preview = false;
            return false;
        }
        if let Some(next) = panel_keys::scroll_text(keys, key, self.appearance_scroll, self.page) {
            self.appearance_scroll = next;
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
        let overflow = content.len() > room;
        self.page = room.max(1);
        self.appearance_scroll = self
            .appearance_scroll
            .min(content.len().saturating_sub(room.max(1)));
        let mut lines = vec![Line::from(fit(
            vec![Span::styled(
                format!(
                    " Preview / {} controls",
                    local_function_label(ctx.keys, 4, Action::ProviderSetup)
                ),
                ctx.theme.fg(Tok::Accent),
            )],
            w,
        ))];
        lines.extend(content.into_iter().skip(self.appearance_scroll).take(room));
        while lines.len() + 1 < h {
            lines.push(Line::default());
        }
        if h > 1 {
            let mut hints = self.preview_footer(ctx.keys);
            if overflow {
                hints.insert(
                    1.min(hints.len()),
                    panel_keys::segment(
                        &pair_label(ctx.keys, Action::SelectPageUp, Action::SelectPageDown),
                        "scroll",
                    ),
                );
            }
            lines.push(panel_keys::sheet_footer(ctx.theme, hints, w));
        }
        lines
    }

    /// Confirm and cancel fall through to the open editor; its other keys do not.
    pub(super) fn preview_footer(&self, keys: &Keymap) -> Vec<String> {
        let confirm = keys.label(Action::SelectConfirm);
        let cancel = keys.label(Action::SelectCancel);
        let mut hints = self.footer(keys);
        hints.retain(|hint| {
            hint.split_once(' ')
                .is_some_and(|(key, _)| key == confirm || key == cancel)
        });
        hints.insert(
            1.min(hints.len()),
            format!(
                "{} controls",
                local_function_label(keys, 4, Action::ProviderSetup)
            ),
        );
        hints
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
