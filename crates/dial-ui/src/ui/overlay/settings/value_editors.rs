//! Structured editors the settings panel opens instead of a one-line text field:
//! - [`ListEditor`] for string lists with known values (check, order, add custom entries or folders);
//! - [`EntriesEditor`] for TOML maps and arrays (one entry per row, edited one at a time).
//!
//! Both work on a copy and return the whole new value on save; Esc discards it. Ctrl+E leaves for
//! the plain text editor, so every value stays typeable.
use crate::config::schema::PathKind;
use crate::ui::editor::Editor;
use crate::ui::keys::{Action, Keymap};
use crate::ui::overlay::path_browser::{BrowseStep, BrowserView, PathBrowser};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::Path;
use toml::{Table, Value};

/// What a key did.
#[derive(Debug, PartialEq)]
pub(super) enum Step {
    Stay,
    Cancel,
    Save(Value),
    /// Switch to the plain text editor.
    Type,
}

/// What the panel draws for a structured editor.
pub(super) struct View {
    /// Rows in the visible window: label and whether the cursor is on it.
    pub rows: Vec<(String, bool)>,
    /// An open text input: its prompt and editor.
    pub input: Option<(&'static str, Editor)>,
    /// An open folder browser.
    pub browser: Option<BrowserView>,
    pub note: Option<String>,
}

fn ctrl_e(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e')
}

fn window(len: usize, cursor: usize, height: usize) -> std::ops::Range<usize> {
    let height = height.max(1);
    let first = cursor
        .saturating_sub(height / 2)
        .min(len.saturating_sub(height));
    first..(first + height).min(len)
}

/// Move the cursor for the shared navigation keys; `None` when the key is not navigation.
fn navigate(keys: &Keymap, key: &KeyEvent, cursor: usize, len: usize) -> Option<usize> {
    let last = len.saturating_sub(1);
    if keys.matches(Action::SelectUp, key) {
        Some(if cursor == 0 { last } else { cursor - 1 })
    } else if keys.matches(Action::SelectDown, key) {
        Some(if cursor >= last { 0 } else { cursor + 1 })
    } else if keys.matches(Action::SelectPageUp, key) {
        Some(cursor.saturating_sub(8))
    } else if keys.matches(Action::SelectPageDown, key) {
        Some((cursor + 8).min(last))
    } else {
        None
    }
}

/// How a list adds a value that is not offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AddBy {
    Text,
    Folder,
}

enum Adding {
    Text(Editor),
    Folder(Box<PathBrowser>),
}

pub(super) struct ListEditor {
    /// Chosen values, in saved order.
    items: Vec<String>,
    /// Offered values and their labels, in display order.
    known: Vec<(String, String)>,
    ordered: bool,
    add_by: AddBy,
    cursor: usize,
    adding: Option<Adding>,
    /// Where the folder browser starts.
    base: std::path::PathBuf,
}

impl ListEditor {
    pub(super) fn new(
        current: &Value,
        known: Vec<(String, String)>,
        ordered: bool,
        add_by: AddBy,
        base: &Path,
    ) -> Self {
        let items = match current {
            Value::Array(values) => values
                .iter()
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        ListEditor {
            items,
            known,
            ordered,
            add_by,
            cursor: 0,
            adding: None,
            base: base.to_path_buf(),
        }
    }

    /// Offered values that are not chosen yet.
    fn unchosen(&self) -> Vec<&(String, String)> {
        self.known
            .iter()
            .filter(|(value, _)| !self.items.contains(value))
            .collect()
    }

    fn label(&self, value: &str) -> String {
        self.known
            .iter()
            .find(|(v, _)| v == value)
            .map_or_else(|| value.to_owned(), |(_, label)| label.clone())
    }

    /// Rows: chosen values, offered values, then the add row.
    fn len(&self) -> usize {
        self.items.len() + self.unchosen().len() + 1
    }

    fn add_label(&self) -> &'static str {
        match self.add_by {
            AddBy::Text => "+ Add another value…",
            AddBy::Folder => "+ Add a folder…",
        }
    }

