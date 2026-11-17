use super::*;
use crossterm::event::KeyEventKind;

fn typed(glyph: char, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(glyph), modifiers)
}

#[test]
fn option_characters_read_back_as_the_alt_chord_that_typed_them() {
    for (glyph, base, shift) in [
        ('˚', 'k', false),
        ('µ', 'm', false),
        ('®', 'r', false),
        ('π', 'p', false),
        ('ø', 'o', false),
        ('ß', 's', false),
        ('å', 'a', false),
        ('∑', 'w', false),
        ('¬', 'l', false),
        ('√', 'v', false),
        ('‘', ']', false),
        ('–', '-', false),
        ('Å', 'a', true),
    ] {
        let mut want = KeyModifiers::ALT;
        if shift {
            want |= KeyModifiers::SHIFT;
        }
        // Terminals report the bare character, sometimes with Alt or (for Shift glyphs) Shift.
        for reported in [KeyModifiers::NONE, KeyModifiers::ALT, want] {
            let alias = as_alt_chord(&typed(glyph, reported)).expect("an Option character");
            assert_eq!(
                (alias.code, alias.modifiers),
                (KeyCode::Char(base), want),
                "{glyph}"
            );
        }
    }
}

#[test]
fn ordinary_characters_and_other_modifiers_are_not_chords() {
    for glyph in ['k', 'K', '§', 'é', '日', ' ', '-'] {
        assert!(
            as_alt_chord(&typed(glyph, KeyModifiers::NONE)).is_none(),
            "{glyph}"
        );
    }
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::SUPER,
        KeyModifiers::SHIFT,
    ] {
        assert!(
            as_alt_chord(&typed('˚', modifiers)).is_none(),
            "{modifiers:?}"
        );
    }
    let mut release = typed('˚', KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(as_alt_chord(&release).unwrap().kind, KeyEventKind::Release);
}

#[test]
fn every_table_entry_is_unique_and_names_a_bindable_key() {
    for (index, (glyph, base, _)) in US_OPTION.iter().enumerate() {
        assert!(!glyph.is_ascii(), "{glyph}");
        assert!(base.is_ascii_graphic(), "{base}");
        assert!(
            US_OPTION[index + 1..]
                .iter()
                .all(|(other, ..)| other != glyph),
            "duplicate {glyph}"
        );
    }
}

#[test]
fn only_the_us_and_abc_layouts_enable_the_translation() {
    let prefs = |kind: &str, name: &str| {
        let mut source = plist::Dictionary::new();
        source.insert("InputSourceKind".into(), kind.into());
        source.insert("KeyboardLayout Name".into(), name.into());
        let mut prefs = plist::Dictionary::new();
        prefs.insert(
            "AppleSelectedInputSources".into(),
            plist::Value::Array(vec![source.into()]),
        );
        plist::Value::Dictionary(prefs)
    };
    assert!(selected_layout_is_us(&prefs("Keyboard Layout", "U.S.")));
    assert!(selected_layout_is_us(&prefs("Keyboard Layout", "ABC")));
    assert!(!selected_layout_is_us(&prefs("Keyboard Layout", "German")));
    assert!(!selected_layout_is_us(&prefs(
        "Keyboard Layout",
        "Swedish - Pro"
    )));
    assert!(!selected_layout_is_us(&prefs("Input Mode", "U.S.")));
    assert!(!selected_layout_is_us(&plist::Value::Dictionary(
        plist::Dictionary::new()
    )));
}
