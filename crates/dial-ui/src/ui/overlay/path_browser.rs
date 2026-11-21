//! A keyboard file browser for path-valued settings. It lists one directory at a time (folders
//! first), filters as you type, and returns the chosen absolute path. Enter checks a typed path
//! asynchronously: folders open when ready, and a second Enter chooses a checked file.
//! Reading is bounded and read-only; the caller decides what to save.
use crate::config::schema::PathKind;
use crate::ui::editor::Editor;
use crate::ui::keys::{Action, Keymap};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};

mod loader;
use loader::{Loader, Reply, Request};

/// Directory entries read at most; a larger folder is listed partially with a note.
const MAX_ENTRIES: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    name: String,
    dir: bool,
    executable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    /// Choose the directory being shown (directory settings only).
    Here,
    Parent,
    Entry(usize),
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub enum BrowseStep {
    Stay,
    Cancel,
    Chosen(PathBuf),
    /// Leave the browser for plain text entry (Ctrl+E).
    Type,
}

/// What the panel draws for the browser.
pub struct BrowserView {
    /// The directory shown, with the home directory written as `~`.
    pub title: String,
    pub filter: Editor,
    pub shown: usize,
    pub total: usize,
    /// Visible rows: label and whether the cursor is on it.
    pub rows: Vec<(String, bool)>,
    /// A problem to show (unreadable folder, unknown typed path, truncated listing).
    pub note: Option<String>,
}

pub struct PathBrowser {
    kind: PathKind,
    dir: PathBuf,
    entries: Vec<Entry>,
    cursor: usize,
    filter: Editor,
    note: Option<String>,
    home: Option<PathBuf>,
    listing_note: Option<String>,
    loader: Option<Loader>,
    loading: bool,
    validating: bool,
    validated: Option<PathBuf>,
}

impl PathBrowser {
    /// Open beside `current` (a saved value, possibly empty or relative to `base`), else in `base`.
    pub fn open(kind: PathKind, current: &str, base: &Path) -> Self {
        let loader = Loader::new(kind);
        let (loader, note) = match loader {
            Ok(loader) => (Some(loader), None),
            Err(error) => (None, Some(error)),
        };
        let browser = PathBrowser {
            kind,
            dir: base.to_path_buf(),
            entries: Vec::new(),
            cursor: 0,
            filter: Editor::default(),
            note: None,
            home: crate::config::paths::home_dir(),
            listing_note: note,
            loading: loader.is_some(),
            loader,
            validating: false,
            validated: None,
        };
        if let Some(loader) = &browser.loader {
            loader.request(Request::Open {
                path: browser.resolve(current.trim(), base),
                base: base.to_path_buf(),
            });
        }
        browser
    }

