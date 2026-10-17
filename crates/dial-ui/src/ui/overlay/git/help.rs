//! The `?` keyboard reference for the git overlay. Lists only the keys `GitOverlay::key` and
//! its edit mode handle.

use crate::ui::{
    chrome,
    theme::{Theme, Tok},
};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};

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
                keys: &["up", "down", "k", "j"],
                label: "Move the cursor one row",
            },
            Binding {
                keys: &["pgup", "pgdn"],
                label: "Move the cursor a page",
            },
            Binding {
                keys: &["home", "end", "g", "G"],
                label: "First or last row of the diff",
            },
            Binding {
                keys: &["left", "right", "h", "l"],
                label: "Scroll the diff sideways",
            },
            Binding {
                keys: &["enter"],
                label: "Next hunk in the diff; focus the diff from the sidebar",
            },
            Binding {
                keys: &["tab"],
                label: "Switch between the diff and the changes",
            },
            Binding {
                keys: &["]", "["],
                label: "Next or previous file",
            },
            Binding {
                keys: &["alt+down", "alt+up"],
                label: "Next or previous hunk, crossing files",
            },
        ],
    },
    Group {
        title: "Stage",
        rows: &[
            Binding {
                keys: &["space"],
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
                keys: &["delete"],
                label: "Discard the file (press twice)",
            },
            Binding {
                keys: &["shift+up", "shift+down", "m"],
                label: "Extend a line range in the diff, or mark the lines",
            },
            Binding {
                keys: &["space", "s", "u", "x"],
                label: "Stage, unstage or discard the marked lines",
            },
            Binding {
                keys: &["esc"],
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
                keys: &["ctrl+g"],
                label: "Generate the commit message with AI",
            },
            Binding {
                keys: &["ctrl+s"],
                label: "Commit (in the message field)",
            },
            Binding {
                keys: &["ctrl+t"],
                label: "Toggle amend (in the message field)",
            },
            Binding {
                keys: &["tab", "esc"],
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
                keys: &["q", "esc"],
                label: "Close the panel",
            },
        ],
    },
];

/// The sheet body, sized to its content; the panel frame supplies the border and title.
pub(super) fn lines(theme: &Theme) -> Vec<Line<'static>> {
    let keys_text = |binding: &Binding| binding.keys.join(" / ");
    let key_w = GROUPS
        .iter()
        .flat_map(|group| group.rows)
        .map(|binding| keys_text(binding).chars().count())
        .max()
        .unwrap_or(0);
    let mut out = Vec::new();
    for (index, group) in GROUPS.iter().enumerate() {
        if index > 0 {
            out.push(Line::default());
        }
        out.push(Line::from(vec![
            Span::raw(" "),
            chrome::label(theme, group.title),
        ]));
        for binding in group.rows {
            out.push(Line::from(vec![
                Span::styled(
                    format!("   {:<key_w$}", keys_text(binding)),
                    theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {}", binding.label), theme.fg(Tok::Text)),
            ]));
        }
    }
    out.push(Line::default());
    let mut footer = vec![Span::raw(" ")];
    footer.extend(chrome::key_hint_spans(theme, &[("?/Esc", "close")]));
    out.push(Line::from(footer));
    out
}

#[cfg(test)]
mod tests;
