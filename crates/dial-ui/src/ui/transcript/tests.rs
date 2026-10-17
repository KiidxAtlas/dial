use super::*;
use crate::ui::theme::Glyphs;
use ratatui::style::Modifier;
#[test]
fn coherent_runtime_custom_cards_keep_state_preview_and_expanded_evidence() {
    use crate::runtime::{
        Phase, Tree,
        presentation::{BodyStyle, Presentation},
    };
    let env = Env::new();
    let tree = Tree::default();
    let body = tree.start("tool", "extension_name", None);
    body.correlate("custom-call");
    body.present(Presentation {
        label: "Inspect\u{1b}[31m".into(),
        target: "parser ownership\n".into(),
        body: BodyStyle::Text,
        preview: Some("short finding".into()),
        facts: vec!["3 findings".into()],
        ..Default::default()
    });
    body.settle(
        "FULL_EVIDENCE_SENTINEL\nsecond line".into(),
        None,
        Phase::Incomplete,
    );
    let mut transcript = Transcript::new();
    transcript.tool_start("custom-call", "extension_name", "opaque summary", "");
    transcript.bind_tool("custom-call", body);
    transcript.tool_end("custom-call", "ignored duplicate", false, 0.1);
    let compact = text_of(&transcript.all_rows(100, &env.ctx()));
    assert!(
        compact.contains("Inspect")
            && compact.contains("parser ownership")
            && compact.contains("incomplete")
    );
    assert!(compact.contains("short finding") && !compact.contains("FULL_EVIDENCE_SENTINEL"));
    let expanded = text_of(&transcript.all_rows(
        100,
        &TranscriptCtx {
            tools_expanded: true,
            ..env.ctx()
        },
    ));
    assert!(expanded.contains("FULL_EVIDENCE_SENTINEL") && expanded.contains("second line"));
    assert!(!expanded.contains("ignored duplicate"));
    assert!(
        !expanded.contains("\u{1b}") && !compact.contains("\u{1b}"),
        "Tool-owned labels are still untrusted terminal text"
    );
}

#[test]
fn consistent_ux_expansion_reveals_committed_reads_searches_and_checks() {
    let env = Env::new();
    let mut transcript = Transcript::new();
    for (id, name, summary, result) in [
        (
            "read1",
            "read",
            "src/parser.rs",
            "[src/parser.rs#AB12]\n1: fn parse_tail() {}",
        ),
        (
            "search1",
            "grep",
            "parse_tail",
            "src/parser.rs:1: fn parse_tail() {}",
        ),
        (
            "check1",
            "bash",
            "Verify parser: cargo test",
            "test parse_tail ... ok\n[exit code: 0]",
        ),
    ] {
        let input = if name == "bash" {
            serde_json::json!({"command":"cargo test","_dial_check":"parser"}).to_string()
        } else {
            String::new()
        };
        transcript.tool_start(id, name, summary, &input);
        transcript.tool_end(id, result, false, 0.1);
    }
    transcript.take_settled(80, &env.ctx());
    let collapsed = text_of(&transcript.committed_rows(80, &env.ctx()));
    assert!(
        !collapsed.contains("fn parse_tail()") && !collapsed.contains("test parse_tail ... ok")
    );
    let expanded = TranscriptCtx {
        tools_expanded: true,
        ..env.ctx()
    };
    let rows = transcript.committed_rows(80, &expanded);
    assert_fits(&rows, 80);
    let text = text_of(&rows);
    assert!(text.contains("fn parse_tail()") && text.contains("test parse_tail ... ok"));
    assert!(
        transcript.take_settled(80, &expanded).is_empty(),
        "repainting does not redeliver committed records"
    );
}

#[test]
fn consistent_ux_worker_cards_expand_reports_without_internal_json() {
    let env = Env::new();
    let mut transcript = Transcript::new();
    transcript.tool_start("task1", "task", "Parser scout", "");
    transcript.tool_end("task1", "Worker task-1: done", false, 0.1);
    transcript.tool_data("task1", &serde_json::json!([{"id":"task-1","name":"Parser scout","status":"done","steps":3,"result":"The parser drops the final line.\nFix parse_tail in parser.rs.","assignment":{"task":"Inspect parser"},"portable_rules":{"regenerations":0}}]));
    let expanded = TranscriptCtx {
        tools_expanded: true,
        ..env.ctx()
    };
    let text = text_of(&transcript.all_rows(80, &expanded));
    assert!(text.contains("Fix parse_tail in parser.rs"));
    assert!(!text.contains("portable_rules") && !text.contains("regenerations"));
}

#[test]
fn consistent_ux_worker_card_settles_with_its_workers() {
    use crate::runtime::{Phase, Tree};
    let env = Env::new();
    let tree = Tree::default();
    let call = tree.start("tool", "task", None);
    let worker = tree.start("worker", "Parser scout", Some(call.read(|n| n.id.clone())));
    let mut transcript = Transcript::new();
    transcript.tool_start("spawn", "task", "Parser scout", "");
    transcript.bind_tool("spawn", call.clone());
    call.settle("Worker started".into(), None, Phase::Completed);
    transcript.tool_end("spawn", "Worker started", false, 0.1);
    transcript.sync_agents(&[worker.snapshot()], 100);
    assert!(transcript.take_settled(80, &env.ctx()).is_empty());
    worker.settle(
        "Parser drops the final line.".into(),
        None,
        Phase::Completed,
    );
    transcript.sync_agents(&[worker.snapshot()], 200);
    let expanded = TranscriptCtx {
        tools_expanded: true,
        ..env.ctx()
    };
    let settled = text_of(&transcript.take_settled(80, &expanded));
    assert!(settled.contains("Parser drops the final line."));
    assert!(transcript.take_settled(80, &expanded).is_empty());

    transcript.tool_start("status", "task", "Worker status", "");
    transcript.tool_end("status", "done", false, 0.1);
    transcript.tool_data(
        "status",
        &serde_json::json!({"id":"task-1","status":"done","result":"Parser drops the final line."}),
    );
    transcript.sync_agents(&[worker.snapshot()], 300);
    assert!(
        text_of(&transcript.take_settled(80, &expanded)).contains("Parser drops the final line.")
    );
}

#[test]
fn consistent_ux_compact_diffs_honor_the_requested_preview_limit() {
    let mut env = Env::new();
    env.ui.tool_output_lines = 3;
    let mut transcript = Transcript::new();
    transcript.tool_start("edit1", "edit", "a.rs", "");
    let lines = (0..30).map(|i| format!("+line_{i}\n")).collect::<String>();
    transcript.tool_end(
        "edit1",
        &format!(
            "[a.rs#AB12]\nupdated (+30 -0 lines)\nchange preview (\n@@ -1,0 +1,30 @@\n{lines}"
        ),
        false,
        0.1,
    );
    let compact = text_of(&transcript.all_rows(80, &env.ctx()));
    assert!(!compact.contains("line_20") && compact.contains("more lines"));
    let expanded = TranscriptCtx {
        tools_expanded: true,
        ..env.ctx()
    };
    assert!(text_of(&transcript.all_rows(80, &expanded)).contains("line_29"));
}