    pub(super) fn value(&self) -> Value {
        Value::Array(self.items.iter().cloned().map(Value::String).collect())
    }

    fn push(&mut self, value: String) {
        let value = value.trim().to_owned();
        if !value.is_empty() && !self.items.contains(&value) {
            self.items.push(value);
        }
        self.cursor = self.items.len().saturating_sub(1);
    }

    pub(super) fn paste(&mut self, text: &str) {
        match &mut self.adding {
            Some(Adding::Text(editor)) => editor.insert(text),
            Some(Adding::Folder(browser)) => browser.paste(text),
            None => {}
        }
    }

    pub(super) fn key(&mut self, key: &KeyEvent, keys: &Keymap) -> Step {
        if let Some(adding) = &mut self.adding {
            match adding {
                Adding::Text(editor) => {
                    if keys.matches(Action::SelectCancel, key) {
                        self.adding = None;
                    } else if keys.matches(Action::SelectConfirm, key) {
                        let text = editor.text.clone();
                        self.adding = None;
                        self.push(text);
                    } else {
                        editor.key_with(*key, keys);
                    }
                }
                Adding::Folder(browser) => match browser.key(key, keys) {
                    BrowseStep::Stay => {}
                    BrowseStep::Cancel => self.adding = None,
                    BrowseStep::Type => self.adding = Some(Adding::Text(Editor::default())),
                    BrowseStep::Chosen(path) => {
                        self.adding = None;
                        self.push(path.display().to_string());
                    }
                },
            }
            return Step::Stay;
        }
        if ctrl_e(key) {
            return Step::Type;
        }
        if keys.matches(Action::SelectCancel, key) {
            return Step::Cancel;
        }
        let len = self.len();
        self.cursor = self.cursor.min(len - 1);
        if let Some(next) = navigate(keys, key, self.cursor, len) {
            self.cursor = next;
            return Step::Stay;
        }
        let chosen = self.items.len();
        let on_add = self.cursor == len - 1;
        let reorder = key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if on_add => {
                self.adding = Some(match self.add_by {
                    AddBy::Text => Adding::Text(Editor::default()),
                    AddBy::Folder => Adding::Folder(Box::new(PathBrowser::open(
                        PathKind::Directory,
                        "",
                        &self.base,
                    ))),
                });
            }
            _ if keys.matches(Action::SelectConfirm, key) => return Step::Save(self.value()),
            KeyCode::Char(' ') if self.cursor < chosen => {
                self.items.remove(self.cursor);
            }
            KeyCode::Char(' ') if !on_add => {
                let value = self.unchosen()[self.cursor - chosen].0.clone();
                self.items.push(value);
                self.cursor = self.items.len() - 1;
            }
            KeyCode::Delete | KeyCode::Backspace if self.cursor < chosen => {
                self.items.remove(self.cursor);
            }
            KeyCode::Up | KeyCode::Down if reorder && self.ordered && self.cursor < chosen => {
                let to = if key.code == KeyCode::Up {
                    self.cursor.checked_sub(1)
                } else {
                    Some(self.cursor + 1).filter(|to| *to < chosen)
                };
                if let Some(to) = to {
                    self.items.swap(self.cursor, to);
                    self.cursor = to;
                }
            }
            _ => {}
        }
        Step::Stay
    }

    pub(super) fn view(&self, height: usize) -> View {
        if let Some(Adding::Folder(browser)) = &self.adding {
            return View {
                rows: Vec::new(),
                input: None,
                browser: Some(browser.view(height.saturating_sub(2))),
                note: None,
            };
        }
        let mut rows: Vec<String> = self
            .items
            .iter()
            .enumerate()
            .map(|(n, value)| {
                if self.ordered {
                    format!("[x] {}. {}", n + 1, self.label(value))
                } else {
                    format!("[x] {}", self.label(value))
                }
            })
            .collect();
        rows.extend(
            self.unchosen()
                .into_iter()
                .map(|(_, label)| format!("[ ] {label}")),
        );
        rows.push(self.add_label().to_owned());
        let cursor = self.cursor.min(rows.len() - 1);
        let range = window(rows.len(), cursor, height);
        View {
            rows: rows
                .into_iter()
                .enumerate()
                .skip(range.start)
                .take(range.len())
                .map(|(n, label)| (label, n == cursor))
                .collect(),
            input: match &self.adding {
                Some(Adding::Text(editor)) => Some(("Add", editor.clone())),
                _ => None,
            },
            browser: None,
            note: None,
        }
    }

    pub(super) fn ordered(&self) -> bool {
        self.ordered
    }

    #[cfg(test)]
    pub(super) fn is_adding(&self) -> bool {
        self.adding.is_some()
    }
}

