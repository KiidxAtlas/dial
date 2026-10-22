use super::*;
use crate::ui::status::Phase;

struct Env {
    theme: Theme,
    keys: Keymap,
    ui: UiConfig,
    status: StatusData,
}
impl Env {
    fn new() -> Env {
        // Most tests read the framed shape, whose rows carry visible borders; the band has its own.
        let ui = UiConfig {
            session_accent: false,
            composer_shape: ComposerShape::Box,
            ..UiConfig::default()
        };
        let status = StatusData {
            provider: "mlx".into(),
            model: "qwen3-9b".into(),
            project_path: "/work/projects/dial".into(),
            ..StatusData::default()
        };
        Env {
            theme: Theme::dark(),
            keys: Keymap::defaults(),
            ui,
            status,
        }
    }
    fn ctx(&self) -> ComposerCtx<'_> {
        ComposerCtx {
            theme: &self.theme,
            keys: &self.keys,
            ui: &self.ui,
            status: &self.status,
        }
    }
}

#[test]
fn composer_and_history_search_honor_rebound_editor_keys() {
    let mut env = Env::new();
    env.keys
        .rebind(Action::EditorDeleteBackward, &["F6"])
        .unwrap();
    env.keys.rebind(Action::EditorWordLeft, &["F7"]).unwrap();
    let mut c = Composer::new(Vec::new(), 100);
    c.set_draft("one two");
    c.handle_key(
        KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
        &env.ctx(),
    );
    assert_eq!(
        c.editor.cursor,
        "one two".len(),
        "old word binding is disabled"
    );
    c.handle_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE), &env.ctx());
    assert_eq!(c.editor.cursor, 4);
    c.handle_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE), &env.ctx());
    assert_eq!(c.draft(), "onetwo");
    c.search = Some(Search {
        query: Editor::default(),
        entries: vec![],
        selected: 0,
        scroll: 0,
    });
    c.handle_paste("cafe\u{301}");
    c.handle_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE), &env.ctx());
    assert_eq!(c.search.as_ref().unwrap().query.text, "caf");
}

#[test]
fn bounded_queue_keeps_overflow_draft_and_manages_only_selected_item() {
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_busy(true);
    for index in 0..32 {
        composer.set_draft(&format!("message {index}"));
        composer.submit_queued(index % 2 == 0);
    }
    composer.set_draft("kept overflow draft");
    composer.submit_queued(false);
    assert_eq!(composer.queued_messages().len(), 32);
    assert_eq!(composer.draft(), "kept overflow draft");
    assert!(composer.queue_notice.is_some());
    assert!(!composer.edit_queued(1));
    assert!(composer.remove_queued(1));
    assert_eq!(composer.queued_messages()[1].0, "message 2");
    composer.set_draft("");
    assert!(composer.edit_queued(1));
    assert_eq!(composer.draft(), "message 2");
    assert!(
        composer
            .editing_queue
            .as_ref()
            .is_some_and(|edit| edit.follow_up)
    );
}

#[test]
fn cancelling_a_queued_edit_restores_the_original_item_in_place() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    for (text, follow_up) in [("first", false), ("second", true), ("third", false)] {
        c.set_draft(text);
        c.submit_queued(follow_up);
    }
    let original = c.queued_messages();
    // Edit the middle item, change it, then cancel: content, order and routing are unchanged.
    assert!(c.edit_queued(1));
    c.set_draft("");
    assert!(c.edit_queued(1), "replacing the draft restored the item");
    typed(&mut c, &env, " changed");
    assert_eq!(c.queued_messages().len(), 2);
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::None);
    assert_eq!(c.queued_messages(), original);
    assert_eq!(c.draft(), "");
    // Up edits the last item; cancelling puts it back last.
    assert_eq!(press(&mut c, &env, KeyCode::Up), ComposerEvent::None);
    assert_eq!(c.draft(), "third");
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::None);
    assert_eq!(c.queued_messages(), original);
    // Saving an edit replaces the item at its original position and keeps its routing.
    assert!(c.edit_queued(1));
    c.editor.set("second edited");
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert_eq!(
        c.queued_messages(),
        vec![
            ("first".into(), false),
            ("second edited".into(), true),
            ("third".into(), false)
        ]
    );
}

