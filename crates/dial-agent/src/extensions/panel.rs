//! Declarative extension panels. An extension describes a small dialog: text, a list, inputs,
//! selects and buttons. The UI adapter renders it, owns focus and editing, and reports exactly one
//! typed [`Outcome`]. A panel is human input, never tool authorization: no outcome grants or denies
//! a tool call, and tool approvals keep their own contract (`Ui::approve*`).
use anyhow::{Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};

/// Upper bound on widgets plus nested list rows and select choices.
pub const MAX_ELEMENTS: usize = 64;
/// Upper bound on any single author-supplied string in a panel.
pub const MAX_TEXT_BYTES: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Element {
    Text(String),
    /// Markdown source; adapters render it as plain wrapped text.
    Markdown(String),
    List(Vec<String>),
    /// One line of editable text. `value` is the starting text.
    Input {
        id: String,
        label: String,
        value: String,
    },
    /// Choose one of `options`; `selected` is the starting index.
    Select {
        id: String,
        label: String,
        options: Vec<String>,
        selected: usize,
    },
    /// Submits the panel with this button's `id` as the outcome's `button`.
    Button {
        id: String,
        label: String,
        primary: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Panel {
    pub title: String,
    /// Identity of this revision. A late update from the extension may replace the shown panel only
    /// when its generation matches the one on screen (see [`accepts_generation`]).
    pub generation: u64,
    pub elements: Vec<Element>,
}

/// The typed value of one input or select when the panel is submitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Text(String),
    /// Index into the select's `options`.
    Choice(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The user pressed `button`; `values` holds every input and select keyed by element id.
    Submitted {
        button: String,
        values: BTreeMap<String, Value>,
    },
    /// Escape or Ctrl+C. The caller restores whatever draft it was holding.
    Cancelled,
    /// No interactive surface can show the panel. Always explicit; never a fake submission.
    Unsupported(String),
}

impl Panel {
    /// Rejects panels an adapter must not show: missing title or buttons, duplicate or malformed
    /// ids, empty selects, and oversized layouts.
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.title.trim().is_empty(), "panel needs a title");
        ensure!(
            self.elements.len() <= MAX_ELEMENTS,
            "panel has more than {MAX_ELEMENTS} elements"
        );
        check_text(&self.title)?;
        let mut count = self.elements.len();
        let mut ids = BTreeSet::new();
        let mut buttons = 0usize;
        for element in &self.elements {
            match element {
                Element::Text(text) | Element::Markdown(text) => check_text(text)?,
                Element::List(items) => check_items(&mut count, items)?,
                Element::Input { id, label, value } => {
                    claim(&mut ids, id)?;
                    check_text(label)?;
                    check_text(value)?;
                }
                Element::Select {
                    id,
                    label,
                    options,
                    selected,
                } => {
                    claim(&mut ids, id)?;
                    check_text(label)?;
                    check_items(&mut count, options)?;
                    ensure!(!options.is_empty(), "select {id:?} has no options");
                    ensure!(
                        *selected < options.len(),
                        "select {id:?} starts out of range"
                    );
                }
                Element::Button { id, label, .. } => {
                    claim(&mut ids, id)?;
                    check_text(label)?;
                    buttons += 1;
                }
            }
        }
        ensure!(buttons > 0, "panel needs at least one button");
        Ok(())
    }

    /// The values an adapter starts from: each input's text and each select's starting choice.
    pub fn initial_values(&self) -> BTreeMap<String, Value> {
        self.elements
            .iter()
            .filter_map(|element| match element {
                Element::Input { id, value, .. } => Some((id.clone(), Value::Text(value.clone()))),
                Element::Select { id, selected, .. } => {
                    Some((id.clone(), Value::Choice(*selected)))
                }
                _ => None,
            })
            .collect()
    }

    /// Checks that a submitted value matches the element it names: text for inputs, an in-range
    /// choice for selects. Adapters call this before reporting [`Outcome::Submitted`].
    pub fn check_values(&self, values: &BTreeMap<String, Value>) -> Result<()> {
        for element in &self.elements {
            match element {
                Element::Input { id, .. } => match values.get(id) {
                    Some(Value::Text(_)) => {}
                    other => bail!("input {id:?} needs text, got {other:?}"),
                },
                Element::Select { id, options, .. } => match values.get(id) {
                    Some(Value::Choice(index)) if *index < options.len() => {}
                    other => bail!("select {id:?} needs a choice, got {other:?}"),
                },
                _ => {}
            }
        }
        Ok(())
    }
}

/// A late update may replace the shown panel only when it targets the same generation.
pub fn accepts_generation(shown: u64, update: u64) -> bool {
    shown == update
}

fn check_text(text: &str) -> Result<()> {
    ensure!(
        text.len() <= MAX_TEXT_BYTES,
        "panel text is longer than {MAX_TEXT_BYTES} bytes"
    );
    Ok(())
}

fn check_items(count: &mut usize, items: &[String]) -> Result<()> {
    *count = count.saturating_add(items.len());
    ensure!(
        *count <= MAX_ELEMENTS,
        "panel exceeds its {MAX_ELEMENTS}-element layout budget"
    );
    for item in items {
        check_text(item)?;
    }
    Ok(())
}

fn claim<'a>(ids: &mut BTreeSet<&'a str>, id: &'a str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "invalid element id {id:?}"
    );
    ensure!(ids.insert(id), "duplicate element id {id:?}");
    Ok(())
}

mod wire;
pub use wire::{outcome_to_json, panel_from_json};
#[cfg(test)]
mod tests;
