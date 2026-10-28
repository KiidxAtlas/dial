use super::*;
use crate::llm::model_config::UiConfig;
use crate::ui::theme::{Glyphs, Theme};
use ratatui::text::Line;

/// Runs `f` against a transcript context in the given glyph set and expansion.
fn ctx_with(ascii: bool, expanded: bool, f: impl FnOnce(&TranscriptCtx)) {
    let mut theme = Theme::builtin("dial-dark").unwrap();
    if ascii {
        theme = theme.with_glyphs(Glyphs::Ascii);
    }
    let ui = UiConfig::default();
    f(&TranscriptCtx {
        theme: &theme,
        ui: &ui,
        tick: 0,
        tools_expanded: expanded,
        expand_key: "ctrl+o",
    });
}

fn rendered(rows: &[Line<'_>]) -> Vec<String> {
    rows.iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn sent(name: &str, input: &str, output: &str, state: ToolState) -> Tool {
    Tool {
        id: "call".into(),
        name: name.into(),
        summary: String::new(),
        presentation: Default::default(),
        input: input.into(),
        state,
        output: output.into(),
        progress: None,
        elapsed: Some(0.01),
        todo: None,
        agents: Vec::new(),
    }
}

#[test]
fn a_delivered_note_reads_as_a_quoted_message_not_work() {
    ctx_with(false, false, |ctx| {
        let t = sent(
            "message",
            r#"{"to":"reviewer","text":"please look at the diff"}"#,
            r#"{"receipts":[{"to":"reviewer","status":"queued"}]}"#,
            ToolState::Completed,
        );
        let rows = rendered(&irc_card(&t, 60, ctx).unwrap());
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].starts_with('✉'), "{rows:?}");
        assert!(rows[0].contains("IRC") && rows[0].contains("➤ reviewer"));
        assert!(rows[0].trim_end().ends_with("queued"), "{rows:?}");
        assert_eq!(rows[1].trim_end(), "   ▏ please look at the diff");
    });
}

#[test]
fn a_broadcast_lists_each_receipt_and_why_one_was_refused() {
    ctx_with(true, false, |ctx| {
        let t = sent(
            "message",
            r#"{"to":"all","text":"rebase first"}"#,
            r#"{"receipts":[{"to":"a","status":"queued"},{"to":"c","status":"refused","reason":"not running"}]}"#,
            ToolState::Completed,
        );
        let rows = rendered(&irc_card(&t, 60, ctx).unwrap());
        assert!(
            rows[0].starts_with('@') && rows[0].contains("-> all"),
            "{rows:?}"
        );
        assert!(
            rows[0].contains("1 queued") && rows[0].contains("1 refused"),
            "{rows:?}"
        );
        assert_eq!(rows[1].trim_end(), "   | rebase first");
        assert_eq!(rows[2].trim_end(), "   |- a [queued]");
        assert_eq!(rows[3].trim_end(), "   `- c [refused] - not running");
        assert!(rows.iter().all(|r| r.is_ascii()), "{rows:?}");
    });
}

#[test]
fn nothing_delivered_is_flagged_and_peers_send_shares_the_lane() {
    ctx_with(false, false, |ctx| {
        let t = sent(
            "peers",
            r#"{"action":"send","to":"490adb","text":"hi"}"#,
            r#"{"to":"490adb","status":"refused","reason":"gone"}"#,
            ToolState::Completed,
        );
        let rows = rendered(&irc_card(&t, 60, ctx).unwrap());
        assert!(rows[0].starts_with(ctx.theme.sym(Sym::Cross2)), "{rows:?}");
        assert!(rows[0].trim_end().ends_with("refused"), "{rows:?}");
        assert!(
            rows.last().unwrap().contains("490adb ⟦refused⟧ — gone"),
            "{rows:?}"
        );
    });
}

#[test]
fn other_calls_failures_and_long_notes_keep_their_bounds() {
    let long = format!(r#"{{"to":"x","text":"{}"}}"#, "word ".repeat(80));
    ctx_with(false, false, |ctx| {
        // Listing recipients or claiming scopes is not a message.
        for (name, input) in [
            ("message", "{}"),
            ("peers", r#"{"action":"claim","scopes":["a"]}"#),
            ("read", r#"{"text":"x"}"#),
        ] {
            assert!(irc_card(&sent(name, input, "", ToolState::Completed), 60, ctx).is_none());
        }
        let failed = sent(
            "message",
            r#"{"to":"x","text":"y"}"#,
            "boom",
            ToolState::Failed,
        );
        assert!(irc_card(&failed, 60, ctx).is_none());

        let running = sent("message", &long, "", ToolState::Running);
        for w in [1, 8, 20, 60] {
            let rows = irc_card(&running, w, ctx).unwrap();
            assert!(rows.iter().all(|r| text::line_width(r) <= w), "width {w}");
        }
        let rows = rendered(&irc_card(&running, 40, ctx).unwrap());
        assert_eq!(rows.len(), 1 + QUOTE_ROWS + 1, "{rows:?}");
    });
    ctx_with(false, true, |ctx| {
        let rows = irc_card(&sent("message", &long, "", ToolState::Running), 40, ctx).unwrap();
        assert!(rows.len() > 1 + QUOTE_ROWS + 1);
    });
}

#[test]
fn unknown_receipts_keep_evidence_and_interrupted_notes_name_the_phase() {
    ctx_with(false, false, |ctx| {
        let input = r#"{"to":"reviewer","text":"Please review"}"#;
        for output in [
            "delivery service unavailable",
            r#"{"to":"reviewer","status":"unknown"}"#,
        ] {
            let t = sent("message", input, output, ToolState::Completed);
            assert!(
                irc_card(&t, 80, ctx).is_none(),
                "unknown evidence: {output}"
            );
        }
        let t = sent("message", input, "", ToolState::Interrupted);
        let rows = rendered(&irc_card(&t, 80, ctx).unwrap());
        assert!(rows[0].contains("interrupted"), "{rows:?}");
    });
}
