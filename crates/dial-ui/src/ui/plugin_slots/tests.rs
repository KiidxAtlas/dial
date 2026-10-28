use super::{MAX_HEADER_ROWS, Placement, PluginSlots};
use crate::ui::theme::{Glyphs, Theme};

fn lines(text: &[&str]) -> Vec<String> {
    text.iter().map(|line| (*line).to_owned()).collect()
}

fn plain(line: &ratatui::text::Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

#[test]
fn empty_slots_take_no_rows() {
    let slots = PluginSlots::collect([(Placement::Header, lines(&["  ", ""]).as_slice())]);
    let theme = Theme::dark();
    assert!(slots.is_empty());
    assert!(slots.header_rows(80, &theme).is_empty());
    assert!(slots.footer_rows(80, &theme).is_empty());
    assert!(slots.status_row(80, &theme).is_none());
    assert_eq!(slots.title(), None);
}

#[test]
fn status_lines_from_every_plugin_share_one_row() {
    let a = lines(&["mail 3"]);
    let b = lines(&["build ok"]);
    let slots = PluginSlots::collect([
        (Placement::Status, a.as_slice()),
        (Placement::Status, b.as_slice()),
    ]);
    let theme = Theme::dark();
    let row = slots.status_row(80, &theme).expect("status row");
    assert_eq!(plain(&row), "  mail 3 · build ok");
    assert!(slots.header_rows(80, &theme).is_empty());
}

#[test]
fn control_sequences_are_stripped_from_every_placement() {
    let hostile = lines(&["\x1b]0;owned\x07ok\x1b[31m red\x1b[0m\nsecond"]);
    let slots = PluginSlots::collect([
        (Placement::Header, hostile.as_slice()),
        (Placement::Footer, hostile.as_slice()),
        (Placement::Status, hostile.as_slice()),
        (Placement::Title, hostile.as_slice()),
    ]);
    let theme = Theme::dark();
    let header = slots.header_rows(80, &theme);
    assert_eq!(header.len(), 1);
    let text = plain(&header[0]);
    assert!(!text.contains('\x1b') && !text.contains('\x07') && !text.contains('\n'));
    assert!(slots.title().is_some_and(|t| !t.contains('\x1b')));
}

#[test]
fn rows_are_fitted_to_the_width_with_an_ellipsis() {
    let long = lines(&["0123456789abcdefghijklmnopqrstuvwxyz"]);
    let slots = PluginSlots::collect([(Placement::Footer, long.as_slice())]);
    let theme = Theme::dark().with_glyphs(Glyphs::Ascii);
    let rows = slots.footer_rows(12, &theme);
    assert_eq!(rows.len(), 1);
    let text = plain(&rows[0]);
    assert!(text.ends_with("..."), "{text}");
    assert!(crate::ui::text::width(&text) <= 12, "{text}");
}

#[test]
fn header_rows_are_capped_across_plugins() {
    let many: Vec<String> = (0..20).map(|i| format!("row {i}")).collect();
    let slots = PluginSlots::collect([(Placement::Header, many.as_slice())]);
    assert_eq!(slots.header_rows(80, &Theme::dark()).len(), MAX_HEADER_ROWS);
}

#[test]
fn first_title_wins() {
    let first = lines(&["Build"]);
    let second = lines(&["Other"]);
    let slots = PluginSlots::collect([
        (Placement::Title, first.as_slice()),
        (Placement::Title, second.as_slice()),
    ]);
    assert_eq!(slots.title(), Some("Build"));
}
