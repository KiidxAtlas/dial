use super::*;

fn info() -> WelcomeInfo {
    WelcomeInfo {
        version: "0.3.1".into(),
        provider: "lmstudio".into(),
        model: "qwen3.5-9b".into(),
        project: "dial".into(),
        tips: vec![
            ("Ctrl+P".into(), "actions".into()),
            ("/".into(), "commands".into()),
            ("!".into(), "bash".into()),
        ],
        recent: vec![
            ("fix the parser".into(), "2h".into()),
            ("add welcome banner".into(), "1d".into()),
        ],
        resume_hint: "/resume to continue one".into(),
        setup_hint: "Ctrl+K to choose one".into(),
        branch: String::new(),
        approval: "ask".into(),
        sandbox: "dev".into(),
    }
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

#[test]
fn never_wider_than_the_terminal_at_any_width() {
    for theme in [
        Theme::dark(),
        Theme::mono(),
        Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii),
    ] {
        for width in 1..=200u16 {
            let rows = rows(&info(), width, &theme);
            assert!(!rows.is_empty(), "width {width}");
            for row in &rows {
                assert!(
                    text::line_width(row) <= width as usize,
                    "width {width}: {:?}",
                    plain(&vec![row.clone()])
                );
            }
        }
    }
}

#[test]
fn wide_banner_shows_wordmark_card_identity_hints_and_recent_sessions() {
    let text = plain(&rows(&info(), 100, &Theme::dark()));
    let joined = text.join("\n");
    for needle in [
        WORDMARK[0],
        WORDMARK[1],
        "dial 0.3.1",
        "welcome back",
        TAGLINE,
        "qwen3.5-9b",
        "via lmstudio",
        "ask",
        "asks before every edit and command",
        "sandbox",
        "dev",
        "Ctrl+P",
        "actions",
        "RECENT",
        "fix the parser",
        "add welcome banner",
    ] {
        assert!(joined.contains(needle), "missing {needle:?}\n{joined}");
    }
    assert!(
        !joined.contains('\u{256d}') && !joined.contains('\u{2502}'),
        "the facts stand free, without a frame:\n{joined}"
    );
    for (row, reflection) in REFLECTION.iter().enumerate() {
        assert!(
            joined.contains(reflection),
            "reflection row {row}\n{joined}"
        );
    }
    assert!(
        joined.contains('\u{2508}'),
        "the axis divides mark and reflection"
    );
    let name = text.iter().position(|l| l.contains("dial 0.3.1")).unwrap();
    assert!(
        text[name].contains(WORDMARK[WORDMARK.len() - 2]),
        "the name sits beside the wordmark's baseline: {text:?}"
    );
    let recent = text.iter().find(|l| l.contains("fix the parser")).unwrap();
    assert!(recent.trim_end().ends_with("2h"), "{recent}");
}
#[test]
fn hints_wrap_instead_of_overflowing() {
    let mut many = info();
    many.tips = (0..12)
        .map(|i| (format!("Key{i}"), format!("does thing {i}")))
        .collect();
    let text = plain(&rows(&many, 60, &Theme::dark())).join("\n");
    for i in 0..12 {
        assert!(text.contains(&format!("Key{i}")), "{text}");
    }
}

#[test]
fn empty_recent_and_tips_drop_their_sections() {
    let mut bare = info();
    bare.recent.clear();
    bare.tips.clear();
    let joined = plain(&rows(&bare, 90, &Theme::dark())).join("\n");
    assert!(joined.contains("welcome"));
    assert!(!joined.contains("welcome back"));
    assert!(!joined.contains("RECENT"));
    assert!(!joined.contains("Ctrl+P"));
    assert!(
        !joined.contains("TRY"),
        "starters need the tips to be enabled"
    );
}