#[test]
fn follow_up_key_and_hint_follow_remapping() {
    let mut env = Env::new();
    env.keys.rebind(Action::FollowUp, &["Ctrl+Alt+F"]).unwrap();
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_busy(true);
    composer.set_draft("next task");
    assert!(
        composer
            .hint(&env.ctx())
            .join(" ")
            .contains(&env.keys.label(Action::FollowUp))
    );
    key(
        &mut composer,
        &env,
        KeyCode::Char('f'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    assert_eq!(composer.queued_messages(), vec![("next task".into(), true)]);
}

#[test]
fn file_refresh_keeps_selection_identity_and_draft() {
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_files(vec!["a.rs".into(), "b.rs".into()]);
    composer.set_draft("@");
    composer.step_menu(1, true, 5);
    assert_eq!(composer.menu_selection().unwrap().0, "b.rs");
    composer.set_files(vec!["a.rs".into(), "c.rs".into(), "b.rs".into()]);
    assert_eq!(composer.menu_selection().unwrap().0, "b.rs");
    assert_eq!(composer.draft(), "@");
}

fn event(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

#[test]
fn shared_workers_and_session_rail_render_with_an_editable_composer() {
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
    let mut env = Env::new();
    env.status.phase = Phase::Working;
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_draft("Make the header spacing tighter");
    use crate::ui::subagent_model::{AgentEntry, AgentStatus, HubSnapshot};
    let board = super::super::worker_pins::WorkerBoard::from_snapshot(HubSnapshot {
        agents: vec![
            AgentEntry {
                id: "w1".into(),
                name: "Inspect visuals".into(),
                status: AgentStatus::Running,
                current: Some("generating".into()),
                steps: 2,
                ..Default::default()
            },
            AgentEntry {
                id: "w2".into(),
                name: "Review styles".into(),
                status: AgentStatus::Done,
                steps: 3,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let files = vec![
        super::super::rail::FileDiff {
            path: "app/components/Header.vue".into(),
            added: Some(12),
            removed: Some(4),
            ..Default::default()
        },
        super::super::rail::FileDiff {
            path: "app/assets/app.css".into(),
            added: Some(6),
            removed: Some(2),
            ..Default::default()
        },
        super::super::rail::FileDiff {
            path: "screenshots/header.png".into(),
            untracked: true,
            ..Default::default()
        },
    ];
    let mut rows = super::super::rail::session(
        "abcdef123",
        "Polish the site header",
        &files,
        110,
        4,
        &env.theme,
    );
    rows.extend(super::super::worker_pins::rows(
        &board,
        110,
        4,
        crate::llm::model_config::PinnedWorkers::Collapsed,
        true,
        0,
        &env.theme,
    ));
    rows.extend(composer.view(110, &env.ctx()).rows);
    let height = rows.len() as u16;
    let mut terminal = Terminal::new(TestBackend::new(110, height)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(rows.clone()), frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    for expected in [
        "+12",
        "3 changed files",
        "step 2",
        "generating",
        "Make the header spacing tighter",
    ] {
        assert!(text.contains(expected), "missing {expected}");
    }
    if let Ok(path) = std::env::var("DIAL_TEST_EXECUTION_CAPTURE") {
        let cells = buffer.content.iter().map(|cell|serde_json::json!({"symbol":cell.symbol(),"fg":format!("{:?}",cell.fg),"bg":format!("{:?}",cell.bg)})).collect::<Vec<_>>();
        std::fs::write(
            path,
            serde_json::json!({"width":110,"height":height,"cells":cells}).to_string(),
        )
        .unwrap();
    }
}

#[test]
fn local_model_progress_preserves_the_draft_and_renders_all_stages() {
    use crate::llm::{InferencePhase, ModelProgress};
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
    let mut env = Env::new();
    env.status.phase = Phase::Working;
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_busy(true);
    env.status.elapsed_secs = 24.0;
    env.status.tick = 6;
    composer.set_draft("Keep this draft while the model runs");
    let mut terminal = Terminal::new(TestBackend::new(110, 15)).unwrap();
    let mut stages = Vec::new();
    for phase in [
        InferencePhase::Loading,
        InferencePhase::Processing,
        InferencePhase::Generating,
    ] {
        let mut progress = ModelProgress::new(phase);
        if phase == InferencePhase::Processing {
            progress.processed_tokens = Some(2048);
            progress.prompt_tokens = Some(6000);
        }
        if phase == InferencePhase::Generating {
            progress.output_tokens = Some(128);
            progress.tokens_per_second = Some(31.2);
        }
        env.status.model_progress = Some(progress);
        let view = composer.view(110, &env.ctx());
        let text = view
            .rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Keep this draft"));
        assert_eq!(view.cursor.0, 2);
        assert!(
            view.rows[0]
                .spans
                .iter()
                .any(|s| s.content.contains("turn 24s"))
        );
        assert!(
            !view.rows[1]
                .spans
                .iter()
                .any(|s| { s.content.contains("processing") || s.content.contains("generating") })
        );
        stages.push(view.rows);
    }
    terminal
        .draw(|frame| {
            for (index, rows) in stages.iter().enumerate() {
                frame.render_widget(
                    Paragraph::new(rows.clone()),
                    ratatui::layout::Rect::new(0, index as u16 * 5, 110, 5),
                );
            }
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    if let Ok(path) = std::env::var("DIAL_TEST_PROGRESS_CAPTURE") {
        let cells=buffer.content.iter().map(|cell|serde_json::json!({"symbol":cell.symbol(),"fg":format!("{:?}",cell.fg),"bg":format!("{:?}",cell.bg),"modifiers":format!("{:?}",cell.modifier)})).collect::<Vec<_>>();
        std::fs::write(
            path,
            serde_json::json!({"width":110,"height":15,"cells":cells}).to_string(),
        )
        .unwrap();
    }
}
#[test]
fn activity_row_preserves_cursor_and_draft_across_widths_and_idle_transition() {
    let mut env = Env::new();
    let mut composer = Composer::new(Vec::new(), 100);
    composer.set_draft("Keep my draft");
    for width in [8, 20, 40, 80, 110] {
        env.status.phase = Phase::Idle;
        let idle = composer.view(width, &env.ctx());
        env.status.phase = Phase::Working;
        composer.set_busy(true);
        let active = composer.view(width, &env.ctx());
        assert_eq!(active.cursor, (idle.cursor.0 + 1, idle.cursor.1));
        assert_eq!(active.rows.len(), idle.rows.len() + 1);
        assert!(active.rows.iter().all(|r| r.width() <= usize::from(width)));
        env.status.phase = Phase::Idle;
        composer.set_busy(false);
        let settled = composer.view(width, &env.ctx());
        assert_eq!(settled.cursor, idle.cursor);
        assert_eq!(settled.rows, idle.rows);
    }
}

fn key(c: &mut Composer, env: &Env, code: KeyCode, mods: KeyModifiers) -> ComposerEvent {
    c.handle_key(event(code, mods), &env.ctx())
}
fn press(c: &mut Composer, env: &Env, code: KeyCode) -> ComposerEvent {
    key(c, env, code, KeyModifiers::NONE)
}
fn ctrl(c: &mut Composer, env: &Env, ch: char) -> ComposerEvent {
    key(c, env, KeyCode::Char(ch), KeyModifiers::CONTROL)
}
fn typed(c: &mut Composer, env: &Env, text: &str) {
    for ch in text.chars() {
        press(c, env, KeyCode::Char(ch));
    }
}
fn lines(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}
fn screen(c: &Composer, env: &Env, width: u16) -> Vec<String> {
    lines(&c.view(width, &env.ctx()).rows)
}
#[test]
fn double_tap_fires_only_inside_window_and_resets() {
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let mut tap = DoubleTap::default();
    assert!(!tap.press(start), "first press never completes a pair");
    assert!(
        tap.press(start + Duration::from_millis(300)),
        "second press inside window"
    );
    assert!(
        !tap.press(start + Duration::from_millis(350)),
        "a completed pair resets, so the third press starts over"
    );
    assert!(tap.press(start + Duration::from_millis(500)));
    let mut slow = DoubleTap::default();
    assert!(!slow.press(start));
    assert!(
        !slow.press(start + Duration::from_millis(900)),
        "outside window"
    );
}

#[test]
fn subagents_hub_on_left_needs_empty_draft() {
    let env = Env::new();
    let ctx = env.ctx();
    let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
    let mut c = composer();
    c.editor.insert("abc");
    for _ in 0..2 {
        let event = c.edit_key(left, &ctx);
        assert!(!matches!(
            event,
            ComposerEvent::Action(Action::SubagentsHub)
        ));
    }
    c.editor.text.clear();
    c.editor.cursor = 0;
    assert!(matches!(c.edit_key(left, &ctx), ComposerEvent::None));
    assert!(matches!(
        c.edit_key(left, &ctx),
        ComposerEvent::Action(Action::SubagentsHub)
    ));
}

fn composer() -> Composer {
    Composer::new(vec!["older prompt".into(), "recent prompt".into()], 100)
}

#[test]
fn action_palette_shortcuts_preserve_pasted_drafts_and_the_running_queue() {
    for busy in [false, true] {
        let env = Env::new();
        let mut composer = composer();
        composer.busy = busy;
        composer.set_draft("日本語 draft\n");
        composer.handle_paste(&"long pasted reference\n".repeat(20));
        composer.queued.push_back(("next request".into(), true));
        let draft = composer.draft();
        let pastes = composer.pastes.clone();
        let cursor = composer.editor.cursor;
        for (code, modifiers) in [
            (KeyCode::Char('p'), KeyModifiers::CONTROL),
            (KeyCode::Char('k'), KeyModifiers::ALT),
        ] {
            assert!(matches!(
                key(&mut composer, &env, code, modifiers),
                ComposerEvent::Action(Action::CommandPalette)
            ));
            assert_eq!(composer.draft(), draft);
            assert_eq!(composer.editor.cursor, cursor);
            assert_eq!(composer.pastes, pastes);
            assert_eq!(composer.queued.back(), Some(&("next request".into(), true)));
        }
    }
}

fn cmd(name: &str, desc: &str, kind: CommandKind) -> CommandEntry {
    CommandEntry {
        name: name.into(),
        description: desc.into(),
        kind,
        args_hint: None,
    }
}
fn commands() -> Vec<CommandEntry> {
    let b = |n: &str, d: &str| cmd(n, d, CommandKind::Builtin);
    vec![
        b("help", "Show help"),
        b("model", "Switch model"),
        b("modelpreset", "Model presets"),
        b("memory", "Memory tools"),
        b("settings", "Open settings"),
        b("sessions", "Browse sessions"),
        b("search", "Search files"),
        b("add-dir", "Add a directory to the workspace"),
        b("remove-dir", "Remove an added directory"),
        b("dirs", "List the workspace directories"),
        b("git", "Show git status and recent commits"),
        cmd("skill:review", "Review a diff", CommandKind::Skill),
        cmd("deploy", "Run the deploy script", CommandKind::Custom),
    ]
}
fn with_commands() -> Composer {
    let mut c = composer();
    c.set_commands(commands());
    c
}

#[test]
fn every_row_fits_the_width_and_the_box_has_its_structure_at_all_sizes() {
    let env = Env::new();
    let mut c = composer();
    c.set_commands(commands());
    c.set_draft("界e\u{301}👩‍💻 some fairly long text that must wrap around the narrow box several times\nsecond line");
    for width in [1u16, 2, 7, 8, 9, 20, 60, 120] {
        let view = c.view(width, &env.ctx());
        let rows = lines(&view.rows);
        for (row, line) in view.rows.iter().zip(&rows) {
            assert!(
                text::line_width(row) <= width as usize,
                "width {width}: {line:?}"
            );
        }
        assert!(
            (view.cursor.0 as usize) < rows.len()
                && (view.cursor.1 as usize) < width.max(1) as usize,
            "width {width}: cursor {:?} in {rows:?}",
            view.cursor
        );
        if width >= 8 {
            let (top, bottom) = (&rows[0], &rows[rows.len() - 1]);
            assert!(
                top.starts_with('╭')
                    && top.trim_end().ends_with('╮')
                    && text::width(top) == width as usize,
                "{top:?}"
            );
            assert!(
                bottom.starts_with('╰')
                    && bottom.ends_with('╯')
                    && text::width(bottom) == width as usize,
                "{bottom:?}"
            );
            for middle in &rows[1..rows.len() - 1] {
                assert!(
                    middle.starts_with('│') && middle.ends_with('│'),
                    "{middle:?}"
                );
            }
        } else {
            assert_eq!(
                rows.len(),
                1,
                "a bare input row below {MIN_BOX_WIDTH} cells"
            );
        }
    }
    assert_eq!(
        c.view(0, &env.ctx()).rows.len(),
        1,
        "width 0 must not panic"
    );
    let empty = Composer::new(vec![], 10);
    for width in [1u16, 8, 60] {
        assert!(!empty.view(width, &env.ctx()).rows.is_empty());
    }
}

#[test]
fn cursor_follows_text_cells_including_wide_and_wrapped_text() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    assert_eq!(
        c.view(30, &env.ctx()).cursor,
        (1, 2),
        "empty draft: inside the left border and padding"
    );
    typed(&mut c, &env, "ab");
    assert_eq!(c.view(30, &env.ctx()).cursor, (1, 4));
    c.set_draft("日本語");
    assert_eq!(
        c.view(30, &env.ctx()).cursor,
        (1, 2 + 6),
        "wide characters count two cells each"
    );
    press(&mut c, &env, KeyCode::Left);
    assert_eq!(c.view(30, &env.ctx()).cursor, (1, 2 + 4));
    c.set_draft("e\u{301}👩‍💻x");
    assert_eq!(
        c.view(30, &env.ctx()).cursor,
        (1, 2 + 1 + 2 + 1),
        "combining marks and ZWJ emoji are single graphemes"
    );
    // Inner width at 12 cells is 8 (the frame takes four): nine narrow chars wrap.
    c.set_draft("123456789");
    let view = c.view(12, &env.ctx());
    assert_eq!(view.cursor, (2, 3), "{:?}", lines(&view.rows));
    assert_eq!(view.rows.len(), 4);
    c.set_draft("1234567日");
    let rows = screen(&c, &env, 12);
    assert_eq!(
        rows[1], "│ 1234567  │",
        "a wide character that does not fit moves to the next row: {rows:?}"
    );
    assert!(rows[2].starts_with("│ 日"));
    c.set_draft("a\nb");
    assert_eq!(c.view(30, &env.ctx()).cursor, (2, 3));
}

#[test]
fn long_drafts_scroll_to_keep_the_cursor_visible() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_draft(
        &(0..30)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let view = c.view(40, &env.ctx());
    let rows = lines(&view.rows);
    assert_eq!(rows.len(), MAX_INPUT_ROWS + 2);
    assert!(
        rows[MAX_INPUT_ROWS].contains("line 29") && !rows.iter().any(|r| r.contains("line 5 ")),
        "{rows:?}"
    );
    assert_eq!(
        view.cursor.0 as usize, MAX_INPUT_ROWS,
        "cursor on the last visible row"
    );
    for _ in 0..29 {
        press(&mut c, &env, KeyCode::Up);
    }
    let view = c.view(40, &env.ctx());
    assert!(lines(&view.rows)[1].contains("line 0") && view.cursor.0 == 1);
}

#[test]
fn line_shape_has_rules_without_side_borders() {
    let mut env = Env::new();
    env.ui.composer_shape = ComposerShape::Line;
    let mut c = Composer::new(vec![], 10);
    c.set_draft("hello");
    let view = c.view(40, &env.ctx());
    let rows = lines(&view.rows);
    assert_eq!(rows.len(), 3);
    assert!(
        rows[0].starts_with("──") && rows[0].ends_with("──") && text::width(&rows[0]) == 40,
        "{:?}",
        rows[0]
    );
    assert_eq!(rows[1], "hello");
    assert!(
        rows[2].starts_with("──") && rows[2].ends_with("──") && text::width(&rows[2]) == 40,
        "{:?}",
        rows[2]
    );
    assert!(
        !rows
            .iter()
            .any(|r| r.contains('│') || r.contains('╭') || r.contains('╰')),
        "{rows:?}"
    );
    assert_eq!(view.cursor, (1, 5));
    env.ui.composer_shape = ComposerShape::Box;
    assert!(screen(&c, &env, 40)[0].starts_with('╭'));
}

#[test]
fn the_status_line_sits_in_the_top_border_and_follows_the_ui_config() {
    let mut env = Env::new();
    env.status.prompt_tokens = Some(3900);
    env.status.context_window = 32_000;
    let c = Composer::new(vec![], 10);
    let top = screen(&c, &env, 100)[0].clone();
    assert!(
        top.contains("mlx/qwen3-9b") && top.contains("3.9k/32k") && text::width(&top) == 100,
        "{top:?}"
    );
    env.ui.status = crate::llm::model_config::StatusPreset::Minimal;
    let top = screen(&c, &env, 100)[0].clone();
    assert!(
        top.contains("mlx/qwen3-9b") && !top.contains("projects/dial"),
        "{top:?}"
    );
}

#[test]
fn transparent_status_has_no_background_in_the_composer_border() {
    let mut env = Env::new();
    let c = Composer::new(vec![], 10);
    let has_bg = |env: &Env| {
        c.view(100, &env.ctx()).rows[0]
            .spans
            .iter()
            .any(|s| s.style.bg.is_some())
    };
    assert!(!has_bg(&env), "the default status line has no fill");
    env.ui.status_transparent = false;
    assert!(
        !has_bg(&env),
        "legacy opacity settings do not add filled blocks"
    );
    env.ui.status_transparent = true;
    assert!(!has_bg(&env));
}

#[test]
fn border_color_follows_state_and_the_session_accent_is_stable() {
    let mut env = Env::new();
    let mut c = Composer::new(vec![], 10);
    let border = |c: &Composer, env: &Env| c.view(60, &env.ctx()).rows[1].spans[0].style.fg;
    assert_eq!(border(&c, &env), env.theme.fg(Tok::Border).fg);
    c.set_busy(true);
    assert_eq!(border(&c, &env), env.theme.fg(Tok::Accent).fg);
    c.set_busy(false);
    c.set_draft("!ls");
    assert_eq!(border(&c, &env), env.theme.fg(Tok::ShellMode).fg);
    c.clear();
    env.ui.session_accent = true;
    env.status.session = "sess-1".into();
    let first = border(&c, &env);
    assert_eq!(first, border(&c, &env), "same session, same accent");
    let accents: Vec<_> = (0..40)
        .map(|n| {
            env.status.session = format!("session-{n}");
            border(&c, &env)
        })
        .collect();
    assert!(
        accents
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1,
        "different sessions get different accents"
    );
    env.status.session.clear();
    assert_eq!(
        border(&c, &env),
        env.theme.fg(Tok::Border).fg,
        "no session, no accent"
    );
}

#[test]
fn shell_mode_starts_with_bang_and_colors_the_marker() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    assert_eq!(c.mode(), InputMode::Normal);
    typed(&mut c, &env, "!ls -la");
    assert_eq!(c.mode(), InputMode::Shell);
    let rows = c.view(60, &env.ctx()).rows;
    let marker = rows[1]
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "!")
        .expect("marker span");
    assert_eq!(marker.style.fg, env.theme.fg(Tok::ShellMode).fg);
    assert!(lines(&rows)[2].contains("shell mode"));
    assert!(
        matches!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::Submit(t) if t == "!ls -la")
    );
    assert_eq!(c.mode(), InputMode::Normal);
    c.set_draft("not !shell");
    assert_eq!(c.mode(), InputMode::Normal);
}

// ---- hints and keymap -------------------------------------------------------------------

#[test]
fn primary_hints_follow_context_and_effective_bindings() {
    let mut env = Env::new();
    let mut c = with_commands();
    assert_eq!(&c.hint(&env.ctx())[..2], ["Enter send", "/ commands"]);
    env.keys.rebind(Action::Submit, &["F2"]).unwrap();
    env.keys.rebind(Action::Newline, &["F3"]).unwrap();
    c.set_draft("a draft");
    assert_eq!(&c.hint(&env.ctx())[..2], ["F2 send", "F3 newline"]);
    c.set_busy(true);
    assert_eq!(c.hint(&env.ctx())[0], "Esc interrupt");
    env.keys.rebind(Action::Interrupt, &["F4"]).unwrap();
    assert_eq!(
        &c.hint(&env.ctx())[..2],
        ["F4 interrupt", "F2 send now or later"]
    );
    env.keys.rebind(Action::Interrupt, &[]).unwrap();
    assert!(!c.hint(&env.ctx()).iter().any(|h| h.contains("interrupt")));
    c.set_busy(false);
    env.keys.rebind(Action::Submit, &[]).unwrap();
    env.keys.rebind(Action::Newline, &[]).unwrap();
    assert!(c.hint(&env.ctx()).is_empty());
    c.set_draft("/mo");
    env.keys.rebind(Action::SelectUp, &[]).unwrap();
    env.keys.rebind(Action::SelectDown, &["F5"]).unwrap();
    env.keys.rebind(Action::Complete, &[]).unwrap();
    env.keys.rebind(Action::Dismiss, &[]).unwrap();
    assert_eq!(c.hint(&env.ctx()), ["F5 select"]);
    env.keys.rebind(Action::SelectDown, &[]).unwrap();
    assert!(c.hint(&env.ctx()).is_empty());
    ctrl(&mut c, &env, 'r');
    assert!(
        c.hint(&env.ctx()).is_empty(),
        "no orphan select slash in search either"
    );
}

#[test]
fn busy_and_queue_edit_hints_respect_literal_control_precedence() {
    let mut env = Env::new();
    let mut c = with_commands();
    c.set_busy(true);
    c.set_draft("/mo");
    assert_eq!(
        c.hint(&env.ctx())[0],
        "Ctrl+C interrupt",
        "Esc dismisses a menu first"
    );
    for follow_up in [false, true] {
        c.set_draft("queued draft");
        let mods = if follow_up {
            KeyModifiers::ALT
        } else {
            KeyModifiers::NONE
        };
        key(&mut c, &env, KeyCode::Enter, mods);
        if !follow_up {
            assert!(c.choice_open());
            press(&mut c, &env, KeyCode::Enter);
        }
        press(&mut c, &env, KeyCode::Up);
        let hints = c.hint(&env.ctx());
        assert_eq!(
            hints[0], "Ctrl+C interrupt",
            "Esc cancels an edited queue item first"
        );
        assert_eq!(
            hints[1],
            format!(
                "editing queued message ({})",
                if follow_up { "send after turn" } else { "send now" }
            )
        );
        assert!(hints.iter().any(|h| h == "Esc cancel edit"));
        press(&mut c, &env, KeyCode::Enter);
        assert_eq!(
            c.queued.pop_front(),
            Some(("queued draft".into(), follow_up)),
            "rendering must not change queue routing"
        );
    }
    env.keys
        .rebind(Action::Submit, &["Alt+Enter", "F2"])
        .unwrap();
    c.set_draft("draft");
    assert!(
        c.hint(&env.ctx())
            .iter()
            .any(|h| h == &format!("{} send now or later", env.keys.label(Action::Submit)))
    );
    key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
    assert!(
        c.choice_open(),
        "remapped submit opens the send choice, not direct follow-up"
    );
    key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
    assert_eq!(
        c.queued.back(),
        Some(&("draft".into(), false)),
        "explicit submit remapping owns the chord"
    );
}

#[test]
fn footer_drops_whole_items_at_exact_width_boundaries() {
    for ascii in [false, true] {
        let mut env = Env::new();
        if ascii {
            env.theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
        }
        env.keys
            .rebind(Action::Interrupt, &["Ctrl+Shift+F12"])
            .unwrap();
        let mut c = composer();
        c.set_busy(true);
        let hints = c.hint(&env.ctx());
        let sep = format!(" {} ", env.theme.sym(Sym::Dot));
        for shape in [ComposerShape::Box, ComposerShape::Line] {
            env.ui.composer_shape = shape;
            for width in 0..=120u16 {
                let view = c.view(width, &env.ctx());
                assert!(
                    view.rows
                        .iter()
                        .all(|r| text::line_width(r) <= width as usize)
                );
                if width < MIN_BOX_WIDTH as u16 {
                    continue;
                }
                let footer = view.rows.last().unwrap().clone();
                let shown: String = footer
                    .spans
                    .iter()
                    .skip(1)
                    .take(footer.spans.len().saturating_sub(2))
                    .map(|s| s.content.as_ref())
                    .collect();
                let keep = (0..=hints.len())
                    .rev()
                    .find(|&n| text::width(&hints[..n].join(&sep)) <= width as usize - 6)
                    .unwrap();
                assert_eq!(shown, hints[..keep].join(&sep), "width {width}");
                assert_eq!(text::line_width(&footer), width as usize);
                if ascii {
                    assert!(lines(&view.rows).iter().all(|r| r.is_ascii()));
                }
            }
        }
    }
}

#[test]
fn unmatched_completion_rows_are_informational_and_width_safe() {
    for ascii in [false, true] {
        let mut env = Env::new();
        if ascii {
            env.theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
        }
        for (draft, message) in [
            ("/zzzz", "No matching commands"),
            ("@zzzz", "No matching files"),
        ] {
            let mut c = with_commands();
            c.set_files(vec!["src/main.rs".into()]);
            c.set_draft(draft);
            assert!(!c.menu_open());
            assert!(screen(&c, &env, 80).first().unwrap().contains(message));
            let row = c.view(80, &env.ctx()).rows.remove(0);
            assert!(
                row.spans
                    .iter()
                    .any(|s| s.content.contains(message) && s.style == env.theme.fg(Tok::Dim))
            );
            for width in 0..=80u16 {
                let view = c.view(width, &env.ctx());
                assert!(
                    view.rows
                        .iter()
                        .all(|r| text::line_width(r) <= width as usize)
                );
                assert!((view.cursor.0 as usize) < view.rows.len());
                assert!(view.cursor.1 < width.max(1));
                if ascii {
                    assert!(lines(&view.rows).iter().all(|r| r.is_ascii()));
                }
            }
            assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
            assert_eq!(c.draft(), draft, "no completion is selected");
            assert_eq!(
                press(&mut c, &env, KeyCode::Enter),
                ComposerEvent::Submit(draft.into())
            );
            c.set_draft(draft);
            press(&mut c, &env, KeyCode::Left);
            assert!(
                !screen(&c, &env, 80).iter().any(|r| r.contains(message)),
                "inactive query has no empty row"
            );
        }
    }
}

#[test]
fn hints_come_from_the_keymap_and_change_with_state() {
    let env = Env::new();
    let mut c = Composer::new(vec!["hello world".into()], 10);
    let hint = |c: &Composer| {
        screen(c, &env, 110)
            .into_iter()
            .find(|r| r.starts_with('╰'))
            .unwrap()
    };
    let idle = hint(&c);
    assert!(
        idle.contains(&format!("{} model", env.keys.label(Action::ModelSelect)))
            && idle.contains(&format!("{} settings", env.keys.label(Action::Settings))),
        "{idle:?}"
    );
    c.set_busy(true);
    assert!(
        hint(&c).contains(&format!("{} interrupt", env.keys.label(Action::Interrupt))),
        "{:?}",
        hint(&c)
    );
    c.set_busy(false);
    typed(&mut c, &env, "hel");
    assert!(c.ghost().is_some(), "history suggests a continuation");
    let drafting = hint(&c);
    typed(&mut c, &env, "x");
    assert!(c.ghost().is_none());
    assert_eq!(
        hint(&c),
        drafting,
        "a suggestion appearing or vanishing leaves the bar unchanged"
    );
    c.clear();
    c.set_commands(commands());
    typed(&mut c, &env, "/m");
    assert!(
        hint(&c).contains(&format!("{} complete", env.keys.label(Action::Complete))),
        "{:?}",
        hint(&c)
    );
    c.clear();
    ctrl(&mut c, &env, 'c');
    assert!(
        hint(&c).contains(&format!(
            "{} again to exit",
            env.keys.label(Action::ClearDraft)
        )),
        "{:?}",
        hint(&c)
    );
    ctrl(&mut c, &env, 'r');
    assert!(hint(&c).contains("accept") && hint(&c).contains("cancel"));
    let mut quiet = Env::new();
    quiet.ui.hints = false;
    let bottom = screen(&Composer::new(vec![], 10), &quiet, 80)
        .into_iter()
        .last()
        .unwrap();
    assert!(
        bottom.chars().all(|ch| ch == '─' || ch == '╰' || ch == '╯'),
        "{bottom:?}"
    );
}

#[test]
fn app_shortcuts_come_from_the_keymap_and_remapping_is_honored() {
    let mut env = Env::new();
    let mut c = Composer::new(vec![], 10);
    let cases = [
        (Action::ModelSelect, KeyCode::Char('m'), KeyModifiers::ALT),
        (Action::ModelRoles, KeyCode::Char('r'), KeyModifiers::ALT),
        (Action::ProviderSetup, KeyCode::Char('p'), KeyModifiers::ALT),
        (Action::Settings, KeyCode::Char(','), KeyModifiers::CONTROL),
        (
            Action::ToolsExpand,
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        ),
        (Action::ToolsToggle, KeyCode::Char('o'), KeyModifiers::ALT),
        (
            Action::ThinkingToggle,
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
        ),
        (Action::ApprovalCycle, KeyCode::BackTab, KeyModifiers::SHIFT),
        (Action::DisplayReset, KeyCode::Char('l'), KeyModifiers::ALT),
        (
            Action::EditExternal,
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
        ),
    ];
    for (action, code, mods) in cases {
        assert_eq!(
            key(&mut c, &env, code, mods),
            ComposerEvent::Action(action),
            "{action:?}"
        );
    }
    assert_eq!(c.draft(), "", "shortcuts never type text");

    if cfg!(target_os = "macos") {
        c.set_draft("keep this draft");
        assert_eq!(
            ctrl(&mut c, &env, 'k'),
            ComposerEvent::Action(Action::ModelSelect)
        );
        assert_eq!(
            c.draft(),
            "keep this draft",
            "model selection must not kill draft text"
        );
        c.set_draft("");
        assert!(
            screen(&c, &env, 110)
                .last()
                .unwrap()
                .contains("Ctrl+K model")
        );
    }

    env.keys.rebind(Action::ModelSelect, &["Ctrl+K"]).unwrap();
    assert_eq!(
        ctrl(&mut c, &env, 'k'),
        ComposerEvent::Action(Action::ModelSelect),
        "the remapped chord fires, ahead of the editor's kill-line"
    );
    assert_eq!(
        key(&mut c, &env, KeyCode::Char('m'), KeyModifiers::ALT),
        ComposerEvent::None,
        "the old chord no longer does"
    );
    assert!(
        screen(&c, &env, 110)
            .last()
            .unwrap()
            .contains("Ctrl+K model"),
        "{:?}",
        screen(&c, &env, 110).last()
    );
}

// ---- editing, keys, submit --------------------------------------------------------------

#[test]
fn grapheme_editing_and_line_keys_work_through_the_editor() {
    let mut env = Env::new();
    // Exercise the editor's Ctrl+K with the app-level shortcut disabled.
    env.keys.rebind(Action::ModelSelect, &[]).unwrap();
    let mut c = composer();
    c.set_draft("a e\u{301}👩‍💻界");
    press(&mut c, &env, KeyCode::Left);
    press(&mut c, &env, KeyCode::Backspace);
    assert_eq!(c.draft(), "a e\u{301}界");
    press(&mut c, &env, KeyCode::Backspace);
    assert_eq!(c.draft(), "a 界");
    press(&mut c, &env, KeyCode::Delete);
    assert_eq!(c.draft(), "a ");
    c.set_draft("界a\n1234");
    press(&mut c, &env, KeyCode::Left);
    press(&mut c, &env, KeyCode::Up);
    press(&mut c, &env, KeyCode::Char('!'));
    assert_eq!(
        c.draft(),
        "界a!\n1234",
        "Up moves between lines before it browses history"
    );
    ctrl(&mut c, &env, 'a');
    ctrl(&mut c, &env, 'k');
    assert_eq!(c.draft(), "\n1234");
    c.set_draft("one two  ");
    ctrl(&mut c, &env, 'w');
    assert_eq!(c.draft(), "one ");
    c.set_draft("abc");
    ctrl(&mut c, &env, 'a');
    ctrl(&mut c, &env, 'd');
    assert_eq!(
        c.draft(),
        "bc",
        "Ctrl-D in a non-empty draft deletes forward"
    );
}

#[test]
fn word_movement_jumps_over_words() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_draft("alpha  beta gamma");
    key(&mut c, &env, KeyCode::Left, KeyModifiers::ALT);
    typed(&mut c, &env, "|");
    assert_eq!(c.draft(), "alpha  beta |gamma");
    key(&mut c, &env, KeyCode::Left, KeyModifiers::CONTROL);
    key(&mut c, &env, KeyCode::Left, KeyModifiers::CONTROL);
    typed(&mut c, &env, "<");
    assert_eq!(c.draft(), "alpha  <beta |gamma");
    key(&mut c, &env, KeyCode::Right, KeyModifiers::ALT);
    typed(&mut c, &env, ">");
    assert_eq!(c.draft(), "alpha  <beta> |gamma");
}

#[test]
fn key_release_events_are_ignored() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    let mut release = event(KeyCode::Char('x'), KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(c.handle_key(release, &env.ctx()), ComposerEvent::None);
    assert_eq!(c.draft(), "");
    c.set_draft("text");
    let mut release = event(KeyCode::Enter, KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert_eq!(c.handle_key(release, &env.ctx()), ComposerEvent::None);
    assert_eq!(c.draft(), "text", "a release of Enter must not submit");
}

#[test]
fn newline_keys_insert_and_enter_submits_without_trimming_or_sending_blanks() {
    let env = Env::new();
    let mut c = composer();
    typed(&mut c, &env, "one");
    key(&mut c, &env, KeyCode::Enter, KeyModifiers::SHIFT);
    typed(&mut c, &env, "two");
    key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
    ctrl(&mut c, &env, 'j');
    typed(&mut c, &env, "three");
    assert_eq!(c.draft(), "one\ntwo\n\nthree");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("one\ntwo\n\nthree".into())
    );
    assert_eq!(c.draft(), "");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::None,
        "an empty draft submits nothing"
    );
    typed(&mut c, &env, "   ");
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert_eq!(c.draft(), "   ", "whitespace is not discarded");
    assert_eq!(c.history().last().unwrap(), "one\ntwo\n\nthree");
}

#[test]
fn busy_queue_distinguishes_steering_followups_and_survives_interrupt() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    typed(&mut c, &env, "steer");
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert!(c.choice_open(), "Enter asks before routing the draft");
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    typed(&mut c, &env, "later");
    key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::Interrupt);
    assert_eq!(c.take_steering(), vec!["steer"]);
    assert_eq!(c.take_follow_up().as_deref(), Some("later"));
    assert_eq!(c.take_follow_up(), None);
    typed(&mut c, &env, "keep this");
    press(&mut c, &env, KeyCode::Enter);
    press(&mut c, &env, KeyCode::Enter);
    press(&mut c, &env, KeyCode::Up);
    assert_eq!(c.draft(), "keep this");
    press(&mut c, &env, KeyCode::Esc);
    assert_eq!(c.draft(), "");
    assert_eq!(
        c.take_steering(),
        vec!["keep this"],
        "Esc cancels the edit, not the queued message"
    );
}

#[test]
fn busy_hints_follow_the_draft_and_queue() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    let submit = env.keys.label(Action::Submit);
    let follow_up = env.keys.label(Action::FollowUp);
    let hints = |c: &Composer| c.hint(&env.ctx());
    assert!(
        hints(&c)
            .iter()
            .all(|h| !h.contains("steer") && !h.contains("queued")),
        "an empty Enter with nothing queued advertises no send: {:?}",
        hints(&c)
    );
    typed(&mut c, &env, "later");
    assert!(hints(&c).contains(&format!("{submit} send now or later")));
    assert!(hints(&c).contains(&format!("{follow_up} send after turn")));
    c.set_draft("");
    c.queued.push_back(("oldest".into(), false));
    c.queued.push_back(("newer".into(), true));
    assert!(
        hints(&c).contains(&format!("{submit} stop or keep")),
        "an empty Enter with a queue stops the turn: {:?}",
        hints(&c)
    );
    assert!(
        hints(&c)
            .iter()
            .all(|h| !h.contains("steer") && !h.contains("after turn")),
        "an empty draft neither steers nor queues"
    );
}

#[test]
fn busy_queue_panel_counts_messages_beyond_its_three_rows() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    for index in 0..3 {
        c.queued.push_back((format!("message {index}"), false));
    }
    let rows = lines(&c.view(100, &env.ctx()).rows).join("\n");
    assert!(!rows.contains("more queued"), "{rows}");
    c.queued.push_back(("message 3".into(), true));
    c.queued.push_back(("message 4".into(), false));
    let rows = lines(&c.view(100, &env.ctx()).rows).join("\n");
    assert!(rows.contains("+2 more queued"), "{rows}");
    assert!(
        !rows.contains("message 4"),
        "only three messages are listed: {rows}"
    );
    assert!(
        rows.contains(&format!(
            "5 queued · {} stop or keep · {} edit last · /queue manage",
            env.keys.label(Action::Submit),
            env.keys.label(Action::SelectUp)
        )),
        "{rows}"
    );
}

