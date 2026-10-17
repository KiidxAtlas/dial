use super::*;

fn item(title: &str) -> DesktopMenuItem {
    DesktopMenuItem {
        title: title.into(),
        path: vec![title.into()],
        enabled: true,
        checked: false,
        has_submenu: false,
        shortcut: None,
    }
}

#[test]
fn computer_exact_label_wins_before_ellipsis_normalization() {
    assert_eq!(
        match_index(&[item("Open…"), item("Open")], "OPEN").unwrap(),
        1
    );
    assert_eq!(match_index(&[item("Open…")], "open...").unwrap(), 0);
}

#[test]
fn computer_duplicate_and_normalized_ambiguities_refuse() {
    assert!(match_index(&[item("Save"), item("SAVE")], "save").is_err());
    assert!(match_index(&[item("Save…"), item("Save...")], "save").is_err());
    assert!(match_index(&[item("Save")], "Save As").is_err());
}

#[test]
fn computer_disabled_or_submenu_never_qualifies_as_a_command() {
    let mut disabled = item("Save");
    disabled.enabled = false;
    let items = [disabled, item("Save…")];
    let index = match_index(&items, "Save").unwrap();
    assert!(require_command(&items[index]).is_err());
    let mut submenu = item("File");
    submenu.has_submenu = true;
    assert!(require_command(&submenu).is_err());
}

#[test]
fn computer_invalid_paths_fail_before_native_access() {
    assert!(validate_path(&[], false).is_err());
    assert!(validate_path(&["\0".into()], true).is_err());
    assert!(validate_path(&[" ".into()], true).is_err());
    assert!(validate_path(&vec!["File".into(); 33], true).is_err());
}
