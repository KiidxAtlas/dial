use super::super::settings::testkit::*;
use super::*;
use crate::storage::session::SessionInfo;
use crossterm::event::{KeyEvent, KeyModifiers};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

fn entry(name: &str) -> PaletteEntry {
    PaletteEntry {
        command: CommandEntry {
            name: name.into(),
            description: name.into(),
            kind: CommandKind::Builtin,
            args_hint: None,
        },
        during_turn: "Uses the normal turn policy.",
    }
}

fn help() -> UnifiedEntry {
    UnifiedEntry {
        kind: UnifiedKind::Help,
        label: "models".into(),
        description: "Choose a model".into(),
        command: "help models".into(),
        session: None,
    }
}

fn session(id: &str, title: &str, summary: &str) -> UnifiedEntry {
    UnifiedEntry {
        kind: UnifiedKind::Session,
        label: title.into(),
        description: summary.into(),
        command: format!("resume {id}"),
        session: Some(SessionInfo {
            id: id.into(),
            title: title.into(),
            path: format!("/private/{id}.jsonl").into(),
            started: "2026-01-01T00:00:00Z".into(),
            updated: "2026-01-01T00:00:00Z".into(),
            messages: 2,
            user_messages: 1,
            model: "test/model".into(),
            parent_id: None,
            first_message: summary.into(),
            last_message: String::new(),
            last_answer: String::new(),
            project: "local".into(),
        }),
    }
}

fn enter(palette: &mut PaletteOverlay) -> OverlayEvent {
    palette.handle(
        &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &Keymap::defaults(),
    )
}

fn paste(palette: &mut PaletteOverlay, text: &str) {
    assert!(matches!(
        palette.handle(&Event::Paste(text.into()), &Keymap::defaults()),
        OverlayEvent::None
    ));
}

fn rendered(palette: &mut PaletteOverlay) -> String {
    let rig = Rig::new();
    let ctx = DrawCtx {
        theme: &rig.theme,
        keys: &rig.keys,
        status: &rig.status,
        preview_status: &status_preview,
    };
    palette
        .compose(120, 24, &ctx)
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn settle(palette: &mut PaletteOverlay) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        palette.poll_unified();
        if !palette
            .unified
            .as_ref()
            .unwrap()
            .status()
            .contains("loading")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "local fixture worker did not settle"
        );
        std::thread::yield_now();
    }
}

#[test]
fn opt_in_is_lazy_cached_and_grouped_without_changing_default_goals() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let setting = format!("settings {}", crate::config::schema::registry()[0].path);
    let mut palette = PaletteOverlay::new(vec![entry(&setting), entry("model")], false)
        .with_unified_search(vec![help()], move || {
            count.fetch_add(1, Ordering::SeqCst);
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(vec![session("s1", "models", "Saved work")])
        });
    assert!(matches!(palette.rows().first(), Some(Row::Goal(_))));
    rendered(&mut palette);
    paste(&mut palette, "model");
    rendered(&mut palette);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        palette
            .rows()
            .iter()
            .all(|row| matches!(row, Row::Entry(_)))
    );
    palette.search.set("all:");
    let loading = rendered(&mut palette);
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(loading.contains("loading local session summaries"));
    release_tx.send(()).unwrap();
    settle(&mut palette);
    let rows = palette.rows();
    assert_eq!(
        rows,
        [
            Row::Entry(1),
            Row::Entry(0),
            Row::Unified(1),
            Row::Unified(0)
        ]
    );
    let shown = rendered(&mut palette);
    for kind in ["Actions", "Settings", "Sessions", "Help"] {
        assert!(shown.contains(kind), "{shown}");
    }
    palette.search.set("all: models");
    rendered(&mut palette);
    palette.search.set("all:");
    rendered(&mut palette);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn result_matching_is_passive_and_activation_uses_existing_commands() {
    let mut palette = PaletteOverlay::new(vec![entry("model")], true)
        .with_unified_search(vec![help()], || {
            Ok(vec![session("s1", "models", "Saved work")])
        });
    paste(&mut palette, "all: models");
    settle(&mut palette);
    for (kind, expected) in [
        (UnifiedKind::Session, "command:/resume s1"),
        (UnifiedKind::Help, "command:/help models"),
    ] {
        palette.cursor = palette.rows().iter().position(|row| matches!(row, Row::Unified(index) if palette.unified.as_ref().unwrap().entries[*index].kind == kind)).unwrap();
        assert!(matches!(enter(&mut palette), OverlayEvent::Action(action) if action == expected));
    }
    assert_eq!(
        palette.search.text, "all: models",
        "activation does not replace even the palette draft"
    );
}

#[test]
fn unavailable_actions_and_empty_or_failed_results_are_safe() {
    let mut palette = PaletteOverlay::new(vec![entry("commit")], false)
        .with_command_context(crate::ui::command_surface::AvailabilityContext {
            git: Some(false),
            busy: false,
            session: true,
        })
        .with_unified_search(vec![help()], || Err("index unavailable".into()));
    paste(&mut palette, "all: commit");
    settle(&mut palette);
    assert_eq!(palette.rows(), [Row::Entry(0)]);
    assert!(matches!(enter(&mut palette), OverlayEvent::None));
    assert!(rendered(&mut palette).contains("needs a git repository"));
    palette.search.set("all: no-match-123");
    assert!(palette.rows().is_empty());
    assert!(matches!(enter(&mut palette), OverlayEvent::None));
    let shown = rendered(&mut palette);
    assert!(
        shown.contains("No local results match") && shown.contains("Session listing unavailable"),
        "{shown}"
    );
    palette.search.set("all: models");
    assert_eq!(
        palette.rows(),
        [Row::Unified(0)],
        "help survives a listing error"
    );
}

#[test]
fn session_results_use_the_picker_rank_and_never_fuzzy_match_summary_or_path() {
    let mut palette = PaletteOverlay::new(vec![], false).with_unified_search(vec![], || {
        Ok(vec![
            session("summary", "Other topic", "Callback retry"),
            session("title", "Callback retry", "Other topic"),
        ])
    });
    paste(&mut palette, "all: retry callback");
    settle(&mut palette);
    assert_eq!(palette.rows(), [Row::Unified(1), Row::Unified(0)]);
    for query in ["all: clbck", "all: private"] {
        palette.search.set(query);
        let expected = if query.ends_with("clbck") {
            vec![Row::Unified(1)]
        } else {
            vec![]
        };
        assert_eq!(palette.rows(), expected);
    }
}