#[test]
fn empty_enter_asks_before_stopping_and_only_the_explicit_stop_sends_the_oldest() {
    let mut env = Env::new();
    for follow_up in [false, true] {
        let mut c = composer();
        c.set_busy(true);
        c.set_draft("send now");
        if follow_up {
            key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
        } else {
            press(&mut c, &env, KeyCode::Enter);
            press(&mut c, &env, KeyCode::Enter);
        }
        c.set_draft("keep queued");
        key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
        let queued = c.queued_messages();
        let history = c.history();
        assert_eq!(queued.len(), 2);
        // The default choice keeps the turn running: nothing is sent or removed.
        assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
        assert!(c.choice_open());
        assert_eq!(c.queued_messages(), queued);
        assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
        assert!(!c.choice_open());
        assert_eq!(c.queued_messages(), queued);
        // Only the explicit stop sends the oldest message, once, and keeps the rest queued.
        press(&mut c, &env, KeyCode::Enter);
        press(&mut c, &env, KeyCode::Down);
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit("send now".into())
        );
        assert_eq!(c.queued_messages(), vec![("keep queued".into(), true)]);
        assert_eq!(c.history(), history);
        assert_eq!(c.draft(), "");
        assert!(!c.choice_open());
        assert_eq!(c.take_follow_up().as_deref(), Some("keep queued"));
        assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    }
    env.keys.rebind(Action::Submit, &["F2"]).unwrap();
    let mut c = composer();
    c.set_busy(true);
    c.set_draft("queued");
    key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
    c.set_draft("   ");
    assert_eq!(press(&mut c, &env, KeyCode::F(2)), ComposerEvent::None);
    assert!(!c.choice_open(), "a blank draft never opens a choice");
    assert_eq!(
        c.queued_messages().len(),
        1,
        "a nonempty draft never flushes the queue"
    );
    c.set_draft("");
    assert_eq!(press(&mut c, &env, KeyCode::F(2)), ComposerEvent::None);
    assert!(c.choice_open());
    press(&mut c, &env, KeyCode::Down);
    assert_eq!(
        press(&mut c, &env, KeyCode::F(2)),
        ComposerEvent::Submit("queued".into())
    );
}

