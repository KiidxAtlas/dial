use super::super::{Transcript, TranscriptCtx, TurnOutcome, TurnStats};
use super::*;
use crate::llm::model_config::UiConfig;
use crate::ui::theme::{Glyphs, Theme};

fn plain(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|row| row.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn stats(outcome: Option<TurnOutcome>) -> TurnStats {
    TurnStats {
        clock: String::new(),
        elapsed_secs: 12.5,
        prompt_tokens: Some(18_000),
        output_tokens: Some(900),
        tokens_per_second: Some(42.0),
        outcome,
    }
}

fn ctx<'a>(theme: &'a Theme, ui: &'a UiConfig) -> TranscriptCtx<'a> {
    TranscriptCtx {
        theme,
        ui,
        tick: 0,
        tools_expanded: false,
        expand_key: "Ctrl+O",
    }
}

fn presentation(tone: Tone, target: &str) -> Presentation {
    Presentation {
        tone,
        target: target.into(),
        ..Presentation::default()
    }
}

#[test]
fn calls_are_counted_by_kind_and_edited_files_listed_once() {
    let mut recap = Recap::default();
    recap.note(
        "read",
        ToolState::Completed,
        &presentation(Tone::Tools, "a.rs"),
    );
    recap.note(
        "edit",
        ToolState::Completed,
        &presentation(Tone::Edit, "src/a.rs"),
    );
    recap.note(
        "edit",
        ToolState::Completed,
        &presentation(Tone::Edit, "src/a.rs"),
    );
    recap.note(
        "write",
        ToolState::Failed,
        &presentation(Tone::Edit, "src/b.rs"),
    );
    recap.note(
        "bash",
        ToolState::Completed,
        &presentation(Tone::Shell, "ls"),
    );
    recap.note(
        "web",
        ToolState::Completed,
        &presentation(Tone::Connect, "x"),
    );
    recap.note("task", ToolState::Completed, &presentation(Tone::Tools, ""));
    recap.note(
        "mcp__docs__search",
        ToolState::Completed,
        &presentation(Tone::Connect, "q"),
    );
    assert_eq!(
        recap,
        Recap {
            calls: 8,
            lookups: 1,
            commands: 1,
            edits: 3,
            web: 1,
            services: 1,
            workers: 1,
            failed: 1,
            files: vec!["src/a.rs".into()],
            ..Recap::default()
        }
    );
}

fn settled(name: &str, tone: Tone, target: &str, input: &str, output: &str, ok: bool) -> Tool {
    Tool {
        id: name.into(),
        name: name.into(),
        summary: String::new(),
        presentation: presentation(tone, target),
        input: input.into(),
        state: if ok {
            ToolState::Completed
        } else {
            ToolState::Failed
        },
        output: output.into(),
        progress: None,
        elapsed: Some(0.5),
        todo: None,
        agents: Vec::new(),
        outcome: None,
    }
}

#[test]
fn edits_add_their_line_counts_and_the_last_check_names_the_verdict() {
    let theme = Theme::dark();
    let ui = UiConfig::default();
    let mut recap = Recap::default();
    let edits = [
        ("src/a.rs", "change preview (+3 -1 lines)"),
        ("src/a.rs", "change preview (+2 -0 lines)"),
        ("src/b.rs", "updated"),
    ];
    for (path, output) in edits {
        let t = settled("edit", Tone::Edit, path, "{}", output, true);
        recap.note(&t.name, t.state, &t.presentation);
        recap.note_result(&t);
    }
    let test = |output: &str, ok: bool| {
        settled(
            "bash",
            Tone::Shell,
            "cargo test",
            r#"{"command":"cargo test"}"#,
            output,
            ok,
        )
    };
    for t in [
        test("test result: FAILED. 1 passed; 1 failed; 0 ignored", false),
        test("test result: ok. 9 passed; 0 failed; 0 ignored", true),
    ] {
        recap.note(&t.name, t.state, &t.presentation);
        recap.note_result(&t);
    }
    assert_eq!(recap.lines.get("src/a.rs"), Some(&(5, 1)));
    assert_eq!(recap.lines.get("src/b.rs"), None);
    let check = recap.check.clone().expect("verdict");
    assert!(check.ok && check.summary == "9 passed", "{check:?}");

    let rows = card(&stats(None), &recap, 60, &ctx(&theme, &ui)).unwrap();
    let lines = plain(&rows);
    let all = lines.join("\n");
    assert!(all.contains("tests 9 passed"), "{all}");
    let a = lines.iter().find(|l| l.contains("src/a.rs")).unwrap();
    assert!(a.contains("+5") && a.trim_end().ends_with("1 │"), "{a}");
    let b = lines.iter().find(|l| l.contains("src/b.rs")).unwrap();
    assert!(!b.contains('+'), "{b}");
    for line in &lines {
        assert_eq!(text::width(line), 60, "{line:?}");
    }
}

#[test]
fn a_turn_with_work_closes_with_a_framed_recap_card() {
    let theme = Theme::dark();
    let ui = UiConfig::default();
    let mut recap = Recap::default();
    recap.note(
        "edit",
        ToolState::Completed,
        &presentation(Tone::Edit, "src/lib.rs"),
    );
    recap.note(
        "bash",
        ToolState::Failed,
        &presentation(Tone::Shell, "cargo test"),
    );
    let outcome = TurnOutcome {
        status: "completed".into(),
        changed_files: 1,
        handoff: vec!["Checks: cargo test failed".into()],
        ..TurnOutcome::default()
    };
    let rows = card(&stats(Some(outcome)), &recap, 80, &ctx(&theme, &ui)).expect("card");
    let lines = plain(&rows);
    for line in &lines {
        assert_eq!(text::width(line), 80, "{line:?}");
    }
    let all = lines.join("\n");
    assert!(lines[0].starts_with(theme.sym(Sym::BoxTl)), "{all}");
    assert!(lines[0].contains("1 file changed"), "{all}");
    assert!(all.contains("2 calls") && all.contains("1 edit") && all.contains("1 failed"));
    assert!(
        all.contains("Changed") && all.contains("src/lib.rs"),
        "{all}"
    );
    assert!(
        all.contains("Handoff") && all.contains("cargo test failed"),
        "{all}"
    );
    assert!(
        all.contains("/review-file") && all.contains("/undo"),
        "{all}"
    );
    let last = lines.last().unwrap();
    assert!(last.contains("12.5s") || last.contains("12"), "{last}");
    assert!(last.contains("18k in") || last.contains("in"), "{last}");
    // The outline takes the accent card border, like edit cards.
    assert_eq!(rows[0].spans[0].style.fg, theme.fg(Tok::BorderAccent).fg);
}

#[test]
fn failed_turns_take_the_error_outline_and_plain_answers_keep_one_line() {
    let theme = Theme::dark();
    let ui = UiConfig::default();
    let mut recap = Recap::default();
    recap.note(
        "read",
        ToolState::Completed,
        &presentation(Tone::Tools, "a"),
    );
    let failed = TurnOutcome {
        status: "failed".into(),
        ..TurnOutcome::default()
    };
    let rows = card(&stats(Some(failed)), &recap, 60, &ctx(&theme, &ui)).unwrap();
    assert_eq!(rows[0].spans[0].style.fg, theme.fg(Tok::Error).fg);
    assert!(card(&stats(None), &Recap::default(), 80, &ctx(&theme, &ui)).is_none());
    assert!(card(&stats(None), &recap, 39, &ctx(&theme, &ui)).is_none());
}

#[test]
fn the_transcript_gathers_the_turns_calls_into_its_closing_card() {
    let theme = Theme::dark();
    let ui = UiConfig::default();
    let mut t = Transcript::new();
    t.push_user("first");
    t.tool_start("0", "bash", "old turn", "");
    t.tool_end("0", "ok", false, 0.1);
    t.push_user("second");
    t.tool_start("1", "bash", "echo hi", r#"{"command":"echo hi"}"#);
    t.tool_end("1", "hi", false, 0.2);
    t.tool_start("2", "read", "src/main.rs", r#"{"path":"src/main.rs"}"#);
    t.tool_end("2", "fn main() {}", false, 0.1);
    t.turn_stats(stats(None));
    t.finish_turn();
    let text = plain(&t.all_rows(80, &ctx(&theme, &ui))).join("\n");
    assert!(
        text.contains("2 calls"),
        "only this turn's calls count: {text}"
    );
    for w in 1..=100u16 {
        for row in t.all_rows(w, &ctx(&theme, &ui)) {
            assert!(text::line_width(&row) <= w as usize);
        }
    }
    let ascii = Theme::dark().with_glyphs(Glyphs::Ascii);
    for row in plain(&t.all_rows(80, &ctx(&ascii, &ui))) {
        assert!(row.is_ascii(), "{row:?}");
    }
}

#[test]
fn many_changed_files_are_bounded() {
    let theme = Theme::dark();
    let ui = UiConfig::default();
    let mut recap = Recap::default();
    for i in 0..10 {
        recap.note(
            "edit",
            ToolState::Completed,
            &presentation(Tone::Edit, &format!("src/file_{i}.rs")),
        );
    }
    let text = plain(&card(&stats(None), &recap, 60, &ctx(&theme, &ui)).unwrap()).join("\n");
    assert!(text.contains("src/file_5.rs") && !text.contains("src/file_6.rs"));
    assert!(text.contains("+4 more"), "{text}");
}