#[test]
fn consistent_ux_rendered_fixture_prioritizes_findings_and_the_composer() {
    use crate::ui::{
        composer::{Composer, ComposerCtx},
        keys::Keymap,
        status::StatusData,
        welcome::{self, WelcomeInfo},
    };
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
    let mut env = Env::new();
    env.theme = Theme::builtin("dial-dark").unwrap();
    let mut transcript = Transcript::new();
    transcript.push_user("Why do agents in Dial randomly stop?");
    transcript.tool_start("r", "read", "src/agent/child.rs", "");
    transcript.tool_end(
        "r",
        "[src/agent/child.rs#AB12]\n1: child completion path",
        false,
        0.12,
    );
    transcript.tool_start("t", "task", "Trace recovery", "");
    transcript.tool_end("t", "Worker task-1: done", false, 0.5);
    transcript.tool_data("t", &serde_json::json!([{"name":"Recovery scout","agent":"scout","status":"done","steps":3,"result":"Malformed child replies bypassed the parent's retry limit."}]));
    transcript.assistant_delta("The child loop could retry malformed calls indefinitely. Parent and child now share bounded recovery, and an unfinished report stays incomplete.");
    transcript.finish_turn();
    let mut rows = welcome::rows(
        &WelcomeInfo {
            version: "0.1.1".into(),
            provider: "chatgpt".into(),
            model: "gpt-6.1-sol".into(),
            project: "dial".into(),
            ..Default::default()
        },
        100,
        &env.theme,
    );
    rows.extend(transcript.all_rows(100, &env.ctx()));
    let keys = Keymap::defaults();
    let status = StatusData {
        model: "gpt-6.1-sol".into(),
        provider: "chatgpt".into(),
        project_path: "dial".into(),
        ..Default::default()
    };
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_draft("Keep working on the parser");
    rows.extend(
        composer
            .view(
                100,
                &ComposerCtx {
                    theme: &env.theme,
                    keys: &keys,
                    ui: &env.ui,
                    status: &status,
                },
            )
            .rows,
    );
    let height = rows.len() as u16;
    let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(rows.clone()), frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    assert!(
        screen.contains("Malformed child replies") && screen.contains("Keep working on the parser")
    );
    assert!(
        !screen.contains("portable_rules")
            && !screen.contains("PROMPT CACHE")
            && !screen.contains("DIAL MK-I")
    );
    assert_fits(&rows, 100);
    if let Ok(path) = std::env::var("DIAL_TEST_TRANSCRIPT_CAPTURE") {
        let cells: Vec<_> = buffer.content.iter().map(|cell| serde_json::json!({"symbol":cell.symbol(), "fg":format!("{:?}",cell.fg), "bg":format!("{:?}",cell.bg), "modifiers":format!("{:?}",cell.modifier)})).collect();
        std::fs::write(
            path,
            serde_json::json!({"width":100,"height":height,"cells":cells}).to_string(),
        )
        .unwrap();
    }
}

#[test]
fn streaming_prefix_matches_full_render_at_every_delta() {
    for source in [
        "Hello **world**.\n\nAnother paragraph.\n\nFinal.",
        "- first\n- second\n\nParagraph\n\nNext",
        "> quoted\n>\n> more\n\nParagraph",
        "```rust\nlet n = 1;\n\n// comment\n```\n\nAfter.\n\nMore",
        "| A | B |\n|---|---|\n| 1 | 2 |\n\nParagraph\n\nNext",
        "A [reference][id].\n\n[id]: https://example.com\n",
        "Plain prefix\n\nAnother paragraph\n\n[link]: https://example.com\n",
        "日本語 text\n\nAnother é paragraph",
    ] {
        for width in [20, 60] {
            let mut cache = StreamingRows::default();
            let theme = Theme::dark();
            for end in (0..=source.len()).filter(|&n| source.is_char_boundary(n)) {
                assert_eq!(
                    *cache.render(&source[..end], width, &theme),
                    markdown::render(&source[..end], width, &theme, false),
                    "at {end}, width {width}: {source:?}"
                );
            }
        }
    }
    let source = "A **styled prefix**.\n\nAnother paragraph.\n\nFinal.";
    let mut cache = StreamingRows::default();
    cache.render(source, 20, &Theme::dark());
    for theme in [Theme::light(), Theme::mono()] {
        for width in [10, 80] {
            assert_eq!(
                *cache.render(source, width, &theme),
                markdown::render(source, width, &theme, false)
            );
        }
    }
}

#[test]
fn streaming_cache_rerenders_when_settled_text_replaces_the_streamed_prefix() {
    let theme = Theme::dark();
    let mut cache = StreamingRows::default();
    let streamed = "First paragraph.\n\nSecond paragraph.\n\nTail";
    cache.render(streamed, 40, &theme);
    let settled = "Rewritten opening.\n\nSecond paragraph.\n\nTail";
    assert_eq!(
        *cache.render(settled, 40, &theme),
        markdown::render(settled, 40, &theme, false)
    );
}

#[test]
#[ignore = "manual streaming rendering measurement"]
fn streaming_render_measurement() {
    let paragraph = "A streamed answer with **styled words**, a number 42, and ordinary prose that wraps across terminal rows.\n\n";
    let source = paragraph.repeat(200);
    let source = &source[..20_000];
    let theme = Theme::dark();
    let before = std::time::Instant::now();
    for end in (40..=source.len()).step_by(40) {
        std::hint::black_box(markdown::render(&source[..end], 100, &theme, false));
    }
    let full = before.elapsed();
    let mut cache = StreamingRows::default();
    let before = std::time::Instant::now();
    for end in (40..=source.len()).step_by(40) {
        std::hint::black_box(cache.render(&source[..end], 100, &theme));
    }
    eprintln!(
        "20KB / 40-byte deltas: full={full:?}, cached={:?}",
        before.elapsed()
    );
}

/// Same workload as the byte-slice measurement, but with producer-owned `Vec` chunks.
#[test]
#[ignore = "manual Vec<T> streaming rendering measurement"]
fn streaming_render_vec_delta_measurement() {
    let paragraph = "A streamed answer with **styled words**, a number 42, and ordinary prose that wraps across terminal rows.\n\n";
    let source = paragraph.repeat(240);
    let answer = &source[..20_000];
    let deltas: Vec<String> = answer
        .as_bytes()
        .chunks(40)
        .map(|chunk| String::from_utf8(chunk.to_vec()).unwrap())
        .collect();
    let mut source = String::with_capacity(20_000);
    let mut cache = StreamingRows::default();
    let before = std::time::Instant::now();
    for delta in deltas {
        source.push_str(&delta);
        std::hint::black_box(cache.render(&source, 100, &Theme::dark()));
    }
    eprintln!("20KB / Vec<String> 40-byte deltas: {:?}", before.elapsed());
}