#[test]
fn busy_slash_commands_bypass_both_message_queues() {
    let env = Env::new();
    for modifiers in [KeyModifiers::NONE, KeyModifiers::ALT] {
        for command in ["/help", "/model example", "  /unknown"] {
            let mut c = composer();
            c.set_busy(true);
            c.set_draft("existing follow-up");
            key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
            c.set_draft(command);
            assert_eq!(
                key(&mut c, &env, KeyCode::Enter, modifiers),
                ComposerEvent::Submit(command.into())
            );
            assert_eq!(c.draft(), "");
            assert!(c.take_steering().is_empty());
            assert_eq!(c.take_follow_up().as_deref(), Some("existing follow-up"));
            assert_eq!(c.take_follow_up(), None);
        }
    }
}

#[test]
fn ctrl_c_preserves_busy_draft_and_arms_exit_only_when_idle() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    typed(&mut c, &env, "draft");
    assert_eq!(ctrl(&mut c, &env, 'c'), ComposerEvent::None);
    assert_eq!(c.draft(), "", "Ctrl-C clears a non-empty draft");
    assert!(
        !screen(&c, &env, 100)
            .last()
            .unwrap()
            .contains("again to exit")
    );
    assert_eq!(
        ctrl(&mut c, &env, 'c'),
        ComposerEvent::None,
        "first Ctrl-C on an empty idle draft only arms exit"
    );
    assert!(
        screen(&c, &env, 100)
            .last()
            .unwrap()
            .contains("again to exit")
    );
    assert_eq!(ctrl(&mut c, &env, 'c'), ComposerEvent::Exit);

    typed(&mut c, &env, "x");
    ctrl(&mut c, &env, 'c');
    ctrl(&mut c, &env, 'c');
    typed(&mut c, &env, "y");
    assert_eq!(
        ctrl(&mut c, &env, 'c'),
        ComposerEvent::None,
        "typing disarms: this Ctrl-C clears the draft"
    );
    assert_eq!(
        ctrl(&mut c, &env, 'c'),
        ComposerEvent::None,
        "and the next one arms again rather than exiting"
    );

    c.set_busy(true);
    typed(&mut c, &env, "busy draft");
    assert_eq!(ctrl(&mut c, &env, 'c'), ComposerEvent::Interrupt);
    assert_eq!(c.draft(), "busy draft");
}

#[test]
fn ctrl_d_exits_only_on_an_empty_idle_draft_and_esc_is_inert_when_idle() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::None);
    assert_eq!(ctrl(&mut c, &env, 'd'), ComposerEvent::Exit);
    c.set_busy(true);
    assert_eq!(
        ctrl(&mut c, &env, 'd'),
        ComposerEvent::None,
        "never exits in the middle of a turn"
    );
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::Interrupt);
    c.set_busy(false);
    c.set_draft("keep");
    assert_ne!(ctrl(&mut c, &env, 'd'), ComposerEvent::Exit);
}

// ---- history ----------------------------------------------------------------------------

#[test]
fn history_browsing_restores_the_unsent_draft_and_discards_browsed_edits() {
    let env = Env::new();
    let mut c = composer();
    c.set_draft("unsent draft");
    press(&mut c, &env, KeyCode::Left);
    press(&mut c, &env, KeyCode::Up);
    assert_eq!(c.draft(), "recent prompt");
    press(&mut c, &env, KeyCode::Char('!'));
    press(&mut c, &env, KeyCode::Up);
    assert_eq!(c.draft(), "older prompt");
    press(&mut c, &env, KeyCode::Up);
    assert_eq!(c.draft(), "older prompt", "stops at the oldest entry");
    press(&mut c, &env, KeyCode::Down);
    assert_eq!(
        c.draft(),
        "recent prompt",
        "the edit made while browsing is not kept"
    );
    press(&mut c, &env, KeyCode::Down);
    press(&mut c, &env, KeyCode::Char('!'));
    assert_eq!(c.draft(), "unsent draf!t", "draft and cursor come back");
    assert_eq!(c.history(), ["older prompt", "recent prompt"]);
    c.clear();
    assert_eq!(press(&mut c, &env, KeyCode::Down), ComposerEvent::None);
    assert_eq!(c.draft(), "", "Down with nothing to browse does nothing");
}

#[test]
fn browsing_a_multiline_entry_moves_within_it_before_leaving_it() {
    let env = Env::new();
    let mut c = Composer::new(vec!["first".into(), "a\nb".into()], 10);
    press(&mut c, &env, KeyCode::Up);
    assert_eq!(c.draft(), "a\nb");
    press(&mut c, &env, KeyCode::Up);
    assert_eq!(c.draft(), "a\nb", "moved to line 1 of the entry");
    press(&mut c, &env, KeyCode::Up);
    assert_eq!(c.draft(), "first");
}

#[test]
fn browsing_past_a_command_keeps_walking_history_until_the_entry_is_edited() {
    let env = Env::new();
    let mut c = Composer::new(
        vec![
            "oldest prompt".into(),
            "/model".into(),
            "newest prompt".into(),
        ],
        10,
    );
    c.set_commands(vec![
        cmd("model", "Choose a model", CommandKind::Builtin),
        cmd("models", "List models", CommandKind::Builtin),
    ]);
    press(&mut c, &env, KeyCode::Up);
    press(&mut c, &env, KeyCode::Up);
    assert_eq!(c.draft(), "/model");
    assert!(!c.menu_open(), "a recalled command does not open the menu");
    press(&mut c, &env, KeyCode::Up);
    assert_eq!(c.draft(), "oldest prompt", "Up keeps browsing history");
    press(&mut c, &env, KeyCode::Down);
    assert_eq!(c.draft(), "/model");
    press(&mut c, &env, KeyCode::Char('s'));
    assert!(
        c.menu_open(),
        "editing the recalled command completes it again"
    );
}

#[test]
fn history_is_bounded_deduplicated_sanitized_and_only_holds_submissions() {
    let env = Env::new();
    let mut c = Composer::new((0..10).map(|n| format!("entry {n}")).collect(), 3);
    assert_eq!(c.history(), ["entry 7", "entry 8", "entry 9"]);
    c.push_history("entry 9".into());
    c.push_history("   ".into());
    c.push_history("bad\x1b[31m!".into());
    assert_eq!(c.history(), ["entry 8", "entry 9", "bad!"]);
    c.handle_paste("pasted");
    assert_eq!(c.history().len(), 3, "paste never records history");
    let mut unlimited = Composer::new(vec!["x".into()], 0);
    typed(&mut unlimited, &env, "y");
    press(&mut unlimited, &env, KeyCode::Enter);
    assert_eq!(
        unlimited.history(),
        ["x", "y"],
        "limit 0 keeps all submissions"
    );
}

#[test]
fn ctrl_r_searches_history_incrementally_below_the_box() {
    let env = Env::new();
    let mut c = Composer::new(
        vec![
            "git status".into(),
            "cargo test".into(),
            "git commit".into(),
            "ls".into(),
        ],
        10,
    );
    c.set_draft("do not lose me");
    ctrl(&mut c, &env, 'r');
    let view = c.view(60, &env.ctx());
    let rows = lines(&view.rows);
    let search_row = rows
        .iter()
        .position(|r| r.starts_with("search"))
        .expect("prompt row below the box");
    assert!(
        search_row > 1
            && rows[search_row + 1].contains("ls")
            && rows[search_row + 2].contains("git commit"),
        "newest first: {rows:?}"
    );
    assert_eq!(
        view.cursor,
        (search_row as u16, 9),
        "the cursor sits in the search prompt"
    );
    typed(&mut c, &env, "git");
    let rows = screen(&c, &env, 60);
    assert!(
        rows.iter().any(|r| r.contains("git commit"))
            && rows.iter().any(|r| r.contains("git status"))
            && !rows.iter().any(|r| r.contains("cargo test")),
        "{rows:?}"
    );
    press(&mut c, &env, KeyCode::Down);
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert_eq!(
        c.draft(),
        "git status",
        "Enter loads the selected entry without sending it"
    );
    assert_eq!(c.history().len(), 4);
    assert!(
        !screen(&c, &env, 60).iter().any(|r| r.starts_with("search")),
        "search closes"
    );
}

#[test]
fn search_cancel_keeps_the_draft_and_ctrl_r_again_selects_the_next_match() {
    let env = Env::new();
    let mut c = Composer::new(vec!["alpha one".into(), "alpha two".into()], 10);
    c.set_draft("my draft");
    ctrl(&mut c, &env, 'r');
    typed(&mut c, &env, "alpha");
    ctrl(&mut c, &env, 'r');
    press(&mut c, &env, KeyCode::Enter);
    assert_eq!(
        c.draft(),
        "alpha one",
        "Ctrl-R again moved to the older match"
    );
    c.set_draft("my draft");
    ctrl(&mut c, &env, 'r');
    c.handle_paste("alpha\nx");
    let rows = screen(&c, &env, 60);
    assert!(
        rows.iter()
            .any(|r| r.starts_with("search") && r.contains("alpha x")),
        "paste is a single line in the query: {rows:?}"
    );
    assert!(rows.iter().any(|r| r.contains("No matching history")));
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert_eq!(c.draft(), "my draft", "Enter with no match changes nothing");
    ctrl(&mut c, &env, 'r');
    typed(&mut c, &env, "zzz");
    press(&mut c, &env, KeyCode::Esc);
    assert_eq!(c.draft(), "my draft");
    assert!(!screen(&c, &env, 60).iter().any(|r| r.starts_with("search")));
    assert_eq!(
        press(&mut c, &env, KeyCode::Esc),
        ComposerEvent::None,
        "closing the search consumed Esc"
    );
}

// ---- ghost suggestions ------------------------------------------------------------------

