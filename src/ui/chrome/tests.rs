use super::*;

#[test]
fn tags_and_status_are_plain_text_without_fill() {
    let theme = Theme::dark();
    assert_eq!(tag(&theme, Section::Learn, "memory").content, "MEMORY");
    assert_eq!(tag(&theme, Section::Learn, "memory").style.bg, None);
    assert_eq!(
        status_chip(&theme, Status::Ready, "ready").content,
        "● ready"
    );
    assert_eq!(status_chip(&theme, Status::Ready, "ready").style.bg, None);
}

#[test]
fn framed_lines_have_one_width_and_keep_the_title() {
    let theme = Theme::dark();
    for width in [6usize, 20, 80] {
        let rows = vec![Line::from(
            "a row that is longer than a narrow frame can hold",
        )];
        let lines = framed_lines(&theme, Section::Dial, "Sessions", "3 found", rows, width);
        for line in &lines {
            assert_eq!(text::line_width(line), width, "width {width}");
        }
    }
    let lines = framed_lines(&theme, Section::Dial, "Sessions", "3 found", vec![], 40);
    let top: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(top.contains("Sessions") && top.contains("3 found"), "{top}");
}

#[test]
fn every_tab_strip_marks_the_active_tab_without_a_fill() {
    let theme = Theme::dark();
    let active = tab_style(&theme, true);
    let idle = tab_style(&theme, false);
    assert_eq!(active.fg, theme.fg(Tok::Accent).fg);
    assert!(
        active
            .add_modifier
            .contains(Modifier::UNDERLINED | Modifier::BOLD)
    );
    assert_eq!(idle.fg, theme.fg(Tok::Muted).fg);
    assert!(active.bg.is_none() && idle.bg.is_none());
}
