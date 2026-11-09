//! The `?` keyboard reference for the git overlay. Lists only the keys `GitOverlay::key` and
//! its edit mode handle.

use super::super::panel_keys::{SheetGroup, sheet};
use crate::ui::theme::Theme;
use ratatui::text::Line;

/// One sheet row: the key names that trigger it and what they do.
pub(super) struct Binding {
    pub keys: &'static [&'static str],
    pub label: &'static str,
}

pub(super) struct Group {
    pub title: &'static str,
    pub rows: &'static [Binding],
}

pub(super) const GROUPS: &[Group] = &[
    Group {
        title: "Move",
        rows: &[
            Binding {
                keys: &["Up", "Down", "k", "j"],
                label: "Move the cursor one row",
            },
            Binding {
                keys: &["PgUp", "PgDn"],
                label: "Move the cursor a page",
            },
            Binding {
                keys: &["Home", "End", "g", "G"],
                label: "First or last row of the diff",
            },
            Binding {
                keys: &["Left", "Right", "h", "l"],
                label: "Scroll the diff sideways",
            },
            Binding {
                keys: &["Enter"],
                label: "Next hunk in the diff; focus the diff from the sidebar",
            },
            Binding {
                keys: &["Tab"],
                label: "Switch Status / Diff / Commit / Log tabs (Shift+Tab goes back)",
            },
            Binding {
                keys: &["]", "["],
                label: "Next or previous file",
            },
            Binding {
                keys: &["Alt+Down", "Alt+Up"],
                label: "Next or previous hunk, crossing files",
            },
        ],
    },
    Group {
        title: "Stage",
        rows: &[
            Binding {
                keys: &["Space"],
                label: "Stage or unstage the selected file (sidebar) or hunk (diff)",
            },
            Binding {
                keys: &["s", "u"],
                label: "Stage or unstage the hunk under the cursor",
            },
            Binding {
                keys: &["S", "U"],
                label: "Stage all or unstage all (sidebar)",
            },
            Binding {
                keys: &["x"],
                label: "Discard the hunk under the cursor",
            },
            Binding {
                keys: &["Delete"],
                label: "Discard the file (press twice)",
            },
            Binding {
                keys: &["Shift+Up", "Shift+Down", "m"],
                label: "Extend a line range in the diff, or mark the lines",
            },
            Binding {
                keys: &["Space", "s", "u", "x"],
                label: "Stage, unstage or discard the marked lines",
            },
            Binding {
                keys: &["Esc"],
                label: "Clear the line marks",
            },
            Binding {
                keys: &["A"],
                label: "AI-group the changes for staging",
            },
        ],
    },
    Group {
        title: "View",
        rows: &[
            Binding {
                keys: &["v"],
                label: "Cycle the diff view",
            },
            Binding {
                keys: &["1", "2", "3", "4"],
                label: "File, split, inline or hunk view",
            },
            Binding {
                keys: &["b"],
                label: "Ignore whitespace",
            },
            Binding {
                keys: &["w"],
                label: "Wrap long lines",
            },
            Binding {
                keys: &["r"],
                label: "Refresh",
            },
        ],
    },
    Group {
        title: "Commit",
        rows: &[
            Binding {
                keys: &["c"],
                label: "Write the commit message",
            },
            Binding {
                keys: &["Ctrl+G"],
                label: "Generate the commit message with AI",
            },
            Binding {
                keys: &["Ctrl+S"],
                label: "Commit (in the message field)",
            },
            Binding {
                keys: &["Ctrl+T"],
                label: "Toggle amend (in the message field)",
            },
            Binding {
                keys: &["Tab", "Esc"],
                label: "Leave the message field",
            },
        ],
    },
    Group {
        title: "Panel",
        rows: &[
            Binding {
                keys: &["?"],
                label: "Show or hide this sheet",
            },
            Binding {
                keys: &["q", "Esc"],
                label: "Close the panel",
            },
        ],
    },
];

/// The sheet body `width` cells wide, drawn by the shared panel key sheet; the panel supplies the
/// title.
pub(super) fn lines(theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let groups: Vec<SheetGroup<'_>> = GROUPS
        .iter()
        .map(|group| SheetGroup {
            title: group.title,
            rows: group
                .rows
                .iter()
                .map(|binding| (binding.keys.join(" / "), binding.label.to_owned()))
                .collect(),
        })
        .collect();
    sheet(theme, &groups, "", "?/Esc", width)
}

#[cfg(test)]
mod tests;
