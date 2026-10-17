use super::*;

#[test]
fn syntax_tokens_preserve_text_and_degrade() {
    let source = "let value = \"hello\"; // comment";
    let colored = highlight(&theme(), "rust", source, &mut false);
    assert_eq!(
        colored
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>(),
        source
    );
    assert_eq!(colored[0].style, theme().fg(Tok::SyntaxKeyword));
    assert!(
        colored
            .iter()
            .any(|s| s.content == "\"hello\"" && s.style == theme().fg(Tok::SyntaxString))
    );
    assert!(
        colored
            .iter()
            .any(|s| s.content == "// comment" && s.style == theme().fg(Tok::SyntaxComment))
    );
    for mode in [
        super::super::theme::ColorMode::None,
        super::super::theme::ColorMode::Ansi16,
        super::super::theme::ColorMode::Ansi256,
    ] {
        let palette = theme().downgraded(mode);
        let spans = highlight(&palette, "rust", source, &mut false);
        assert!(
            spans
                .iter()
                .all(|s| !matches!(s.style.fg, Some(ratatui::style::Color::Rgb(..))))
        );
        if mode == super::super::theme::ColorMode::None {
            assert!(spans.iter().all(|s| s.style.fg.is_none()));
        }
    }
    let unknown = highlight(&theme(), "unknown", source, &mut false);
    assert_eq!(unknown.len(), 1);
    let mut comment = false;
    highlight(&theme(), "c", "/* open", &mut comment);
    assert!(comment);
    let closed = highlight(&theme(), "c", "end */ int n = 42;", &mut comment);
    assert!(!comment);
    assert!(
        closed
            .iter()
            .any(|s| s.content == "42" && s.style == theme().fg(Tok::SyntaxNumber))
    );
}

fn theme() -> Theme {
    Theme::dark()
}