    /// The directory being shown.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn resolve(&self, typed: &str, base: &Path) -> Option<PathBuf> {
        if typed.is_empty() {
            return None;
        }
        let path = if typed == "~" {
            self.home.clone()?
        } else if let Some(rest) = typed
            .strip_prefix("~/")
            .or_else(|| cfg!(windows).then(|| typed.strip_prefix("~\\")).flatten())
        {
            self.home.as_ref()?.join(rest)
        } else {
            base.join(typed)
        };
        Some(path)
    }

    fn enter(&mut self, dir: PathBuf) {
        if self.loader.is_none() {
            return;
        }
        self.dir = dir.clone();
        self.cursor = 0;
        self.filter.set("");
        self.note = None;
        self.listing_note = None;
        self.entries.clear();
        self.validated = None;
        self.validating = false;
        self.loading = self.loader.is_some();
        if let Some(loader) = &self.loader {
            loader.request(Request::List { dir, select: None });
        }
    }
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Collect only ready results; this never waits for filesystem work.
    pub fn poll(&mut self) {
        let Some(reply) = self.loader.as_ref().and_then(Loader::collect) else {
            return;
        };
        let was_validating = self.validating;
        self.loading = false;
        self.validating = false;
        match reply {
            Reply::Listing {
                dir,
                entries,
                note,
                select,
            } => {
                // A typed directory was validated; discard its old path/filter now, not sooner.
                if was_validating {
                    self.filter.set("");
                }
                self.dir = dir;
                self.entries = entries;
                self.listing_note = note;
                self.note = None;
                if let Some(name) = select {
                    self.select_name(&name);
                }
            }
            Reply::Validated(path) => {
                self.validated = Some(path);
                self.note = Some("Path checked; Enter chooses it".to_owned());
            }
            Reply::Error(error) => self.note = Some(error),
        }
    }

    fn select_name(&mut self, name: &str) {
        if let Some(at) = self
            .rows()
            .iter()
            .position(|row| matches!(row, Row::Entry(i) if self.entries[*i].name == name))
        {
            self.cursor = at;
        }
    }

    fn edited(&mut self) {
        self.validated = None;
        if self.validating {
            if let Some(loader) = &self.loader {
                loader.cancel();
            }
            self.validating = false;
            self.loading = false;
        }
        self.note = None;
    }

    fn rows(&self) -> Vec<Row> {
        let filter = self.filter.text.trim().to_lowercase();
        // A typed path is resolved on Enter, not used as a filter.
        let typing_path = is_typed_path(&filter);
        let mut rows = Vec::new();
        if self.kind == PathKind::Directory && (filter.is_empty() || typing_path) {
            rows.push(Row::Here);
        }
        if self.dir.parent().is_some() && (filter.is_empty() || typing_path) {
            rows.push(Row::Parent);
        }
        if typing_path {
            return rows;
        }
        rows.extend(
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    // Hidden entries appear once the filter asks for them.
                    (!entry.name.starts_with('.') || filter.starts_with('.'))
                        && filter
                            .split_whitespace()
                            .all(|term| entry.name.to_lowercase().contains(term))
                })
                .map(|(i, _)| Row::Entry(i)),
        );
        rows
    }

    fn label(&self, row: &Row) -> String {
        match row {
            Row::Here => format!("Use this folder: {}", self.display(&self.dir)),
            Row::Parent => "../ (up one folder)".to_owned(),
            Row::Entry(i) => {
                let entry = &self.entries[*i];
                if entry.dir {
                    format!("{}/", entry.name)
                } else if entry.executable && self.kind == PathKind::Executable {
                    format!("{}  (program)", entry.name)
                } else {
                    entry.name.clone()
                }
            }
        }
    }

    fn display(&self, path: &Path) -> String {
        match &self.home {
            Some(home) if path == home => "~".to_owned(),
            Some(home) => path.strip_prefix(home).map_or_else(
                |_| path.display().to_string(),
                |rest| format!("~/{}", rest.display()),
            ),
            None => path.display().to_string(),
        }
    }

    pub fn key(&mut self, key: &KeyEvent, keys: &Keymap) -> BrowseStep {
        self.poll();
        let rows = self.rows();
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        let filter_empty = self.filter.text.is_empty();
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            return BrowseStep::Type;
        }
        if keys.matches(Action::SelectCancel, key) {
            if filter_empty {
                return BrowseStep::Cancel;
            }
            self.filter.set("");
            self.edited();
            return BrowseStep::Stay;
        }
        if keys.matches(Action::SelectConfirm, key) {
            if is_typed_path(self.filter.text.trim()) {
                return self.go_typed();
            }
            if self.loading {
                return BrowseStep::Stay;
            }
            return match rows.get(self.cursor) {
                Some(Row::Here) => BrowseStep::Chosen(self.dir.clone()),
                Some(Row::Parent) => {
                    self.up();
                    BrowseStep::Stay
                }
                Some(Row::Entry(i)) => {
                    let entry = &self.entries[*i];
                    let path = self.dir.join(&entry.name);
                    if entry.dir {
                        self.enter(path);
                        BrowseStep::Stay
                    } else {
                        BrowseStep::Chosen(path)
                    }
                }
                None => BrowseStep::Stay,
            };
        }
        let last = rows.len().saturating_sub(1);
        let page = 8;
        let moved = if keys.matches(Action::SelectUp, key) {
            Some(if self.cursor == 0 {
                last
            } else {
                self.cursor - 1
            })
        } else if keys.matches(Action::SelectDown, key) {
            Some(if self.cursor >= last {
                0
            } else {
                self.cursor + 1
            })
        } else if keys.matches(Action::SelectPageUp, key) {
            Some(self.cursor.saturating_sub(page))
        } else if keys.matches(Action::SelectPageDown, key) {
            Some((self.cursor + page).min(last))
        } else {
            None
        };
        if let Some(next) = moved {
            self.cursor = next;
            return BrowseStep::Stay;
        }
        if filter_empty {
            match key.code {
                KeyCode::Left | KeyCode::Backspace => {
                    self.up();
                    return BrowseStep::Stay;
                }
                KeyCode::Right => {
                    if let Some(Row::Entry(i)) = rows.get(self.cursor)
                        && self.entries[*i].dir
                    {
                        let path = self.dir.join(&self.entries[*i].name);
                        self.enter(path);
                    }
                    return BrowseStep::Stay;
                }
                _ => {}
            }
        }
        let before = self.filter.text.clone();
        if self.filter.key_with(*key, keys) && self.filter.text != before {
            self.cursor = 0;
            self.edited();
        }
        BrowseStep::Stay
    }

    /// Insert pasted text into the filter (a pasted path can then be opened with Enter).
    pub fn paste(&mut self, text: &str) {
        self.filter.insert(text);
        self.cursor = 0;
        self.edited();
    }

    fn up(&mut self) {
        let from = self.dir.clone();
        if let Some(parent) = from.parent() {
            self.enter(parent.to_path_buf());
            if let Some(name) = from.file_name().and_then(|n| n.to_str())
                && let Some(loader) = &self.loader
            {
                loader.request(Request::List {
                    dir: parent.to_path_buf(),
                    select: Some(name.to_owned()),
                });
            }
        }
    }

    fn go_typed(&mut self) -> BrowseStep {
        if let Some(path) = self.validated.take() {
            return BrowseStep::Chosen(path);
        }
        if self.validating {
            return BrowseStep::Stay;
        }
        let Some(path) = self.resolve(self.filter.text.trim(), &self.dir) else {
            return BrowseStep::Stay;
        };
        if let Some(loader) = &self.loader {
            loader.request(Request::Validate(path));
            self.loading = true;
            self.validating = true;
            self.note = None;
        }
        BrowseStep::Stay
    }

    /// The visible window of at most `height` rows around the cursor.
    pub fn view(&self, height: usize) -> BrowserView {
        let rows = self.rows();
        let cursor = self.cursor.min(rows.len().saturating_sub(1));
        let height = height.max(1);
        let first = cursor
            .saturating_sub(height / 2)
            .min(rows.len().saturating_sub(height));
        BrowserView {
            title: self.display(&self.dir),
            filter: self.filter.clone(),
            shown: rows.iter().filter(|r| matches!(r, Row::Entry(_))).count(),
            total: self.entries.len(),
            rows: rows
                .iter()
                .enumerate()
                .skip(first)
                .take(height)
                .map(|(n, row)| (self.label(row), n == cursor))
                .collect(),
            note: {
                let status = if self.loading {
                    Some(
                        if self.validating {
                            "Checking typed path…"
                        } else {
                            "Loading folder…"
                        }
                        .to_owned(),
                    )
                } else {
                    self.note.clone().or_else(|| {
                        is_typed_path(self.filter.text.trim())
                            .then(|| "Enter checks/opens the typed path".to_owned())
                    })
                };
                match (&self.listing_note, status) {
                    (Some(listing), Some(status)) => Some(format!("{listing} · {status}")),
                    (Some(listing), None) => Some(listing.clone()),
                    (None, status) => status,
                }
            },
        }
    }
}

fn is_typed_path(filter: &str) -> bool {
    filter.contains('/')
        || filter.starts_with('~')
        || (cfg!(windows) && (filter.contains('\\') || filter.as_bytes().get(1) == Some(&b':')))
}

#[cfg(test)]
mod tests;
