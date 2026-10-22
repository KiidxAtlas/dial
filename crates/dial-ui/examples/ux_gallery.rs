//! Render the real terminal components to a local HTML review sheet, without providers or secrets.
//! cargo run -p dial-ui --example ux_gallery -- before
//! Output: $TMPDIR/dial-ux-review/before.html (use `after` for a separate comparison).
use dial_agent::agent::workflow::questions::{Question, QuestionOption, QuestionRequest};
use dial_agent::agent::workflow::todo::TodoList;
use dial_llm::{
    config::settings_store::Scope,
    llm::{
        catalog::{Discovery, ModelEntry, ProviderStatus},
        model_config::{ModelSelection, RoleSource, RoleStatus, UiConfig},
    },
};
use dial_storage::storage::session::SessionInfo;
use dial_ui::ui::{
    Rows,
    composer::{CommandEntry, CommandKind, Composer, ComposerCtx},
    keys::Keymap,
    overlay::{
        DrawCtx,
        models::ModelOverlay,
        palette::{PaletteEntry, PaletteOverlay},
        providers::ProviderOverlay,
        settings::SettingsOverlay,
        shortcuts::ShortcutOverlay,
        subagents::SubagentsOverlay,
    },
    questions::Dialog,
    session_picker::Picker,
    status::{Phase, StatusData},
    subagent_model::{AgentEntry, AgentStatus, HubSnapshot, Totals},
    theme::{Glyphs, Theme},
    transcript::{Transcript, TranscriptCtx},
    welcome::{self, View, WelcomeInfo},
    worker_pins::WorkerBoard,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
    widgets::Paragraph,
};
use std::{fmt::Write as _, fs};
use unicode_width::UnicodeWidthStr;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn color(c: Color) -> String {
    match c {
        Color::Reset => "inherit".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(n) if n >= 232 => {
            let v = 8 + (n - 232) as u16 * 10;
            format!("rgb({v},{v},{v})")
        }
        Color::Indexed(n) if n >= 16 => {
            let n = n - 16;
            let v = |x: u8| if x == 0 { 0 } else { 55 + x as u16 * 40 };
            format!("rgb({},{},{})", v(n / 36), v(n / 6 % 6), v(n % 6))
        }
        other => {
            let n = match other {
                Color::Black => 0,
                Color::Red => 1,
                Color::Green => 2,
                Color::Yellow => 3,
                Color::Blue => 4,
                Color::Magenta => 5,
                Color::Cyan => 6,
                Color::Gray => 7,
                Color::DarkGray => 8,
                Color::LightRed => 9,
                Color::LightGreen => 10,
                Color::LightYellow => 11,
                Color::LightBlue => 12,
                Color::LightMagenta => 13,
                Color::LightCyan => 14,
                Color::White => 15,
                Color::Indexed(n) => n as usize,
                _ => 7,
            };
            [
                "#15171e", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2",
                "#abb2bf", "#626875", "#ff7a85", "#b1df90", "#ffda94", "#85c7ff", "#e1a0ff",
                "#86e5eb", "#f1f3f5",
            ][n]
                .into()
        }
    }
}
fn screen(out: &mut String, title: &str, buffer: &Buffer, light: bool) {
    write!(
        out,
        "<h2>{}</h2><div class=screen style=\"background:{};color:{}\">",
        escape(title),
        if light { "#fafafa" } else { "#111318" },
        if light { "#202531" } else { "#e0e3ea" }
    )
    .unwrap();
    for y in 0..buffer.area.height {
        out.push_str("<div class=row>");
        let mut x = 0;
        while x < buffer.area.width {
            let c = &buffer[(x, y)];
            let mut symbols = String::new();
            let mut next = x;
            // Coalesce adjacent cells with identical presentation. One span per cell made
            // the review sheet needlessly huge; runs preserve the exact column widths.
            while next < buffer.area.width {
                let cell = &buffer[(next, y)];
                if cell.fg != c.fg || cell.bg != c.bg || cell.modifier != c.modifier {
                    break;
                }
                symbols.push_str(cell.symbol());
                next += cell
                    .symbol()
                    .width()
                    .max(1)
                    .min((buffer.area.width - next) as usize) as u16;
            }
            let width = next - x;
            let reverse = c.modifier.contains(Modifier::REVERSED);
            let (fg, bg) = if reverse { (c.bg, c.fg) } else { (c.fg, c.bg) };
            write!(out,"<span style=\"display:inline-block;width:{width}ch;color:{};background:{};font-weight:{};opacity:{};text-decoration:{}\">{}</span>", color(fg), if bg == Color::Reset {"transparent".into()} else {color(bg)}, if c.modifier.contains(Modifier::BOLD) {700} else {400}, if c.modifier.contains(Modifier::DIM) {"0.65"} else {"1"}, if c.modifier.contains(Modifier::CROSSED_OUT) {"line-through"} else if c.modifier.contains(Modifier::UNDERLINED) {"underline"} else {"none"}, escape(&symbols)).unwrap();
            x = next;
        }
        out.push_str("</div>");
    }
    out.push_str("</div>");
}
fn rows_buffer(rows: Rows, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| f.render_widget(Paragraph::new(rows), f.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}
fn main() -> anyhow::Result<()> {
    let label = std::env::args().nth(1).unwrap_or_else(|| "after".into());
    anyhow::ensure!(
        matches!(label.as_str(), "before" | "after"),
        "use before or after"
    );
    let mut out = String::from(
        "<!doctype html><meta charset=utf-8><title>Dial terminal UX review</title><style>body{margin:32px;background:#20232b;color:#e0e3ea;font:14px system-ui}h1{font-size:24px}h2{font-size:14px;margin:28px 0 8px;color:#b8c2d9}.screen{display:table;padding:20px;border:1px solid #434956;border-radius:12px;box-shadow:0 10px 24px #0004}.row{font:14px/21px ui-monospace,Menlo,monospace;white-space:pre;height:21px}p{max-width:850px;line-height:1.5}a{color:#a9ccff}</style>",
    );
    write!(
        out,
        "<h1>Dial · {label} · terminal component review</h1><p>Actual ratatui test-backend cells, not a redesigned web interface. Synthetic fixture data; no provider requests. Browser fonts approximate a terminal. <a href=before.html>Before</a> · <a href=after.html>After</a></p>"
    )?;
    let keys = Keymap::defaults();
    let ui = UiConfig::default();
    for (width, height, theme, light, name) in [
        (
            120,
            40,
            Theme::builtin("dial-dark").unwrap(),
            false,
            "Tall · 120 × 40",
        ),
        (
            100,
            24,
            Theme::builtin("dial-dark").unwrap(),
            false,
            "Dark · 100 × 24",
        ),
        (
            60,
            20,
            Theme::builtin("dial-dark").unwrap(),
            false,
            "Compact · 60 × 20",
        ),
        (
            38,
            16,
            Theme::builtin("dial-dark")
                .unwrap()
                .with_glyphs(Glyphs::Ascii),
            false,
            "ASCII · 38 × 16",
        ),
        (
            100,
            24,
            Theme::builtin("dial").unwrap(),
            true,
            "Light · 100 × 24",
        ),
    ] {
        let info = WelcomeInfo {
            version: "0.1.10".into(),
            provider: "local".into(),
            model: "qwen3.5-9b".into(),
            project: "~/projects/atlas".into(),
            branch: "main".into(),
            approval: "write".into(),
            sandbox: "dev".into(),
            tips: vec![
                (
                    keys.label(dial_ui::ui::keys::Action::CommandPalette),
                    "actions".into(),
                ),
                ("/".into(), "commands".into()),
            ],
            setup_hint: "/model to choose one".into(),
            ..Default::default()
        };
        let mut status = StatusData {
            provider: info.provider.clone(),
            model: info.model.clone(),
            project_path: info.project.clone(),
            branch: info.branch.clone(),
            approval: info.approval.clone(),
            sandbox: info.sandbox.clone(),
            context_window: 32768,
            ..Default::default()
        };
        let mut composer = Composer::new(vec![], 100);
        let context = ComposerCtx {
            theme: &theme,
            keys: &keys,
            ui: &ui,
            status: &status,
        };
        let view = composer.home_view(welcome::content_width(width), &context);
        let rows = welcome::frame(
            &info,
            width,
            height - 1,
            &theme,
            View {
                live: true,
                ..Default::default()
            },
            view,
            Vec::new(),
        )
        .rows;
        screen(
            &mut out,
            &format!("Home / {name}"),
            &rows_buffer(rows, width, height),
            light,
        );
        let mut risky = info.clone();
        risky.model.clear();
        risky.approval = "auto".into();
        risky.sandbox = "off".into();
        screen(
            &mut out,
            &format!("Setup and isolation / {name}"),
            &rows_buffer(
                welcome::render(
                    &risky,
                    width,
                    &theme,
                    View {
                        max_rows: 12,
                        live: true,
                        ..Default::default()
                    },
                ),
                width,
                12,
            ),
            light,
        );
        screen(
            &mut out,
            &format!("Short home selection / {name}"),
            &rows_buffer(
                welcome::render(
                    &info,
                    width,
                    &theme,
                    View {
                        selected: Some(2),
                        max_rows: 3,
                        live: true,
                        ..Default::default()
                    },
                ),
                width,
                3,
            ),
            light,
        );
        status.phase = Phase::Working;
        status.activity = "Inspecting related tests".into();
        status.elapsed_secs = 12.0;
        composer.set_busy(true);
        composer.set_draft("Keep the existing API compatible");
        let view = composer.view(
            width,
            &ComposerCtx {
                theme: &theme,
                keys: &keys,
                ui: &ui,
                status: &status,
            },
        );
        screen(
            &mut out,
            &format!("Working composer / {name}"),
            &rows_buffer(view.rows, width, 6),
            light,
        );
        let working_ctx = ComposerCtx {
            theme: &theme,
            keys: &keys,
            ui: &ui,
            status: &status,
        };
        composer.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
            &working_ctx,
        );
        screen(
            &mut out,
            &format!("Choose message intent / {name}"),
            &rows_buffer(composer.view(width, &working_ctx).rows, width, 12),
            light,
        );
        composer.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            &working_ctx,
        );
        composer.set_draft("");
        let view = composer.view(
            width,
            &ComposerCtx {
                theme: &theme,
                keys: &keys,
                ui: &ui,
                status: &status,
            },
        );
        screen(
            &mut out,
            &format!("Empty working composer / {name}"),
            &rows_buffer(view.rows, width, 6),
            light,
        );
        let mut transcript = Transcript::new();
        transcript.push_user("Keep the existing API compatible.");
        transcript.tool_start("check", "bash", "cargo test", r#"{"command":"cargo test"}"#);
        transcript.tool_end("check", "test result: ok. 18 passed; 0 failed", false, 1.2);
        transcript.tool_start(
            "docs",
            "mcp__context7__query-docs",
            "Look up API compatibility",
            r#"{"libraryId":"/example/sdk","query":"compatible API"}"#,
        );
        transcript.tool_end("docs", "The SDK retains the existing API.", false, 0.8);
        transcript.tool_start(
            "docs-error",
            "mcp__docs__search",
            "Search migration guide",
            r#"{"query":"migration"}"#,
        );
        transcript.tool_end(
            "docs-error",
            "Service unavailable: request timed out.",
            true,
            1.1,
        );
        transcript.assistant_delta("The existing API is preserved. The focused tests pass.");
        transcript.finish_turn();
        let rows = transcript
            .take_settled_with_images(
                width,
                &TranscriptCtx {
                    theme: &theme,
                    ui: &ui,
                    tick: 0,
                    tools_expanded: false,
                    expand_key: &keys.label(dial_ui::ui::keys::Action::ToolsExpand),
                },
            )
            .0;
        screen(
            &mut out,
            &format!("Work evidence / {name}"),
            &rows_buffer(rows, width, 22),
            light,
        );
        let request = QuestionRequest {
            question_id: "review".into(),
            generation: 0,
            session_id: "fixture".into(),
            owner_task_id: None,
            questions: vec![Question {
                id: "direction".into(),
                header: None,
                question: "Which change should come first?".into(),
                options: (1..=12)
                    .map(|n| QuestionOption {
                        label: format!("Choice {n}"),
                        description: None,
                        preview: None,
                    })
                    .collect(),
                multi: false,
                recommended: Some(11),
            }],
            timeout_seconds: 0,
            notify: false,
        };
        let dialog = Dialog::new(&request, std::path::Path::new("."));
        screen(
            &mut out,
            &format!("Short question selection / {name}"),
            &rows_buffer(
                dial_ui::ui::questions::framed(
                    dialog.rows(width as usize, 6),
                    width as usize,
                    6,
                    &theme,
                ),
                width,
                6,
            ),
            light,
        );
        let picker = Picker::new(vec![], vec![], "", "", false);
        screen(
            &mut out,
            &format!("Empty sessions / {name}"),
            &rows_buffer(
                picker.rows(width as usize, height as usize, &theme),
                width,
                height,
            ),
            light,
        );
        let preview = |_: &UiConfig, _: u16| ratatui::text::Line::default();
        let ctx = DrawCtx {
            theme: &theme,
            keys: &keys,
            status: &status,
            preview_status: &preview,
        };
        let mut plan = TodoList::new();
        plan.call(&serde_json::json!({"op":"set", "tasks":[
            {"content":"Locate the compatibility boundary", "phase":"Investigate"},
            {"content":"Implement the adapter without breaking callers", "phase":"Implement"},
            {"content":"Run the provider integration checks"},
            {"content":"Verify the local regression suite"}
        ]}));
        plan.call(&serde_json::json!({"op":"done", "finding":"The public interface must stay stable", "evidence":["src/api.rs:42"]}));
        plan.call(&serde_json::json!({"op":"block", "index":3, "reason":"Provider credentials are unavailable; no live check was run."}));
        screen(
            &mut out,
            &format!("Focused work summary / {name}"),
            &rows_buffer(
                dial_ui::ui::tasks::rows(&plan, width as usize, &theme, None, 4),
                width,
                4,
            ),
            light,
        );
        let mut plan_overlay =
            PaletteOverlay::choice(dial_ui::ui::tasks::plan_picker(&plan).unwrap());
        let mut plan_terminal = Terminal::new(TestBackend::new(width, height))?;
        plan_terminal.draw(|f| plan_overlay.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Interactive work plan / {name}"),
            plan_terminal.backend().buffer(),
            light,
        );
        let mut settings = SettingsOverlay::new(
            toml::Table::new(),
            toml::Table::new(),
            toml::Table::new(),
            Scope::Global,
            vec!["dark".into(), "light".into()],
        )
        .coding_only();
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|f| settings.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Settings / {name}"),
            terminal.backend().buffer(),
            light,
        );
        settings.handle(&crossterm::event::Event::Paste("ui.theme".into()), &keys);
        settings.handle(
            &crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
            &keys,
        );
        settings.handle(&crossterm::event::Event::Paste("dark".into()), &keys);
        terminal.draw(|f| settings.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Filtered theme choices / {name}"),
            terminal.backend().buffer(),
            light,
        );
        let entries = [
            ("model", "Select a model"),
            ("providers", "Connect a provider"),
            ("settings", "Change preferences"),
            ("sessions", "Continue a conversation"),
            ("permissions", "Manage tool permissions"),
            ("todo", "Inspect the work plan and evidence"),
            ("queue", "Manage queued messages"),
            ("workers", "Manage background agents"),
            ("changes", "Inspect changed files"),
            ("review", "Review the current changes"),
        ]
        .into_iter()
        .map(|(name, description)| PaletteEntry {
            command: CommandEntry {
                name: name.into(),
                description: description.into(),
                kind: CommandKind::Builtin,
                args_hint: None,
            },
            during_turn: "Runs after this turn.",
        })
        .collect();
        let mut palette = PaletteOverlay::new(entries, false);
        // Exercise the real worker projections without adding renderer-only production setters.
        let snapshot = HubSnapshot {
            agents: (1..=5)
                .map(|n| AgentEntry {
                    id: format!("task-{n}"),
                    name: if n == 5 {
                        "Needs your direction".into()
                    } else {
                        format!("Worker {n}")
                    },
                    agent: "worker".into(),
                    task: "Preserve public APIs while improving the execution path".into(),
                    status: if n == 5 {
                        AgentStatus::Waiting
                    } else {
                        AgentStatus::Running
                    },
                    current: Some(if n == 5 {
                        "Approval required before execution".into()
                    } else {
                        "Inspecting affected callers".into()
                    }),
                    cancellable: true,
                    steerable: true,
                    ..Default::default()
                })
                .collect(),
            totals: Totals {
                running: 5,
                total: 5,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut workers = SubagentsOverlay::new();
        workers.set_snapshot(snapshot.clone());
        terminal.draw(|f| workers.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Worker roster / {name}"),
            terminal.backend().buffer(),
            light,
        );
        workers.handle(
            &crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageDown,
                crossterm::event::KeyModifiers::NONE,
            )),
            &keys,
        );
        terminal.draw(|f| workers.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Worker roster after paging / {name}"),
            terminal.backend().buffer(),
            light,
        );
        let fixture = tempfile::tempdir()?;
        let layout = dial_llm::config::paths::Layout::with_user_dir(
            fixture.path(),
            fixture.path().join("state"),
        )?;
        let runtime = dial_core::runtime::Tree::default();
        for agent in snapshot.agents {
            runtime.start("worker", &agent.name, None).settle(String::new(), Some(serde_json::json!({
                "id": agent.id,
                "name": agent.name,
                "task": agent.task,
                "status": if agent.status == AgentStatus::Waiting { "needs_input" } else { "running" },
                "current": agent.current,
            })), if agent.status == AgentStatus::Waiting { dial_core::runtime::Phase::NeedsInput } else { dial_core::runtime::Phase::Running });
        }
        let mut board = WorkerBoard::default();
        board.refresh(
            std::time::Instant::now(),
            &layout,
            "gallery",
            &runtime,
            None,
        );
        let pins = dial_ui::ui::worker_pins::rows(
            &board,
            width as usize,
            3,
            dial_llm::llm::model_config::PinnedWorkers::Collapsed,
            true,
            0,
            &theme,
        );
        screen(
            &mut out,
            &format!("Tight worker summary / {name}"),
            &rows_buffer(pins, width, 3),
            light,
        );
        terminal.draw(|f| palette.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Action palette / {name}"),
            terminal.backend().buffer(),
            light,
        );
        palette.handle(
            &crossterm::event::Event::Paste("no-such-action".into()),
            &keys,
        );
        terminal.draw(|f| palette.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Empty action search / {name}"),
            terminal.backend().buffer(),
            light,
        );
        let sessions: Vec<SessionInfo> = [
            (
                "a1",
                "Fix flaky retry in the HTTP client",
                6,
                "2026-01-02T09:00:00Z",
            ),
            (
                "b2",
                "Explain the build pipeline",
                2,
                "2026-01-01T12:00:00Z",
            ),
            ("c3", "", 0, "2025-12-20T08:00:00Z"),
            (
                "d4",
                "Migrate settings storage to TOML",
                14,
                "2025-11-03T16:00:00Z",
            ),
        ]
        .into_iter()
        .map(|(id, title, requests, updated)| SessionInfo {
            id: id.into(),
            path: format!("/{id}.jsonl").into(),
            started: updated.into(),
            updated: updated.into(),
            title: title.into(),
            messages: requests * 2,
            user_messages: requests,
            model: "local/qwen3.5-9b".into(),
            parent_id: None,
            first_message: title.into(),
            last_message: "Keep the public retry API stable".into(),
            last_answer: "The retry loop now honours the deadline.".into(),
            project: "~/projects/atlas".into(),
        })
        .collect();
        let picker = Picker::new(sessions, vec!["d4".into()], "b2", "", false);
        screen(
            &mut out,
            &format!("Sessions / {name}"),
            &rows_buffer(
                picker.rows(width as usize, height as usize, &theme),
                width,
                height,
            ),
            light,
        );
        let entry = |provider: &str, model: &str, source: &str, available: bool| ModelEntry {
            selection: ModelSelection {
                provider: provider.into(),
                model: model.into(),
                base_url: Some(format!("http://{provider}/v1")),
                context_window: Some(32768),
                ..ModelSelection::default()
            },
            label: model.into(),
            source: source.into(),
            available,
            metadata: Default::default(),
        };
        let provider_status =
            |id: &str, label: &str, state: &str, models: usize, detail: &str| ProviderStatus {
                id: id.into(),
                label: label.into(),
                base_url: Some(format!("http://{id}/v1")),
                state: state.into(),
                detail: detail.into(),
                models,
            };
        let discovery = Discovery {
            models: vec![
                entry("local", "qwen3.5-9b", "Local MLX", true),
                entry("ollama", "llama3.2", "Ollama", true),
                entry(
                    "lm-studio",
                    "gemma-3-12b",
                    "LM Studio · loads on first use",
                    true,
                ),
                entry("openai", "gpt-5-mini", "OpenAI (cached, unverified)", false),
            ],
            providers: vec![
                provider_status("local", "Local MLX", "cached", 1, "2 models installed"),
                provider_status("ollama", "Ollama", "reachable", 1, "1 model"),
                provider_status("lm-studio", "LM Studio", "reachable", 1, "1 model"),
                provider_status("openai", "OpenAI", "auth-required", 1, "API key required"),
                provider_status("llama-cpp", "llama.cpp", "unavailable", 0, "Not running"),
            ],
        };
        let main = discovery.models[0].selection.clone();
        let roles = vec![RoleStatus {
            role: "main",
            effective: Some(main.display_name()),
            assigned: Some(main),
            source: RoleSource::Main,
        }];
        let mut models = ModelOverlay::new(Some(discovery.clone()), roles, Scope::Global);
        terminal.draw(|f| models.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Models / {name}"),
            terminal.backend().buffer(),
            light,
        );
        let mut providers = ProviderOverlay::new(
            Some(discovery),
            Default::default(),
            Box::new(|_| false),
            Scope::Global,
        );
        terminal.draw(|f| providers.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Providers / {name}"),
            terminal.backend().buffer(),
            light,
        );
        let mut shortcuts = ShortcutOverlay::new();
        terminal.draw(|f| shortcuts.draw(f, &ctx))?;
        screen(
            &mut out,
            &format!("Keyboard shortcuts / {name}"),
            terminal.backend().buffer(),
            light,
        );
        let mut markdown_transcript = Transcript::new();
        markdown_transcript.push_user("Summarize the retry change.");
        markdown_transcript.assistant_delta(
            "## Retry change\n\nThe client now **honours the deadline**:\n\n1. `retry()` stops once the budget is spent.\n2. Errors keep their original cause.\n\n| Case | Before | After |\n| --- | --- | --- |\n| timeout | retried forever | stops at deadline |\n\n```rust\nlet left = deadline.saturating_duration_since(now);\n```\n\n> Callers that relied on unlimited retries must pass an explicit budget.",
        );
        markdown_transcript.finish_turn();
        let rows = markdown_transcript
            .take_settled_with_images(
                width,
                &TranscriptCtx {
                    theme: &theme,
                    ui: &ui,
                    tick: 0,
                    tools_expanded: false,
                    expand_key: &keys.label(dial_ui::ui::keys::Action::ToolsExpand),
                },
            )
            .0;
        screen(
            &mut out,
            &format!("Answer formatting / {name}"),
            &rows_buffer(rows, width, 24),
            light,
        );
    }
    let dir = std::env::temp_dir().join("dial-ux-review");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{label}.html"));
    fs::write(&path, out)?;
    println!("{}", path.display());
    Ok(())
}
