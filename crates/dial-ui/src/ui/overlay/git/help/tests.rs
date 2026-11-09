use super::*;

/// Every key name the overlay responds to (`mod.rs` key dispatch, the diff and sidebar
/// handlers, and the message editor). Keep in sync when a binding is added.
const BINDING_NAMES: &[&str] = &[
    "Up",
    "Down",
    "k",
    "j",
    "PgUp",
    "PgDn",
    "Home",
    "End",
    "g",
    "G",
    "Left",
    "Right",
    "h",
    "l",
    "Enter",
    "Tab",
    "]",
    "[",
    "Alt+Down",
    "Alt+Up",
    "Shift+Up",
    "Shift+Down",
    "Space",
    "s",
    "u",
    "S",
    "U",
    "x",
    "m",
    "A",
    "Delete",
    "v",
    "1",
    "2",
    "3",
    "4",
    "b",
    "w",
    "r",
    "c",
    "Ctrl+G",
    "Ctrl+S",
    "Ctrl+T",
    "Esc",
    "q",
    "?",
];

#[test]
fn help_table_covers_every_binding_name() {
    for name in BINDING_NAMES {
        assert!(
            GROUPS
                .iter()
                .flat_map(|group| group.rows)
                .any(|binding| binding.keys.contains(name)),
            "help sheet is missing `{name}`"
        );
    }
}

#[test]
fn help_table_lists_no_unknown_names() {
    for binding in GROUPS.iter().flat_map(|group| group.rows) {
        for name in binding.keys {
            assert!(BINDING_NAMES.contains(name), "unknown help key `{name}`");
        }
    }
}
