use super::*;

#[test]
fn computer_native_labels_decode_mnemonics_and_accelerators() {
    assert_eq!(
        native_label("&Open…\tCtrl+O"),
        ("Open…".into(), Some("Ctrl+O".into()))
    );
    assert_eq!(native_label("Rock && &Roll"), ("Rock & Roll".into(), None));
    assert_eq!(
        native_label("&Über…\t  Alt+Ü  "),
        ("Über…".into(), Some("Alt+Ü".into()))
    );
    assert_eq!(native_label("Save &\t "), ("Save &".into(), None));
}

#[test]
fn computer_native_command_ids_are_never_silently_truncated() {
    assert_eq!(native_message(42, 3, 0).unwrap(), (WM_COMMAND, 42));
    assert_eq!(native_message(0, 3, 0).unwrap(), (WM_COMMAND, 0));
    assert!(native_message(0x1_0000, 3, 0).is_err());
    assert!(native_message(u32::MAX, 3, 0).is_err());
    assert_eq!(
        native_message(u32::MAX, 3, MNS_NOTIFYBYPOS).unwrap(),
        (WM_MENUCOMMAND, 3)
    );
}
