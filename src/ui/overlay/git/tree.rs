//! Sidebar file tree: changed files grouped under folded directories.
//! The build and flatten steps are pure; the navigation methods move the overlay cursor over
//! the visible rows only, so collapsed children are never selected.

use super::{GitOverlay, model::*};
use std::collections::{BTreeMap, HashSet};

/// One visible sidebar row under a section header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Entry {
    /// A directory. `name` is the display text (single-child chains joined with `/`).
    Dir {
        key: String,
        name: String,
        depth: usize,
        collapsed: bool,
    },
    /// A changed file; `index` points into the overlay's `changes`.
    File { index: usize, depth: usize },
}

impl Entry {
    pub(super) fn depth(&self) -> usize {
        match self {
            Entry::Dir { depth, .. } | Entry::File { depth, .. } => *depth,
        }
    }
}

/// The collapsed-set key of a directory inside one section.
pub(super) fn dir_key(area: Area, path: &str) -> String {
    let section = match area {
        Area::Staged => "staged",
        Area::Unstaged => "unstaged",
        Area::Commit => "commit",
    };
    format!("{section}:{path}")
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    /// Indexes into `changes`, in list order.
    files: Vec<usize>,
}

/// Visible rows of one section: directories first (sorted by name), then files, at each level.
pub(super) fn flatten(changes: &[Change], area: Area, collapsed: &HashSet<String>) -> Vec<Entry> {
    let mut root = Node::default();
    for (index, change) in changes.iter().enumerate() {
        if change.area != area {
            continue;
        }
        let parts: Vec<&str> = change.path.split('/').collect();
        let Some((_file, dirs)) = parts.split_last() else {
            continue;
        };
        let mut node = &mut root;
        for part in dirs {
            node = node.dirs.entry((*part).to_owned()).or_default();
        }
        node.files.push(index);
    }
    let mut out = Vec::new();
    walk(&root, "", 0, area, collapsed, &mut out);
    out
}

