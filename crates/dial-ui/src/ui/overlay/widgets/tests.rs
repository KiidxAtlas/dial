use super::*;

#[test]
fn compact_scope_caption_names_the_actual_destination() {
    for width in [24, 36, 46] {
        let global = scope_caption(Scope::Global, "atlas", width);
        assert!(global.contains("All projects"), "{global}");
        assert!(!global.contains("atlas"), "{global}");
        let project = scope_caption(Scope::Project, "atlas", width);
        assert!(project.contains("This project"), "{project}");
        assert!(text::width(&global) <= width);
        assert!(text::width(&project) <= width);
    }
}

#[test]
fn scope_caption_keeps_switch_hint_when_it_fits() {
    for scope in [Scope::Global, Scope::Project] {
        let caption = scope_caption(scope, "atlas", 100);
        assert!(caption.contains("Ctrl+B"), "{caption}");
    }
}

#[test]
fn settings_rows_keep_the_value_column_and_only_fill_the_selected_row() {
    let theme = Theme::dark();
    let columns = SettingColumns::new(25, ["Model", "Size"].into_iter());
    let row = |selected| {
        columns.row(
            &theme,
            selected,
            " ",
            "Size",
            theme.fg(Tok::Text),
            [Span::raw("123")],
        )
    };
    assert_eq!(columns.value_width(), 12);
    assert_eq!(row(false).to_string(), "     Size   123");
    assert_eq!(row(true).width(), 25);
    assert_eq!(row(true).style.bg, theme.bg(Tok::SelectedBg).bg);
    assert_eq!(
        SettingColumns::new(2, ["Size"].into_iter()).value_width(),
        0
    );
}

#[test]
fn one_row_shows_the_field_and_two_rows_can_show_its_heading() {
    for (height, first) in [(1, (None, 2)), (2, (Some("B"), 2))] {
        let mut scroll = 4;
        let rows = section_rows(["A", "A", "B"].into_iter(), 2, &mut scroll, height);
        assert_eq!(rows[scroll], first);
        assert_eq!(rows.last(), Some(&(None, 2)));
    }
}

#[test]
fn compact_tabs_prioritize_the_active_label_over_overflow_indicators() {
    let theme = Theme::dark();
    for width in [5, 6, 10, 16] {
        let row = panel_tabs(
            &theme,
            &["All", "Background", "Services", "Tools"],
            3,
            width,
        );
        assert!(row.to_string().contains("Tools"), "{row}");
        assert!(row.width() <= width);
    }
}

#[test]
fn empty_recovery_text_wraps_within_the_pane() {
    let rows = empty_rows(&Theme::dark(), "No hits. Try again.", 12, 2);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].to_string().trim(), "No hits.");
    assert_eq!(rows[1].to_string().trim(), "Try again.");
}
