use std::time::Duration;

pub(super) use crate::git_panel::{
    Area, Change, Commit, Doc, Hunk, Kind, hunk_start, sort_changes,
};

pub(super) const REFRESH: Duration = Duration::from_secs(2);
pub(super) const STATUS_TTL: Duration = Duration::from_secs(6);
pub(super) const CONTEXT: &str = "-U3";
pub(super) const FULL_CONTEXT: &str = "-U1000000";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum View {
    File,
    Split,
    Inline,
    Hunk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    Diff,
    Sidebar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug)]
pub(super) struct Col {
    pub(super) num: u32,
    pub(super) side: Side,
    pub(super) text: String,
}

#[derive(Clone, Debug)]
pub(super) enum RowKind {
    Meta(String),
    Hunk(String),
    Line {
        side: Side,
        old: Option<u32>,
        new: Option<u32>,
        text: String,
    },
    Pair {
        left: Option<Col>,
        right: Option<Col>,
    },
}

#[derive(Clone, Debug)]
pub(super) struct Row {
    pub(super) hunk: Option<usize>,
    pub(super) kind: RowKind,
}

pub(super) struct Shown {
    pub(super) change: Change,
    /// Three lines of context: used for hunk operations and the default views.
    pub(super) doc: Doc,
    /// Whole-file diff, loaded only while the file view is active.
    pub(super) full: Option<Doc>,
    /// Set for binary files: a metadata placeholder replaces the diff.
    pub(super) binary: Option<Binary>,
}

/// What the binary placeholder shows: the type label, pixel size when the header parses, and bytes.
/// `previews` is set only when the terminal can draw images and at least one side decoded.
pub(super) struct Binary {
    pub(super) kind: &'static str,
    pub(super) size: u64,
    pub(super) dims: Option<(u32, u32)>,
    pub(super) previews: Option<Previews>,
}

/// Decoded raster images for both sides of a changed image file.
pub(super) struct Previews {
    pub(super) before: Option<crate::ui::images::Thumbnail>,
    pub(super) after: Option<crate::ui::images::Thumbnail>,
}

pub(super) fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(index) => path.split_at(index + 1),
        None => ("", path),
    }
}

/// Diff header lines that carry nothing the panel header does not already show.
fn is_routine_head(line: &str) -> bool {
    ["diff --git ", "index ", "--- ", "+++ "]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

/// Flattens a parsed diff into display rows. `only` restricts the output to one hunk.
pub(super) fn layout(doc: &Doc, view: View, only: Option<usize>) -> Vec<Row> {
    let mut rows = Vec::new();
    if only.is_none() {
        // The `diff --git`, `index`, `---` and `+++` lines repeat what the panel header already
        // says; only notable lines (new, deleted or renamed file, mode changes) are kept.
        rows.extend(
            doc.head
                .iter()
                .filter(|line| !is_routine_head(line))
                .map(|line| Row {
                    hunk: None,
                    kind: RowKind::Meta(line.clone()),
                }),
        );
    }
    for (index, hunk) in doc.hunks.iter().enumerate() {
        if only.is_some_and(|o| o != index) {
            continue;
        }
        rows.push(Row {
            hunk: Some(index),
            kind: RowKind::Hunk(hunk.header.clone()),
        });
        let (mut old, mut new) = hunk_start(&hunk.header);
        if view == View::Split {
            split_rows(&mut rows, index, &hunk.lines, &mut old, &mut new);
            continue;
        }
        for line in &hunk.lines {
            let mut chars = line.chars();
            let marker = chars.next().unwrap_or(' ');
            let text = shown(chars.as_str()).to_owned();
            let kind = match marker {
                '+' => {
                    let kind = RowKind::Line {
                        side: Side::Added,
                        old: None,
                        new: Some(new),
                        text,
                    };
                    new += 1;
                    kind
                }
                '-' => {
                    let kind = RowKind::Line {
                        side: Side::Removed,
                        old: Some(old),
                        new: None,
                        text,
                    };
                    old += 1;
                    kind
                }
                '\\' => RowKind::Meta(line.clone()),
                _ => {
                    let kind = RowKind::Line {
                        side: Side::Context,
                        old: Some(old),
                        new: Some(new),
                        text,
                    };
                    old += 1;
                    new += 1;
                    kind
                }
            };
            rows.push(Row {
                hunk: Some(index),
                kind,
            });
        }
    }
    rows
}

/// Hunk text as displayed: a CRLF file leaves a `\r` at the end of each line, which would corrupt the terminal.
fn shown(text: &str) -> &str {
    text.strip_suffix('\r').unwrap_or(text)
}

fn split_rows(rows: &mut Vec<Row>, hunk: usize, lines: &[String], old: &mut u32, new: &mut u32) {
    let mut dels: Vec<Col> = Vec::new();
    let mut adds: Vec<Col> = Vec::new();
    for line in lines {
        let mut chars = line.chars();
        let marker = chars.next().unwrap_or(' ');
        let text = shown(chars.as_str()).to_owned();
        match marker {
            '-' => {
                dels.push(Col {
                    num: *old,
                    side: Side::Removed,
                    text,
                });
                *old += 1;
            }
            '+' => {
                adds.push(Col {
                    num: *new,
                    side: Side::Added,
                    text,
                });
                *new += 1;
            }
            '\\' => {
                flush_pairs(rows, hunk, &mut dels, &mut adds);
                rows.push(Row {
                    hunk: Some(hunk),
                    kind: RowKind::Meta(line.clone()),
                });
            }
            _ => {
                flush_pairs(rows, hunk, &mut dels, &mut adds);
                rows.push(Row {
                    hunk: Some(hunk),
                    kind: RowKind::Pair {
                        left: Some(Col {
                            num: *old,
                            side: Side::Context,
                            text: text.clone(),
                        }),
                        right: Some(Col {
                            num: *new,
                            side: Side::Context,
                            text,
                        }),
                    },
                });
                *old += 1;
                *new += 1;
            }
        }
    }
    flush_pairs(rows, hunk, &mut dels, &mut adds);
}

/// Pairs a run of removed and added lines side by side.
fn flush_pairs(rows: &mut Vec<Row>, hunk: usize, dels: &mut Vec<Col>, adds: &mut Vec<Col>) {
    let mut removed = dels.drain(..);
    let mut added = adds.drain(..);
    loop {
        let left = removed.next();
        let right = added.next();
        if left.is_none() && right.is_none() {
            break;
        }
        rows.push(Row {
            hunk: Some(hunk),
            kind: RowKind::Pair { left, right },
        });
    }
}