struct Env {
    theme: Theme,
    ui: UiConfig,
}

impl Env {
    fn new() -> Self {
        Env {
            theme: Theme::dark(),
            ui: UiConfig::default(),
        }
    }
    fn ctx(&self) -> TranscriptCtx<'_> {
        TranscriptCtx {
            theme: &self.theme,
            ui: &self.ui,
            tick: 0,
            tools_expanded: false,
            expand_key: "Ctrl+O",
        }
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

fn text_of(rows: &Rows) -> String {
    plain(rows).join("\n")
}

fn assert_fits(rows: &Rows, w: u16) {
    for row in rows {
        assert!(
            text::line_width(row) <= w as usize,
            "{:?} exceeds {w}",
            plain(&vec![row.clone()])
        );
    }
}

// ---- lifecycle ----

#[test]
fn settled_blocks_are_handed_out_once_in_order() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.push_user("hello");
    t.notice("Note", "first");
    t.error("second");
    let rows = t.take_settled(40, &ctx);
    let text = text_of(&rows);
    assert!(text.find("hello").unwrap() < text.find("first").unwrap());
    assert!(text.find("first").unwrap() < text.find("second").unwrap());
    assert!(
        !rows.last().unwrap().spans.is_empty(),
        "separators occur between settled blocks"
    );
    assert!(t.take_settled(40, &ctx).is_empty());
}

#[test]
fn background_output_renders_without_pending_tool_and_pty_replaces() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.background_output("pipe-job", "repeated\n", false);
    t.background_output("pipe-job", "repeated\n", false);
    t.background_output("pty-job", "old-screen", true);
    t.background_output("pty-job", "new-screen", true);
    let text = text_of(&t.take_settled(70, &ctx));
    assert!(
        text.contains("PIPE-JOB") && text.contains("PTY-JOB"),
        "{text}"
    );
    assert_eq!(
        text.matches("repeated").count(),
        2,
        "pipe deltas must not be deduplicated: {text}"
    );
    assert!(
        text.contains("new-screen") && !text.contains("old-screen"),
        "{text}"
    );
    t.background_output("pipe-job", "after scrollback", false);
    assert!(text_of(&t.take_settled(70, &ctx)).contains("after scrollback"));
}

#[test]
fn background_live_panel_is_utf8_bounded_while_full_output_stays_in_artifacts() {
    let mut t = Transcript::new();
    t.background_output("unicode", &"😀".repeat(5000), false);
    t.background_output("unicode", "done", false);
    let Kind::Notice { text, .. } = &t.blocks[0].kind else {
        panic!("missing job panel")
    };
    assert!(text.len() <= 16 * 1024);
    assert!(text.ends_with("done"));
    assert!(text.starts_with('😀'));
}

#[test]
fn a_job_panel_holds_its_place_and_is_filled_in_place() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.notice_pending("Usage");
    assert!(
        t.take_settled(60, &ctx).is_empty(),
        "the pending panel stays live"
    );
    assert!(t.job_waiting("Usage"));
    t.notice_merge("Usage", "No usage data.");
    t.settle_job_notices();
    let text = text_of(&t.take_settled(60, &ctx));
    assert!(!text.contains(Transcript::JOB_STARTED), "{text}");
    assert!(text.contains("No usage data."), "{text}");
    assert_eq!(text.matches("USAGE").count(), 1, "{text}");
}

#[test]
fn later_output_releases_a_pending_job_panel() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.notice_pending("Benchmark");
    t.push_user("keep working");
    assert!(
        !t.is_streaming(),
        "a long job must not hold the conversation back"
    );
    let text = text_of(&t.take_settled(60, &ctx));
    assert!(
        text.find("Started").unwrap() < text.find("keep working").unwrap(),
        "{text}"
    );
    t.notice_merge("Benchmark", "suite finished");
    let text = text_of(&t.take_settled(60, &ctx));
    assert!(
        text.contains("suite finished"),
        "the result follows as its own notice: {text}"
    );
}

#[test]
fn progress_lines_append_and_an_unfilled_panel_is_dropped() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.notice_pending("Images");
    t.notice_merge("Images", "Sessions covered: 0");
    t.notice_merge("Images", "Images found: 0");
    assert!(t.notice_has_output("Images"));
    t.settle_job_notices();
    let text = text_of(&t.take_settled(60, &ctx));
    assert!(
        text.contains("Sessions covered") && text.contains("Images found"),
        "{text}"
    );
    assert_eq!(text.matches("IMAGES").count(), 1, "{text}");

    t.notice_pending("Setup");
    t.settle_job_notices();
    assert!(!t.is_streaming());
    assert!(text_of(&t.take_settled(60, &ctx)).is_empty());
}

#[test]
fn settled_blocks_have_one_separator_between_them() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.notice("", "one");
    t.notice("", "two");
    assert_eq!(plain(&t.take_settled(20, &ctx)), ["one", "", "two"]);
}

#[test]
fn active_block_holds_back_everything_after_it() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.tool_start("a", "bash", "sleep 1", "");
    t.notice("", "after");
    assert!(t.is_streaming());
    assert!(
        t.take_settled(40, &ctx).is_empty(),
        "the pending tool blocks the notice"
    );
    t.tool_end("a", "done", false, 1.0);
    let text = text_of(&t.take_settled(40, &ctx));
    assert!(text.find("sleep 1").unwrap() < text.find("after").unwrap());
    assert!(!t.is_streaming());
}

#[test]
fn shared_prose_survives_insertions_and_commits_each_delta_once() {
    let mut env = Env::new();
    env.ui.show_thinking = true;
    let ctx = env.ctx();
    let root = tempfile::tempdir().unwrap();
    image::RgbaImage::new(1, 1)
        .save(root.path().join("preview.png"))
        .unwrap();
    let reference =
        crate::llm::images::attachment(root.path(), std::path::Path::new("preview.png")).unwrap();
    let prefix = "SHARED-PREFIX\n\nSHARED-MIDDLE";
    let answer = format!("{prefix} SHARED-SUFFIX");
    for kind in ["assistant", "reasoning"] {
        for insertion in ["notice", "user", "image"] {
            for max_rows in [1, 20] {
                let tree = crate::runtime::Tree::default();
                let body = tree.start(kind, kind, None);
                let mut t = Transcript::new();
                body.append(prefix);
                t.bind_prose(&body);
                let mut committed = t.live(80, &ctx, max_rows).0;
                if max_rows == 1 {
                    assert!(
                        !committed.is_empty(),
                        "{kind} should spill its stable prefix"
                    );
                }
                match insertion {
                    "notice" => t.notice("Help", "INSERTED-NOTICE"),
                    "user" => t.push_user("INSERTED-USER"),
                    "image" => t.image(crate::ui::images::Thumbnail::load(&reference).unwrap()),
                    _ => unreachable!(),
                }
                assert!(t.is_streaming(), "{kind} must survive {insertion}");
                assert!(t.take_settled(80, &ctx).is_empty());
                assert_eq!(
                    body.read(|n| n.result.phase),
                    crate::runtime::Phase::Running
                );
                body.append(" SHARED-SUFFIX");
                t.bind_prose(&body.clone());
                committed.extend(t.live(80, &ctx, max_rows).0);
                assert!(t.is_streaming());
                assert!(t.take_settled(80, &ctx).is_empty());
                body.settle(answer.clone(), None, crate::runtime::Phase::Completed);
                t.bind_prose(&body);
                assert!(!t.is_streaming());
                committed.extend(t.take_settled(80, &ctx));
                let text = text_of(&committed);
                for marker in ["SHARED-PREFIX", "SHARED-MIDDLE", "SHARED-SUFFIX"] {
                    assert_eq!(
                        text.matches(marker).count(),
                        1,
                        "{kind}, {insertion}, {max_rows} rows:\n{text}"
                    );
                }
                assert_eq!(plain(&committed), plain(&t.all_rows(80, &ctx)));
                assert!(t.take_settled(80, &ctx).is_empty());
            }
        }
    }
}