fn walk(
    node: &Node,
    prefix: &str,
    depth: usize,
    area: Area,
    collapsed: &HashSet<String>,
    out: &mut Vec<Entry>,
) {
    for (name, child) in &node.dirs {
        // Single-directory chains with no files of their own collapse into one row.
        let mut label = name.clone();
        let mut path = join(prefix, name);
        let mut tail = child;
        while tail.files.is_empty() && tail.dirs.len() == 1 {
            let Some((next, only)) = tail.dirs.first_key_value() else {
                break;
            };
            label = format!("{label}/{next}");
            path = format!("{path}/{next}");
            tail = only;
        }
        let key = dir_key(area, &path);
        let folded = collapsed.contains(&key);
        out.push(Entry::Dir {
            key,
            name: label,
            depth,
            collapsed: folded,
        });
        if !folded {
            walk(tail, &path, depth + 1, area, collapsed, out);
        }
    }
    for &index in &node.files {
        out.push(Entry::File { index, depth });
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// The directory row a visible row sits under: the nearest earlier row with a smaller depth.
pub(super) fn parent_of(entries: &[Entry], index: usize) -> Option<usize> {
    let depth = entries.get(index)?.depth();
    (0..index).rev().find(|&i| entries[i].depth() < depth)
}

impl GitOverlay {
    /// Visible sidebar rows in display order: staged then unstaged, or the commit's files.
    pub(super) fn entries(&self) -> Vec<Entry> {
        if self.commit.is_some() {
            return flatten(&self.changes, Area::Commit, &self.collapsed);
        }
        let mut out = flatten(&self.changes, Area::Staged, &self.collapsed);
        out.extend(flatten(&self.changes, Area::Unstaged, &self.collapsed));
        out
    }

    /// Row under the cursor when it is a file, or the first file row when the cursor is out of range.
    pub(super) fn first_file_row(&self) -> usize {
        self.entries()
            .iter()
            .position(|entry| matches!(entry, Entry::File { .. }))
            .unwrap_or(0)
    }

    /// Moves the file cursor to the neighbouring file row, skipping folder rows and collapsed
    /// children. Lands on the first hunk going forward, the last going back.
    pub(super) fn select_file(&mut self, delta: isize) {
        let entries = self.entries();
        let is_file = |entry: &Entry| matches!(entry, Entry::File { .. });
        let next = if delta < 0 {
            entries[..self.cursor.min(entries.len())]
                .iter()
                .rposition(is_file)
        } else {
            entries
                .iter()
                .enumerate()
                .skip(self.cursor + 1)
                .find(|(_, entry)| is_file(entry))
                .map(|(index, _)| index)
        };
        if let Some(index) = next {
            self.cursor = index;
            self.open_cursor(delta);
        }
    }

    /// Sidebar `up`/`down`: moves one visible row, folder rows included.
    pub(super) fn move_side(&mut self, delta: isize) {
        let last = self.entries().len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
        if self.selected().is_some() {
            self.open_cursor(delta);
        } else {
            self.load_shown();
        }
    }

    /// Sidebar `enter`: folds or unfolds a directory row; on a file, focuses the diff.
    pub(super) fn side_enter(&mut self) {
        match self.entries().get(self.cursor) {
            Some(Entry::Dir { key, .. }) => {
                let key = key.clone();
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
            }
            Some(Entry::File { .. }) => self.focus = Focus::Diff,
            None => {}
        }
    }

    /// Sidebar `left`: folds an open directory, otherwise jumps to the parent directory row.
    pub(super) fn side_left(&mut self) {
        let entries = self.entries();
        match entries.get(self.cursor) {
            Some(Entry::Dir {
                key,
                collapsed: false,
                ..
            }) => {
                self.collapsed.insert(key.clone());
            }
            Some(_) => {
                if let Some(parent) = parent_of(&entries, self.cursor) {
                    self.cursor = parent;
                    self.load_shown();
                }
            }
            None => {}
        }
    }

    /// Sidebar `right`: unfolds a folded directory, steps into an open one, focuses the diff on a file.
    pub(super) fn side_right(&mut self) {
        let entries = self.entries();
        match entries.get(self.cursor) {
            Some(Entry::Dir {
                key,
                collapsed: true,
                ..
            }) => {
                self.collapsed.remove(key);
            }
            Some(Entry::Dir { .. }) => self.move_side(1),
            Some(Entry::File { .. }) => self.focus = Focus::Diff,
            None => {}
        }
    }

    /// Lands on the first or last hunk of the file under the cursor, as `select_file` always has.
    fn open_cursor(&mut self, delta: isize) {
        self.hunk = if delta < 0 { usize::MAX } else { 0 };
        self.load_shown();
        let target = if delta < 0 {
            self.hunk_rows().last()
        } else {
            self.hunk_rows().next()
        };
        self.cursor_row = target.unwrap_or(0);
        self.sync_hunk();
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{Area, Change, Kind};
    use super::*;
    use crate::git_panel::change_paths;

    fn change(path: &str, area: Area) -> Change {
        Change {
            path: path.to_owned(),
            orig: None,
            kind: Kind::Modified,
            area,
        }
    }

    fn dir(key: &str, name: &str, depth: usize, collapsed: bool) -> Entry {
        Entry::Dir {
            key: key.to_owned(),
            name: name.to_owned(),
            depth,
            collapsed,
        }
    }

    fn sample() -> Vec<Change> {
        vec![
            change("src/a.rs", Area::Unstaged),
            change("src/ui/overlay/x.rs", Area::Unstaged),
            change("src/ui/y.rs", Area::Unstaged),
            change("README.md", Area::Unstaged),
        ]
    }

    #[test]
    fn nested_directories_flatten_depth_first() {
        let rows = flatten(&sample(), Area::Unstaged, &HashSet::new());
        assert_eq!(
            rows,
            vec![
                dir("unstaged:src", "src", 0, false),
                dir("unstaged:src/ui", "ui", 1, false),
                dir("unstaged:src/ui/overlay", "overlay", 2, false),
                Entry::File { index: 1, depth: 3 },
                Entry::File { index: 2, depth: 2 },
                Entry::File { index: 0, depth: 1 },
                Entry::File { index: 3, depth: 0 },
            ]
        );
    }

    #[test]
    fn collapsed_directory_hides_its_children() {
        let mut collapsed = HashSet::new();
        collapsed.insert("unstaged:src/ui".to_owned());
        let rows = flatten(&sample(), Area::Unstaged, &collapsed);
        assert_eq!(
            rows,
            vec![
                dir("unstaged:src", "src", 0, false),
                dir("unstaged:src/ui", "ui", 1, true),
                Entry::File { index: 0, depth: 1 },
                Entry::File { index: 3, depth: 0 },
            ]
        );
    }

    #[test]
    fn single_directory_chains_compress() {
        let changes = vec![
            change("a/b/c.txt", Area::Unstaged),
            change("a/b/d/e.txt", Area::Unstaged),
        ];
        let rows = flatten(&changes, Area::Unstaged, &HashSet::new());
        assert_eq!(
            rows,
            vec![
                dir("unstaged:a/b", "a/b", 0, false),
                dir("unstaged:a/b/d", "d", 1, false),
                Entry::File { index: 1, depth: 2 },
                Entry::File { index: 0, depth: 1 },
            ]
        );
    }

    #[test]
    fn sections_are_separate() {
        let mut changes = sample();
        changes[0].area = Area::Staged;
        let staged = flatten(&changes, Area::Staged, &HashSet::new());
        assert_eq!(
            staged,
            vec![
                dir("staged:src", "src", 0, false),
                Entry::File { index: 0, depth: 1 },
            ]
        );
    }

    #[test]
    fn parent_of_finds_enclosing_directory() {
        let rows = flatten(&sample(), Area::Unstaged, &HashSet::new());
        assert_eq!(parent_of(&rows, 3), Some(2));
        assert_eq!(parent_of(&rows, 2), Some(1));
        assert_eq!(parent_of(&rows, 0), None);
        assert_eq!(parent_of(&rows, 6), None);
    }

    #[test]
    fn rename_entry_yields_both_paths() {
        let renamed = Change {
            path: "new/name.rs".to_owned(),
            orig: Some("old.rs".to_owned()),
            kind: Kind::Renamed,
            area: Area::Staged,
        };
        let rows = flatten(
            std::slice::from_ref(&renamed),
            Area::Staged,
            &HashSet::new(),
        );
        assert_eq!(
            rows,
            vec![
                dir("staged:new", "new", 0, false),
                Entry::File { index: 0, depth: 1 }
            ]
        );
        assert_eq!(change_paths(&renamed), vec!["new/name.rs", "old.rs"]);
    }

    #[test]
    fn unstaged_half_of_a_staged_rename_stages_only_its_path() {
        let half = Change {
            path: "new.rs".to_owned(),
            orig: Some("old.rs".to_owned()),
            kind: Kind::Modified,
            area: Area::Unstaged,
        };
        assert_eq!(change_paths(&half), vec!["new.rs"]);
    }
}