#[test]
fn subcommand_menu_lists_filters_and_completes_without_submitting() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_commands(vec![cmd(
        "improve",
        "Self-improvement",
        CommandKind::Builtin,
    )]);
    c.set_argument_completions(vec!["/improve jobs".into(), "/improve stop".into()]);
    typed(&mut c, &env, "/improve ");
    assert!(c.menu_open(), "a space should reveal subcommands");
    let rows = screen(&c, &env, 80);
    assert!(rows.iter().any(|row| row.contains("improve jobs")));
    assert!(rows.iter().any(|row| row.contains("improve stop")));
    press(&mut c, &env, KeyCode::Down);
    assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
    assert_eq!(c.draft(), "/improve stop");
    assert!(!c.menu_open());
    c.set_draft("/improve jo");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/improve jobs".into())
    );
    assert_eq!(c.draft(), "");
}

#[test]
fn model_settings_completion_preserves_bare_model_selection() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_commands(vec![cmd("model", "Switch model", CommandKind::Builtin)]);
    c.set_argument_completions(vec!["/model settings".into(), "/model defaults".into()]);
    typed(&mut c, &env, "/model");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/model".into())
    );
    c.set_draft("/model ");
    assert!(c.menu_open(), "a space should reveal model settings");
    assert!(
        screen(&c, &env, 80)
            .iter()
            .any(|row| row.contains("model settings"))
    );
    c.set_draft("/model se");
    assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
    assert_eq!(c.draft(), "/model settings");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/model settings".into())
    );
    c.set_draft("/model de");
    assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
    assert_eq!(c.draft(), "/model defaults");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/model defaults".into())
    );
}

#[test]
fn nested_argument_menus_show_only_the_next_word_and_respect_input_controls() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_commands(vec![cmd("skills", "Skills", CommandKind::Builtin)]);
    c.set_argument_completions(vec![
        "/skills list".into(),
        "/skills enable review".into(),
        "/skills enable review --user".into(),
        "/skills enable lint".into(),
    ]);
    c.set_draft("/skills ");
    assert_eq!(
        c.menu.as_ref().unwrap().matches.len(),
        2,
        "nested choices collapse to one enable row"
    );
    c.set_draft("/skills enable ");
    let rows = screen(&c, &env, 80);
    assert!(rows.iter().any(|row| row.contains("skills enable review")));
    assert!(rows.iter().any(|row| row.contains("skills enable lint")));
    assert!(!rows.iter().any(|row| row.contains("--user")));
    press(&mut c, &env, KeyCode::Tab);
    typed(&mut c, &env, " ");
    assert_eq!(c.ghost(), Some("--user"));
    assert!(c.menu_open());
    press(&mut c, &env, KeyCode::Esc);
    assert!(!c.menu_open());
    assert_eq!(c.draft(), "/skills enable review ");
    typed(&mut c, &env, "--u");
    assert!(c.menu_open());
    press(&mut c, &env, KeyCode::Left);
    assert!(!c.menu_open());
    press(&mut c, &env, KeyCode::End);
    assert!(c.menu_open());
    for width in 1..100 {
        assert!(
            screen(&c, &env, width)
                .iter()
                .all(|row| text::width(row) <= width as usize)
        );
    }
    c.set_busy(true);
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/skills enable review --user".into())
    );
    assert_eq!(c.draft(), "");
    assert!(c.take_steering().is_empty());
    c.set_draft("");
    c.handle_paste("/skills enable ");
    assert!(c.menu_open());
    assert_eq!(c.draft(), "/skills enable ");
    c.set_draft("/skills nope");
    assert!(
        screen(&c, &env, 80)
            .iter()
            .any(|row| row.contains("No matching subcommands"))
    );
    c.set_draft("/skills enable\nreview");
    assert!(!c.menu_open());
    c.set_draft("/unknown ");
    assert!(!c.menu_open());
}

#[test]
fn enter_runs_selected_commands_and_subcommands() {
    let env = Env::new();
    let mut c = Composer::new(Vec::new(), 100);
    c.set_commands(vec![
        cmd("skills", "Skills", CommandKind::Builtin),
        cmd("status", "Status", CommandKind::Builtin),
    ]);
    c.set_argument_completions(vec!["/skills sources".into()]);
    for (draft, expected) in [
        ("/s", "/status"),
        ("/skills", "/skills"),
        ("/skills ", "/skills sources"),
        ("/skills so", "/skills sources"),
        ("/skills sources", "/skills sources"),
    ] {
        c.set_draft(draft);
        assert!(c.menu_open());
        press(&mut c, &env, KeyCode::Down);
        assert_eq!(
            press(&mut c, &env, KeyCode::Enter),
            ComposerEvent::Submit(expected.into())
        );
        assert_eq!(c.draft(), "");
        assert_eq!(c.history().last().map(String::as_str), Some(expected));
    }
}

#[test]
fn command_arguments_are_dim_ghosts_without_history_and_complete_without_submitting() {
    let env = Env::new();
    let mut c = Composer::new(Vec::new(), 100);
    c.set_commands(vec![
        cmd("skills", "Manage skills", CommandKind::Builtin),
        cmd("understanding", "Knowledge", CommandKind::Builtin),
    ]);
    c.set_argument_completions(
        vec![
            "/skills sources",
            "/skills enable",
            "/skills enable review",
            "/skills enable review --user",
            "/understanding architecture",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    );
    typed(&mut c, &env, "/understanding ");
    assert_eq!(c.ghost(), Some("architecture"));
    let view = c.view(60, &env.ctx());
    let ghost = view
        .rows
        .iter()
        .rev()
        .flat_map(|row| &row.spans)
        .find(|span| span.content == "architecture")
        .unwrap();
    assert!(ghost.style.add_modifier.contains(Modifier::DIM));
    assert_eq!(c.draft(), "/understanding ");
    assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
    assert_eq!(c.draft(), "/understanding architecture");
    c.set_draft("/skills so");
    assert_eq!(c.ghost(), Some("urces"));
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/skills sources".into())
    );
    assert_eq!(c.draft(), "");
    c.set_draft("/skills enable ");
    assert_eq!(c.ghost(), Some("review"));
    press(&mut c, &env, KeyCode::Right);
    assert_eq!(c.draft(), "/skills enable review");
    typed(&mut c, &env, " --u");
    assert_eq!(c.ghost(), Some("ser"));
    press(&mut c, &env, KeyCode::Tab);
    assert_eq!(c.draft(), "/skills enable review --user");
    for width in 1..50 {
        c.set_draft("/understanding ");
        assert!(
            screen(&c, &env, width)
                .iter()
                .all(|line| text::width(line) <= width as usize)
        );
    }
}

#[test]
fn command_ghosts_prefer_metadata_and_respect_dismissal_cursor_and_busy_state() {
    let env = Env::new();
    let mut c = Composer::new(
        vec!["/skills stale-history".into(), "/custom remembered".into()],
        100,
    );
    c.set_argument_completions(vec![
        "/skills sources".into(),
        "/skills sources".into(),
        "/skills bad\ncommand".into(),
    ]);
    assert_eq!(c.argument_completions, vec!["/skills sources"]);
    c.set_draft("/skills ");
    assert_eq!(c.ghost(), Some("sources"));
    press(&mut c, &env, KeyCode::Esc);
    assert_eq!(c.ghost(), None);
    press(&mut c, &env, KeyCode::Tab);
    assert_eq!(c.draft(), "/skills ");
    typed(&mut c, &env, "s");
    assert_eq!(c.ghost(), Some("ources"));
    press(&mut c, &env, KeyCode::Left);
    assert_eq!(c.ghost(), None);
    press(&mut c, &env, KeyCode::End);
    c.set_busy(true);
    assert_eq!(c.ghost(), None);
    c.set_busy(false);
    assert_eq!(c.ghost(), Some("ources"));
    c.set_draft("/custom ");
    assert_eq!(
        c.ghost(),
        Some("remembered"),
        "unregistered argument choices still use history"
    );
    c.set_draft("/skills sources");
    assert_eq!(c.ghost(), None);
}

#[test]
fn ghost_suggestion_is_dim_and_only_changes_the_draft_when_accepted() {
    let env = Env::new();
    let mut c = composer();
    typed(&mut c, &env, "rec");
    let view = c.view(60, &env.ctx());
    let row = &view.rows[1];
    assert!(
        lines(&view.rows)[1].contains("recent prompt"),
        "{:?}",
        lines(&view.rows)
    );
    let ghost = row
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "ent prompt")
        .expect("ghost span");
    assert!(
        ghost.style.add_modifier.contains(Modifier::DIM)
            && ghost.style.fg == env.theme.fg(Tok::Dim).fg,
        "ghost is dim"
    );
    let typed_span = row
        .spans
        .iter()
        .find(|s| s.content.contains("rec") && !s.content.contains("│"))
        .unwrap();
    assert!(!typed_span.style.add_modifier.contains(Modifier::DIM));
    assert_eq!(
        view.cursor,
        (1, 5),
        "the cursor stays after the typed text, before the ghost"
    );
    assert_eq!(c.draft(), "rec");
    press(&mut c, &env, KeyCode::Tab);
    assert_eq!(c.draft(), "recent prompt");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("recent prompt".into())
    );
}

#[test]
fn word_ghost_fills_from_learned_words_and_tab_accepts_it() {
    let env = Env::new();
    let mut c = Composer::new(Vec::new(), 100);
    c.words.learn_prompt("authentication flow");
    typed(&mut c, &env, "aut");
    assert_eq!(c.ghost(), Some("hentication"));
    press(&mut c, &env, KeyCode::Tab);
    assert_eq!(c.editor.text, "authentication");
}

#[test]
fn history_ghost_takes_precedence_over_word_ghost() {
    let env = Env::new();
    let mut c = Composer::new(vec!["aut-run the tests".into()], 100);
    c.words.learn_prompt("authentication flow");
    typed(&mut c, &env, "aut");
    assert_eq!(c.ghost(), Some("-run the tests"));
}

#[test]
fn word_completion_off_shows_no_word_ghost_and_learns_nothing() {
    let env = Env::new();
    let mut c = Composer::new(Vec::new(), 100);
    c.set_word_completion(false);
    typed(&mut c, &env, "zebrafish report");
    press(&mut c, &env, KeyCode::Enter);
    assert!(c.words.is_empty(), "nothing learned while disabled");
    // Prompt-history completion remains independent of learned-word completion.
    c.history.clear();
    c.words.learn_prompt("zebrafish");
    typed(&mut c, &env, "zeb");
    assert_eq!(c.ghost(), None);
}

#[test]
fn submitting_learns_words_and_saves_them_for_the_next_session() {
    let env = Env::new();
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("learned-words.json");
    let mut c = Composer::new(Vec::new(), 100);
    c.load_learned_words(store.clone());
    typed(&mut c, &env, "zebrafish report");
    press(&mut c, &env, KeyCode::Enter);
    assert!(store.is_file(), "learned words saved after submit");
    let mut next = Composer::new(Vec::new(), 100);
    next.load_learned_words(store);
    typed(&mut next, &env, "zeb");
    assert_eq!(next.ghost(), Some("rafish"));
}

#[test]
fn right_accepts_the_ghost_enter_sends_only_typed_text_and_esc_hides_it_until_editing() {
    let env = Env::new();
    let mut c = composer();
    typed(&mut c, &env, "old");
    press(&mut c, &env, KeyCode::Right);
    assert_eq!(c.draft(), "older prompt");
    c.clear();
    typed(&mut c, &env, "old");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("old".into()),
        "the suggestion is not sent"
    );
    typed(&mut c, &env, "rec");
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::None);
    assert!(
        !lines(&c.view(60, &env.ctx()).rows)[1].contains("recent prompt"),
        "Esc hides the suggestion"
    );
    assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
    assert_eq!(c.draft(), "rec", "and Tab no longer accepts it");
    for _ in 0..3 {
        press(&mut c, &env, KeyCode::Backspace);
    }
    typed(&mut c, &env, "rec");
    assert!(
        lines(&c.view(60, &env.ctx()).rows)[1].contains("recent prompt"),
        "editing away and back brings it back"
    );
}

#[test]
fn ghost_appears_only_at_the_end_of_an_idle_draft_and_prefers_the_newest_entry() {
    let env = Env::new();
    let mut c = Composer::new(
        vec![
            "e\u{301}clair old".into(),
            "ex newer".into(),
            "e\u{301}clair new".into(),
        ],
        10,
    );
    typed(&mut c, &env, "e");
    assert_eq!(
        c.ghost(),
        Some("x newer"),
        "never offer half of a combining sequence"
    );
    c.set_draft("e\u{301}");
    assert_eq!(c.ghost(), Some("clair new"));
    press(&mut c, &env, KeyCode::Left);
    assert_eq!(c.ghost(), None, "no suggestion mid-line");
    press(&mut c, &env, KeyCode::End);
    assert!(c.ghost().is_some());
    c.set_busy(true);
    assert_eq!(c.ghost(), None, "no suggestion while a turn runs");
    assert!(!lines(&c.view(60, &env.ctx()).rows)[1].contains("clair"));
    assert_eq!(press(&mut c, &env, KeyCode::Tab), ComposerEvent::None);
    assert_eq!(c.draft(), "e\u{301}");
    c.set_busy(false);
    c.clear();
    assert_eq!(c.ghost(), None, "an empty draft has no suggestion");
    c.set_draft("ex newer");
    assert_eq!(c.ghost(), None, "an exact match leaves nothing to suggest");
}

#[test]
fn multiline_ghost_wraps_inside_the_box_and_fits_the_width() {
    let env = Env::new();
    let mut c = Composer::new(
        vec!["start of a long multi\nline entry that goes on".into()],
        10,
    );
    typed(&mut c, &env, "start");
    for width in [8u16, 12, 30] {
        let view = c.view(width, &env.ctx());
        assert!(
            view.rows
                .iter()
                .all(|r| text::line_width(r) <= width as usize)
        );
        assert!((view.cursor.0 as usize) < view.rows.len());
    }
}