#[test]
fn repeated_shared_prose_binding_tracks_identity_and_settlement() {
    let env = Env::new();
    let ctx = env.ctx();
    let first_tree = crate::runtime::Tree::default();
    let second_tree = crate::runtime::Tree::default();
    let first = first_tree.start("assistant", "first", None);
    let second = second_tree.start("assistant", "second", None);
    // Separate trees can issue the same journal id, but these are distinct live bodies.
    assert_eq!(first.read(|n| n.id.clone()), second.read(|n| n.id.clone()));
    let mut t = Transcript::new();
    first.append("FIRST-BODY");
    for _ in 0..3 {
        t.bind_prose(&first.clone());
        assert!(t.is_streaming());
        assert!(t.take_settled(80, &ctx).is_empty());
    }
    second.append("SECOND-BODY");
    t.bind_prose(&second);
    first.settle("FIRST-BODY".into(), None, crate::runtime::Phase::Completed);
    t.bind_prose(&first.clone());
    let mut committed = t.take_settled(80, &ctx);
    assert!(t.is_streaming(), "the second body is still running");
    assert_eq!(plain(&committed), ["FIRST-BODY"]);
    t.bind_prose(&first);
    assert!(t.take_settled(80, &ctx).is_empty());
    second.append("-TAIL");
    t.bind_prose(&second.clone());
    second.settle(
        "SECOND-BODY-TAIL".into(),
        None,
        crate::runtime::Phase::Completed,
    );
    t.bind_prose(&second);
    assert!(
        !t.is_streaming(),
        "settlement must reach the existing binding"
    );
    committed.extend(t.take_settled(80, &ctx));
    t.bind_prose(&second.clone());
    assert!(t.take_settled(80, &ctx).is_empty());
    assert_eq!(plain(&committed), ["FIRST-BODY", "", "SECOND-BODY-TAIL"]);
    assert_eq!(plain(&committed), plain(&t.all_rows(80, &ctx)));
}

#[test]
fn assistant_streams_into_one_block_and_rerenders_as_it_grows() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.assistant_delta("Hello **wor");
    let (spilled, live) = t.live(40, &ctx, 20);
    assert!(spilled.is_empty());
    assert_eq!(plain(&live), ["Hello **wor", ""]);
    t.assistant_delta("ld** again");
    let (_, live) = t.live(40, &ctx, 20);
    assert_eq!(plain(&live), ["Hello world again", ""]);
    assert!(
        live[0]
            .spans
            .iter()
            .any(|s| s.content.contains("world") && s.style.add_modifier.contains(Modifier::BOLD))
    );
    assert!(t.take_settled(40, &ctx).is_empty(), "still streaming");
    t.finish_turn();
    assert_eq!(plain(&t.take_settled(40, &ctx)), ["Hello world again"]);
}

#[test]
fn a_new_kind_of_block_ends_the_streaming_one() {
    let mut env = Env::new();
    env.ui.show_thinking = true;
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.thinking_delta("pondering");
    t.assistant_delta("answer");
    let settled = text_of(&t.take_settled(40, &ctx));
    assert!(settled.contains("pondering") && !settled.contains("answer"));
    t.tool_start("1", "read", "a.txt", "");
    let mid = text_of(&t.take_settled(40, &ctx));
    assert!(
        mid.contains("answer") && !mid.contains("Read"),
        "the pending tool blocks only itself: {mid}"
    );
    t.tool_end("1", "x", false, 0.1);
    assert!(text_of(&t.take_settled(40, &ctx)).contains("Read"));
}

#[test]
fn finish_turn_marks_unfinished_tools_interrupted() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.tool_start("x", "bash", "sleep 100", "");
    t.finish_turn();
    let text = text_of(&t.take_settled(40, &ctx));
    assert!(text.contains("interrupted"), "{text}");
}

// ---- user / reasoning / stats / errors ----

#[test]
fn user_block_is_compact_and_has_a_prompt_marker() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.push_user("hello there, this is a fairly long message that wraps");
    let rows = t.take_settled(20, &ctx);
    let body: Vec<_> = rows.iter().collect();
    assert!(body.len() >= 4);
    for row in &body {
        assert!(text::line_width(row) <= 20);
    }
    assert!(plain(&rows)[0].starts_with("▎ › hello"));
    assert_eq!(body[0].spans[0].style.fg, env.theme.fg(Tok::Accent).fg);
}

#[test]
fn user_text_is_sanitized() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.push_user("hi \x1b]0;pwned\x07 there\x1b[2J");
    let text = text_of(&t.take_settled(30, &ctx));
    assert!(text.contains("hi  there") && !text.contains("pwned") && !text.contains('\x1b'));
}

#[test]
fn reasoning_is_muted_italic_prose_or_collapses() {
    let mut env = Env::new();
    env.ui.show_thinking = true;
    let mut t = Transcript::new();
    t.thinking_delta("first thought\nsecond thought\n\nthird");
    t.finish_turn();
    let rows = t.all_rows(40, &env.ctx());
    assert!(plain(&rows).contains(&"▎ first thought".to_owned()));
    assert_eq!(rows[0].spans[1].style, env.theme.fg(Tok::ThinkingText));
    env.ui.show_thinking = false;
    let rows = t.all_rows(40, &env.ctx());
    assert_eq!(plain(&rows), ["› reasoning (3 lines)"]);
}