#[test]
fn a_first_visit_suggests_requests_and_a_return_visit_says_how_to_resume() {
    let mut first = info();
    first.recent.clear();
    let text = plain(&rows(&first, 120, &Theme::dark())).join("\n");
    assert!(text.contains("START WITH"), "{text}");
    let bullet = Theme::dark().sym(Sym::Bullet).to_owned();
    for starter in STARTERS.iter() {
        assert!(text.contains(&format!("{bullet}  {starter}")), "{text}");
    }
    assert!(!text.contains("/resume"));
    let back = plain(&rows(&info(), 120, &Theme::dark())).join("\n");
    assert!(back.contains("/resume to continue one") && !back.contains("explain this project"));
    let ascii = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    assert!(
        plain(&rows(&first, 120, &ascii))
            .iter()
            .all(|r| r.is_ascii())
    );
}
#[test]
fn a_missing_model_is_a_warning_with_the_way_to_fix_it() {
    let mut none = info();
    none.model.clear();
    let theme = Theme::dark();
    let rows = rows(&none, 100, &theme);
    let warn = rows
        .iter()
        .flat_map(|r| r.spans.iter())
        .find(|s| s.content.contains("no model selected"))
        .expect("warning");
    assert_eq!(warn.style.fg, theme.fg(Tok::Warning).fg);
    assert!(plain(&rows).join("\n").contains("Ctrl+K to choose one"));
}

#[test]
fn auto_approval_with_the_sandbox_off_reads_as_a_sentence_and_is_flagged() {
    let theme = Theme::dark();
    let mut risky = info();
    risky.approval = "auto".into();
    risky.sandbox = "off".into();
    let lines = rows(&risky, 120, &theme);
    let rendered = plain(&lines);
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("approval") && row.contains("auto"))
    );
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("sandbox") && row.contains("off"))
    );
    let spans: Vec<Span<'_>> = lines.iter().flat_map(|l| l.spans.iter().cloned()).collect();
    let sandbox = spans
        .iter()
        .find(|span| span.content == "off")
        .expect("sandbox span");
    assert_eq!(sandbox.style.fg, theme.fg(Tok::Warning).fg);
}

#[test]
fn long_project_path_preserves_project_name() {
    let mut welcome = info();
    welcome.project = "/a/very/long/parent/path/to/somewhere/deep/my-project".into();
    let rendered = plain(&rows(&welcome, 50, &Theme::dark())).join("\n");
    assert!(rendered.contains('…'));
    assert!(rendered.contains("my-project"), "{rendered}");
}

#[test]
fn wordmark_rows_share_one_width_so_the_title_column_lines_up() {
    let w = text::width(WORDMARK[0]);
    for mark in WORDMARK.iter().chain(&REFLECTION) {
        assert_eq!(text::width(mark), w, "{mark:?}");
    }
    assert_eq!(
        text::width(WORDMARK_SMALL[0]),
        text::width(WORDMARK_SMALL[1])
    );
}

#[test]
fn shorter_terminals_shed_the_reflection_then_the_large_wordmark() {
    let info = info();
    let full = plain(&rows(&info, 100, &Theme::dark()));
    let fits = |max_rows: usize| {
        plain(&render(
            &info,
            100,
            &Theme::dark(),
            View {
                max_rows,
                ..View::default()
            },
        ))
        .join("\n")
    };
    let large = fits(full.len() - 1);
    assert!(
        large.contains(WORDMARK[0]) && !large.contains(REFLECTION[1]),
        "{large}"
    );
    let small = fits(full.len() - 4);
    assert!(
        small.contains(WORDMARK_SMALL[1]) && !small.contains(WORDMARK[0]),
        "{small}"
    );
}

#[test]
fn narrow_drops_the_mark_but_keeps_identity() {
    let text = plain(&rows(&info(), 20, &Theme::dark())).join("\n");
    assert!(!text.contains('▁'));
    assert!(text.contains("dial"));
}

#[test]
fn long_and_hostile_fields_are_sanitized_and_ellipsized() {
    let hostile = WelcomeInfo {
        version: "1\x1b[31m".into(),
        provider: "p\x1b]0;x\x07rov".into(),
        model: "m".repeat(500),
        project: "日本語".repeat(50),
        tips: vec![("k\x1b[2J".into(), "tip\x1b[2J".repeat(40))],
        recent: vec![("t\nitle".repeat(60), "\x1b[1m3d".into())],
        resume_hint: "/resume\x1b[2J".repeat(30),
        setup_hint: String::new(),
        branch: String::new(),
        approval: String::new(),
        sandbox: String::new(),
    };
    for width in [10u16, 30, 69, 70, 120] {
        let rows = rows(&hostile, width, &Theme::dark());
        for row in &rows {
            assert!(text::line_width(row) <= width as usize);
            for span in &row.spans {
                assert!(
                    !span.content.chars().any(char::is_control),
                    "{:?}",
                    span.content
                );
                assert!(!span.content.contains("[2J") && !span.content.contains("[31m"));
            }
        }
        assert!(plain(&rows).join("").contains('…'), "width {width}");
    }
}