fn plain(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

fn md(src: &str, width: u16) -> Rows {
    render(src, width, &theme(), false)
}

fn assert_fits(rows: &Rows, width: u16) {
    for (i, row) in rows.iter().enumerate() {
        let w = text::line_width(row);
        assert!(
            w <= width as usize,
            "row {i} is {w} cells wide, limit {width}: {:?}",
            plain(&vec![row.clone()])
        );
    }
}

/// The span containing `needle`, so a test can assert the style a user would see on that text.
fn style_of(rows: &Rows, needle: &str) -> Style {
    rows.iter()
        .flat_map(|l| l.spans.iter())
        .find(|s| s.content.contains(needle))
        .unwrap_or_else(|| panic!("no span with {needle:?} in {:?}", plain(rows)))
        .style
}

const SAMPLE: &str = "# Title\n\nSome *emphasis*, **strong**, ~~gone~~ and `code` with a [link](https://example.com/a).\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n\n> quote\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n";

#[test]
fn empty_and_zero_width_produce_nothing() {
    assert!(md("", 40).is_empty());
    assert!(md("   \n\n  ", 40).is_empty());
    assert!(md("hello", 0).is_empty());
}

#[test]
fn every_construct_fits_every_width() {
    for width in 1..=100u16 {
        assert_fits(&md(SAMPLE, width), width);
    }
}

#[test]
fn prose_wraps_on_words_without_losing_text() {
    let rows = md("alpha beta gamma delta epsilon zeta eta theta", 14);
    assert_fits(&rows, 14);
    assert_eq!(
        plain(&rows),
        ["alpha beta", "gamma delta", "epsilon zeta", "eta theta"]
    );
}

#[test]
fn paragraphs_are_separated_by_one_blank_row() {
    assert_eq!(
        plain(&md("one\n\ntwo\n\n\n\nthree", 20)),
        ["one", "", "two", "", "three"]
    );
}

#[test]
fn soft_break_is_a_space_and_hard_break_a_new_row() {
    assert_eq!(plain(&md("one\ntwo", 20)), ["one two"]);
    assert_eq!(plain(&md("one  \ntwo", 20)), ["one", "two"]);
    assert_eq!(plain(&md("one\\\ntwo", 20)), ["one", "two"]);
}

#[test]
fn emphasis_strong_strike_and_code_carry_their_styles() {
    let rows = md("plain *slanted* **heavy** ~~gone~~ `mono`", 80);
    let t = theme();
    assert!(
        style_of(&rows, "slanted")
            .add_modifier
            .contains(Modifier::ITALIC)
    );
    assert!(
        style_of(&rows, "heavy")
            .add_modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        style_of(&rows, "gone")
            .add_modifier
            .contains(Modifier::CROSSED_OUT)
    );
    assert_eq!(style_of(&rows, "mono").fg, t.fg(Tok::MdCode).fg);
    let p = style_of(&rows, "plain");
    assert!(p.add_modifier.is_empty());
}

#[test]
fn nested_emphasis_combines_and_survives_wrapping() {
    let rows = md("**bold and _both_ here** tail", 9);
    assert_fits(&rows, 9);
    let both: Vec<_> = rows
        .iter()
        .flat_map(|l| &l.spans)
        .filter(|s| s.content.contains("both"))
        .collect();
    assert!(both.iter().all(|s| {
        s.style
            .add_modifier
            .contains(Modifier::BOLD | Modifier::ITALIC)
    }));
    for word in ["bold", "and", "here"] {
        assert!(
            style_of(&rows, word).add_modifier.contains(Modifier::BOLD),
            "{word}"
        );
    }
    assert!(
        !style_of(&rows, "tail")
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn headings_are_styled_and_h1_gets_a_rule() {
    let t = theme();
    let rows = md("# Big\n\n## Medium\n\n### Small", 20);
    let text = plain(&rows);
    assert_eq!(text[0], "Big");
    assert!(text[1].chars().all(|c| t.sym(Sym::Hr).contains(c)) && text[1].chars().count() == 20);
    assert!(text.contains(&"Medium".to_owned()));
    assert!(text.contains(&"### Small".to_owned()));
    assert_eq!(style_of(&rows, "Big").fg, t.fg(Tok::MdHeading).fg);
    assert!(
        style_of(&rows, "Medium")
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn links_show_dim_url_unless_it_is_the_text() {
    let t = theme();
    let rows = md("see [docs](https://x.dev/a) now", 80);
    assert_eq!(plain(&rows), ["see docs (https://x.dev/a) now"]);
    assert_eq!(style_of(&rows, "docs").fg, t.fg(Tok::MdLink).fg);
    assert_eq!(
        style_of(&rows, "https://x.dev/a").fg,
        t.fg(Tok::MdLinkUrl).fg
    );
    let auto = md("<https://x.dev/a>", 80);
    assert_eq!(plain(&auto), ["https://x.dev/a"]);
}

#[test]
fn fenced_code_has_a_gutter_and_plain_language_label() {
    let rows = md("```rust\nlet x = 1;\n```", 20);
    let text = plain(&rows);
    let bar = theme().sym(Sym::Quote).to_owned();
    assert_eq!(text, [format!("{bar} RUST"), format!("{bar} let x = 1;")]);
    assert!(
        rows.iter()
            .all(|l| l.spans.iter().all(|s| s.style.bg.is_none()))
    );
    assert!(text.iter().all(|l| text::width(l) <= 20));
}

#[test]
fn code_lines_wrap_inside_the_gutter() {
    let rows = md("    aaaaaaaaaaaaaaaaaaaa", 12);
    assert_fits(&rows, 12);
    let text = plain(&rows);
    let bar = theme().sym(Sym::Quote).to_owned();
    assert!(text.len() >= 2, "{text:?}");
    assert!(text.iter().all(|l| l.starts_with(bar.as_str())), "{text:?}");
}

#[test]
fn narrow_width_drops_the_gutter_but_keeps_the_code() {
    let rows = md("```\nabcdefghij\n```", 5);
    assert_fits(&rows, 5);
    assert_eq!(plain(&rows), ["abcde", "fghij"]);
}

#[test]
fn diff_fences_color_added_removed_and_context() {
    let t = theme();
    let rows = md("```diff\n@@ -1 +1 @@\n-old\n+new\n same\n```", 30);
    assert_eq!(style_of(&rows, "+new").fg, t.fg(Tok::DiffAdded).fg);
    assert_eq!(style_of(&rows, "-old").fg, t.fg(Tok::DiffRemoved).fg);
    assert_eq!(style_of(&rows, " same").fg, t.fg(Tok::DiffContext).fg);
    assert_eq!(style_of(&rows, "@@ -1").fg, t.fg(Tok::Accent).fg);
    // the same lines in a normal language fence are not colored as a diff
    let plain_rows = md("```rust\n-old\n+new\n```", 30);
    assert!(plain_rows.iter().flat_map(|line| &line.spans).all(|span| {
        span.style.fg != t.fg(Tok::DiffAdded).fg && span.style.fg != t.fg(Tok::DiffRemoved).fg
    }));
}

#[test]
fn unlabeled_fence_that_looks_like_a_patch_is_colored() {
    let rows = md("```\ndiff --git a/x b/x\n@@ -1 +1 @@\n+added\n```", 40);
    assert_eq!(style_of(&rows, "+added").fg, theme().fg(Tok::DiffAdded).fg);
}

#[test]
fn lists_nest_with_hanging_indents() {
    let rows = md(
        "- alpha beta gamma delta\n  - inner item that is long\n- two",
        16,
    );
    assert_fits(&rows, 16);
    let text = plain(&rows);
    let bullet = theme().sym(Sym::Bullet).to_owned();
    assert!(text[0].starts_with(&format!("{bullet} alpha")), "{text:?}");
    assert!(
        text[1].starts_with("  gamma"),
        "continuation hangs under the text: {text:?}"
    );
    let nested = text.iter().position(|l| l.contains("inner")).unwrap();
    assert!(
        text[nested].starts_with(&format!("  {bullet} inner")),
        "{text:?}"
    );
    assert!(text.last().unwrap().starts_with(&format!("{bullet} two")));
}

#[test]
fn ordered_lists_keep_their_numbers() {
    let text = plain(&md("3. three\n4. four\n   more text here", 30));
    assert_eq!(text, ["3. three", "4. four more text here"]);
}

#[test]
fn task_lists_show_checkboxes() {
    let rows = md("- [x] done\n- [ ] todo", 30);
    assert_eq!(plain(&rows), ["[x] done", "[ ] todo"]);
    assert_eq!(style_of(&rows, "[x]").fg, theme().fg(Tok::Success).fg);
}

#[test]
fn loose_lists_get_blank_rows_between_items_and_tight_ones_do_not() {
    assert_eq!(plain(&md("- a\n\n- b", 20)).len(), 3);
    assert_eq!(plain(&md("- a\n- b", 20)).len(), 2);
}

#[test]
fn blockquotes_carry_a_bar_on_every_row_including_nested_blocks() {
    let t = theme();
    let rows = md("> first line\n>\n> second line that wraps around", 16);
    assert_fits(&rows, 16);
    let bar = t.sym(Sym::Quote);
    assert!(
        plain(&rows).iter().all(|l| l.starts_with(bar)),
        "{:?}",
        plain(&rows)
    );
    assert_eq!(rows[0].spans[0].style.fg, t.fg(Tok::MdQuoteBorder).fg);
    assert!(
        style_of(&rows, "first")
            .add_modifier
            .contains(Modifier::ITALIC)
    );
}

#[test]
fn horizontal_rule_spans_the_width() {
    let rows = md("a\n\n---\n\nb", 10);
    assert_eq!(plain(&rows)[2].chars().count(), 10);
    assert_eq!(rows[2].spans[0].style.fg, theme().fg(Tok::MdHr).fg);
}

const TABLE: &str =
    "| name | qty | note |\n|:-----|----:|:----:|\n| apple | 3 | red |\n| fig | 12 | purple |\n";

#[test]
fn table_is_a_box_grid_with_alignment() {
    let text = plain(&md(TABLE, 60));
    assert_eq!(text.len(), 6, "{text:?}");
    assert!(text[0].starts_with('╭') && text[0].contains('┬'));
    assert!(text[2].starts_with('├') && text[2].contains('┼'));
    assert!(text[5].starts_with('╰') && text[5].contains('┴'));
    assert!(text.iter().all(|l| text::width(l) == text::width(&text[0])));
    assert!(
        text[3].contains("│ apple │   3 │"),
        "numbers are right aligned: {text:?}"
    );
    assert!(text[4].contains("│  12 │"), "{text:?}");
}

#[test]
fn table_header_is_bold() {
    assert!(
        style_of(&md(TABLE, 60), "name")
            .add_modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn table_shrinks_and_wraps_cells_to_fit() {
    let src = "| key | description |\n|---|---|\n| a | a rather long description that must wrap inside its column |\n";
    for width in [20u16, 24, 30, 40] {
        let rows = md(src, width);
        assert_fits(&rows, width);
        let text = plain(&rows);
        assert!(text[0].starts_with('╭'), "width {width}: {text:?}");
        let joined = text.join(" ");
        for word in ["rather", "long", "wrap", "column"] {
            assert!(joined.contains(word), "width {width} lost {word}: {text:?}");
        }
    }
}

#[test]
fn table_too_narrow_falls_back_to_key_value_records() {
    let rows = md(TABLE, 12);
    assert_fits(&rows, 12);
    let text = plain(&rows);
    assert!(text.iter().all(|l| !l.contains('┬')), "{text:?}");
    assert!(text.contains(&"name: apple".to_owned()), "{text:?}");
    assert!(text.contains(&"qty: 12".to_owned()), "{text:?}");
    assert!(
        text.iter().any(|l| l.contains('─')),
        "records are separated by a rule: {text:?}"
    );
}

#[test]
fn unterminated_fence_still_renders_code() {
    let rows = md("text\n\n```py\nprint(1)\nprint(", 24);
    assert_fits(&rows, 24);
    let text = plain(&rows);
    assert!(text.iter().any(|l| l.contains("print(1)")));
    assert!(text.last().unwrap().contains("print("));
}

#[test]
fn unterminated_markup_is_shown_literally() {
    assert_eq!(plain(&md("a **bold start", 40)), ["a **bold start"]);
    assert_eq!(plain(&md("a *x", 40)), ["a *x"]);
    assert_eq!(plain(&md("[text](http://x", 40)), ["[text](http://x"]);
    assert!(!plain(&md("| a | b |\n|---", 40)).is_empty());
}

#[test]
fn every_prefix_of_a_document_renders_within_width() {
    // what streaming does: render each growing prefix
    for end in (0..SAMPLE.len()).filter(|i| SAMPLE.is_char_boundary(*i)) {
        for width in [3u16, 7, 20, 60] {
            assert_fits(&md(&SAMPLE[..end], width), width);
        }
    }
}

#[test]
fn huge_unbroken_line_is_hard_wrapped() {
    let rows = md(&"x".repeat(10_000), 37);
    assert_fits(&rows, 37);
    assert_eq!(rows.len(), 10_000usize.div_ceil(37));
    let code = md(&format!("```\n{}\n```", "y".repeat(5_000)), 40);
    assert_fits(&code, 40);
}

#[test]
fn wide_characters_and_emoji_never_overflow() {
    let src = "日本語のテキストは幅が二つです 🎉🎉🎉 mixed ascii 汉字\n\n```\n日本語日本語日本語\n```\n\n| 名前 | 値 |\n|--|--|\n| 日本 | 🎉 |";
    for width in 1..=40u16 {
        assert_fits(&md(src, width), width);
    }
    let rows = md("日本語日本語", 5);
    assert_eq!(plain(&rows), ["日本", "語日", "本語"]);
}

#[test]
fn hostile_escape_sequences_never_reach_the_rows() {
    let src = "safe \x1b[31mred\x1b[0m \x1b]0;pwned\x07title \x1b]8;;http://evil\x1b\\link\x1b]8;;\x1b\\ \u{202e}rtl\r\n\n```\n\x1b[2Jcode\x1b[H\n```\n| \x1b[1mh | b |\n|--|--|\n| \u{9b}31mx | y |";
    for row in md(src, 40) {
        for span in row.spans {
            assert!(
                !span
                    .content
                    .chars()
                    .any(|c| c.is_control() || c == '\u{202e}'),
                "{:?}",
                span.content
            );
            assert!(
                !span.content.contains("pwned")
                    && !span.content.contains("[31m")
                    && !span.content.contains("[2J")
            );
        }
    }
}

#[test]
fn html_is_shown_as_dim_text_not_dropped() {
    let rows = md("before <b>x</b> after", 40);
    let joined = plain(&rows).join("");
    assert!(
        joined.contains("<b>") && joined.contains("after"),
        "{joined}"
    );
}

#[test]
fn ascii_mode_uses_ascii_glyphs_for_structure() {
    let rows = render(
        "- a\n\n```rs\nx\n```\n\n> q\n\n| a |\n|--|\n| b |",
        30,
        &theme(),
        true,
    );
    for row in plain(&rows) {
        assert!(row.is_ascii(), "{row:?}");
    }
}