#[test]
fn stats_row_is_dim_and_gated_by_setting() {
    let mut env = Env::new();
    env.ui.show_turn_stats = true;
    let mut t = Transcript::new();
    t.turn_stats(TurnStats {
        clock: "2026-10-01 10:54:32".into(),
        elapsed_secs: 26.84,
        prompt_tokens: Some(11_000),
        output_tokens: Some(59),
        tokens_per_second: Some(2.2),
        outcome: None,
    });
    let rows = t.all_rows(80, &env.ctx());
    let line = &plain(&rows)[0];
    assert!(
        line.contains("26.8s") && line.contains("11K in") && line.contains("59 out"),
        "{line}"
    );
    assert!(
        !line.contains("2026-10-01"),
        "the compact footer omits redundant clock/rate accounting"
    );
    assert!(
        rows[0]
            .spans
            .iter()
            .any(|s| s.content.contains("26.8s") && s.style.fg == env.theme.fg(Tok::Muted).fg)
    );
    env.ui.show_turn_stats = false;
    assert!(t.all_rows(80, &env.ctx()).is_empty());
    let mut t2 = Transcript::new();
    t2.turn_stats(TurnStats {
        clock: String::new(),
        elapsed_secs: 1.0,
        prompt_tokens: None,
        output_tokens: None,
        tokens_per_second: None,
        outcome: None,
    });
    env.ui.show_turn_stats = true;
    let text = plain(&t2.all_rows(80, &env.ctx()))[0].clone();
    assert!(text.contains("1.0s") && !text.contains(" in") && !text.contains(" out"));
}

#[test]
fn errors_and_notices_wrap_with_hanging_indent() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.error("connection refused while talking to the model server");
    t.notice("Heads up", "something noteworthy happened here");
    let rows = t.all_rows(24, &env.ctx());
    assert_fits(&rows, 24);
    let text = plain(&rows);
    assert!(text[0].starts_with("✗ connection"));
    assert!(text[1].starts_with("  "));
    assert_eq!(rows[0].spans[0].style.fg, env.theme.fg(Tok::Error).fg);
    assert!(text.iter().any(|l| l.to_uppercase().contains("HEADS UP")));
}

// ---- tools ----

#[test]
fn bash_tool_has_a_compact_header_and_output_without_bookkeeping() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.tool_start("1", "bash", "ls -la", "");
    t.tool_end(
        "1",
        "total 0\nfile.txt\n[exit code: 0]\n[complete output: artifact://abc]",
        false,
        0.06,
    );
    let rows = t.take_settled(40, &ctx);
    let text = plain(&rows);
    assert!(text[0].contains("✓ Run") && text[0].contains("ls -la") && text[0].ends_with("0.06s"));
    assert!(text[1].contains("total 0") && text[2].contains("file.txt"));
    assert!(!text_of(&rows).contains("exit code") && !text_of(&rows).contains("artifact://"));
    // Shell bookkeeping stays out of the body; the run time sits in the header row.
    // The block ends with its separating blank row.
    assert_eq!(text.len(), 3, "{text:?}");
    assert_eq!(text::line_width(&rows[0]), 40);
    assert_eq!(
        rows[0].spans[4].style.fg,
        env.theme.fg(Tok::SectionSandbox).fg
    );
}

#[test]
fn failed_tool_output_wraps_at_word_boundaries() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("1", "bash", "probe", "");
    t.tool_end(
        "1",
        "connection refused while talking to model server",
        true,
        0.01,
    );
    let rows = t.all_rows(24, &env.ctx());
    assert_fits(&rows, 24);
    let output = plain(&rows);
    assert!(
        output
            .iter()
            .any(|line| line.contains("connection refused")),
        "{output:?}"
    );
    assert!(
        output.iter().any(|line| line.contains("while talking to")),
        "{output:?}"
    );
}

#[test]
fn tool_gutter_follows_family_and_failure() {
    let env = Env::new();
    let render = |end: Option<bool>| {
        let mut t = Transcript::new();
        t.tool_start("1", "bash", "x", "");
        if let Some(err) = end {
            t.tool_end("1", "out", err, 1.0);
        }
        t.all_rows(40, &env.ctx())
    };
    let pending = render(None);
    let ok = render(Some(false));
    let err = render(Some(true));
    // Span 0 is the work-group rail; the family gutter follows the indent and the status glyph.
    let gutter = |rows: &Rows| rows[0].spans[4].style.fg;
    assert_eq!(gutter(&pending), env.theme.fg(Tok::SectionSandbox).fg);
    assert_eq!(gutter(&ok), env.theme.fg(Tok::SectionSandbox).fg);
    assert_eq!(gutter(&err), env.theme.fg(Tok::Error).fg);
    assert!(plain(&err)[0].contains('✗'));
    let body = err
        .iter()
        .flat_map(|r| &r.spans)
        .find(|s| s.content.contains("out"))
        .unwrap();
    assert_eq!(body.style.fg, env.theme.fg(Tok::Error).fg);
}

#[test]
fn pending_spinner_animates_with_tick() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("1", "bash", "build", "");
    t.tool_start("2", "read", "a.rs", "");
    let frame = |tick: u64| {
        let ctx = TranscriptCtx { tick, ..env.ctx() };
        plain(&t.all_rows(40, &ctx))
    };
    let (a, b) = (frame(0), frame(1));
    assert_ne!(a, b);
    assert!(a[1].contains(env.theme.spinner(0)) && b[1].contains(env.theme.spinner(1)));
    let compact = a.iter().find(|l| l.contains("Read")).unwrap();
    let after_rail = |l: &str| l.trim_start_matches('│').trim_start().to_owned();
    assert!(
        after_rail(compact).starts_with(env.theme.spinner(0)),
        "{compact}"
    );
    assert!(
        b.iter()
            .any(|l| after_rail(l).starts_with(env.theme.spinner(1)) && l.contains("Read"))
    );
}

#[test]
fn a_run_of_tool_calls_hangs_off_one_rail_and_prose_does_not() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.push_user("fix it");
    t.tool_start("1", "read", "a.rs", "");
    t.tool_end("1", "1: fn a() {}", false, 0.1);
    t.tool_start("2", "bash", "cargo test", "");
    t.tool_end("2", "error: one failed\nmore\n", true, 0.2);
    t.assistant_delta("Done.");
    t.finish_turn();
    for width in [40u16, 80] {
        let rows = t.all_rows(width, &env.ctx());
        assert_fits(&rows, width);
        let text = plain(&rows);
        let first = text.iter().position(|r| r.contains("Read")).unwrap();
        let last = text.iter().rposition(|r| r.starts_with('│')).unwrap();
        assert!(
            text[first..=last].iter().all(|r| r.starts_with('│')),
            "every row of the group, bodies included, is on the rail: {text:?}"
        );
        assert!(
            text.iter()
                .filter(|r| r.contains("fix it") || r.contains("Done."))
                .all(|r| !r.starts_with('│')),
            "{text:?}"
        );
    }
}

