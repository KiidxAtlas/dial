use super::*;
use crate::llm::model_config::UiConfig;

fn tool(name: &str, state: ToolState) -> Tool {
    Tool {
        id: "call".into(),
        name: name.into(),
        summary: "Lookup".into(),
        presentation: crate::runtime::presentation::Presentation::default(),
        input: "{}".into(),
        state,
        output: "service evidence\nsecond line".into(),
        progress: None,
        elapsed: Some(0.8),
        todo: None,
        agents: Vec::new(),
    }
}

#[test]
fn mcp_identity_status_evidence_and_bounds() {
    for theme_name in ["dial-dark", "dial", "mono"] {
        for ascii in [false, true] {
            let mut theme = Theme::builtin(theme_name).unwrap();
            if ascii {
                theme = theme.with_glyphs(super::super::theme::Glyphs::Ascii);
            }
            let ui = UiConfig::default();
            for state in [
                ToolState::Running,
                ToolState::Completed,
                ToolState::Failed,
                ToolState::Interrupted,
            ] {
                for expanded in [false, true] {
                    let ctx = TranscriptCtx {
                        theme: &theme,
                        ui: &ui,
                        tick: 0,
                        tools_expanded: expanded,
                        expand_key: "ctrl+o",
                    };
                    let t = tool("mcp__context7__query-docs", state);
                    for width in [0, 1, 5, 6, 12, 38, 80] {
                        let rows = tool_rows(&t, width, &ctx);
                        assert!(
                            rows.iter()
                                .all(|row| text::line_width(row) <= width as usize)
                        );
                    }
                    let rows = tool_rows(&t, 80, &ctx);
                    let rendered = rows
                        .iter()
                        .flat_map(|r| r.spans.iter())
                        .map(|s| s.content.as_ref())
                        .collect::<String>();
                    assert!(rendered.contains("context7"));
                    assert!(rendered.contains("query-docs"));
                    if expanded {
                        assert!(rendered.contains("service evidence"));
                    }
                    if state == ToolState::Failed {
                        assert!(rendered.contains("failed"));
                    }
                    // A service frame: titled top edge naming MCP and the server, sides, bottom.
                    let lines = plain(&rows);
                    let (top, bottom) = (&lines[0], lines.last().unwrap());
                    let (tl, bl, side) = if ascii {
                        ('+', '+', '|')
                    } else {
                        ('╭', '╰', '│')
                    };
                    assert!(top.starts_with(tl) && top.contains("MCP"), "{top:?}");
                    assert!(bottom.starts_with(bl), "{bottom:?}");
                    for row in &lines[1..lines.len() - 1] {
                        assert!(row.starts_with(side) && row.ends_with(side), "{row:?}");
                    }
                    assert_eq!(rows[1].spans[0].style.bg, theme.bg(Tok::PanelBg).bg);
                    if state == ToolState::Failed {
                        let glyph = &rows[1].spans[1];
                        assert_eq!(glyph.style.fg, theme.fg(Tok::Error).fg);
                    }
                }
            }
        }
    }
}

#[test]
fn native_and_incomplete_mcp_names_are_not_service_cards() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let ui = UiConfig::default();
    let ctx = TranscriptCtx {
        theme: &theme,
        ui: &ui,
        tick: 0,
        tools_expanded: false,
        expand_key: "ctrl+o",
    };
    for name in [
        "read",
        "tool_search",
        "mcp_servers",
        "mcp__server",
        "mcp____action",
        "mcp__server__",
    ] {
        let t = tool(name, ToolState::Completed);
        assert_eq!(tool_rows(&t, 80, &ctx), card(&t, 80, &ctx));
    }
}

fn ctx_for<'a>(theme: &'a Theme, ui: &'a UiConfig, expanded: bool) -> TranscriptCtx<'a> {
    TranscriptCtx {
        theme,
        ui,
        tick: 0,
        tools_expanded: expanded,
        expand_key: "Ctrl+O",
    }
}

fn ascii_theme() -> Theme {
    Theme::builtin("dial-dark")
        .unwrap()
        .with_glyphs(super::super::theme::Glyphs::Ascii)
}

