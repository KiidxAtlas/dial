use super::*;
use crate::config::settings_store::Scope;
use crate::ui::overlay::settings::testkit::*;
use crate::ui::overlay::{DrawCtx, OverlayEvent};
use crate::ui::theme::{Glyphs, Theme};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use toml::Table;

fn overlay(path: &str, scope: Scope) -> SettingsOverlay {
    let mut overlay = SettingsOverlay::new(
        Table::new(),
        Table::new(),
        Table::new(),
        scope,
        vec!["dark".into(), "light".into(), "mono".into()],
    );
    assert!(overlay.focus_path(path));
    overlay
}

#[test]
fn appearance_choices_preview_without_mutation_and_save_only_on_enter() {
    let rig = Rig::new();
    for scope in [Scope::Global, Scope::Project] {
        for path in [
            "ui.motion",
            "ui.show_tools",
            "ui.status",
            "ui.status_separator",
            "ui.theme",
            "ui.pinned_workers",
            "ui.worker_live_preview",
            "ui.session_rail",
            "ui.show_thinking",
        ] {
            let mut o = overlay(path, scope);
            let before = o.current_path(path);
            let original = (o.effective.clone(), o.global.clone(), o.project.clone());
            assert!(matches!(
                o.handle(&key(KeyCode::Enter), &rig.keys),
                OverlayEvent::None
            ));
            let event = o.handle(&key(KeyCode::Down), &rig.keys);
            assert!(matches!(
                event,
                OverlayEvent::None | OverlayEvent::PreviewTheme(Some(_))
            ));
            let (def, draft) = o.appearance_draft().unwrap();
            assert_eq!(def.path, path);
            assert_ne!(draft, before, "{path}");
            let preview: Value = Value::try_from(o.preview_config()).unwrap();
            assert_eq!(
                crate::config::schema::get(
                    preview.as_table().unwrap(),
                    path.strip_prefix("ui.").unwrap()
                ),
                Some(&draft)
            );
            assert_eq!(
                (&o.effective, &o.global, &o.project),
                (&original.0, &original.1, &original.2)
            );
            assert!(o.pending.is_empty());
            let event = o.handle(&key(KeyCode::Esc), &rig.keys);
            assert!(matches!(
                event,
                OverlayEvent::None | OverlayEvent::PreviewTheme(None)
            ));
            assert!(o.appearance_draft().is_none());
            assert_eq!(o.current_path(path), before);
            assert_eq!(
                (&o.effective, &o.global, &o.project),
                (&original.0, &original.1, &original.2)
            );
            o.handle(&key(KeyCode::Enter), &rig.keys);
            o.handle(&key(KeyCode::Down), &rig.keys);
            let OverlayEvent::Set {
                path: saved,
                value,
                scope: destination,
            } = o.handle(&key(KeyCode::Enter), &rig.keys)
            else {
                panic!("explicit confirmation must save {path}")
            };
            assert_eq!((saved.as_str(), value, destination), (path, draft, scope));
            assert_eq!(
                (&o.effective, &o.global, &o.project),
                (&original.0, &original.1, &original.2)
            );
        }
    }
}

#[test]
fn appearance_outcomes_render_together_at_wide_narrow_and_ascii_sizes() {
    for theme in [Theme::dark(), Theme::mono().with_glyphs(Glyphs::Ascii)] {
        let rig = Rig::with_theme(theme);
        for width in [60, 120] {
            let mut o = overlay("ui.show_tools", Scope::Global);
            o.handle(&key(KeyCode::Enter), &rig.keys);
            o.handle(&key(KeyCode::Down), &rig.keys);
            o.handle(
                &Event::Key(KeyEvent::new(KeyCode::F(4), KeyModifiers::SHIFT)),
                &rig.keys,
            );
            let preview = |_: &crate::llm::model_config::UiConfig, _: u16| {
                ratatui::text::Line::from("STATUS SAMPLE")
            };
            let ctx = DrawCtx {
                theme: &rig.theme,
                keys: &rig.keys,
                status: &rig.status,
                preview_status: &preview,
            };
            let first = render(width, 32, |f| o.draw(f, &ctx));
            let second = render(width, 32, |f| o.draw(f, &ctx));
            assert_eq!(first, second);
            let shown = screen(&first);
            for label in [
                "Preview status line: STATUS SAMPLE",
                "Preview theme:",
                "(unsaved)",
                "Preview tools: hidden",
                "Preview motion:",
                "Preview transcript:",
                "Preview worker cards:",
                "Preview session rail:",
                "Enter save",
                "Esc discard",
            ] {
                assert!(
                    shown.contains(label),
                    "missing {label} at {width}:\n{shown}"
                );
            }
        }
    }
}