#[test]
fn user_messages_are_tinted_across_the_width() {
    let mut env = Env::new();
    env.theme = Theme::builtin("dial-dark").unwrap();
    let mut t = Transcript::new();
    t.push_user("a question that wraps onto a second line here");
    let tint = env.theme.bg(Tok::UserBg).bg;
    assert!(tint.is_some_and(|c| c != ratatui::style::Color::Reset));
    let rows = t.all_rows(30, &env.ctx());
    let user: Vec<_> = rows.iter().filter(|r| text::line_width(r) > 0).collect();
    assert!(user.len() >= 2, "{:?}", plain(&rows));
    for row in user {
        assert_eq!(text::line_width(row), 30, "the tint spans the width");
        assert!(row.spans.iter().all(|s| s.style.bg == tint), "{row:?}");
    }
}

#[test]
fn long_output_is_capped_with_a_hint_until_expanded() {
    let env = Env::new();
    let mut t = Transcript::new();
    let output = (1..=20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    t.tool_start("1", "bash", "seq 20", "");
    t.tool_end("1", &output, false, 2.0);
    let capped = text_of(&t.all_rows(50, &env.ctx()));
    assert!(
        capped.contains("line 8") && !capped.contains("line 9"),
        "{capped}"
    );
    assert!(
        capped.contains("… 12 more lines (Ctrl-O expands)"),
        "{capped}"
    );
    let expanded = text_of(&t.all_rows(
        50,
        &TranscriptCtx {
            tools_expanded: true,
            ..env.ctx()
        },
    ));
    assert!(expanded.contains("line 20") && !expanded.contains("more lines"));
}

#[test]
fn shared_result_revisions_update_cached_rows_and_output_settings_invalidate_them() {
    let mut env = Env::new();
    let tree = crate::runtime::Tree::default();
    let body = tree.start("tool", "bash", None);
    let mut t = Transcript::new();
    t.tool_start("shared", "bash", "check", "");
    t.bind_tool("shared", body.clone());
    body.append("one\ntwo\nthree\nfour\n");
    env.ui.tool_output_lines = 2;
    let first = text_of(&t.all_rows(80, &env.ctx()));
    assert!(first.contains("one"));
    assert!(!first.contains("four"));
    env.ui.tool_output_lines = 10;
    assert!(text_of(&t.all_rows(80, &env.ctx())).contains("four"));
    body.settle(
        "typed result".into(),
        Some(serde_json::json!({"ok":true})),
        crate::runtime::Phase::Completed,
    );
    t.tool_end("shared", "legacy projection", false, 0.2);
    let done = text_of(&t.all_rows(80, &env.ctx()));
    assert!(done.contains("typed result"));
    assert!(!done.contains("legacy projection"));
    assert!(!t.take_settled(80, &env.ctx()).is_empty());
    assert!(t.take_settled(80, &env.ctx()).is_empty());
}

#[test]
fn output_lines_setting_controls_the_cap() {
    let mut env = Env::new();
    env.ui.tool_output_lines = 2;
    let mut t = Transcript::new();
    t.tool_start("1", "bash", "x", "");
    t.tool_end("1", "a\nb\nc", false, 1.0);
    let text = text_of(&t.all_rows(40, &env.ctx()));
    assert!(
        text.contains("▎ b") && !text.contains("▎ c") && text.contains("1 more line (Ctrl-O"),
        "{text}"
    );
}

#[test]
fn edit_output_colors_diff_lines_and_hashline_previews() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("1", "edit", "src/main.rs", "");
    t.tool_end(
        "1",
        "[src/main.rs#AB12]\nupdated (+1 -1 lines)\n@@ -1 +1 @@\n-old line\n+new line\n3:context",
        false,
        0.5,
    );
    let rows = t.all_rows(60, &env.ctx());
    assert!(plain(&rows)[1].contains("src/main.rs") && !plain(&rows)[1].contains('$'));
    let style = |needle: &str| {
        rows.iter()
            .flat_map(|r| &r.spans)
            .find(|s| s.content.contains(needle))
            .unwrap()
            .style
            .fg
    };
    assert_eq!(style("+new line"), env.theme.fg(Tok::DiffAdded).fg);
    assert_eq!(style("-old line"), env.theme.fg(Tok::DiffRemoved).fg);
    assert_eq!(style("#AB12"), env.theme.fg(Tok::Accent).fg);
    assert_eq!(style("3:"), env.theme.fg(Tok::Dim).fg);
    assert_eq!(style("context"), env.theme.fg(Tok::ToolOutput).fg);
    assert!(
        plain(&rows)
            .iter()
            .any(|r| r.contains("✓ Edit") && r.ends_with("0.50s"))
    );
}

#[test]
fn a_failing_exit_code_is_flagged_in_the_card_header() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("1", "bash", "cargo test", "");
    t.tool_end("1", "1 failed\n[exit code: 101]", true, 2.0);
    let rows = t.all_rows(50, &env.ctx());
    let header = &rows[0];
    let exit = header
        .spans
        .iter()
        .find(|s| s.content.contains("exit 101"))
        .expect("exit badge");
    assert_eq!(exit.style.fg, env.theme.fg(Tok::Error).fg);
    assert!(!text_of(&rows[..rows.len() - 1].to_vec()).contains("exit code"));
    assert_eq!(text::line_width(header), 50);
}

#[test]
fn edit_cards_show_the_numbered_diff_not_the_model_facing_preview() {
    let env = Env::new();
    let mut t = Transcript::new();
    let patch = "[src/a.rs#2F12]\nPUT 2.=2:\n+    new();\n";
    t.tool_start("1", "edit", patch, patch);
    let output = "[src/a.rs#EF7D]\nupdated (+1 -1 lines)\nremoved lines not present in your new text (old numbering):\n  2:     old();\n1:fn a() {\n2:    new();\n3:}\n\nchange preview (+1 -1 lines)\n@@ -1,3 +1,3 @@\n fn a() {\n-    old();\n+    new();\n }\n";
    t.tool_end("1", output, false, 0.1);
    let rows = t.all_rows(60, &env.ctx());
    let text = plain(&rows);
    assert!(
        text[1].contains("src/a.rs") && !text[1].contains("PUT"),
        "titled by the file it touches: {text:?}"
    );
    let body: Vec<&str> = text
        .iter()
        .map(|r| r.trim_matches(|c: char| c == '▎' || c == '│' || c == ' '))
        .collect();
    for expected in [
        "src/a.rs",
        "updated (+1 -1 lines)",
        "1  fn a() {",
        "2 -    old();",
        "2 +    new();",
        "3  }",
    ] {
        assert!(
            body.contains(&expected),
            "{expected:?} missing from {body:?}"
        );
    }
    assert!(
        !text_of(&rows).contains("removed lines not present")
            && !text_of(&rows).contains("1:fn a()"),
        "model-facing parts are hidden"
    );
    let style = |needle: &str| {
        rows.iter()
            .flat_map(|r| &r.spans)
            .find(|s| s.content.contains(needle))
            .unwrap()
            .style
            .fg
    };
    assert_eq!(style("+    new();"), env.theme.fg(Tok::DiffAdded).fg);
    assert_eq!(style("-    old();"), env.theme.fg(Tok::DiffRemoved).fg);
    assert!(!text_of(&rows).contains("change preview"));
}