/// One-line TOML text of a value.
fn inline(value: &Value) -> String {
    value.to_string()
}

/// A bare TOML key when possible, otherwise a quoted one.
fn key_text(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        key.to_owned()
    } else {
        Value::String(key.to_owned()).to_string()
    }
}

/// Which entry an open input edits.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    New,
    Entry(usize),
    Scalar(usize),
}

fn credential_field(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "headers" | "auth" | "api_key" | "token" | "password" | "secret" | "env"
    )
}

/// A new entry is staged separately so cancelling never inserts a partial record.
enum Creating {
    Name(Editor),
    Kind {
        name: Option<String>,
        cursor: usize,
    },
    Scalar {
        name: Option<String>,
        template: Value,
        editor: Editor,
    },
    Nested {
        name: Option<String>,
        editor: Box<EntriesEditor>,
    },
}

const ENTRY_TYPES: [&str; 6] = ["String", "Boolean", "Integer", "Decimal", "Table", "Array"];

fn scalar_value(template: &Value, text: &str) -> Result<Value, String> {
    Ok(match template {
        Value::String(_) => Value::String(text.to_owned()),
        Value::Boolean(_) => {
            Value::Boolean(text.trim().parse().map_err(|_| "Enter true or false")?)
        }
        Value::Integer(_) => {
            Value::Integer(text.trim().parse().map_err(|_| "Enter a whole number")?)
        }
        Value::Float(_) => {
            let number: f64 = text.trim().parse().map_err(|_| "Enter a number")?;
            if !number.is_finite() {
                return Err("Enter a finite number".into());
            }
            Value::Float(number)
        }
        Value::Datetime(_) => {
            Value::Datetime(text.trim().parse().map_err(|_| "Enter a date or time")?)
        }
        _ => return Err("Use the nested editor for this value".into()),
    })
}

fn scalar_prompt(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "Text",
        Value::Boolean(_) => "true / false",
        Value::Integer(_) => "Whole number",
        Value::Float(_) => "Number",
        _ => "Date / time",
    }
}

pub(super) struct EntriesEditor {
    /// The working copy: a table of named entries, or an array of items.
    value: Value,
    cursor: usize,
    input: Option<(Target, Editor)>,
    note: Option<String>,
    changed: bool,
    /// Nested table fields use the same editor, preserving all untouched siblings.
    fields: Option<(usize, Box<EntriesEditor>)>,
    /// Focused mode edits scalar values without TOML and never serializes records in rows.
    focused: bool,
    sensitive: bool,
    reveal: bool,
    creating: Option<Creating>,
}

impl EntriesEditor {
    pub(super) fn new(value: Value) -> Self {
        let value = match value {
            Value::Table(_) | Value::Array(_) => value,
            _ => Value::Table(Table::new()),
        };
        EntriesEditor {
            value,
            cursor: 0,
            input: None,
            note: None,
            changed: false,
            fields: None,
            focused: false,
            sensitive: false,
            reveal: false,
            creating: None,
        }
    }

    pub(super) fn for_setting(value: Value, sensitive: bool) -> Self {
        let mut editor = Self::new(value);
        editor.focused = true;
        editor.sensitive = sensitive;
        editor
    }

    fn entry_sensitive(&self, index: usize) -> bool {
        self.sensitive
            || match &self.value {
                Value::Table(table) => table
                    .keys()
                    .nth(index)
                    .is_some_and(|key| credential_field(key)),
                _ => false,
            }
    }

