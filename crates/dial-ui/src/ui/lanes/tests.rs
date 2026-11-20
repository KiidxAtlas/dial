use super::*;
use crate::llm::model_config::UiConfig;
use crate::ui::theme::Theme;

fn render(event: &UiEvent, w: u16, expanded: bool) -> Vec<String> {
    let theme = Theme::builtin("dial-dark").unwrap();
    let ui = UiConfig::default();
    let ctx = TranscriptCtx {
        theme: &theme,
        ui: &ui,
        tick: 0,
        tools_expanded: expanded,
        expand_key: "ctrl+o",
    };
    let rows = rows(event, w, &ctx);
    for row in &rows {
        assert!(text::line_width(row) <= w as usize, "{w}: {row:?}");
    }
    rows.iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn switch(reason: SwitchReason) -> UiEvent {
    UiEvent::new(
        EventKind::Model,
        "Primary model failed; continuing with fallback",
    )
    .with_detail(EventDetail::ModelSwitch {
        from: Some("anthropic/opus".into()),
        to: "openai/gpt".into(),
        reason,
    })
}

#[test]
fn a_model_switch_reads_from_to_and_why_with_the_sentence_under_expand() {
    let rows = render(&switch(SwitchReason::Fallback), 90, false);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].contains("anthropic/opus → openai/gpt")
            && rows[0].contains("fallback after an error"),
        "{rows:?}"
    );
    let expanded = render(&switch(SwitchReason::Promotion), 90, true).join("\n");
    assert!(
        expanded.contains("larger context window") && expanded.contains("Primary model failed")
    );
    for w in [1, 8, 20, 40] {
        render(&switch(SwitchReason::Restore), w, false);
    }
}

#[test]
fn an_advisor_review_is_a_quoted_card_marked_unverified() {
    let event = UiEvent::new(
        EventKind::Coach,
        "Advisor concern: tests skip the error path",
    )
    .with_detail(EventDetail::Review {
        severity: "concern".into(),
        note: "tests skip the error path".into(),
    });
    let rows = render(&event, 60, false);
    assert!(rows[0].starts_with('╭') && rows[0].contains("Advisor") && rows[0].contains("concern"));
    assert!(
        rows.iter()
            .any(|r| r.contains("▏ tests skip the error path")),
        "{rows:?}"
    );
    assert!(
        rows.last().unwrap().contains("unverified advice"),
        "{rows:?}"
    );
    // Too narrow for a card: the plain sentence line.
    let narrow = render(&event, 24, false).join("\n");
    assert!(!narrow.contains('╭'), "{narrow}");
}

#[test]
fn a_recovery_is_an_amber_card_that_folds_long_reports() {
    let event = UiEvent::new(
        EventKind::Recovery,
        "Stream stalled after partial text; continuing (1/3)",
    );
    let rows = render(&event, 60, false);
    assert!(
        rows[0].contains("Recovered") && rows[1].contains("Stream stalled"),
        "{rows:?}"
    );
    let long = UiEvent::new(EventKind::Recovery, "a\nb\nc\nd\ne");
    let folded = render(&long, 60, false);
    assert!(folded.last().unwrap().contains("2 more"), "{folded:?}");
    assert_eq!(folded.len(), 5, "top edge, three rows, foot: {folded:?}");
    assert!(folded[3].contains("│ c"), "{folded:?}");
    let open = render(&long, 60, true).join("\n");
    assert!(open.contains("│ e"), "{open}");
}

#[test]
fn a_check_that_did_not_pass_names_its_status_in_the_failure_color() {
    assert_eq!(
        check_problem("Verification lint: CodeFailed"),
        Some(("lint".into(), "code failed".into()))
    );
    assert_eq!(
        check_problem("Verification unit tests: TimedOut"),
        Some(("unit tests".into(), "timed out".into()))
    );
    assert_eq!(
        check_problem("Recording the configured check baseline before edits."),
        None
    );
    let theme = Theme::builtin("dial-dark").unwrap();
    let ui = UiConfig::default();
    let ctx = TranscriptCtx {
        theme: &theme,
        ui: &ui,
        tick: 0,
        tools_expanded: false,
        expand_key: "ctrl+o",
    };
    let failed = rows(
        &UiEvent::new(EventKind::Verification, "Verification lint: CodeFailed"),
        60,
        &ctx,
    );
    let first = &failed[0].spans[0];
    assert_eq!(first.content, theme.sym(Sym::Cross2));
    assert_eq!(first.style.fg, theme.fg(Tok::Error).fg);
    let plain: String = failed[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(plain.contains("lint · code failed"), "{plain}");
    // An informational verification event never shows a success mark in green.
    let baseline = rows(
        &UiEvent::new(
            EventKind::Verification,
            "Recording the configured check baseline before edits.",
        ),
        60,
        &ctx,
    );
    assert_ne!(baseline[0].spans[0].style.fg, theme.fg(Tok::Success).fg);
    // The ASCII preset joins the status with its ASCII dot, never a Unicode `·`.
    let ascii = theme.clone().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let plain: String = rows(
        &UiEvent::new(EventKind::Verification, "Verification lint: CodeFailed"),
        60,
        &TranscriptCtx {
            theme: &ascii,
            ..ctx
        },
    )[0]
    .spans
    .iter()
    .map(|s| s.content.as_ref())
    .collect();
    assert!(plain.contains("lint - code failed"), "{plain}");
    assert!(!plain.contains('·'), "{plain}");
}

#[test]
fn an_investigation_rewind_draws_a_divider() {
    let event = UiEvent::new(
        EventKind::Investigation,
        "Investigation rewound to its anchored report (e1, checkpoint e2, archived exploration e3).",
    );
    let rows = render(&event, 60, false);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].contains("rewound") && rows[0].starts_with('─'),
        "{rows:?}"
    );
    let committed = UiEvent::new(
        EventKind::Investigation,
        "Investigation checkpoint committed.",
    );
    assert!(!render(&committed, 60, false)[0].starts_with('─'));
}
