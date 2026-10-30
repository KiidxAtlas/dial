use super::*;
use crate::runtime::extensions::{Location, View};

fn snapshot(revision: u64, elements: Value) -> OwnedView {
    OwnedView {
        plugin: "demo".into(),
        revision,
        view: View {
            id: "workspace".into(),
            title: "Workspace".into(),
            location: Location::Sidebar,
            elements: serde_json::from_value(elements).unwrap(),
        },
    }
}
fn draw(view: &mut ViewState) -> Vec<String> {
    view.rows(60, 12, &Theme::dark(), true)
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}
fn press(view: &mut ViewState, code: KeyCode) -> Option<ViewEvent> {
    view.key(KeyEvent::new(code, KeyModifiers::NONE), &Keymap::defaults())
}

#[test]
fn edits_survive_new_revision_and_events_address_current_snapshot_only() {
    let elements = json!([{"type":"input","id":"query","label":"Query","value":"a"}]);
    let mut view = ViewState::new(snapshot(1, elements.clone()));
    draw(&mut view);
    press(&mut view, KeyCode::Char('b'));
    assert!(!view.update(snapshot(1, json!([]))));
    assert!(!view.update(snapshot(0, json!([]))));
    assert!(view.update(snapshot(2, elements)));
    assert!(
        press(&mut view, KeyCode::Enter).is_none(),
        "invalidated target must be redrawn before dispatch"
    );
    assert!(
        draw(&mut view)
            .iter()
            .any(|line| line.contains("Query: ab"))
    );
    let event = press(&mut view, KeyCode::Enter).unwrap();
    assert_eq!(event.plugin, "demo");
    assert_eq!(
        event.params,
        json!({"name":"workspace","revision":2,"event":{"type":"change","node":"query","value":"ab"}})
    );
    view.update(snapshot(
        3,
        json!([{"type":"button","id":"other","label":"Other"}]),
    ));
    draw(&mut view);
    assert_eq!(
        press(&mut view, KeyCode::Enter).unwrap().params["event"]["node"],
        "other"
    );
}

#[test]
fn tree_expansion_tabs_selection_and_find_work_without_callbacks() {
    let mut view = ViewState::new(snapshot(
        1,
        json!([
            {"type":"tree","nodes":[{"id":"root","label":"Root","children":[{"id":"leaf","label":"Leaf"}]}]},
            {"type":"tabs","tabs":[{"id":"one","title":"First","elements":[{"type":"text","text":"First body"}]},{"id":"two","title":"Second","elements":[{"type":"button","id":"run","label":"Run"}]}]}
        ]),
    ));
    assert!(!draw(&mut view).iter().any(|line| line.contains("Leaf")));
    assert!(press(&mut view, KeyCode::Enter).is_none());
    assert!(draw(&mut view).iter().any(|line| line.contains("Leaf")));
    press(&mut view, KeyCode::Down);
    let event = press(&mut view, KeyCode::Enter).unwrap();
    assert_eq!(event.params["event"]["row"], "leaf");
    press(&mut view, KeyCode::Char('/'));
    for c in "Second".chars() {
        press(&mut view, KeyCode::Char(c));
    }
    press(&mut view, KeyCode::Enter); // leave search
    assert!(press(&mut view, KeyCode::Enter).is_none()); // select tab locally
    let rows = draw(&mut view);
    assert!(rows.iter().any(|line| line.contains("Run")));
    assert!(!rows.iter().any(|line| line.contains("First body")));
}

#[test]
fn select_cycles_values_and_emits_only_on_confirmation() {
    let mut view = ViewState::new(snapshot(
        4,
        json!([{"type":"select","id":"mode","label":"Mode","options":[{"value":"fast","label":"Fast"},{"value":"safe","label":"Safe"}],"value":"fast"}]),
    ));
    draw(&mut view);
    assert!(press(&mut view, KeyCode::Right).is_none());
    assert!(draw(&mut view).iter().any(|line| line.contains("Safe")));
    assert_eq!(
        press(&mut view, KeyCode::Enter).unwrap().params["event"]["value"],
        "safe"
    );
}

#[test]
fn every_component_renders_with_terminal_safe_bounded_rows_at_tiny_sizes() {
    let elements = json!([
        {"type":"text","text":"\u{1b}[31munsafe\n界界界界界"},
        {"type":"markdown","text":"# Heading\n**bold**"},
        {"type":"code","text":"fn main() {}","language":"rust"},
        {"type":"diff","before":"a\nb\nc","after":"a\nx\nc","path":"file.rs"},
        {"type":"list","items":[{"id":"item","label":"Item","description":"Details"}]},
        {"type":"table","columns":[{"id":"name","label":"Name"}],"rows":[{"id":"row","cells":["Cell"]}]},
        {"type":"progress","value":1.0,"total":2.0,"label":"Loading"},
        {"type":"button","id":"run","label":"Run"}
    ]);
    for (width, height) in [(0, 0), (1, 1), (4, 3), (20, 8), (80, 80)] {
        let mut view = ViewState::new(snapshot(1, elements.clone()));
        let rows = view.rows(width, height, &Theme::dark(), true);
        assert!(rows.len() <= height);
        for row in rows {
            assert!(row.width() <= width);
            assert!(!row.spans.iter().any(|span| span.content.contains('\u{1b}')));
        }
    }
}

#[test]
fn paging_moves_readonly_content_and_keeps_rows_bounded() {
    let mut view = ViewState::new(snapshot(
        1,
        json!([{"type":"text","text":(0..80).map(|i| format!("row {i}")).collect::<Vec<_>>().join("\n")}]),
    ));
    let before = view.rows(40, 6, &Theme::dark(), true);
    press(&mut view, KeyCode::PageDown);
    let after = view.rows(40, 6, &Theme::dark(), true);
    assert_ne!(before, after);
    assert_eq!(after.len(), 6);
}

#[test]
fn paging_past_controls_does_not_snap_back_and_text_arrows_scroll() {
    let mut view = ViewState::new(snapshot(
        1,
        json!([
            {"type":"button","id":"top","label":"Top"},
            {"type":"text","text":(0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")}
        ]),
    ));
    let first = view.rows(40, 6, &Theme::dark(), true);
    press(&mut view, KeyCode::PageDown);
    let next = view.rows(40, 6, &Theme::dark(), true);
    assert_ne!(first, next);
    assert!(!next.iter().any(|row| row.to_string().contains("[ Top ]")));
    assert!(
        press(&mut view, KeyCode::Enter).is_none(),
        "an offscreen control must not receive confirmation"
    );
    let mut text = ViewState::new(snapshot(
        1,
        json!([{"type":"text","text":"one\ntwo\nthree\nfour\nfive\nsix"}]),
    ));
    let before = text.rows(20, 4, &Theme::dark(), true);
    press(&mut text, KeyCode::Down);
    assert_ne!(before, text.rows(20, 4, &Theme::dark(), true));
}