#[test]
fn small_terminal_toggle_preserves_draft_and_exposes_all_preview_rows() {
    for theme in [Theme::dark(), Theme::mono().with_glyphs(Glyphs::Ascii)] {
        let rig = Rig::with_theme(theme);
        for path in ["ui.show_tools", "ui.pinned_workers", "ui.tool_output_lines"] {
            let mut o = overlay(path, Scope::Project);
            o.handle(&key(KeyCode::Enter), &rig.keys);
            o.handle(&key(KeyCode::Down), &rig.keys);
            let draft = o.appearance_draft().unwrap().1;
            let original = (o.effective.clone(), o.global.clone(), o.project.clone());
            let preview = |_: &crate::llm::model_config::UiConfig, _: u16| {
                ratatui::text::Line::from("STATUS")
            };
            let ctx = DrawCtx {
                theme: &rig.theme,
                keys: &rig.keys,
                status: &rig.status,
                preview_status: &preview,
            };
            let controls = screen(&render(40, 12, |f| o.draw(f, &ctx)));
            // The footer keeps saving over previewing when both do not fit; the key still works.
            let wider = screen(&render(48, 12, |f| o.draw(f, &ctx)));
            assert!(wider.contains("Shift+F4 preview"), "{wider}");
            assert!(
                !controls.contains("Preview tools:"),
                "small controls retain room for editing"
            );
            o.handle(
                &Event::Key(KeyEvent::new(KeyCode::F(4), KeyModifiers::SHIFT)),
                &rig.keys,
            );
            let first = screen(&render(40, 12, |f| o.draw(f, &ctx)));
            assert!(
                first.contains("Shift+F4 controls") && first.contains("Preview status line:"),
                "{first}"
            );
            o.handle(&key(KeyCode::End), &rig.keys);
            let last = screen(&render(40, 12, |f| o.draw(f, &ctx)));
            assert!(
                last.contains("Preview session rail:") && last.contains("Side pane:"),
                "{last}"
            );
            o.handle(
                &Event::Key(KeyEvent::new(KeyCode::F(4), KeyModifiers::SHIFT)),
                &rig.keys,
            );
            assert_eq!(o.appearance_draft().unwrap().1, draft);
            assert!(o.pending.is_empty());
            assert_eq!(
                (&o.effective, &o.global, &o.project),
                (&original.0, &original.1, &original.2)
            );
            o.handle(
                &Event::Key(KeyEvent::new(KeyCode::F(4), KeyModifiers::SHIFT)),
                &rig.keys,
            );
            o.handle(&key(KeyCode::Esc), &rig.keys);
            assert!(o.appearance_draft().is_none());
            assert!(o.pending.is_empty());
            assert_eq!(
                (&o.effective, &o.global, &o.project),
                (&original.0, &original.1, &original.2)
            );
        }
    }
}

/// F4 is the global Providers fallback, so the preview answers Shift+F4; plain F4 only once
/// the user has moved Providers off it. Hints name whichever key works.
#[test]
fn preview_key_never_gives_the_global_providers_key_a_second_meaning() {
    let plain = Event::Key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
    let mut rig = Rig::new();
    let mut o = overlay("ui.show_tools", Scope::Project);
    o.handle(&plain, &rig.keys);
    assert!(!o.appearance_preview, "plain F4 belongs to Providers");
    rig.keys
        .rebind(crate::ui::keys::Action::ProviderSetup, &["Alt+P"])
        .unwrap();
    assert_eq!(
        super::super::local_function_label(&rig.keys, 4, crate::ui::keys::Action::ProviderSetup),
        "F4"
    );
    o.handle(&plain, &rig.keys);
    assert!(o.appearance_preview, "a free F4 previews");
}