// ---- paste ------------------------------------------------------------------------------

#[test]
fn long_pastes_collapse_and_expand_on_submit_short_pastes_stay_literal() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    let long: String = (1..=25).map(|n| format!("line {n}\n")).collect();
    c.handle_paste("small\npaste");
    assert_eq!(
        c.draft(),
        "small\npaste",
        "short pastes are inserted as typed"
    );
    c.clear();
    typed(&mut c, &env, "before ");
    c.handle_paste(&long);
    assert_eq!(c.draft(), "before [Pasted text #1 +25 lines]");
    assert!(lines(&c.view(60, &env.ctx()).rows)[1].contains("[Pasted text #1 +25 lines]"));
    typed(&mut c, &env, " after");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit(format!("before {long} after"))
    );
    assert!(
        c.history().last().unwrap().contains("line 25"),
        "history keeps what was actually sent"
    );
    c.handle_paste(&"x".repeat(2001));
    assert_eq!(
        c.draft(),
        "[Pasted text #2 +1 lines]",
        "very long single lines collapse as well"
    );
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("x".repeat(2001))
    );
    c.handle_paste(&"y".repeat(2000));
    assert!(
        c.draft().starts_with("yyy"),
        "exactly at the limit stays literal"
    );
    c.handle_paste(&"z\n".repeat(20));
    assert!(
        !c.draft().contains("Pasted"),
        "20 lines is not over the limit"
    );
}

#[test]
fn an_edited_placeholder_stays_literal_and_pasted_lookalikes_are_not_expanded() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    let long = "a\n".repeat(30);
    c.handle_paste(&long);
    press(&mut c, &env, KeyCode::Backspace);
    typed(&mut c, &env, " ");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("[Pasted text #1 +30 lines ".into()),
        "edited placeholder is sent as typed"
    );

    c.handle_paste(&long);
    let first = c.draft();
    c.handle_paste(&format!("{}\n{first}", "b\n".repeat(25)));
    assert!(c.draft().contains("#2"), "{}", c.draft());
    let sent = match press(&mut c, &env, KeyCode::Enter) {
        ComposerEvent::Submit(t) => t,
        other => panic!("{other:?}"),
    };
    assert!(
        sent.starts_with(&long) && sent.contains(&"b\n".repeat(25)),
        "both placeholders expand"
    );
    assert!(
        sent.ends_with(&first),
        "text inside a paste that looks like a placeholder stays literal: {sent:?}"
    );
    // Placeholders do not leak into the next draft.
    typed(&mut c, &env, "[Pasted text #1 +30 lines]");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("[Pasted text #1 +30 lines]".into())
    );
}

#[test]
fn paste_never_submits_and_is_sanitized() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.handle_paste("one\r\ntwo\rthree\x1b[31m\x07");
    assert_eq!(c.draft(), "one\ntwo\nthree");
    key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT);
    assert_eq!(c.history().len(), 0);
}

#[test]
fn guarded_raw_paste_never_submits_or_steers_an_active_turn() {
    let env = Env::new();
    for busy in [false, true] {
        let mut c = Composer::new(vec![], 10);
        c.set_busy(busy);
        let mut guard = crate::ui::input::PasteGuard::default();
        let now = std::time::Instant::now();
        for ch in "first line\nsecond line\n".chars() {
            let code = if ch == '\n' {
                KeyCode::Enter
            } else {
                KeyCode::Char(ch)
            };
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            let event = if let Some(text) = guard.text_for_key(key, true, now) {
                c.handle_paste(text);
                ComposerEvent::None
            } else {
                c.handle_key(key, &env.ctx())
            };
            assert_eq!(event, ComposerEvent::None);
        }
        assert_eq!(c.draft(), "first line\nsecond line\n");
        assert!(c.history().is_empty());
        assert!(c.take_steering().is_empty());
        assert!(c.take_follow_up().is_none());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            guard.text_for_key(enter, false, now + std::time::Duration::from_secs(1)),
            None
        );
        let event = c.handle_key(enter, &env.ctx());
        if busy {
            assert_eq!(event, ComposerEvent::None);
            assert!(c.choice_open());
            assert!(c.take_steering().is_empty());
            assert_eq!(c.handle_key(enter, &env.ctx()), ComposerEvent::None);
            assert_eq!(c.take_steering(), vec!["first line\nsecond line\n"]);
        } else {
            assert_eq!(
                event,
                ComposerEvent::Submit("first line\nsecond line\n".into())
            );
        }
    }
}

// ---- slash menu -------------------------------------------------------------------------

#[test]
fn slash_opens_a_grouped_menu_above_the_box_that_filters_fuzzily_as_you_type() {
    let env = Env::new();
    let mut c = with_commands();
    assert!(!c.menu_open());
    typed(&mut c, &env, "/");
    assert!(c.menu_open());
    // Everything above the box's top border is the menu.
    let menu = |c: &Composer| {
        let rows = screen(c, &env, 80);
        let top = rows.iter().position(|r| r.starts_with('╭')).unwrap();
        assert_eq!(rows.len(), top + 3, "the box stays at the bottom: {rows:?}");
        rows[..top].to_vec()
    };
    let rows = menu(&c);
    assert!(rows.len() > 2, "menu rows come before the box: {rows:?}");
    for name in ["help", "model", "settings"] {
        assert!(rows.iter().any(|r| r.contains(name)), "{name}: {rows:?}");
    }
    assert!(
        rows[0].contains("model") && rows[0].contains("Switch model"),
        "name and description on a row, models first: {:?}",
        rows[0]
    );
    assert!(rows[0].trim_end().ends_with("MODELS"), "{rows:?}");
    let settings = rows.iter().position(|r| r.contains("Show help")).unwrap();
    assert!(rows[settings].trim_end().ends_with("SETTINGS"), "{rows:?}");
    assert!(
        !rows[settings + 1].contains("SETTINGS"),
        "a group is named once: {rows:?}"
    );
    typed(&mut c, &env, "mo");
    let names: Vec<_> = menu(&c).iter().map(|r| r.trim().to_owned()).collect();
    assert!(
        names[0].contains("model") && !names[0].contains("modelpreset"),
        "the exact-prefix match leads: {names:?}"
    );
    assert!(
        !names.iter().any(|r| r.ends_with("MODELS")),
        "filtered lists are ranked, not grouped: {names:?}"
    );
    for expected in ["modelpreset", "memory", "remove-dir"] {
        assert!(
            names.iter().any(|r| r.contains(expected)),
            "/mo finds {expected}: {names:?}"
        );
    }
    assert!(
        !names
            .iter()
            .any(|r| r.contains("help") || r.contains("settings")),
        "{names:?}"
    );
    press(&mut c, &env, KeyCode::Backspace);
    press(&mut c, &env, KeyCode::Backspace);
    press(&mut c, &env, KeyCode::Backspace);
    assert!(!c.menu_open(), "deleting the slash closes the menu");
    typed(&mut c, &env, "/zzz");
    assert!(!c.menu_open(), "no match, no selectable menu");
    assert_eq!(screen(&c, &env, 80).len(), 4);
    assert!(screen(&c, &env, 80)[0].contains("No matching commands"));
}

#[test]
fn palette_prioritizes_prefix_then_word_start_and_matches_descriptions() {
    let entries = vec![
        cmd("memory", "", CommandKind::Builtin),
        cmd("set-model", "", CommandKind::Builtin),
        cmd("model", "", CommandKind::Builtin),
        cmd("choose", "Switch provider", CommandKind::Builtin),
    ];
    let names: Vec<_> = rank(&entries, "mo")
        .iter()
        .map(|m| entries[m.index].name.as_str())
        .collect();
    assert_eq!(names, ["model", "set-model", "memory"]);
    assert_eq!(rank(&entries, "provider")[0].index, 3);
}

#[test]
fn menu_matches_track_the_current_query_synchronously() {
    let mut c = with_commands();
    c.set_draft("/old");
    c.set_draft("/new");
    let menu = c.menu.as_ref().unwrap();
    assert_eq!(menu.query, "new");
    let expected = rank(&c.commands, "new");
    assert_eq!(menu.matches.len(), expected.len());
    assert!(c.menu_open() == !expected.is_empty());
}

#[test]
fn autocomplete_ignores_results_for_a_stale_query() {
    let mut c = composer();
    c.set_files(
        (0..=SYNC_RANK_LIMIT)
            .map(|i| format!("dir/{i}.txt"))
            .collect(),
    );
    c.set_draft("@old");
    c.set_draft("@new");
    c.autocomplete_tx
        .send((
            c.candidates_generation,
            "old".into(),
            Some(0),
            vec![Match {
                index: 0,
                name_hits: Vec::new(),
                desc_hits: Vec::new(),
            }],
        ))
        .unwrap();
    c.refresh();
    let menu = c.menu.as_ref().unwrap();
    assert_eq!(menu.query, "new");
    assert!(menu.matches.is_empty());
}

#[test]
fn autocomplete_drops_results_from_a_previous_candidate_set() {
    let mut c = composer();
    c.set_files(
        (0..=SYNC_RANK_LIMIT)
            .map(|i| format!("dir/{i}.txt"))
            .collect(),
    );
    c.set_draft("@new");
    let stale = c.candidates_generation;
    c.set_files(
        (0..=SYNC_RANK_LIMIT)
            .map(|i| format!("lib/{i}.txt"))
            .collect(),
    );
    assert_ne!(c.candidates_generation, stale);
    // Same query, same file_start, but computed against the previous candidate set.
    c.autocomplete_tx
        .send((
            stale,
            "new".into(),
            Some(0),
            vec![Match {
                index: 0,
                name_hits: Vec::new(),
                desc_hits: Vec::new(),
            }],
        ))
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while c.autocomplete_pending() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
        c.poll_autocomplete();
    }
    let menu = c.menu.as_ref().unwrap();
    assert_eq!(menu.query, "new");
    assert!(!menu.pending);
    assert!(menu.matches.is_empty());
}

#[test]
fn large_file_lists_rank_in_the_background_and_land_on_poll() {
    let mut c = composer();
    c.set_files(
        (0..=SYNC_RANK_LIMIT)
            .map(|i| format!("dir/{i}.txt"))
            .collect(),
    );
    c.set_draft("@1999");
    assert!(c.autocomplete_pending());
    assert!(!c.menu_open());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while c.autocomplete_pending() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
        c.poll_autocomplete();
    }
    assert!(!c.autocomplete_pending());
    assert!(c.menu_open());
    assert_eq!(c.menu.as_ref().unwrap().matches[0].index, 1999);
}

#[test]
fn file_completion_fuzzy_matches_and_preserves_surrounding_draft() {
    let env = Env::new();
    let mut c = composer();
    c.set_files(vec!["src/storage/session.rs".into(), "src/app.rs".into()]);
    typed(&mut c, &env, "inspect @ssess");
    assert!(c.menu_open());
    press(&mut c, &env, KeyCode::Tab);
    assert_eq!(c.draft(), "inspect @src/storage/session.rs ");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("inspect @src/storage/session.rs ".into())
    );
}

#[test]
fn menu_rows_have_icons_by_kind_a_pointer_and_a_selected_background() {
    let mut env = Env::new();
    // Every fixture command must be visible so each kind's icon can be checked.
    env.ui.autocomplete_items = 32;
    let mut c = with_commands();
    typed(&mut c, &env, "/");
    let view = c.view(80, &env.ctx());
    let rows = lines(&view.rows);
    let menu_start = 0;
    let row_of = |name: &str| {
        rows[menu_start..]
            .iter()
            .position(|r| r.contains(name))
            .map(|i| i + menu_start)
            .unwrap()
    };
    let t = &env.theme;
    assert!(rows[row_of("help")].contains(&format!("{} help", t.sym(Sym::IconCommand))));
    assert!(
        rows[row_of("skill:review")].contains(&format!("{} skill:review", t.sym(Sym::IconSkill)))
    );
    assert!(rows[row_of("deploy")].contains(&format!("{} deploy", t.sym(Sym::IconSettings))));
    assert!(
        rows[menu_start].starts_with(t.sym(Sym::Pointer))
            && !rows[menu_start + 1].starts_with(t.sym(Sym::Pointer)),
        "{rows:?}"
    );
    let sel_bg = t.bg(Tok::SelectedBg).bg;
    assert!(sel_bg.is_some());
    assert!(
        view.rows[menu_start]
            .spans
            .iter()
            .all(|s| s.style.bg == sel_bg),
        "the whole selected row is highlighted"
    );
    assert!(
        view.rows[menu_start + 1]
            .spans
            .iter()
            .all(|s| s.style.bg.is_none())
    );
    assert_eq!(
        text::line_width(&view.rows[menu_start]),
        80,
        "the selected row spans the width"
    );
    let matched = {
        typed(&mut c, &env, "se");
        c.view(80, &env.ctx()).rows[menu_start].clone()
    };
    let hit = matched
        .spans
        .iter()
        .find(|s| s.style.add_modifier.contains(Modifier::BOLD) && s.content.contains('s'))
        .expect("matched characters are emphasized");
    // On the selection fill an accent would blur into the fill, so matches read in the body color.
    assert_eq!(hit.style.fg, env.theme.fg(Tok::Text).fg);
    let mut ascii = Env::new();
    ascii.theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let mut c = with_commands();
    typed(&mut c, &ascii, "/");
    let rows = screen(&c, &ascii, 80);
    assert!(
        rows[0].starts_with("> model") && rows.iter().all(|r| r.is_ascii()),
        "ascii pointer and no icons: {rows:?}"
    );
}