fn plain(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn settled(name: &str, input: &str, output: &str, failed: bool, secs: f64) -> Tool {
    let mut t = tool(name, ToolState::Running);
    t.input = input.into();
    t.presentation = crate::agent::legacy_tool_presentation(name, "", input);
    let args = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    t.presentation = crate::agent::legacy_tool_result(name, &args, output, failed);
    t.state = if failed {
        ToolState::Failed
    } else {
        ToolState::Completed
    };
    t.output = output.into();
    t.elapsed = Some(secs);
    t
}

#[test]
fn result_previews_wrap_on_word_boundaries() {
    let theme = ascii_theme();
    let ui = UiConfig::default();
    let t = settled(
        "bash",
        r#"{"command":"cargo test"}"#,
        "test result: ok. 18 passed; 0 failed",
        false,
        1.2,
    );
    let rows = plain(&tool_rows(&t, 38, &ctx_for(&theme, &ui, false)));
    assert!(rows.iter().all(|r| !r.ends_with("fai")), "{rows:?}");
    assert!(rows.iter().any(|r| r.trim() == "failed"), "{rows:?}");
}

#[test]
fn narrow_mcp_headers_drop_timing_before_the_target() {
    let theme = ascii_theme();
    let ui = UiConfig::default();
    let t = settled(
        "mcp__docs__search",
        r#"{"query":"migration"}"#,
        "Service unavailable: request timed out.",
        true,
        1.1,
    );
    let rows = plain(&tool_rows(&t, 38, &ctx_for(&theme, &ui, false)));
    assert!(
        rows[0].contains("MCP : docs"),
        "server named in the frame: {rows:?}"
    );
    let head = &rows[1];
    assert!(
        head.contains("search") && head.contains("migration"),
        "action and the whole target stay: {head:?}"
    );
    assert!(
        head.starts_with("| x") && head.contains("failed"),
        "{head:?}"
    );
    assert!(!head.contains("1.10s"), "timing gives way first: {head:?}");
    // With room, everything is shown.
    let wide = plain(&tool_rows(&t, 100, &ctx_for(&theme, &ui, false)));
    assert!(wide[1].contains("failed") && wide[1].contains("1.10s"));
}

#[test]
fn clipped_targets_end_with_an_ellipsis_and_paths_keep_their_file_name() {
    let theme = ascii_theme();
    let ui = UiConfig::default();
    let ctx = ctx_for(&theme, &ui, false);
    let cmd = settled(
        "bash",
        r#"{"command":"cargo build --release --workspace --all-features"}"#,
        "failed\n[exit code: 101]",
        true,
        3.4,
    );
    let head = &plain(&tool_rows(&cmd, 38, &ctx))[0];
    assert!(
        head.contains("cargo build") && head.contains("..."),
        "{head:?}"
    );
    assert!(
        head.contains("exit 101") && !head.contains("failed"),
        "{head:?}"
    );
    let read = settled(
        "read",
        r#"{"path":"crates/dial-ui/src/ui/overlay/subagents/tests.rs"}"#,
        "1: a",
        false,
        0.1,
    );
    let head = &plain(&tool_rows(&read, 38, &ctx))[0];
    assert!(
        head.contains("...") && head.contains("tests.rs"),
        "{head:?}"
    );
}

#[test]
fn wrapped_diff_lines_hang_under_the_code_column() {
    let theme = Theme::builtin("dial-dark").unwrap();
    let ui = UiConfig::default();
    let output = "[src/lib.rs#EF7D]\nupdated (+1 -1 lines)\n\nchange preview (+1 -1 lines)\n@@ -1,2 +1,2 @@\n fn a() {\n-    old_function_name_that_is_long(argument_one, argument_two);\n+    new();\n";
    let t = settled("edit", r#"{"path":"src/lib.rs"}"#, output, false, 0.1);
    let rows = plain(&tool_rows(&t, 40, &ctx_for(&theme, &ui, false)));
    assert!(rows[0].contains("+1 −1"), "{rows:?}");
    let at = rows
        .iter()
        .position(|r| r.contains("old_function"))
        .unwrap();
    let next = &rows[at + 1];
    let code_col = rows[at].find("-    old").unwrap();
    assert!(
        next[..code_col].trim().is_empty() && !next[code_col..].starts_with(' '),
        "continuation starts at the code column: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("updated (")),
        "the header already states the totals: {rows:?}"
    );
}

#[test]
fn a_finished_plan_says_so() {
    use crate::agent::workflow::todo::TodoList;
    let theme = Theme::builtin("dial-dark").unwrap();
    let ui = UiConfig::default();
    let mut before = TodoList::new();
    before.call(
        &serde_json::json!({"op": "set", "tasks": [{"content": "read"}, {"content": "fix"}]}),
    );
    let mut after = before.clone();
    after.call(&serde_json::json!({"op": "done"}));
    after.call(&serde_json::json!({"op": "done"}));
    let mut t = settled("todo", r#"{"op":"done"}"#, "2/2 done", false, 0.0);
    t.todo = Some(crate::ui::tasks::changes(&before, &after));
    let rows = plain(&tool_rows(&t, 60, &ctx_for(&theme, &ui, false)));
    assert!(rows[0].contains("all 2 done"), "{rows:?}");
}
