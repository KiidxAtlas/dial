use super::*;

#[test]
fn sanitize_removes_terminal_control_but_keeps_text_and_newlines() {
    assert_eq!(
        sanitize("a\x1b[31mred\x1b[0m\x1b]0;owned\x07 b\tc\nd\u{202e}e\x07"),
        "ared b    c\nde"
    );
    assert_eq!(sanitize("\u{9d}title\u{9c}ok"), "ok");
}

#[test]
fn clip_and_wrap_never_split_wide_or_combining_graphemes() {
    assert_eq!(clip("日本語", 5), "日本");
    assert_eq!(clip("e\u{301}x", 1), "e\u{301}");
    let rows = wrap(vec![Line::from("日本語日本語")], 5);
    assert!(rows.iter().all(|r| line_width(r) <= 5), "{rows:?}");
    assert_eq!(
        rows.iter().map(|r| r.to_string()).collect::<String>(),
        "日本語日本語"
    );
    assert_eq!(
        wrap(vec![Line::from("日")], 1).len(),
        1,
        "an unfittable grapheme terminates as ?"
    );
}

#[test]
fn justify_keeps_the_right_side_and_trims_the_left() {
    let s = |t: &str| Span::raw(t.to_owned());
    let row = justify(
        vec![s("model name that is long")],
        vec![s("42%")],
        14,
        Style::default(),
    );
    let text: String = row.iter().map(|x| x.content.as_ref()).collect();
    assert_eq!(width(&text), 14);
    assert!(
        text.ends_with("42%") && text.starts_with("model"),
        "{text:?}"
    );
}

#[test]
fn fuzzy_ranks_prefix_and_boundary_matches_first_and_rejects_non_subsequences() {
    fn rank<'a>(q: &str, items: &[&'a str]) -> Vec<&'a str> {
        let mut found: Vec<(i32, &'a str)> = items
            .iter()
            .filter_map(|t| fuzzy(q, t).map(|(s, _)| (s, *t)))
            .collect();
        found.sort_by_key(|a| std::cmp::Reverse(a.0));
        found.into_iter().map(|(_, t)| t).collect()
    }
    assert_eq!(
        rank("mo", &["remove-dir", "model", "memory", "modelpreset"])[0],
        "model"
    );
    assert!(fuzzy("zz", "model").is_none());
    assert_eq!(fuzzy("", "anything").unwrap().0, 0);
    assert_eq!(fuzzy("st", "settings").unwrap().1, vec![0, 2]);
    assert!(fuzzy("MODEL", "model").is_some(), "case-insensitive");
}

#[test]
fn word_wrap_breaks_on_spaces_and_hard_splits_long_words() {
    assert_eq!(wrap_words("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
    assert_eq!(wrap_words("abcdefgh", 3), vec!["abc", "def", "gh"]);
    assert_eq!(wrap_words("a\n\nb", 5), vec!["a", "", "b"]);
}

#[test]
fn tiny_widths_never_exceed_their_cell_limit() {
    for max in 0..4 {
        for ascii in [false, true] {
            assert!(width(&ellipsize("abcdef", max, ascii)) <= max);
            assert!(width(&ellipsize_start("abcdef", max, ascii)) <= max);
        }
    }
    // A wide grapheme that cannot fit a one-cell row becomes `?`, with no empty first row.
    assert_eq!(wrap_words("界界", 1), vec!["?", "?"]);
    assert!(wrap_words("ab界cd", 2).iter().all(|row| width(row) <= 2));
}
