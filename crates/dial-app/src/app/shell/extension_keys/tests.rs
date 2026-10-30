use super::*;
#[test]
fn shortcuts_cannot_shadow_builtins_or_plain_typing() {
    let keys = Keymap::defaults();
    for key in ["a", "Shift+A", "Ctrl+C", "Enter", "Tab", "Esc"] {
        assert!(
            admitted(Chord::parse(key).unwrap(), &keys).is_err(),
            "{key}"
        );
    }
    assert!(admitted(Chord::parse("Ctrl+Alt+B").unwrap(), &keys).is_ok());
}
#[test]
fn overrides_roundtrip_and_route_only_the_registered_chord() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(FILE);
    let map = BTreeMap::from([
        ("demo/build".to_owned(), "Ctrl+Alt+B".to_owned()),
        ("demo/off".to_owned(), String::new()),
    ]);
    crate::platform::atomic::write_private(&path, toml::to_string_pretty(&map).unwrap().as_bytes())
        .unwrap();
    assert_eq!(overrides(&path).unwrap(), map);
    let mut keys = ExtensionKeys::default();
    keys.bindings.push((
        "demo".into(),
        "build".into(),
        Chord::parse("Ctrl+Alt+B").unwrap(),
    ));
    let builtin = Keymap::defaults();
    assert_eq!(
        keys.key(
            KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ),
            &builtin
        ),
        Some("/build".into())
    );
    assert_eq!(
        keys.key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &builtin
        ),
        None
    );
    assert_eq!(
        keys.key(
            KeyEvent::new_with_kind(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                KeyEventKind::Release
            ),
            &builtin
        ),
        None
    );
}

#[test]
fn workspace_overrides_merge_without_mutating_global_bindings() {
    let user = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        user.path().join(FILE),
        "\"demo/run\" = \"Ctrl+Alt+B\"\n\"demo/keep\" = \"Ctrl+Alt+K\"\n",
    )
    .unwrap();
    std::fs::write(workspace.path().join(FILE), "\"demo/run\" = \"\"\n").unwrap();
    let merged = layered_overrides(user.path(), workspace.path()).unwrap();
    assert_eq!(merged["demo/run"], "");
    assert_eq!(merged["demo/keep"], "Ctrl+Alt+K");
    assert_eq!(
        overrides(&user.path().join(FILE)).unwrap()["demo/run"],
        "Ctrl+Alt+B"
    );
}
