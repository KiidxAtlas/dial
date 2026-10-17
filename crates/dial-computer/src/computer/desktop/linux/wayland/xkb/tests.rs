use std::{
    fs::File,
    io::Write,
    os::fd::{FromRawFd, OwnedFd},
};

use super::{KeyStroke, KeyboardLayout};

const FR: &str = include_str!("../testdata/fr.xkb");
const US_FR: &str = include_str!("../testdata/us-fr.xkb");

fn stroke(keycode: u32, modifiers: &[u32]) -> KeyStroke {
    KeyStroke {
        keycode,
        modifiers: modifiers.to_vec(),
    }
}

/// Newer libxkbcommon (e.g. 1.13) serializes group labels as `symbols[1]`.
fn numeric_group_labels(keymap: &str) -> String {
    keymap
        .replace("symbols[Group", "symbols[")
        .replace("type[Group", "type[")
}

#[test]
fn reads_the_keymap_from_the_start_of_a_shared_fd() {
    // SAFETY: `memfd_create` returns a fresh descriptor or -1; ownership
    // moves into the `File` exactly once.
    let mut file = unsafe {
        let fd = libc::memfd_create(c"xkb".as_ptr(), 0);
        assert!(fd >= 0, "memfd_create failed");
        File::from_raw_fd(fd)
    };
    // The compositor writes the keymap and leaves the shared offset at EOF.
    file.write_all(FR.as_bytes()).unwrap();
    let fd = OwnedFd::from(file.try_clone().unwrap());

    let layout = KeyboardLayout::from_fd(fd, FR.len()).expect("keymap at offset 0 must compile");
    assert_eq!(layout.resolve_char('a'), Some(stroke(16, &[])));
}

#[test]
fn resolves_numeric_group_labels() {
    let mut layout = KeyboardLayout::compile(&numeric_group_labels(US_FR))
        .expect("US/French fixture must compile");
    assert_eq!(layout.resolve_char('a'), Some(stroke(30, &[])));
    assert_eq!(layout.resolve_char('@'), Some(stroke(3, &[42])));
    layout.update_modifiers(0, 0, 0, 1);
    assert_eq!(layout.resolve_char('a'), Some(stroke(16, &[])));
    assert_eq!(layout.resolve_char('é'), Some(stroke(3, &[])));

    let layout =
        KeyboardLayout::compile(&numeric_group_labels(FR)).expect("French fixture must compile");
    assert_eq!(layout.resolve_char('#'), Some(stroke(4, &[100])));
}

#[test]
fn resolves_french_levels_and_altgr() {
    let layout = KeyboardLayout::compile(FR).expect("French fixture must compile");
    assert_eq!(layout.resolve_char('a'), Some(stroke(16, &[])));
    assert_eq!(layout.resolve_char('A'), Some(stroke(16, &[42])));
    assert_eq!(layout.resolve_char('é'), Some(stroke(3, &[])));
    assert_eq!(layout.resolve_char('1'), Some(stroke(2, &[42])));
    assert_eq!(layout.resolve_char('#'), Some(stroke(4, &[100])));
}

#[test]
fn resolves_only_the_active_group_in_a_multi_layout_keymap() {
    let mut layout = KeyboardLayout::compile(US_FR).expect("US/French fixture must compile");
    assert_eq!(layout.resolve_char('a'), Some(stroke(30, &[])));
    assert_eq!(layout.resolve_char('q'), Some(stroke(16, &[])));
    assert_eq!(layout.resolve_char('é'), None);
    layout.update_modifiers(0, 0, 0, 1);
    assert_eq!(layout.resolve_char('a'), Some(stroke(16, &[])));
    assert_eq!(layout.resolve_char('é'), Some(stroke(3, &[])));
}

#[test]
fn modifier_event_group_change_updates_resolution() {
    let mut layout = KeyboardLayout::compile(US_FR).expect("US/French fixture must compile");
    assert_eq!(layout.resolve_char('a'), Some(stroke(30, &[])));
    layout.update_modifiers(0, 0, 0, 1);
    assert_eq!(layout.active_group(), 1);
    assert_eq!(layout.resolve_char('a'), Some(stroke(16, &[])));
}

#[test]
fn serialized_modifier_fields_affect_resolution() {
    let mut layout = KeyboardLayout::compile(US_FR).expect("US/French fixture must compile");
    let shift = 1;
    layout.update_modifiers(shift, 0, 0, 0);
    assert_eq!(layout.resolve_char('A'), Some(stroke(30, &[])));
    layout.update_modifiers(0, shift, 0, 0);
    assert_eq!(layout.resolve_char('A'), Some(stroke(30, &[])));
    layout.update_modifiers(0, 0, shift, 0);
    assert_eq!(layout.resolve_char('A'), Some(stroke(30, &[])));
}

#[test]
fn num_lock_does_not_change_non_keypad_levels() {
    let mut layout = KeyboardLayout::compile(FR).expect("French fixture must compile");
    layout.update_modifiers(0, 0, 1 << 4, 0);

    assert_eq!(layout.resolve_char('a'), Some(stroke(16, &[])));
    assert_eq!(layout.resolve_char('1'), Some(stroke(2, &[42])));
}

#[test]
fn caps_lock_uses_the_alphabetic_type_map() {
    let mut layout = KeyboardLayout::compile(FR).expect("French fixture must compile");
    layout.update_modifiers(0, 0, 1 << 1, 0);

    assert_eq!(layout.resolve_char('a'), Some(stroke(16, &[42])));
    assert_eq!(layout.resolve_char('A'), Some(stroke(16, &[])));
    assert_eq!(layout.resolve_char('é'), Some(stroke(3, &[])));
}
