use super::*;

fn press(editor: &mut Editor, code: KeyCode, mods: KeyModifiers) -> bool {
    editor.key(KeyEvent::new(code, mods))
}

fn typed(editor: &mut Editor, text: &str) {
    for c in text.chars() {
        press(editor, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

#[test]
fn alt_backspace_deletes_the_previous_word_and_preserves_unicode_boundaries() {
    let mut editor = Editor::default();
    editor.set("naïve 🦀 word");

    assert!(editor.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)));
    assert_eq!(editor.text, "naïve 🦀 ");
    assert_eq!(editor.cursor, editor.text.len());
}

#[test]
fn alt_backspace_skips_spaces_before_deleting_word() {
    let mut editor = Editor::default();
    editor.set("keep word   ");

    editor.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
    assert_eq!(editor.text, "keep ");
    assert_eq!(editor.cursor, editor.text.len());
}

#[test]
fn typing_coalesces_into_word_sized_undo_units_and_redo_restores_them() {
    let mut e = Editor::default();
    typed(&mut e, "hello world");
    assert!(e.undo());
    assert_eq!(e.text, "hello ");
    assert!(e.undo());
    assert_eq!(e.text, "hello");
    assert!(e.undo());
    assert_eq!(e.text, "");
    assert!(!e.undo(), "nothing left to undo");
    assert!(e.redo());
    assert_eq!(e.text, "hello");
    assert!(e.redo());
    assert!(e.redo());
    assert_eq!(e.text, "hello world");
    assert_eq!(e.cursor, e.text.len());
    // A new edit clears redo.
    e.undo();
    typed(&mut e, "!");
    assert!(!e.redo());
    assert_eq!(e.text, "hello !");
}

#[test]
fn cursor_motion_breaks_a_typing_run_and_undo_keys_are_bound() {
    let mut e = Editor::default();
    typed(&mut e, "ab");
    press(&mut e, KeyCode::Left, KeyModifiers::NONE);
    typed(&mut e, "X");
    assert_eq!(e.text, "aXb");
    assert!(press(&mut e, KeyCode::Char('-'), KeyModifiers::CONTROL));
    assert_eq!(e.text, "ab", "only the typing after the motion is undone");
    assert!(
        press(&mut e, KeyCode::Char('7'), KeyModifiers::CONTROL),
        "legacy Ctrl+_"
    );
    assert_eq!(e.text, "");
    assert!(press(&mut e, KeyCode::Char('-'), KeyModifiers::ALT));
    assert_eq!(e.text, "ab");
}

#[test]
fn set_is_undoable() {
    let mut e = Editor::default();
    e.set("draft");
    e.set("");
    assert!(e.undo());
    assert_eq!(e.text, "draft");
}

#[test]
fn consecutive_kills_accumulate_and_yank_pop_cycles() {
    let mut e = Editor::default();
    e.set("one two three");
    // Two backward word kills accumulate into a single entry, in reading order.
    press(&mut e, KeyCode::Char('w'), KeyModifiers::CONTROL);
    press(&mut e, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(e.text, "one ");
    assert_eq!(e.kill_ring().peek(), Some("two three"));
    assert_eq!(e.kill_ring().len(), 1);
    // A motion breaks accumulation.
    press(&mut e, KeyCode::Home, KeyModifiers::NONE);
    press(&mut e, KeyCode::Char('k'), KeyModifiers::CONTROL);
    assert_eq!(e.text, "");
    assert_eq!(e.kill_ring().len(), 2);
    press(&mut e, KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert_eq!(e.text, "one ");
    press(&mut e, KeyCode::Char('y'), KeyModifiers::ALT);
    assert_eq!(e.text, "two three", "yank-pop replaces the yank");
    press(&mut e, KeyCode::Char('y'), KeyModifiers::ALT);
    assert_eq!(e.text, "one ");
    assert_eq!(e.cursor, e.text.len());
    // Yank-pop only follows a yank.
    typed(&mut e, "x");
    press(&mut e, KeyCode::Char('y'), KeyModifiers::ALT);
    assert_eq!(e.text, "one x");
}

#[test]
fn forward_kills_append_and_kill_ring_is_bounded() {
    let mut e = Editor::default();
    e.set("alpha beta gamma");
    e.cursor = 0;
    press(&mut e, KeyCode::Char('d'), KeyModifiers::ALT);
    press(&mut e, KeyCode::Char('d'), KeyModifiers::ALT);
    assert_eq!(e.text, " gamma");
    assert_eq!(e.kill_ring().peek(), Some("alpha beta"));
    let mut ring = KillRing::default();
    for i in 0..(MAX_KILLS + 5) {
        ring.push(&i.to_string(), false, false);
    }
    assert_eq!(ring.len(), MAX_KILLS);
    ring.push("", false, false);
    assert_eq!(ring.len(), MAX_KILLS, "empty kills are ignored");
}

#[test]
fn word_motion_respects_kinds_joiners_and_graphemes() {
    let text = "let don't = well-known.call(e\u{301}x);";
    let mut at = text.len();
    let mut stops = Vec::new();
    while at > 0 {
        at = word_left(text, at);
        stops.push(&text[at..]);
    }
    assert_eq!(
        stops,
        [
            ");",
            "e\u{301}x);",
            "(e\u{301}x);",
            "call(e\u{301}x);",
            ".call(e\u{301}x);",
            "well-known.call(e\u{301}x);",
            "= well-known.call(e\u{301}x);",
            "don't = well-known.call(e\u{301}x);",
            text,
        ]
    );
    assert_eq!(word_right(text, 0), 3);
    assert_eq!(
        &text[word_right(text, 3)..],
        " = well-known.call(e\u{301}x);"
    );
    let mut e = Editor::default();
    e.set("alpha  beta");
    press(&mut e, KeyCode::Left, KeyModifiers::CONTROL);
    assert_eq!(e.cursor, 7);
    press(&mut e, KeyCode::Char('b'), KeyModifiers::ALT);
    assert_eq!(e.cursor, 0);
    press(&mut e, KeyCode::Char('f'), KeyModifiers::ALT);
    assert_eq!(e.cursor, 5);
    press(&mut e, KeyCode::Right, KeyModifiers::CONTROL);
    assert_eq!(e.cursor, 11);
}

#[test]
fn jump_to_character_searches_across_lines_and_any_key_cancels() {
    let mut e = Editor::default();
    e.set("abc\nxbz");
    e.cursor = 0;
    assert!(press(&mut e, KeyCode::Char(']'), KeyModifiers::CONTROL));
    press(&mut e, KeyCode::Char('b'), KeyModifiers::NONE);
    assert_eq!(e.cursor, 1);
    press(&mut e, KeyCode::Char('5'), KeyModifiers::CONTROL);
    press(&mut e, KeyCode::Char('b'), KeyModifiers::NONE);
    assert_eq!(e.cursor, 5, "the next match, skipping the current one");
    press(
        &mut e,
        KeyCode::Char(']'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    press(&mut e, KeyCode::Char('a'), KeyModifiers::NONE);
    assert_eq!(e.cursor, 0);
    press(&mut e, KeyCode::Char(']'), KeyModifiers::CONTROL);
    press(&mut e, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(
        (e.cursor, e.text.as_str()),
        (0, "abc\nxbz"),
        "cancelled, not moved"
    );
    press(&mut e, KeyCode::Char(']'), KeyModifiers::CONTROL);
    press(&mut e, KeyCode::Char('q'), KeyModifiers::NONE);
    assert_eq!(e.cursor, 0, "no match leaves the cursor");
}

#[test]
fn inserting_text_cancels_a_pending_jump() {
    let mut e = Editor::default();
    e.set("abc");
    e.cursor = 0;
    press(&mut e, KeyCode::Char(']'), KeyModifiers::CONTROL);
    e.insert("z");
    typed(&mut e, "c");
    assert_eq!(
        e.text, "zcabc",
        "the typed letter is text, not a jump target"
    );
}