#[test]
fn todo_calls_render_the_list_with_their_changes() {
    use crate::agent::workflow::todo::TodoList;
    let env = Env::new();
    let mut before = TodoList::new();
    before.call(
        &serde_json::json!({"op": "set", "tasks": [{"content": "read"}, {"content": "fix"}]}),
    );
    let mut after = before.clone();
    after.call(&serde_json::json!({"op": "done"}));
    let mut t = Transcript::new();
    t.tool_start("1", "todo", "done", "");
    t.tool_todo("1", crate::ui::tasks::changes(&before, &after));
    t.tool_end(
        "1",
        "Completed 1. read. Started 2. fix.\n1/2 done\n[x] 1. read\n[~] 2. fix",
        false,
        0.0,
    );
    assert_eq!(
        plain(&t.all_rows(60, &env.ctx()))[..3],
        ["│ ✓ Plan    1/2 done", "│  ✓ read", "│  ● fix"]
    );
}

#[test]
fn write_error_output_is_not_colored_as_a_diff() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("1", "write", "a.txt", "");
    t.tool_end("1", "-bad: refused", true, 0.1);
    let rows = t.all_rows(40, &env.ctx());
    let span = rows
        .iter()
        .flat_map(|r| &r.spans)
        .find(|s| s.content.contains("-bad"))
        .unwrap();
    assert_eq!(span.style.fg, env.theme.fg(Tok::Error).fg);
}

#[test]
fn title_falls_back_to_the_call_arguments() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("1", "bash", "", r#"{"command":"echo hi"}"#);
    t.tool_start("2", "write", "", r#"{"path":"out/a.txt"}"#);
    let text = text_of(&t.all_rows(50, &env.ctx()));
    assert!(
        text.contains("Run") && text.contains("echo hi") && text.contains("out/a.txt"),
        "{text}"
    );
}

#[test]
fn long_commands_are_compact_and_expand_to_the_full_target() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("1", "bash", &"word ".repeat(80), "");
    let rows = t.all_rows(30, &env.ctx());
    assert_fits(&rows, 30);
    assert_eq!(rows.len(), 1, "{:?}", plain(&rows));
    let expanded = t.all_rows(
        30,
        &TranscriptCtx {
            tools_expanded: true,
            ..env.ctx()
        },
    );
    assert_fits(&expanded, 30);
    assert!(expanded.len() > rows.len());
    assert_eq!(
        text_of(&expanded).matches("word").count(),
        80 + text_of(&rows).matches("word").count()
    );
}

#[test]
fn simple_tools_are_compact_rows() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("1", "read", "notes.txt", "");
    t.tool_end("1", "\nfirst real line\nsecond", false, 0.2);
    let rows = t.all_rows(60, &env.ctx());
    assert_eq!(rows.len(), 1);
    assert!(
        plain(&rows)[0].contains("✓ Read")
            && plain(&rows)[0].contains("notes.txt")
            && plain(&rows)[0].contains("3 lines")
    );
    assert_eq!(
        rows[0].spans[4].style.fg,
        env.theme.fg(Tok::SectionTools).fg
    );
    assert_eq!(rows[0].spans[2].style.fg, env.theme.fg(Tok::Success).fg);
    assert_eq!(rows[0].spans[0].style.fg, env.theme.fg(Tok::BorderMuted).fg);
}

#[test]
fn failed_simple_tool_shows_its_error_underneath() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("1", "grep", "foo", "");
    t.tool_end("1", "regex parse error\nunclosed group", true, 0.2);
    let text = plain(&t.all_rows(40, &env.ctx()));
    assert!(text[0].contains("✗ Search") && text[0].contains("foo"));
    assert!(text[1].contains("regex parse error"));
    assert!(text[2].contains("unclosed group"));
}

#[test]
fn show_tools_false_hides_everything_but_errors() {
    let mut env = Env::new();
    env.ui.show_tools = false;
    let mut t = Transcript::new();
    t.tool_start("1", "bash", "ok-cmd", "");
    t.tool_end("1", "fine", false, 0.1);
    t.tool_start("2", "read", "hidden.txt", "");
    t.tool_end("2", "x", false, 0.1);
    t.tool_start("3", "bash", "bad-cmd", "");
    t.tool_end("3", "boom", true, 0.1);
    t.tool_start("4", "read", "missing.txt", "");
    t.tool_end("4", "no such file", true, 0.1);
    t.tool_start("5", "bash", "still-running", "");
    let text = text_of(&t.all_rows(50, &env.ctx()));
    for hidden in ["ok-cmd", "hidden.txt", "still-running"] {
        assert!(!text.contains(hidden), "{hidden} should be hidden\n{text}");
    }
    for shown in ["bad-cmd", "boom", "missing.txt", "no such file"] {
        assert!(text.contains(shown), "{shown} should be visible\n{text}");
    }
}

#[test]
fn hidden_tools_leave_no_stray_blank_rows() {
    let mut env = Env::new();
    env.ui.show_tools = false;
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.notice("", "a");
    t.tool_start("1", "read", "x", "");
    t.tool_end("1", "y", false, 0.1);
    t.notice("", "b");
    assert_eq!(plain(&t.take_settled(20, &ctx)), ["a", "", "b"]);
}

#[test]
fn tool_end_for_unknown_id_is_not_lost() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_end("zzz", "orphan output", false, 0.3);
    assert!(text_of(&t.all_rows(40, &env.ctx())).contains("orphan output"));
}

#[test]
fn parallel_tools_pair_results_by_id() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.tool_start("a", "bash", "first", "");
    t.tool_start("b", "bash", "second", "");
    t.tool_end("b", "B-out", false, 0.1);
    t.tool_end("a", "A-out", false, 0.1);
    let text = text_of(&t.all_rows(40, &env.ctx()));
    assert!(text.find("first").unwrap() < text.find("A-out").unwrap());
    assert!(text.find("A-out").unwrap() < text.find("second").unwrap());
    assert!(text.find("second").unwrap() < text.find("B-out").unwrap());
}

#[test]
fn narrow_widths_never_overflow() {
    let env = Env::new();
    let mut t = Transcript::new();
    t.push_user("日本語 hello world");
    t.thinking_delta("hmm 🎉 thinking");
    t.assistant_delta("# Head\n\n| a | b |\n|--|--|\n| 1 | 2 |\n\n```rust\nlet x = 1;\n```\n");
    t.tool_start("1", "bash", "echo 日本語", "");
    t.tool_end("1", "日本語日本語\nline", false, 1.0);
    t.tool_start("2", "read", "path/to/some/file.rs", "");
    t.tool_end("2", "x", true, 1.0);
    t.notice("title", "body text");
    t.error("an error");
    t.turn_stats(TurnStats {
        clock: "now".into(),
        elapsed_secs: 1.0,
        prompt_tokens: Some(1),
        output_tokens: Some(2),
        tokens_per_second: Some(3.0),
        outcome: None,
    });
    t.finish_turn();
    for w in 1..=80u16 {
        assert_fits(&t.all_rows(w, &env.ctx()), w);
    }
    let ascii = Theme::dark().with_glyphs(Glyphs::Ascii);
    let ctx = TranscriptCtx {
        theme: &ascii,
        ..env.ctx()
    };
    for row in plain(&t.all_rows(60, &ctx)) {
        assert!(
            row.chars()
                .all(|c| c.is_ascii() || text::width(&c.to_string()) == 2),
            "{row:?}"
        );
    }
}

