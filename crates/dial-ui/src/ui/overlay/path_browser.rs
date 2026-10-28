//! A keyboard file browser for path-valued settings. It lists one directory at a time (folders
//! first), filters as you type, and returns the chosen absolute path. Typing a path that contains
//! `/` (or starts with `~`) and pressing Enter jumps to it, so a known path never needs browsing.
//! Reading is bounded and read-only; the caller decides what to save.
use crate::config::schema::PathKind;
use crate::ui::editor::Editor;
use crate::ui::keys::{Action, Keymap};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};

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
}

impl PathBrowser {
    /// Open beside `current` (a saved value, possibly empty or relative to `base`), else in `base`.
    pub fn open(kind: PathKind, current: &str, base: &Path) -> Self {
        let home = crate::config::paths::home_dir();
        let mut browser = PathBrowser {
            kind,
            dir: base.to_path_buf(),
            entries: Vec::new(),
            cursor: 0,
            filter: Editor::default(),
            note: None,
            home,
        };
        let start = browser.resolve(current.trim(), base);
        let dir = match start {
            Some(path) if path.is_dir() => path,
            Some(path) => path
                .parent()
                .filter(|parent| parent.is_dir())
                .map_or_else(|| base.to_path_buf(), Path::to_path_buf),
            None => base.to_path_buf(),
        };
        browser.enter(dir);
        // Land on the saved file itself when it is listed.
        if let Some(name) = Path::new(current.trim())
            .file_name()
            .and_then(|n| n.to_str())
            && let Some(at) = browser
                .rows()
                .iter()
                .position(|row| matches!(row, Row::Entry(i) if browser.entries[*i].name == name))
        {
            browser.cursor = at;
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
        } else if let Some(rest) = typed.strip_prefix("~/") {
            self.home.as_ref()?.join(rest)
        } else {
            base.join(typed)
        };
        Some(normalize(&path))
    }

    fn enter(&mut self, dir: PathBuf) {
        self.dir = normalize(&dir);
        self.cursor = 0;
        self.filter.set("");
        self.note = None;
        self.entries.clear();
        match std::fs::read_dir(&self.dir) {
            Ok(read) => {
                for entry in read.filter_map(Result::ok).take(MAX_ENTRIES + 1) {
                    if self.entries.len() == MAX_ENTRIES {
                        self.note = Some(format!(
                            "Showing the first {MAX_ENTRIES} entries; type to filter or type a path"
                        ));
                        break;
                    }
                    let Ok(name) = entry.file_name().into_string() else {
                        continue;
                    };
                    // Follow symlinks so a linked folder can be opened.
                    let meta = std::fs::metadata(entry.path()).ok();
                    let dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
                    if self.kind == PathKind::Directory && !dir {
                        continue;
                    }
                    self.entries.push(Entry {
                        name,
                        dir,
                        executable: !dir && meta.as_ref().is_some_and(is_executable),
                    });
                }
            }
            Err(error) => self.note = Some(format!("Cannot read this folder: {error}")),
        }
        self.entries.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
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
            self.note = None;
            return BrowseStep::Stay;
        }
        if keys.matches(Action::SelectConfirm, key) {
            if is_typed_path(self.filter.text.trim()) {
                return self.go_typed();
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
            self.note = None;
        }
        BrowseStep::Stay
    }

    /// Insert pasted text into the filter (a pasted path can then be opened with Enter).
    pub fn paste(&mut self, text: &str) {
        self.filter.insert(text);
        self.cursor = 0;
    }

    fn up(&mut self) {
        let from = self.dir.clone();
        if let Some(parent) = from.parent() {
            self.enter(parent.to_path_buf());
            if let Some(name) = from.file_name().and_then(|n| n.to_str())
                && let Some(at) = self
                    .rows()
                    .iter()
                    .position(|row| matches!(row, Row::Entry(i) if self.entries[*i].name == name))
            {
                self.cursor = at;
            }
        }
    }

    fn go_typed(&mut self) -> BrowseStep {
        let typed = self.filter.text.trim().to_owned();
        let base = self.dir.clone();
        let Some(path) = self.resolve(&typed, &base) else {
            return BrowseStep::Stay;
        };
        if path.is_dir() {
            self.enter(path);
            BrowseStep::Stay
        } else if path.exists() && self.kind != PathKind::Directory {
            BrowseStep::Chosen(path)
        } else {
            self.note = Some(if path.exists() {
                format!("{} is not a folder", self.display(&path))
            } else {
                format!("{} does not exist", self.display(&path))
            });
            BrowseStep::Stay
        }
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
            note: self.note.clone().or_else(|| {
                is_typed_path(self.filter.text.trim())
                    .then(|| "Enter opens the typed path".to_owned())
            }),
        }
    }
}

fn is_typed_path(filter: &str) -> bool {
    filter.contains('/') || filter.starts_with('~')
}

/// Remove `.` and `..` components without touching the filesystem (symlinks stay as typed).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests;
