//! Renders an extension's declarative panel and turns keys into its one outcome. Presentation and
//! key handling only: the controller owns the wait loop, cancellation and draft restoration.
use super::{Rows, questions, theme::Theme};
use crate::extensions::panel::{Element, Outcome, Panel, Value, accepts_generation};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::BTreeMap;

pub struct PanelDialog {
    panel: Panel,
    values: BTreeMap<String, Value>,
    /// Index into `panel.elements` of the focused input, select or button.
    focus: usize,
}

fn focusable(element: &Element) -> bool {
    matches!(
        element,
        Element::Input { .. } | Element::Select { .. } | Element::Button { .. }
    )
}

impl PanelDialog {
    pub fn new(panel: Panel) -> Result<Self> {
        panel.validate()?;
        let values = panel.initial_values();
        let focus = panel
            .elements
            .iter()
            .position(focusable)
            .unwrap_or_default();
        Ok(Self {
            panel,
            values,
            focus,
        })
    }

    /// Replaces the shown revision with a newer one of the same generation. Returns `false` and
    /// keeps the current panel when the generation differs. Typed values survive when the new
    /// revision still accepts them.
    pub fn update(&mut self, panel: Panel) -> Result<bool> {
        if !accepts_generation(self.panel.generation, panel.generation) {
            return Ok(false);
        }
        panel.validate()?;
        let mut next = panel.initial_values();
        for (id, value) in &self.values {
            if !next.contains_key(id) {
                continue;
            }
            let mut trial = next.clone();
            trial.insert(id.clone(), value.clone());
            if panel.check_values(&trial).is_ok() {
                next = trial;
            }
        }
        self.values = next;
        self.panel = panel;
        if !self.panel.elements.get(self.focus).is_some_and(focusable) {
            self.focus = self
                .panel
                .elements
                .iter()
                .position(focusable)
                .unwrap_or_default();
        }
        Ok(true)
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Outcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Outcome::Cancelled);
        }
        match key.code {
            KeyCode::Esc => Some(Outcome::Cancelled),
            KeyCode::Tab | KeyCode::Down => {
                self.step(1);
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.step(-1);
                None
            }
            KeyCode::Left => {
                self.cycle(-1);
                None
            }
            KeyCode::Right => {
                self.cycle(1);
                None
            }
            KeyCode::Backspace => {
                self.edit(|text| {
                    text.pop();
                });
                None
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.edit(|text| text.push(c));
                None
            }
            KeyCode::Enter => self.enter(),
            _ => None,
        }
    }

    /// Pasted text goes into the focused input as one line; control characters are dropped.
    pub fn paste(&mut self, value: &str) {
        let clean: String = value.chars().filter(|c| !c.is_control()).collect();
        self.edit(|text| text.push_str(&clean));
    }

    pub fn rows(&self, cols: usize, height: usize, theme: &Theme) -> Rows {
        let mut body = Vec::new();
        let mut focus_line = 0;
        for (index, element) in self.panel.elements.iter().enumerate() {
            let focused = index == self.focus && focusable(element);
            let pointer = if focused { "> " } else { "  " };
            if focused {
                focus_line = body.len();
            }
            match element {
                Element::Text(value) | Element::Markdown(value) => {
                    body.extend(value.lines().map(|line| format!("  {line}")));
                }
                Element::List(items) => {
                    body.extend(items.iter().map(|item| format!("  - {item}")));
                }
                Element::Input { id, label, .. } => {
                    let cursor = if focused { "_" } else { "" };
                    body.push(format!("{pointer}{label}: {}{cursor}", self.text_of(id)));
                }
                Element::Select {
                    id, label, options, ..
                } => {
                    let choice = match self.values.get(id) {
                        Some(Value::Choice(index)) => {
                            options.get(*index).map_or("", String::as_str)
                        }
                        _ => "",
                    };
                    body.push(format!("{pointer}{label}: < {choice} >"));
                }
                Element::Button { label, .. } => {
                    body.push(format!("{pointer}[ {label} ]"));
                }
            }
        }
        let avail = height.saturating_sub(3);
        let start = if avail == 0 || focus_line < avail {
            0
        } else {
            focus_line + 1 - avail
        };
        let mut rows = vec![self.panel.title.clone()];
        rows.extend(body.into_iter().skip(start).take(avail));
        rows.push("Tab move, Enter confirm, Esc cancel".into());
        questions::framed(rows, cols, height, theme)
    }

    fn focus_list(&self) -> Vec<usize> {
        self.panel
            .elements
            .iter()
            .enumerate()
            .filter(|(_, element)| focusable(element))
            .map(|(index, _)| index)
            .collect()
    }

    fn step(&mut self, delta: isize) {
        let list = self.focus_list();
        let Some(pos) = list.iter().position(|&index| index == self.focus) else {
            return;
        };
        let next = (pos as isize + delta).rem_euclid(list.len() as isize) as usize;
        self.focus = list[next];
    }

    fn cycle(&mut self, delta: isize) {
        let Element::Select { id, options, .. } = &self.panel.elements[self.focus] else {
            return;
        };
        let current = match self.values.get(id) {
            Some(Value::Choice(index)) => *index,
            _ => 0,
        };
        let next = (current as isize + delta).rem_euclid(options.len() as isize) as usize;
        self.values.insert(id.clone(), Value::Choice(next));
    }

    fn edit(&mut self, change: impl FnOnce(&mut String)) {
        let Element::Input { id, .. } = &self.panel.elements[self.focus] else {
            return;
        };
        let entry = self
            .values
            .entry(id.clone())
            .or_insert_with(|| Value::Text(String::new()));
        if let Value::Text(text) = entry {
            change(text);
        }
    }

    fn enter(&self) -> Option<Outcome> {
        let button = match &self.panel.elements[self.focus] {
            Element::Button { id, .. } => id.clone(),
            _ => self.default_button()?,
        };
        self.panel.check_values(&self.values).ok()?;
        Some(Outcome::Submitted {
            button,
            values: self.values.clone(),
        })
    }

    /// The first primary button, else the first button. Validation guarantees one exists.
    fn default_button(&self) -> Option<String> {
        let buttons = self
            .panel
            .elements
            .iter()
            .filter_map(|element| match element {
                Element::Button { id, primary, .. } => Some((id, *primary)),
                _ => None,
            });
        let mut first = None;
        for (id, primary) in buttons {
            if primary {
                return Some(id.clone());
            }
            first.get_or_insert_with(|| id.clone());
        }
        first
    }

    fn text_of(&self, id: &str) -> &str {
        match self.values.get(id) {
            Some(Value::Text(text)) => text,
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests;
