//! Key handling inside the settings panel for browsing, pickers, paths and text edits.
use super::super::OverlayEvent;
use super::super::path_browser::BrowseStep;
use super::format::{adjust, choice_value, parse_input, value_text};
use super::memory::{label_for_choice, memory_missing_role, memory_needs};
use super::value_editors::{Step, View};
use super::widgets::{is_ctrl, other_scope, scope_name, typed_char};
use super::{
    ChoiceAction, EMBEDDING_MODEL_PATH, Mode, SettingsOverlay, TYPE_CHOICE, label_of, pick_matches,
};
use crate::config::schema::{Kind, SettingDef};
use crate::config::settings_store::Scope;
use crate::ui::keys::{Action, Keymap};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use toml::Value;

impl SettingsOverlay {
    pub(super) fn browse_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        self.feedback = None;
        if key.code == KeyCode::Delete && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.open_bulk_review();
            return OverlayEvent::None;
        }
        if is_ctrl(key, 'o')
            && ![
                Action::SelectConfirm,
                Action::SelectCancel,
                Action::SelectUp,
                Action::SelectDown,
                Action::TabNext,
                Action::TabPrev,
                Action::SectionNext,
                Action::SectionPrev,
                Action::SelectPageUp,
                Action::SelectPageDown,
            ]
            .iter()
            .any(|action| keys.matches(*action, key))
        {
            self.changed_only = !self.changed_only;
            self.cursor = 0;
            self.scroll = 0;
            return OverlayEvent::None;
        }
        if is_ctrl(key, 's') {
            self.scope = other_scope(self.scope);
            if self.changed_only {
                self.cursor = 0;
                self.scroll = 0;
            }
            // The title's scope label is shed on narrow panels, so say what changed here too.
            self.feedback = Some(
                match self.scope {
                    Scope::Global => "Changes now save for all projects",
                    Scope::Project => "Changes now save for this project only",
                }
                .to_owned(),
            );
            return OverlayEvent::None;
        }
        if is_ctrl(key, 'l') && !keys.matches(Action::TabNext, key) {
            return self.setup_key();
        }
        if super::super::panel_keys::matches(keys, Action::SelectCancel, key) {
            if self.search.is_empty() && !self.changed_only {
                return OverlayEvent::Close;
            }
            if self.search.is_empty() {
                self.changed_only = false;
            } else {
                self.search.clear();
            }
            self.cursor = 0;
            self.scroll = 0;
            return OverlayEvent::None;
        }
        let count = self.visible().len();
        let last = count.saturating_sub(1);
        // Lists stop at their ends, as every other panel's do; Home and End jump there.
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(last);
        } else if keys.matches(Action::SelectPageUp, key) {
            self.cursor = self.cursor.saturating_sub(self.page);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.cursor = (self.cursor + self.page).min(last);
        } else if key.code == KeyCode::Home {
            self.cursor = 0;
        } else if key.code == KeyCode::End {
            self.cursor = last;
        } else if keys.matches(Action::SelectConfirm, key) {
            return self.activate();
        } else if keys.matches(Action::TabNext, key) {
            self.change_tab(1);
        } else if keys.matches(Action::TabPrev, key) {
            self.change_tab(-1);
        } else if keys.matches(Action::SectionNext, key) {
            self.jump_section(true);
        } else if keys.matches(Action::SectionPrev, key) {
            self.jump_section(false);
        } else if key.code == KeyCode::Delete {
            return self.reset_current();
        } else if key.code == KeyCode::Backspace {
            if self.search.pop().is_some() {
                self.cursor = 0;
                self.scroll = 0;
            }
        } else if let Some(c) = typed_char(key) {
            if self.search.is_empty() && c == ' ' {
                return self.activate();
            }
            // Typing already searches, so a leading `/` (the search key elsewhere) is not text.
            if self.search.is_empty() && c == '/' {
                return OverlayEvent::None;
            }
            self.search.push(c);
            self.cursor = 0;
            self.scroll = 0;
        }
        OverlayEvent::None
    }

    pub(super) fn path_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Mode::Path { def, browser } = &mut self.mode else {
            return OverlayEvent::None;
        };
        let def: &'static SettingDef = def;
        match browser.key(key, keys) {
            BrowseStep::Stay => OverlayEvent::None,
            BrowseStep::Cancel => {
                self.mode = Mode::Browse;
                OverlayEvent::None
            }
            BrowseStep::Type => {
                let seed = self.edit_seed(def);
                self.open_text_editor(def, &seed)
            }
            BrowseStep::Chosen(path) => {
                self.mode = Mode::Browse;
                let value = Value::String(path.display().to_string());
                if self.current(def) == value {
                    OverlayEvent::None
                } else {
                    self.emit_set(def, value)
                }
            }
        }
    }

    pub(super) fn structured_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let (def, step, working) = match &mut self.mode {
            Mode::List { def, editor } => {
                let step = editor.key(key, keys);
                (*def, step, editor.value())
            }
            Mode::Entries { def, editor } => {
                let step = editor.key(key, keys);
                (*def, step, editor.value().clone())
            }
            _ => return OverlayEvent::None,
        };
        match step {
            Step::Stay => OverlayEvent::None,
            Step::Cancel => {
                self.mode = Mode::Browse;
                OverlayEvent::None
            }
            // The text editor starts from the working copy, so switching loses nothing.
            Step::Type => self.open_text_editor(def, &working),
            Step::Save(value) => {
                self.mode = Mode::Browse;
                if self.edit_seed(def) == value {
                    OverlayEvent::None
                } else {
                    self.emit_set(def, value)
                }
            }
        }
    }

    pub(super) fn edit_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Mode::Edit { def, editor } = &mut self.mode else {
            return OverlayEvent::None;
        };
        let def: &'static SettingDef = def;
        if def.sensitive() && is_ctrl(key, 'r') {
            self.reveal_sensitive = !self.reveal_sensitive;
        } else if keys.matches(Action::SelectCancel, key) {
            self.notes.remove(def.path);
            self.mode = Mode::Browse;
        } else if keys.matches(Action::SelectConfirm, key) {
            match parse_input(def.kind_for(&self.effective), &editor.text) {
                Ok(value) => {
                    self.mode = Mode::Browse;
                    return self.emit_set(def, value);
                }
                Err(message) => {
                    self.notes.insert(def.path.to_owned(), message);
                }
            }
        } else if keys.matches(Action::SelectUp, key) || keys.matches(Action::SelectDown, key) {
            let dir = if keys.matches(Action::SelectUp, key) {
                1
            } else {
                -1
            };
            if let Some(next) = adjust(def.kind_for(&self.effective), &editor.text, dir) {
                editor.set(&next);
                self.notes.remove(def.path);
            }
        } else if editor.key_with(*key, keys) {
            self.notes.remove(def.path);
        }
        OverlayEvent::None
    }

    pub(super) fn pick_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Mode::Pick { def, .. } = &self.mode else {
            return OverlayEvent::None;
        };
        let def: &'static SettingDef = def;
        let labels: Vec<String> = match &self.mode {
            Mode::Pick { choices, .. } => choices
                .iter()
                .map(|choice| self.choice_label(def, choice))
                .collect(),
            _ => return OverlayEvent::None,
        };
        let is_theme = matches!(def.kind_for(&self.effective), Kind::Theme);
        let page = self.page.max(1);
        let Mode::Pick {
            choices,
            cursor,
            filter,
            ..
        } = &mut self.mode
        else {
            return OverlayEvent::None;
        };
        let at = *cursor;
        let visible = pick_matches(&labels, choices, &filter.text);
        if keys.matches(Action::SelectCancel, key) {
            if self.download_armed.take().is_some() {
                self.feedback = None;
            }
            // Appearance Esc always discards; other pickers first clear their filter.
            if !super::appearance::is_appearance(def) && !filter.text.is_empty() {
                filter.set("");
                return OverlayEvent::None;
            }
            self.mode = Mode::Browse;
            if def.path == "memory.backend" {
                self.memory_intent = None;
            }
            return if is_theme {
                OverlayEvent::PreviewTheme(None)
            } else {
                OverlayEvent::None
            };
        }
        if keys.matches(Action::SelectConfirm, key) {
            // A filter that matches nothing has nothing to save; Esc clears it.
            if !visible.contains(&at) {
                return OverlayEvent::None;
            }
            let chosen = choices[at].clone();
            if chosen == TYPE_CHOICE {
                self.download_armed = None;
                let seed = self.edit_seed(def);
                return self.open_text_editor(def, &seed);
            }
            if let Some(offer) = self.offer(def.path, &chosen).cloned() {
                return self.pick_offer(def, offer);
            }
            let value = choice_value(def.kind_for(&self.effective), &chosen);
            let unchanged = self.current(def) == value;
            self.mode = Mode::Browse;
            if def.path == EMBEDDING_MODEL_PATH {
                return self.embedding_pick(def, &chosen);
            }
            if def.path == "memory.backend" {
                if let Some(role) =
                    memory_missing_role(&chosen, |p| self.current_path(p), |r| self.has_role(r))
                {
                    // Only the Roles view assigns a model role; switching now would leave memory paused.
                    self.memory_intent = None;
                    self.feedback = Some(format!(
                        "{} needs the {role} role: assign it in /roles (Alt+M), then choose it here.",
                        label_for_choice(&chosen)
                    ));
                    return OverlayEvent::None;
                }
                let needs = memory_needs(&chosen, |p| self.current_path(p), |r| self.has_role(r));
                if let Some(first) = needs.first() {
                    // Saving the backend now would fail validation or leave it inactive: collect
                    // its fields first.
                    let labels: Vec<_> = needs.iter().map(|p| label_of(p)).collect();
                    let alternative = if chosen == "sharpshooter" {
                        " (or assign the memory role in /roles, Alt+M)"
                    } else {
                        ""
                    };
                    self.memory_intent = Some(chosen.clone());
                    self.feedback = Some(format!(
                        "To switch, first fill: {}{alternative}. You'll come back here when they're set.",
                        labels.join(", ")
                    ));
                    self.move_to_path(first);
                    return OverlayEvent::None;
                }
                self.memory_intent = None;
            }
            return if unchanged {
                if is_theme {
                    OverlayEvent::PreviewTheme(None)
                } else {
                    OverlayEvent::None
                }
            } else {
                self.emit_set(def, value)
            };
        }
        let pos = visible.iter().position(|&n| n == at).unwrap_or(0);
        let last = visible.len().saturating_sub(1);
        let next = if keys.matches(Action::SelectUp, key) {
            Some(if pos == 0 { last } else { pos - 1 })
        } else if keys.matches(Action::SelectDown, key) {
            Some(if pos >= last { 0 } else { pos + 1 })
        } else if keys.matches(Action::SelectPageUp, key) {
            Some(pos.saturating_sub(page))
        } else if keys.matches(Action::SelectPageDown, key) {
            Some((pos + page).min(last))
        } else if filter.text.is_empty() && key.code == KeyCode::Home {
            Some(0)
        } else if filter.text.is_empty() && key.code == KeyCode::End {
            Some(last)
        } else {
            None
        };
        if let Some(next) = next {
            let Some(&n) = visible.get(next) else {
                return OverlayEvent::None;
            };
            *cursor = n;
            self.download_armed = None;
            self.feedback = None;
            return if is_theme {
                OverlayEvent::PreviewTheme(Some(choices[n].clone()))
            } else {
                OverlayEvent::None
            };
        }
        // Any other editing key goes to the filter; only a change to its text re-fits the selection.
        let before = filter.text.clone();
        if !filter.key_with(*key, keys) || filter.text == before {
            return OverlayEvent::None;
        }
        self.download_armed = None;
        self.feedback = None;
        self.refit_pick()
    }

    /// After the filter text changed, keep the selection on a choice it still shows. The chosen
    /// value is untouched until Enter; a theme picker previews whichever choice is now selected.
    pub(super) fn refit_pick(&mut self) -> OverlayEvent {
        let Mode::Pick { def, choices, .. } = &self.mode else {
            return OverlayEvent::None;
        };
        let def: &'static SettingDef = def;
        let labels: Vec<String> = choices
            .iter()
            .map(|choice| self.choice_label(def, choice))
            .collect();
        let is_theme = matches!(def.kind_for(&self.effective), Kind::Theme);
        let Mode::Pick {
            choices,
            cursor,
            filter,
            ..
        } = &mut self.mode
        else {
            return OverlayEvent::None;
        };
        let visible = pick_matches(&labels, choices, &filter.text);
        if visible.contains(&*cursor) {
            return OverlayEvent::None;
        }
        let Some(&first) = visible.first() else {
            return OverlayEvent::None;
        };
        *cursor = first;
        if is_theme {
            OverlayEvent::PreviewTheme(Some(choices[first].clone()))
        } else {
            OverlayEvent::None
        }
    }

    /// What a picker row shows for a raw choice value. The saved value is always the raw one.
    pub(super) fn choice_label(&self, def: &SettingDef, choice: &str) -> String {
        if choice == TYPE_CHOICE {
            return "Type another value…".to_owned();
        }
        if let Some(offer) = self.offer(def.path, choice) {
            return match offer.action {
                ChoiceAction::Download => format!("{} · download", offer.label),
                ChoiceAction::Install { .. } => format!("{} · install", offer.label),
                ChoiceAction::Unavailable { .. } => format!("{} · setup required", offer.label),
                _ => offer.label.clone(),
            };
        }
        match (def.kind_for(&self.effective), choice) {
            (Kind::AutoBool, "auto") => "Auto (let Dial decide)".to_owned(),
            (Kind::Bool | Kind::AutoBool, "true") => "On (enabled)".to_owned(),
            (Kind::Bool | Kind::AutoBool, "false") => "Off (disabled)".to_owned(),
            _ if def.path == EMBEDDING_MODEL_PATH => self.embedding_choice_label(choice),
            _ => value_text(def, &Value::String(choice.to_owned())),
        }
    }

    /// Indexes of the choices the picker filter keeps, in order.
    pub(super) fn pick_visible(
        &self,
        def: &SettingDef,
        choices: &[String],
        filter: &str,
    ) -> Vec<usize> {
        let labels: Vec<String> = choices
            .iter()
            .map(|choice| self.choice_label(def, choice))
            .collect();
        pick_matches(&labels, choices, filter)
    }

    /// The open browser or structured editor on `path`, windowed to `height` rows.
    pub(super) fn structured_view(&self, path: &str, height: usize) -> Option<View> {
        match &self.mode {
            Mode::Path { def, browser } if def.path == path => Some(View {
                rows: Vec::new(),
                input: None,
                browser: Some(browser.view(height)),
                note: None,
            }),
            Mode::List { def, editor } if def.path == path => Some(editor.view(height)),
            Mode::Entries { def, editor } if def.path == path => Some(editor.view(height)),
            _ => None,
        }
    }

    /// What the open browser or structured editor does, for the description area.
    pub(super) fn structured_help(&self) -> Option<String> {
        Some(match &self.mode {
            Mode::Path { .. } => "Enter opens a folder or chooses the entry; type to filter, or type a path and press Enter. Ctrl+E types the value instead.".to_owned(),
            Mode::List { editor, .. } if editor.ordered() => "Space checks or unchecks; Shift+Up/Down reorders (earlier is preferred); the last row adds another value. Enter saves; Ctrl+E types the list.".to_owned(),
            Mode::List { .. } => "Space checks or unchecks; the last row adds another value. Enter saves; Ctrl+E types the list.".to_owned(),
            Mode::Entries { def, .. } => {
                let layer = if matches!(def.kind, Kind::Toml) {
                    format!(" Editing only the {} layer; inherited values are not copied.", scope_name(self.scope))
                } else {
                    String::new()
                };
                format!("Enter edits an entry; Delete removes it; choose Save to keep the changes.{layer} Ctrl+E edits it as TOML text.")
            }
            _ => return None,
        })
    }
}