#[test]
fn consistent_ux_slash_menu_starts_small_and_finds_legacy_commands() {
    let env = Env::new();
    let mut composer = Composer::new(vec![], 10);
    composer.set_commands(vec![
        cmd("workers", "Open workers", CommandKind::Builtin),
        cmd("hub", "Legacy workers", CommandKind::Builtin),
        cmd("custom", "User command", CommandKind::Custom),
    ]);
    composer.set_draft("/");
    let initial = screen(&composer, &env, 80).join("\n");
    assert!(initial.contains("workers") && initial.contains("custom"));
    assert!(!initial.contains("Legacy workers"));
    composer.set_draft("/hub");
    assert!(
        screen(&composer, &env, 80)
            .join("\n")
            .contains("Legacy workers")
    );
}

#[test]
fn menu_rows_stay_short_and_the_selected_command_shows_usage_and_description() {
    let env = Env::new();
    let description =
        "Switch the active model to something else entirely, now and then for every project";
    let mut c = Composer::new(vec![], 10);
    c.set_commands(vec![
        CommandEntry {
            name: "model".into(),
            description: description.into(),
            kind: CommandKind::Builtin,
            args_hint: Some("[settings|defaults|local]".into()),
        },
        cmd("help", "Show help", CommandKind::Builtin),
    ]);
    typed(&mut c, &env, "/mod");
    let narrow = screen(&c, &env, 60);
    let wide = screen(&c, &env, 110);
    for (rows, width) in [(&narrow, 60), (&wide, 110)] {
        assert!(rows.iter().all(|r| text::width(r) <= width), "{rows:?}");
        let menu = rows.iter().find(|r| r.contains("model")).unwrap();
        assert!(
            !menu.contains("[settings"),
            "usage stays out of the row: {menu}"
        );
        assert!(
            rows.iter()
                .any(|r| r.contains("/model [settings|defaults|local]")),
            "{rows:?}"
        );
    }
    assert!(narrow.iter().any(|r| r.contains('…')), "{narrow:?}");
    assert!(
        wide.iter().any(|r| r.contains("something else entirely")),
        "{wide:?}"
    );
}

#[test]
fn menu_detail_never_overflows_tiny_widths() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_commands(vec![CommandEntry {
        name: "model".into(),
        description: "Switch the active model to something else entirely".into(),
        kind: CommandKind::Builtin,
        args_hint: Some("[settings|defaults|local]".into()),
    }]);
    typed(&mut c, &env, "/mod");
    for width in [8u16, 9, 14, 20] {
        let view = c.view(width, &env.ctx());
        assert!(
            view.rows
                .iter()
                .all(|r| text::line_width(r) <= width as usize),
            "width {width}: {:?}",
            lines(&view.rows)
        );
    }
}

#[test]
fn enter_runs_matching_commands_and_preserves_unknown_names() {
    let env = Env::new();
    let mut c = with_commands();
    typed(&mut c, &env, "/sett");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/settings".into())
    );
    assert_eq!(c.draft(), "");
    assert!(!c.menu_open());
    typed(&mut c, &env, "/help");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/help".into()),
        "an exact name runs"
    );
    typed(&mut c, &env, "/model");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/model".into()),
        "exact beats the longer modelpreset"
    );
    typed(&mut c, &env, "/nope");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/nope".into()),
        "unknown commands go to the controller"
    );
}

#[test]
fn typed_settings_command_with_menu_open_submits_settings_not_a_neighbour() {
    // `/settings` + Enter with the menu open must run `settings`, never another command.
    let env = Env::new();
    let mut c = with_commands();
    typed(&mut c, &env, "/settings");
    assert!(c.menu_open());
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/settings".into())
    );
    assert_eq!(c.draft(), "");
}

#[test]
fn menu_navigation_wraps_and_tab_or_right_completes_the_selection() {
    let env = Env::new();
    let mut c = with_commands();
    typed(&mut c, &env, "/se");
    press(&mut c, &env, KeyCode::Down);
    press(&mut c, &env, KeyCode::Tab);
    let second = c.draft();
    assert!(
        second.starts_with('/') && second.ends_with(' ') && second != "/se ",
        "{second:?}"
    );
    c.set_draft("/se");
    let first = {
        press(&mut c, &env, KeyCode::Tab);
        c.draft()
    };
    c.set_draft("/se");
    press(&mut c, &env, KeyCode::Up);
    press(&mut c, &env, KeyCode::Right);
    let last = c.draft();
    assert_ne!(first, last, "Up from the first row wraps to the last");
    assert_ne!(first, second);
    c.set_draft("/mod");
    ctrl(&mut c, &env, 'n');
    press(&mut c, &env, KeyCode::Tab);
    assert_eq!(
        c.draft(),
        "/modelpreset ",
        "Ctrl-N is the keymap's SelectDown"
    );
}

#[test]
fn long_menus_show_autocomplete_items_rows_with_a_scrollbar_and_follow_the_selection() {
    let mut env = Env::new();
    env.ui.autocomplete_items = 5;
    let mut c = Composer::new(vec![], 10);
    c.set_commands(
        (0..20)
            .map(|n| {
                cmd(
                    &format!("cmd{n:02}"),
                    &format!("number {n}"),
                    CommandKind::Custom,
                )
            })
            .collect(),
    );
    typed(&mut c, &env, "/");
    let t = &env.theme;
    let menu = |c: &Composer| {
        let rows = screen(c, &env, 60);
        rows[..rows.len() - 3].to_vec()
    };
    let rows = menu(&c);
    // Five list rows plus the selected command's usage and description.
    assert_eq!(rows.len(), 7);
    assert!(
        rows[0].contains("cmd00")
            && rows[4].contains("cmd04")
            && !rows.iter().any(|r| r.contains("cmd05"))
    );
    assert!(
        rows[4].contains("1/20"),
        "counter on the last row: {rows:?}"
    );
    assert!(
        rows.iter().take(5).all(|r| text::width(r) == 60),
        "{rows:?}"
    );
    assert!(
        rows[0].ends_with(t.sym(Sym::ScrollThumb))
            && rows[1..5]
                .iter()
                .all(|r| r.ends_with(t.sym(Sym::ScrollTrack))),
        "thumb at the top, track below: {rows:?}"
    );
    for _ in 0..5 {
        press(&mut c, &env, KeyCode::Down);
    }
    let rows = menu(&c);
    assert!(
        rows[0].contains("cmd01") && rows[4].contains("cmd05") && rows[4].contains("6/20"),
        "window follows the selection: {rows:?}"
    );
    assert!(
        rows[4].starts_with(t.sym(Sym::Pointer)),
        "the selection is on the last visible row: {rows:?}"
    );
    press(&mut c, &env, KeyCode::Up);
    assert!(
        menu(&c)[3].starts_with(t.sym(Sym::Pointer)) && menu(&c)[0].contains("cmd01"),
        "moving up inside the window does not scroll"
    );
    for _ in 0..4 {
        press(&mut c, &env, KeyCode::Up);
    }
    assert!(menu(&c)[0].starts_with(t.sym(Sym::Pointer)) && menu(&c)[0].contains("cmd00"));
    press(&mut c, &env, KeyCode::Up);
    let rows = menu(&c);
    assert!(
        rows[4].contains("cmd19")
            && rows[4].contains("20/20")
            && rows[4].starts_with(t.sym(Sym::Pointer)),
        "wrapping to the end scrolls it into view: {rows:?}"
    );
    assert!(
        rows[4].ends_with(t.sym(Sym::ScrollThumb)),
        "thumb at the bottom: {rows:?}"
    );
    key(&mut c, &env, KeyCode::PageUp, KeyModifiers::NONE);
    assert!(
        menu(&c)
            .iter()
            .any(|r| r.starts_with(t.sym(Sym::Pointer)) && r.contains("cmd14")),
        "PageUp moves by a page: {:?}",
        menu(&c)
    );
    typed(&mut c, &env, "1");
    let rows = menu(&c);
    assert_eq!(
        rows.len(),
        7,
        "cmd10..cmd19 and more: still limited to five rows"
    );
    assert!(
        rows[0].contains("cmd1") && rows[0].starts_with(t.sym(Sym::Pointer)),
        "typing resets the selection to the top: {rows:?}"
    );
}

#[test]
fn short_menus_have_no_scrollbar_or_counter() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_commands(vec![
        cmd("a", "first", CommandKind::Custom),
        cmd("b", "second", CommandKind::Custom),
    ]);
    typed(&mut c, &env, "/");
    let rows = screen(&c, &env, 40);
    assert_eq!(rows.len(), 7);
    assert!(
        !rows[..4]
            .iter()
            .any(|r| r.contains("/2") || r.contains(env.theme.sym(Sym::ScrollTrack))),
        "{rows:?}"
    );
}

#[test]
fn esc_dismisses_the_menu_until_the_text_changes() {
    let env = Env::new();
    let mut c = with_commands();
    typed(&mut c, &env, "/se");
    assert!(c.menu_open());
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::None);
    assert!(!c.menu_open() && c.draft() == "/se");
    assert_eq!(screen(&c, &env, 80).len(), 3, "no menu rows");
    typed(&mut c, &env, "t");
    assert!(c.menu_open(), "typing reopens the menu");
    press(&mut c, &env, KeyCode::Esc);
    press(&mut c, &env, KeyCode::Backspace);
    assert!(
        c.menu_open(),
        "any change reopens it, even back to the dismissed prefix"
    );
    press(&mut c, &env, KeyCode::Esc);
    typed(&mut c, &env, "t");
    assert!(c.menu_open());
    c.set_busy(true);
    assert_eq!(
        press(&mut c, &env, KeyCode::Esc),
        ComposerEvent::None,
        "Esc closes the menu before it interrupts"
    );
    assert!(!c.menu_open());
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::Interrupt);
}

#[test]
fn the_menu_applies_only_to_a_single_slash_word_at_the_end_with_commands_registered() {
    let env = Env::new();
    let mut plain = composer();
    typed(&mut plain, &env, "/mo");
    assert!(!plain.menu_open(), "no commands registered, no menu");
    let mut c = with_commands();
    typed(&mut c, &env, "/model ");
    assert!(!c.menu_open(), "a space ends command completion");
    typed(&mut c, &env, "gpt");
    assert!(!c.menu_open());
    c.set_draft("not /a command");
    assert!(!c.menu_open());
    c.set_draft("/mo\nmore");
    assert!(!c.menu_open(), "a newline counts as whitespace");
    c.set_draft("/mo");
    assert!(c.menu_open());
    press(&mut c, &env, KeyCode::Left);
    assert!(!c.menu_open(), "the cursor must be at the end");
    press(&mut c, &env, KeyCode::End);
    assert!(c.menu_open());
    ctrl(&mut c, &env, 'r');
    assert!(!c.menu_open(), "history search replaces the menu");
    press(&mut c, &env, KeyCode::Esc);
    c.set_commands(vec![]);
    assert!(!c.menu_open(), "clearing the commands closes it");
}

#[test]
fn registered_commands_are_sanitized_and_untrusted_names_rejected() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_commands(vec![
        cmd(
            "review",
            "check\x1b]0;owned\x07 this\ndiff",
            CommandKind::Custom,
        ),
        cmd("review", "duplicate", CommandKind::Custom),
        cmd("has space", "bad", CommandKind::Custom),
        cmd("/slash", "bad", CommandKind::Custom),
        cmd("", "bad", CommandKind::Custom),
        cmd("tab\tname", "bad", CommandKind::Custom),
    ]);
    typed(&mut c, &env, "/");
    let rows = screen(&c, &env, 80);
    assert_eq!(rows.len(), 6, "only one command survives: {rows:?}");
    assert!(
        rows[0].contains("review")
            && rows[0].contains("check this diff")
            && !rows[0].contains("owned")
            && !rows[0].contains('\x1b'),
        "{:?}",
        rows[0]
    );
}

#[test]
fn menu_and_search_rows_never_exceed_any_width() {
    let env = Env::new();
    let mut c = with_commands();
    typed(&mut c, &env, "/");
    for width in 0..=60u16 {
        let view = c.view(width, &env.ctx());
        for row in &view.rows {
            assert!(
                text::line_width(row) <= width as usize,
                "menu, width {width}: {:?}",
                lines(&view.rows)
            );
        }
    }
    c.clear();
    c.push_history("a long history entry 日本語日本語 with wide characters in it".into());
    ctrl(&mut c, &env, 'r');
    typed(&mut c, &env, "wide");
    for width in 0..=60u16 {
        let view = c.view(width, &env.ctx());
        for row in &view.rows {
            assert!(
                text::line_width(row) <= width as usize,
                "search, width {width}: {:?}",
                lines(&view.rows)
            );
        }
        assert!(view.rows.is_empty() || (view.cursor.0 as usize) < view.rows.len());
    }
}

#[test]
fn untrusted_draft_text_cannot_inject_terminal_sequences() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_draft("safe\x1b]52;c;x\x07\x1b[31mred\u{202e}end");
    for row in screen(&c, &env, 60) {
        assert!(
            !row.contains('\x1b') && !row.contains('\u{202e}') && !row.contains('\x07'),
            "{row:?}"
        );
    }
    assert_eq!(c.draft(), "saferedend");
}

#[test]
fn spinner_in_the_status_border_animates_while_busy() {
    let mut env = Env::new();
    let mut c = Composer::new(vec![], 10);
    c.set_busy(true);
    env.status.phase = Phase::Working;
    env.status.elapsed_secs = 7.0;
    env.status.tick = 0;
    let a = screen(&c, &env, 100)[0].clone();
    env.status.tick = 1;
    let b = screen(&c, &env, 100)[0].clone();
    assert!(a.contains("7s") && a != b, "{a:?} {b:?}");
    assert!(a.contains(env.theme.spinner(0)) && b.contains(env.theme.spinner(1)));
}