// ---- spill ----

fn long_markdown(paragraphs: usize) -> Vec<String> {
    (1..=paragraphs)
        .map(|i| format!("Paragraph <{i}> has some words in it that wrap around.\n\n"))
        .collect()
}

#[test]
fn nothing_spills_while_the_tail_fits() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.assistant_delta("one\n\ntwo\n\nthree");
    let (spilled, live) = t.live(40, &ctx, 20);
    assert!(spilled.is_empty());
    assert_eq!(plain(&live), ["one", "", "two", "", "three", ""]);
}

#[test]
fn stable_head_spills_once_and_the_remainder_completes_it() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    let mut committed: Rows = Vec::new();
    for chunk in long_markdown(12) {
        t.assistant_delta(&chunk);
        committed.extend(t.take_settled(30, &ctx));
        let (spilled, live) = t.live(30, &ctx, 6);
        assert!(live.len() <= 6);
        committed.extend(spilled);
    }
    assert!(committed.len() > 6, "something must have spilled");
    // spilled paragraphs never reappear in the live tail
    let (_, live) = t.live(30, &ctx, 6);
    let committed_text = text_of(&committed);
    for i in 1..=12 {
        let token = format!("<{i}>");
        assert!(
            !(text_of(&live).contains(&token) && committed_text.contains(&token)),
            "{token} shown twice"
        );
    }
    t.finish_turn();
    committed.extend(t.take_settled(30, &ctx));
    assert_eq!(
        plain(&committed),
        plain(&t.all_rows(30, &ctx)),
        "spilled + settled == everything, once"
    );
}

#[test]
fn spill_never_cuts_an_open_code_fence() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.assistant_delta("intro\n\nnext para\n\n```rust\nlet a = 1;\n\nlet b = 2;\n");
    let mut committed: Rows = Vec::new();
    let (spilled, live) = t.live(30, &ctx, 5);
    committed.extend(spilled);
    let shown = text_of(&committed);
    assert!(
        !shown.contains("let a"),
        "code inside an open fence is not stable: {shown}"
    );
    assert!(text_of(&live).contains("let b = 2;"));
    t.assistant_delta("```\n\nafter\n");
    t.finish_turn();
    committed.extend(t.take_settled(30, &ctx));
    assert_eq!(plain(&committed), plain(&t.all_rows(30, &ctx)));
}

#[test]
fn list_followed_by_blank_is_not_spilled_until_the_next_block_starts() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.assistant_delta("- a\n- b\n\n");
    assert!(
        t.live(20, &ctx, 1).0.is_empty(),
        "the next item could still make the list loose"
    );
    t.assistant_delta("- c\n\nplain text\n");
    let mut committed = t.live(20, &ctx, 1).0;
    t.finish_turn();
    committed.extend(t.take_settled(20, &ctx));
    assert_eq!(plain(&committed), plain(&t.all_rows(20, &ctx)));
}

#[test]
fn spill_is_exactly_once_across_many_frames() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    let mut committed: Rows = Vec::new();
    let doc: String = long_markdown(30).concat() + "tail";
    for piece in doc.as_bytes().chunks(7) {
        t.assistant_delta(std::str::from_utf8(piece).unwrap());
        let (spilled, live) = t.live(24, &ctx, 5);
        committed.extend(spilled);
        assert!(live.len() <= 5);
        // calling again with no new input must not spill the same rows again
        assert!(t.live(24, &ctx, 5).0.is_empty());
    }
    t.finish_turn();
    committed.extend(t.take_settled(24, &ctx));
    assert_eq!(plain(&committed), plain(&t.all_rows(24, &ctx)));
}

#[test]
fn width_change_after_partial_spill_keeps_what_was_spilled() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    let doc = long_markdown(10).concat();
    t.assistant_delta(&doc);
    t.assistant_delta("the <unfinished> last paragraph with many words to wrap");
    let (spilled, _) = t.live(40, &ctx, 6);
    assert!(!spilled.is_empty());
    // narrower terminal: nothing already spilled is repeated, nothing is dropped
    let (more, live) = t.live(17, &ctx, 6);
    assert!(live.len() <= 6);
    let mut everything = spilled;
    everything.extend(more);
    t.finish_turn();
    everything.extend(t.take_settled(17, &ctx));
    let all = text_of(&everything);
    for i in 1..=10 {
        assert_eq!(
            all.matches(&format!("<{i}>")).count(),
            1,
            "paragraph {i} must appear exactly once:\n{all}"
        );
    }
    assert_eq!(all.matches("<unfinished>").count(), 1);
}

#[test]
fn partially_spilled_block_is_not_duplicated_by_committed_rows_replay() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.push_user("question");
    let mut committed = t.take_settled(30, &ctx);
    t.assistant_delta(&long_markdown(10).concat());
    t.assistant_delta("still typing");
    committed.extend(t.live(30, &ctx, 5).0);
    let replay = t.committed_rows(30, &ctx);
    assert_eq!(plain(&replay), plain(&committed));
    assert!(!text_of(&replay).contains("still typing"));
}

#[test]
fn pending_tool_before_stream_prevents_spilling_out_of_order() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    t.tool_start("1", "bash", "build", "");
    t.assistant_delta(&long_markdown(10).concat());
    let (spilled, live) = t.live(30, &ctx, 6);
    assert!(
        spilled.is_empty(),
        "the pending tool must be committed first"
    );
    assert!(live.len() <= 6);
}

#[test]
fn live_with_zero_rows_or_nothing_active_is_empty() {
    let env = Env::new();
    let ctx = env.ctx();
    let mut t = Transcript::new();
    assert_eq!(t.live(30, &ctx, 10), (vec![], vec![]));
    t.assistant_delta("text");
    let (spilled, live) = t.live(30, &ctx, 0);
    assert!(spilled.is_empty() && live.is_empty());
}

#[test]
fn long_reasoning_spills_like_prose() {
    let mut env = Env::new();
    env.ui.show_thinking = true;
    let ctx = env.ctx();
    let mut t = Transcript::new();
    let mut committed = Rows::new();
    for i in 1..=15 {
        t.thinking_delta(&format!("thought number {i} goes on for a while\n\n"));
        committed.extend(t.live(26, &ctx, 4).0);
    }
    assert!(!committed.is_empty());
    t.finish_turn();
    committed.extend(t.take_settled(26, &ctx));
    assert_eq!(plain(&committed), plain(&t.all_rows(26, &ctx)));
}