fn info_without_recent() -> WelcomeInfo {
    let mut first = info();
    first.recent.clear();
    first
}

#[test]
fn wide_terminals_put_the_list_beside_the_facts() {
    let text = plain(&render(
        &info(),
        140,
        &Theme::dark(),
        View {
            live: true,
            ..View::default()
        },
    ));
    let beside = text
        .iter()
        .find(|l| l.contains("fix the parser"))
        .expect("recent row");
    assert!(
        beside.contains("project"),
        "list shares rows with the facts: {text:?}"
    );
    let narrow = plain(&rows(&info(), 100, &Theme::dark()));
    let alone = narrow
        .iter()
        .find(|l| l.contains("fix the parser"))
        .unwrap();
    assert!(!alone.contains("project"), "{narrow:?}");
    for width in 1..=220u16 {
        for row in render(&info(), width, &Theme::dark(), View::default()) {
            assert!(text::line_width(&row) <= width as usize, "width {width}");
        }
    }
}

#[test]
fn the_home_selects_rows_and_picks_sessions_or_starters() {
    let mut home = Home::new(info(), vec!["s1".into(), "s2".into()], false);
    assert!(!home.up(), "nothing selected: Up keeps its history meaning");
    assert!(home.down() && home.down() && home.down());
    assert_eq!(home.selected(), Some(1), "stops at the last row");
    let theme = Theme::dark();
    let pointer = theme.sym(Sym::Pointer);
    let shown = plain(&home.rows(140, 40, &theme)).join("\n");
    assert!(
        shown.contains(&format!("{pointer} add welcome banner")),
        "{shown}"
    );
    assert_eq!(home.pick(), Some(Pick::Resume("s2".into())));
    assert_eq!(home.selected(), None);
    assert!(home.down() && home.up() && home.selected().is_none());

    let mut first = Home::new(info_without_recent(), Vec::new(), false);
    assert!(first.down());
    assert_eq!(first.pick(), Some(Pick::Draft(STARTERS[0])));
    assert!(
        plain(&first.rows(100, 40, &theme))
            .join("\n")
            .contains("to pick one, Enter to edit it")
    );
    let mut bare = info_without_recent();
    bare.tips.clear();
    assert!(
        !Home::new(bare, Vec::new(), false).down(),
        "no list, no selection"
    );
}

#[test]
fn short_terminals_get_the_compact_home_within_the_rows_given() {
    let theme = Theme::dark();
    let mut home = Home::new(info(), vec!["s1".into(), "s2".into()], false);
    let full = home.rows(100, usize::MAX, &theme);
    let short = home.rows(100, 12, &theme);
    assert!(
        short.len() <= 12 && full.len() > 12,
        "{} {}",
        full.len(),
        short.len()
    );
    let text = plain(&short).join("\n");
    assert!(!text.contains(WORDMARK[0]), "{text}");
    assert!(
        text.contains("dial 0.3.1") && text.contains("fix the parser"),
        "{text}"
    );
}

#[test]
fn the_sweep_brightens_the_wordmark_once_then_rests_on_the_gradient() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let at = |sweep: f32| {
        render(
            &info(),
            100,
            &theme,
            View {
                sweep,
                ..View::default()
            },
        )
    };
    assert_ne!(at(0.4)[1], at(1.0)[1], "mid-sweep differs");
    assert_eq!(
        at(1.0),
        rows(&info(), 100, &theme),
        "rest is the plain banner"
    );
    let mut still = Home::new(info(), Vec::new(), false);
    assert!(!still.animating());
    let mut moving = Home::new(info(), Vec::new(), true);
    assert!(moving.animating());
    moving.rows(100, 40, &theme);
    std::thread::sleep(fx::TURN);
    moving.rows(100, 40, &theme);
    assert!(!moving.animating(), "one sweep, then still");
    still.rows(100, 40, &theme);
}