#[test]
fn recording_hint_says_release_when_held_and_busy_paste_lands_in_draft() {
    let env = Env::new();
    let mut c = Composer::new(vec![], 10);
    let rendered = |c: &Composer, env: &Env| -> String {
        c.view(80, &env.ctx())
            .rows
            .iter()
            .flat_map(|row| row.spans.iter().map(|span| span.content.to_string()))
            .collect()
    };
    c.set_busy(true);
    c.set_recording(true, true);
    let shortcut = env.keys.label(Action::VoiceInput);
    assert!(rendered(&c, &env).contains(&shortcut));
    assert!(rendered(&c, &env).contains("release"));
    c.set_recording(false, false);
    c.set_recording(true, false);
    assert!(rendered(&c, &env).contains(&shortcut));
    assert!(!rendered(&c, &env).contains("release"));
    c.set_recording(false, false);
    assert!(!rendered(&c, &env).contains("recording,"));
    // A transcript delivered mid-turn lands in the draft without touching the busy state.
    c.handle_paste("spoken words");
    assert_eq!(c.draft(), "spoken words");
    assert!(c.busy);
}

#[test]
fn busy_empty_input_explains_steering_without_changing_submission() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    for width in 8..=120 {
        let view = c.view(width, &env.ctx());
        assert!(
            view.rows
                .iter()
                .all(|row| text::line_width(row) <= width as usize)
        );
        assert!(view.cursor.1 < width);
    }
    let busy = screen(&c, &env, 80).join("\n");
    assert!(busy.contains("Add direction"));
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert!(c.draft().is_empty());
    typed(&mut c, &env, "Keep the API compatible");
    assert!(!screen(&c, &env, 80).join("\n").contains("Add direction"));
    press(&mut c, &env, KeyCode::Enter);
    assert!(c.choice_open(), "a draft asks whether to steer or queue");
    assert!(c.take_steering().is_empty());
    press(&mut c, &env, KeyCode::Enter);
    assert_eq!(c.take_steering(), ["Keep the API compatible"]);
}

#[test]
fn queued_messages_keep_the_management_entry_visible_in_ascii_and_remapped_submit() {
    let mut env = Env::new();
    env.theme = env.theme.with_glyphs(crate::ui::theme::Glyphs::Ascii);
    env.keys.rebind(Action::Submit, &["F2"]).unwrap();
    env.keys.rebind(Action::SelectUp, &["F3"]).unwrap();
    let mut c = composer();
    c.set_busy(true);
    c.set_draft("a queued request");
    c.submit_queued(true);
    let rows = screen(&c, &env, 60).join("\n");
    assert!(rows.contains("/queue manage"), "{rows}");
    assert!(rows.contains("F2 stop or keep"), "{rows}");
    assert!(rows.is_ascii(), "{rows}");
    assert!(screen(&c, &env, 120).join("\n").contains("F3 edit last"));
}

// ---- busy intent chooser ----------------------------------------------------------------

#[test]
fn busy_enter_offers_automatic_routing_or_queue_and_waits_for_a_choice() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    typed(&mut c, &env, "change the plan");
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert!(c.choice_open());
    assert_eq!(c.draft(), "change the plan");
    assert!(c.queued_messages().is_empty());
    let rows = screen(&c, &env, 100).join("\n");
    assert!(
        rows.contains("Send now") && rows.contains("Send after this turn"),
        "{rows}"
    );
    assert_eq!(press(&mut c, &env, KeyCode::Up), ComposerEvent::None);
    assert_eq!(c.draft(), "change the plan", "navigation keeps the draft");
    assert!(c.choice_open());
    press(&mut c, &env, KeyCode::Down);
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert!(!c.choice_open());
    assert_eq!(c.draft(), "");
    assert_eq!(c.queued_messages(), vec![("change the plan".into(), true)]);
}

#[test]
fn the_default_choice_routes_automatically_and_explicit_queue_stays_queued() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    typed(&mut c, &env, "first");
    press(&mut c, &env, KeyCode::Enter);
    press(&mut c, &env, KeyCode::Enter);
    typed(&mut c, &env, "second");
    press(&mut c, &env, KeyCode::Enter);
    press(&mut c, &env, KeyCode::Down);
    press(&mut c, &env, KeyCode::Enter);
    assert_eq!(
        c.queued_messages(),
        vec![("first".into(), false), ("second".into(), true)]
    );
}

#[test]
fn restored_automatic_messages_keep_order_and_do_not_consume_explicit_followups() {
    let mut c = composer();
    c.set_busy(true);
    c.set_draft("first");
    c.submit_queued(false);
    c.set_draft("after turn");
    c.submit_queued(true);
    c.set_draft("second");
    c.submit_queued(false);
    let pending = c.take_steering();
    assert_eq!(pending, ["first", "second"]);
    assert_eq!(c.queued_messages(), [("after turn".into(), true)]);
    c.set_draft("newer");
    c.submit_queued(false);
    c.restore_pending_messages(pending);
    assert_eq!(c.take_steering(), ["first", "second", "newer"]);
    assert_eq!(c.take_follow_up().as_deref(), Some("after turn"));
    assert!(c.take_steering().is_empty());
}

#[test]
fn escape_closes_the_chooser_and_keeps_the_draft_pastes_and_history_exactly() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    typed(&mut c, &env, "see ");
    c.handle_paste(&"trace line\n".repeat(30));
    let draft = c.draft();
    let pastes = c.pastes.clone();
    let cursor = c.editor.cursor;
    let history = c.history();
    press(&mut c, &env, KeyCode::Enter);
    assert!(c.choice_open());
    assert_eq!(
        press(&mut c, &env, KeyCode::Esc),
        ComposerEvent::None,
        "Esc closes the chooser before it interrupts"
    );
    assert!(!c.choice_open());
    assert_eq!(c.draft(), draft);
    assert_eq!(c.pastes, pastes);
    assert_eq!(c.editor.cursor, cursor);
    assert_eq!(c.history(), history);
    assert!(c.queued_messages().is_empty());
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::Interrupt);
}

#[test]
fn an_empty_enter_while_busy_without_queued_work_is_a_no_op() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert!(!c.choice_open());
    assert!(c.queued_messages().is_empty());
}

#[test]
fn a_stop_choice_whose_queue_drained_while_open_sends_nothing() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    c.set_draft("only");
    c.submit_queued(false);
    press(&mut c, &env, KeyCode::Enter);
    assert!(c.choice_open());
    assert!(c.remove_queued(0));
    press(&mut c, &env, KeyCode::Down);
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert!(!c.choice_open());
}

#[test]
fn ending_the_turn_closes_an_open_choice_and_keeps_the_draft() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    typed(&mut c, &env, "keep this draft");
    press(&mut c, &env, KeyCode::Enter);
    assert!(c.choice_open());
    c.set_busy(false);
    assert!(!c.choice_open());
    assert_eq!(c.draft(), "keep this draft");
    assert!(
        !screen(&c, &env, 100)
            .join("\n")
            .contains("Add direction now")
    );
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("keep this draft".into())
    );
}

#[test]
fn slash_commands_and_the_follow_up_key_never_open_the_chooser() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    typed(&mut c, &env, "/help");
    assert_eq!(
        press(&mut c, &env, KeyCode::Enter),
        ComposerEvent::Submit("/help".into())
    );
    assert!(!c.choice_open());
    typed(&mut c, &env, "next");
    assert_eq!(
        key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT),
        ComposerEvent::None
    );
    assert!(!c.choice_open());
    assert_eq!(c.queued_messages(), vec![("next".into(), true)]);
    c.set_draft("");
    assert_eq!(
        key(&mut c, &env, KeyCode::Enter, KeyModifiers::ALT),
        ComposerEvent::None,
        "an empty follow-up is a no-op"
    );
    assert!(!c.choice_open());
    assert_eq!(c.queued_messages(), vec![("next".into(), true)]);
}

#[test]
fn a_held_enter_while_busy_neither_opens_nor_confirms_a_choice() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    typed(&mut c, &env, "hold");
    press(&mut c, &env, KeyCode::Enter);
    assert!(c.choice_open());
    let repeat = KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat);
    assert_eq!(c.handle_key(repeat, &env.ctx()), ComposerEvent::None);
    assert!(c.choice_open());
    assert_eq!(c.draft(), "hold");
    assert!(c.queued_messages().is_empty());
    press(&mut c, &env, KeyCode::Enter);
    assert_eq!(c.queued_messages(), vec![("hold".into(), false)]);
    assert_eq!(c.handle_key(repeat, &env.ctx()), ComposerEvent::None);
    assert!(!c.choice_open(), "a repeat never opens a new choice");
    assert_eq!(c.queued_messages(), vec![("hold".into(), false)]);
}

#[test]
fn chooser_keys_and_hints_follow_remapping() {
    let mut env = Env::new();
    env.keys.rebind(Action::Submit, &["F2"]).unwrap();
    env.keys.rebind(Action::SelectDown, &["F5"]).unwrap();
    let mut c = composer();
    c.set_busy(true);
    typed(&mut c, &env, "remap");
    assert_eq!(press(&mut c, &env, KeyCode::F(2)), ComposerEvent::None);
    assert!(c.choice_open());
    let hints = c.hint(&env.ctx());
    assert!(hints.contains(&"F2 confirm".to_owned()), "{hints:?}");
    assert!(hints.contains(&"Esc cancel".to_owned()), "{hints:?}");
    assert!(hints.iter().any(|h| h.ends_with("F5 select")), "{hints:?}");
    press(&mut c, &env, KeyCode::F(5));
    assert_eq!(press(&mut c, &env, KeyCode::F(2)), ComposerEvent::None);
    assert_eq!(c.queued_messages(), vec![("remap".into(), true)]);
}

#[test]
fn chooser_rows_fit_every_width_in_both_glyph_sets_and_the_cursor_stays_on_the_draft() {
    for ascii in [false, true] {
        let mut env = Env::new();
        if ascii {
            env.theme = env.theme.with_glyphs(crate::ui::theme::Glyphs::Ascii);
        }
        for queue_choice in [false, true] {
            let mut c = composer();
            c.set_busy(true);
            if queue_choice {
                c.set_draft("queued");
                c.submit_queued(false);
                c.set_draft("");
            } else {
                typed(&mut c, &env, "short draft");
            }
            press(&mut c, &env, KeyCode::Enter);
            assert!(c.choice_open());
            for width in 0..=120u16 {
                let view = c.view(width, &env.ctx());
                assert!(
                    view.rows
                        .iter()
                        .all(|row| text::line_width(row) <= width as usize),
                    "width {width}"
                );
                if width > 0 {
                    assert!(view.cursor.1 < width, "width {width}");
                }
                assert!((view.cursor.0 as usize) < view.rows.len(), "width {width}");
                if ascii {
                    assert!(lines(&view.rows).iter().all(|r| r.is_ascii()));
                }
            }
            if !queue_choice {
                let view = c.view(60, &env.ctx());
                let cursor_row = &lines(&view.rows)[view.cursor.0 as usize];
                assert!(cursor_row.contains("short draft"), "{cursor_row:?}");
            }
        }
    }
}

#[test]
fn saving_a_queued_edit_keeps_its_route_and_never_opens_the_chooser() {
    let env = Env::new();
    for original_follow_up in [false, true] {
        for save_with_follow_up_key in [false, true] {
            let mut c = composer();
            c.set_busy(true);
            c.set_draft("queued item");
            c.submit_queued(original_follow_up);
            assert!(c.edit_queued(0));
            typed(&mut c, &env, " edited");
            let modifiers = if save_with_follow_up_key {
                KeyModifiers::ALT
            } else {
                KeyModifiers::NONE
            };
            assert_eq!(
                key(&mut c, &env, KeyCode::Enter, modifiers),
                ComposerEvent::None
            );
            assert!(!c.choice_open());
            assert_eq!(
                c.queued_messages(),
                vec![("queued item edited".into(), original_follow_up)]
            );
            assert_eq!(c.draft(), "");
        }
    }
}

#[test]
fn a_queue_edit_cleared_to_empty_never_opens_the_stop_choice() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    c.set_draft("other");
    c.submit_queued(false);
    c.set_draft("keep me");
    c.submit_queued(true);
    assert!(c.edit_queued(1));
    for _ in 0..7 {
        press(&mut c, &env, KeyCode::Backspace);
    }
    assert_eq!(c.draft(), "");
    assert_eq!(press(&mut c, &env, KeyCode::Enter), ComposerEvent::None);
    assert!(!c.choice_open());
    assert_eq!(c.queued_messages(), vec![("other".into(), false)]);
    assert_eq!(press(&mut c, &env, KeyCode::Esc), ComposerEvent::None);
    assert_eq!(
        c.queued_messages(),
        vec![("other".into(), false), ("keep me".into(), true)]
    );
}

#[test]
fn a_narrow_send_choice_keeps_confirmation_and_escape_visible_before_navigation_hints() {
    let env = Env::new();
    let mut c = composer();
    c.set_busy(true);
    c.set_draft("a new direction");
    press(&mut c, &env, KeyCode::Enter);
    let shown = screen(&c, &env, 38).join("\n");
    assert!(
        shown.contains("Enter confirm") && shown.contains("Esc cancel"),
        "{shown}"
    );
}