    fn input_sensitive(&self, target: &Target) -> bool {
        match target {
            Target::Entry(index) => self.focused || self.entry_sensitive(*index),
            Target::Scalar(index) => self.entry_sensitive(*index),
            // An arbitrary new TOML entry can contain credentials, even in an otherwise safe table.
            Target::New => self.focused || self.sensitive,
        }
    }

    fn display_entry(&self, index: usize, value: &Value) -> String {
        if self.focused {
            match value {
                Value::Table(table) => return format!("{} fields →", table.len()),
                Value::Array(items) => return format!("{} items →", items.len()),
                _ if self.entry_sensitive(index) => return "(hidden)".into(),
                Value::String(text) => return text.clone(),
                _ => {}
            }
        }
        inline(value)
    }

    fn entries(&self) -> Vec<String> {
        match &self.value {
            Value::Table(table) => table
                .iter()
                .enumerate()
                .map(|(index, (key, value))| {
                    format!("{} = {}", key_text(key), self.display_entry(index, value))
                })
                .collect(),
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(index, value)| self.display_entry(index, value))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn is_table(&self) -> bool {
        self.value.is_table()
    }

    /// Rows: entries, then "add", then "save".
    fn len(&self) -> usize {
        self.entries().len() + 2
    }

    pub(super) fn paste(&mut self, text: &str) {
        match &mut self.creating {
            Some(Creating::Name(editor) | Creating::Scalar { editor, .. }) => {
                editor.insert(text);
                return;
            }
            Some(Creating::Nested { editor, .. }) => {
                editor.paste(text);
                return;
            }
            Some(Creating::Kind { .. }) => return,
            None => {}
        }
        if let Some((_, fields)) = &mut self.fields {
            fields.paste(text);
            return;
        }
        if let Some((_, editor)) = &mut self.input {
            editor.insert(text);
        }
    }

    /// Parse the input line into the working copy.
    fn commit(&mut self, target: &Target, text: &str) -> Result<(), String> {
        if let Target::Scalar(index) = target {
            let old = self.entry(*index).ok_or("Field no longer exists")?;
            let value = scalar_value(old, text)?;
            self.replace_entry(*index, value);
            return Ok(());
        }
        let text = text.trim();
        match &mut self.value {
            Value::Table(table) => {
                let parsed: Table = toml::from_str(text)
                    .map_err(|_| "Write one entry as name = value".to_owned())?;
                if parsed.len() != 1 {
                    return Err("Write exactly one entry as name = value".into());
                }
                let (key, value) = parsed.into_iter().next().expect("one entry");
                let old = match target {
                    Target::Entry(n) | Target::Scalar(n) => table.keys().nth(*n).cloned(),
                    Target::New => None,
                };
                if table.contains_key(&key) && old.as_ref() != Some(&key) {
                    return Err(format!("{key} already exists; edit that entry instead"));
                }
                if let Some(old) = old.filter(|old| *old != key) {
                    table.remove(&old);
                }
                table.insert(key, value);
            }
            Value::Array(items) => {
                let mut parsed: Table = toml::from_str(&format!("value = {text}"))
                    .map_err(|_| "Write one TOML value".to_owned())?;
                let value = parsed.remove("value").ok_or("Write one TOML value")?;
                if !parsed.is_empty() {
                    return Err("Write only one TOML value".into());
                }
                match target {
                    Target::Entry(n) | Target::Scalar(n) if *n < items.len() => items[*n] = value,
                    _ => items.push(value),
                }
            }
            _ => {}
        }
        self.changed = true;
        Ok(())
    }

    fn entry(&self, index: usize) -> Option<&Value> {
        match &self.value {
            Value::Table(table) => table.values().nth(index),
            Value::Array(items) => items.get(index),
            _ => None,
        }
    }

    fn replace_entry(&mut self, index: usize, value: Value) {
        match &mut self.value {
            Value::Table(table) => {
                if let Some(key) = table.keys().nth(index).cloned() {
                    table.insert(key, value);
                }
            }
            Value::Array(items) => items[index] = value,
            _ => {}
        }
        self.changed = true;
    }

    fn insert_created(&mut self, name: Option<String>, value: Value) {
        match &mut self.value {
            Value::Table(table) => {
                let name = name.expect("table creation asks for a name");
                table.insert(name.clone(), value);
                self.cursor = table.keys().position(|key| key == &name).unwrap_or(0);
            }
            Value::Array(items) => {
                self.cursor = items.len();
                items.push(value);
            }
            _ => {}
        }
        self.changed = true;
        self.note = None;
    }

    fn create_key(&mut self, key: &KeyEvent, keys: &Keymap) -> Step {
        let mut creating = self.creating.take().expect("creation in progress");
        // Keep the advanced escape hatch at every creation stage. Accepted nested draft
        // fields can be serialized here, but never inherited configuration.
        if ctrl_e(key) && !matches!(creating, Creating::Nested { .. }) {
            let mut editor = Editor::default();
            if let Creating::Scalar {
                name,
                template,
                editor: input,
            } = &creating
                && let Ok(value) = scalar_value(template, &input.text)
            {
                editor.set(&name.as_ref().map_or_else(
                    || inline(&value),
                    |name| format!("{} = {}", key_text(name), inline(&value)),
                ));
            }
            self.input = Some((Target::New, editor));
            self.note = None;
            self.reveal = false;
            return Step::Stay;
        }
        match &mut creating {
            Creating::Name(editor) => {
                if keys.matches(Action::SelectCancel, key) {
                    self.note = None;
                    return Step::Stay;
                }
                if keys.matches(Action::SelectConfirm, key) {
                    let name = editor.text.trim().to_owned();
                    if name.is_empty() {
                        self.note = Some("Enter a field or record name".into());
                    } else if self
                        .value
                        .as_table()
                        .is_some_and(|table| table.contains_key(&name))
                    {
                        self.note =
                            Some("That name already exists; edit the existing entry".into());
                    } else {
                        creating = Creating::Kind {
                            name: Some(name),
                            cursor: 0,
                        };
                        self.note = None;
                    }
                } else if editor.key_with(*key, keys) {
                    self.note = None;
                }
            }
            Creating::Kind { name, cursor } => {
                if keys.matches(Action::SelectCancel, key) {
                    self.note = None;
                    return Step::Stay;
                }
                if let Some(next) = navigate(keys, key, *cursor, ENTRY_TYPES.len()) {
                    *cursor = next;
                } else if keys.matches(Action::SelectConfirm, key) {
                    let name = name.clone();
                    let template = match *cursor {
                        0 => Value::String(String::new()),
                        1 => Value::Boolean(false),
                        2 => Value::Integer(0),
                        3 => Value::Float(0.0),
                        4 => Value::Table(Table::new()),
                        _ => Value::Array(Vec::new()),
                    };
                    creating = if template.is_table() || template.is_array() {
                        // New fields inherit the same conservative input policy as the parent.
                        let sensitive =
                            self.sensitive || name.as_deref().is_some_and(credential_field);
                        Creating::Nested {
                            name,
                            editor: Box::new(Self::for_setting(template, sensitive)),
                        }
                    } else {
                        Creating::Scalar {
                            name,
                            template,
                            editor: Editor::default(),
                        }
                    };
                    self.reveal = false;
                }
            }
            Creating::Scalar {
                name,
                template,
                editor,
            } => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
                    self.reveal = !self.reveal;
                } else if keys.matches(Action::SelectCancel, key) {
                    self.note = None;
                    return Step::Stay;
                } else if keys.matches(Action::SelectConfirm, key) {
                    match scalar_value(template, &editor.text) {
                        Ok(value) => {
                            self.insert_created(name.clone(), value);
                            return Step::Stay;
                        }
                        Err(message) => self.note = Some(message),
                    }
                } else if editor.key_with(*key, keys) {
                    self.note = None;
                }
            }
            Creating::Nested { name, editor } => match editor.key(key, keys) {
                Step::Cancel => {
                    self.note = None;
                    return Step::Stay;
                }
                Step::Save(value) => {
                    self.insert_created(name.clone(), value);
                    return Step::Stay;
                }
                Step::Type => {
                    let seed = name.as_ref().map_or_else(
                        || inline(editor.value()),
                        |name| format!("{} = {}", key_text(name), inline(editor.value())),
                    );
                    let mut input = Editor::default();
                    input.set(&seed);
                    self.input = Some((Target::New, input));
                    self.reveal = false;
                    return Step::Stay;
                }
                Step::Stay => {}
            },
        }
        self.creating = Some(creating);
        Step::Stay
    }

    pub(super) fn key(&mut self, key: &KeyEvent, keys: &Keymap) -> Step {
        if self.creating.is_some() {
            return self.create_key(key, keys);
        }
        if let Some((index, fields)) = &mut self.fields {
            let index = *index;
            match fields.key(key, keys) {
                Step::Stay => {}
                Step::Cancel => self.fields = None,
                Step::Save(value) => {
                    self.replace_entry(index, value);
                    self.fields = None;
                }
                Step::Type => {
                    let value = fields.value().clone();
                    let seed = match &self.value {
                        Value::Table(table) => format!(
                            "{} = {}",
                            key_text(table.keys().nth(index).expect("selected entry")),
                            inline(&value)
                        ),
                        _ => inline(&value),
                    };
                    let mut editor = Editor::default();
                    editor.set(&seed);
                    self.fields = None;
                    self.reveal = false;
                    self.input = Some((Target::Entry(index), editor));
                }
            }
            return Step::Stay;
        }
        if let Some((target, editor)) = &mut self.input {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
                self.reveal = !self.reveal;
                return Step::Stay;
            }
            if keys.matches(Action::SelectCancel, key) {
                self.input = None;
                self.note = None;
            } else if keys.matches(Action::SelectConfirm, key) {
                let (target, text) = (target.clone(), editor.text.clone());
                match self.commit(&target, &text) {
                    Ok(()) => {
                        self.input = None;
                        self.note = None;
                    }
                    Err(message) => self.note = Some(message),
                }
            } else if editor.key_with(*key, keys) {
                self.note = None;
            }
            return Step::Stay;
        }
        if ctrl_e(key) {
            return Step::Type;
        }
        if keys.matches(Action::SelectCancel, key) {
            return Step::Cancel;
        }
        let len = self.len();
        self.cursor = self.cursor.min(len - 1);
        if let Some(next) = navigate(keys, key, self.cursor, len) {
            self.cursor = next;
            return Step::Stay;
        }
        let count = len - 2;
        if keys.matches(Action::SelectConfirm, key) {
            if self.cursor == len - 1 {
                return Step::Save(self.value.clone());
            }
            if self.focused && self.cursor == count {
                self.creating = Some(if self.is_table() {
                    Creating::Name(Editor::default())
                } else {
                    Creating::Kind {
                        name: None,
                        cursor: 0,
                    }
                });
                self.note = None;
                self.reveal = false;
                return Step::Stay;
            }
            if self.cursor < count
                && let Some(value @ (Value::Table(_) | Value::Array(_))) = self.entry(self.cursor)
            {
                let mut child = Self::new(value.clone());
                child.focused = self.focused;
                child.sensitive = self.entry_sensitive(self.cursor);
                self.fields = Some((self.cursor, Box::new(child)));
                return Step::Stay;
            }
            let (target, seed) = if self.cursor == count {
                (Target::New, String::new())
            } else if self.focused {
                let value = self.entry(self.cursor).expect("selected entry");
                let seed = value.as_str().map_or_else(|| inline(value), str::to_owned);
                (Target::Scalar(self.cursor), seed)
            } else {
                (
                    Target::Entry(self.cursor),
                    self.entries()[self.cursor].clone(),
                )
            };
            let mut editor = Editor::default();
            editor.set(&seed);
            self.reveal = false;
            self.input = Some((target, editor));
            return Step::Stay;
        }
        if matches!(key.code, KeyCode::Delete | KeyCode::Backspace) && self.cursor < count {
            match &mut self.value {
                Value::Table(table) => {
                    if let Some(name) = table.keys().nth(self.cursor).cloned() {
                        table.remove(&name);
                    }
                }
                Value::Array(items) => {
                    items.remove(self.cursor);
                }
                _ => {}
            }
            self.changed = true;
        }
        Step::Stay
    }

    pub(super) fn view(&self, height: usize) -> View {
        if let Some(creating) = &self.creating {
            return match creating {
                Creating::Nested { editor, .. } => editor.view(height),
                Creating::Kind { cursor, .. } => {
                    let range = window(ENTRY_TYPES.len(), *cursor, height);
                    View {
                        rows: ENTRY_TYPES.iter().enumerate().skip(range.start).take(range.len())
                            .map(|(index, label)| (format!("{label} →"), index == *cursor)).collect(),
                        input: None,
                        browser: None,
                        note: Some("Choose the new value's type. Enter: continue; Esc: cancel; Ctrl+E: Advanced TOML.".into()),
                    }
                }
                Creating::Name(editor) | Creating::Scalar { editor, .. } => {
                    let (prompt, sensitive) = match creating {
                        Creating::Name(_) => ("Field / record name", false),
                        Creating::Scalar { template, .. } => (scalar_prompt(template), true),
                        _ => unreachable!(),
                    };
                    let mut shown = editor.clone();
                    if sensitive && !self.reveal {
                        shown.set("••••••••");
                    }
                    View {
                        rows: Vec::new(),
                        input: Some((prompt, shown)),
                        browser: None,
                        note: self.note.clone().or_else(|| Some(if sensitive {
                            "New value masked. Ctrl+R: reveal/hide; Enter: accept; Esc: cancel; Ctrl+E: Advanced TOML.".into()
                        } else {
                            "Enter: choose type; Esc: cancel; Ctrl+E: Advanced TOML.".into()
                        })),
                    }
                }
            };
        }
        if let Some((_, fields)) = &self.fields {
            let mut view = fields.view(height);
            if view.note.is_none() {
                view.note = Some("Editing entry fields; Save returns to the collection. Ctrl+E: advanced entry TOML.".to_owned());
            }
            return view;
        }
        let mut rows = self.entries();
        rows.push(if self.is_table() {
            "+ Add an entry…".to_owned()
        } else {
            "+ Add an item…".to_owned()
        });
        rows.push(if self.changed {
            "Save changes".to_owned()
        } else {
            "Save (no changes yet)".to_owned()
        });
        let cursor = self.cursor.min(rows.len() - 1);
        let range = window(rows.len(), cursor, height);
        View {
            rows: rows
                .into_iter()
                .enumerate()
                .skip(range.start)
                .take(range.len())
                .map(|(n, label)| (label, n == cursor))
                .collect(),
            input: self.input.as_ref().map(|(target, editor)| {
                let prompt = match target {
                    Target::Scalar(index) => scalar_prompt(self.entry(*index).expect("selected field")),
                    _ if self.is_table() => "name = value (Advanced TOML)",
                    _ => "Value (Advanced TOML)",
                };
                let mut shown = editor.clone();
                if self.input_sensitive(target) && !self.reveal {
                    shown.set("••••••••");
                }
                (prompt, shown)
            }),
            browser: None,
            note: self.note.clone().or_else(|| self.input.as_ref().map(|(target, _)| {
                if self.input_sensitive(target) {
                    "Value masked. Ctrl+R: reveal/hide; Enter: accept field; Esc: discard field.".into()
                } else {
                    "Enter: accept field; Esc: discard field. Save changes applies the collection.".into()
                }
            })),
        }
    }

    #[cfg(test)]
    pub(super) fn is_editing(&self) -> bool {
        self.input.is_some()
    }

    /// The working copy, for switching to the plain text editor without losing edits.
    pub(super) fn value(&self) -> &Value {
        &self.value
    }
}

#[cfg(test)]
mod tests;