#[test]
fn the_home_collapses_to_one_summary_line() {
    let mut with_branch = info();
    with_branch.branch = "main".into();
    let summary = plain(&summary(&with_branch, 100, &Theme::dark()));
    let lines: Vec<&String> = summary.iter().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "{summary:?}");
    assert!(
        lines[0].contains("dial 0.3.1 · qwen3.5-9b · dial  main"),
        "{summary:?}"
    );
    for width in 1..=120u16 {
        for row in super::summary(&with_branch, width, &Theme::dark()) {
            assert!(text::line_width(&row) <= width as usize);
        }
    }
}

#[test]
fn ascii_theme_draws_with_ascii_only() {
    let theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    for row in plain(&rows(&info(), 90, &theme)) {
        assert!(row.is_ascii(), "{row:?}");
    }
}

#[test]
fn colors_come_from_the_theme_without_fills() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let rows = rows(&info(), 100, &theme);
    let spans: Vec<&Span> = rows.iter().flat_map(|l| &l.spans).collect();
    assert!(
        spans.iter().all(|s| s.style.bg.is_none()),
        "nothing paints a background"
    );
    // The wordmark is a gradient: its glyphs take several brand colors, none of them default.
    let mark_colors: std::collections::HashSet<_> = rows[1]
        .spans
        .iter()
        .filter(|s| {
            s.content
                .chars()
                .any(|c| matches!(c, '\u{2580}' | '\u{2584}' | '\u{2588}'))
        })
        .map(|s| s.style.fg)
        .collect();
    assert!(mark_colors.len() >= 3, "{mark_colors:?}");
    assert!(!mark_colors.contains(&None));
    let model = spans.iter().find(|s| s.content == "qwen3.5-9b").unwrap();
    assert_eq!(model.style.fg, theme.fg(Tok::Accent).fg);
    let key = spans.iter().find(|s| s.content == "Ctrl+P").unwrap();
    assert_eq!(key.style.fg, theme.fg(Tok::Text).fg);
    let axis = spans
        .iter()
        .find(|s| s.content.starts_with('\u{2508}'))
        .unwrap();
    assert_eq!(axis.style.fg, theme.fg(Tok::BorderMuted).fg);
}

#[test]
fn short_homes_window_the_selected_action_instead_of_cutting_it_off() {
    for theme in [
        Theme::dark(),
        Theme::mono(),
        Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii),
    ] {
        for first_visit in [false, true] {
            let info = if first_visit {
                info_without_recent()
            } else {
                info()
            };
            let mut home = Home::new(info.clone(), vec!["s1".into(), "s2".into()], false);
            for i in 0..items(&info) {
                assert!(home.down());
                for budget in 1..=14 {
                    let rows = home.rows(80, budget, &theme);
                    assert!(rows.len() <= budget);
                    let picked = rows
                        .iter()
                        .find(|row| {
                            row.spans
                                .iter()
                                .any(|s| s.content == theme.sym(Sym::Pointer))
                        })
                        .unwrap_or_else(|| {
                            panic!("selection {i}, budget {budget}: {:?}", plain(&rows))
                        });
                    let expected = if first_visit {
                        STARTERS[i]
                    } else {
                        info.recent[i].0.as_str()
                    };
                    assert!(plain(&vec![picked.clone()])[0].contains(expected));
                }
            }
        }
    }
}

#[test]
fn isolation_and_missing_model_recovery_have_their_own_visible_rows() {
    let mut info = info_without_recent();
    info.approval = "auto".into();
    info.sandbox = "off".into();
    info.model.clear();
    info.setup_hint = "/model to choose a model".into();
    for width in [38, 60, 100, 140] {
        let theme = Theme::dark();
        let rows = render(
            &info,
            width,
            &theme,
            View {
                max_rows: 18,
                ..View::default()
            },
        );
        let rendered = plain(&rows);
        assert!(
            rendered
                .iter()
                .any(|row| row.contains("next") && row.contains("/model")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|row| row.contains("sandbox") && row.contains("off")),
            "{rendered:?}"
        );
        let isolation = rows
            .iter()
            .flat_map(|row| &row.spans)
            .find(|s| s.content == "off")
            .unwrap();
        assert_eq!(isolation.style.fg, theme.fg(Tok::Warning).fg);
        assert!(
            rows.iter()
                .all(|row| text::line_width(row) <= width as usize)
        );
    }
}
