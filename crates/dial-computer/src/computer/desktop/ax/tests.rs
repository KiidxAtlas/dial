use std::collections::{HashMap, HashSet};

use super::{super::error::ErrorCode, *};

#[derive(Default)]
struct Mock {
    props: HashMap<u64, AxProps>,
    children: HashMap<u64, Vec<u64>>,
    /// Nodes read without an identity.
    unidentified: HashSet<u64>,
    /// Nodes whose earlier reads are gone, their identity taken over.
    gone: HashSet<u64>,
}
impl Mock {
    fn handle(&self, id: u64) -> AxHandle {
        if self.unidentified.contains(&id) {
            AxHandle::TestUnidentified(id)
        } else {
            AxHandle::Test(id)
        }
    }
}
fn node(h: &AxHandle) -> u64 {
    match h {
        AxHandle::Test(id) | AxHandle::TestUnidentified(id) => *id,
        #[cfg(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
        _ => unreachable!(),
    }
}
impl AxBackend for Mock {
    fn window_root(&mut self, _: &DesktopWindow) -> CoreResult<AxHandle> {
        Ok(self.handle(1))
    }

    fn window_id(&mut self, _: &AxHandle, _: &[DesktopWindow]) -> CoreResult<String> {
        unreachable!("window ownership is not exercised by tree traversal tests")
    }

    fn props(&mut self, h: &AxHandle) -> CoreResult<AxProps> {
        let id = node(h);
        self.props
            .get(&id)
            .cloned()
            .ok_or_else(|| DesktopError::ax_failed(format!("unreadable test node {id}")))
    }

    fn children(&mut self, h: &AxHandle) -> CoreResult<Vec<AxHandle>> {
        Ok(self
            .children
            .get(&node(h))
            .into_iter()
            .flatten()
            .map(|id| self.handle(*id))
            .collect())
    }

    fn parent(&mut self, _: &AxHandle) -> CoreResult<Option<AxHandle>> {
        Ok(None)
    }

    fn perform(&mut self, _: &AxHandle, _: &str) -> CoreResult<()> {
        Ok(())
    }

    fn set_value(&mut self, _: &AxHandle, _: &str) -> CoreResult<()> {
        Ok(())
    }

    fn focus(&mut self, _: &AxHandle) -> CoreResult<()> {
        Ok(())
    }

    fn element_at(&mut self, x: f64, y: f64) -> CoreResult<Option<AxHandle>> {
        Ok(self.props.iter().find_map(|(id, props)| {
            props
                .bounds
                .filter(|bounds| {
                    x >= bounds.x
                        && x < bounds.x + bounds.width
                        && y >= bounds.y
                        && y < bounds.y + bounds.height
                })
                .map(|_| self.handle(*id))
        }))
    }

    fn focused_element(&mut self) -> CoreResult<Option<AxHandle>> {
        Ok(None)
    }

    fn attributes(&mut self, _: &AxHandle) -> CoreResult<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    fn alive(&mut self, h: &AxHandle) -> bool {
        !self.gone.contains(&node(h))
    }
}
fn p(role: &str, title: Option<&str>) -> AxProps {
    AxProps {
        role: role.into(),
        native_role: role.into(),
        title: title.map(str::to_string),
        value: None,
        description: None,
        enabled: true,
        focused: false,
        bounds: None,
        actions: Vec::new(),
        child_count: 0,
    }
}
fn window() -> DesktopWindow {
    DesktopWindow {
        id: "7".into(),
        title: "Title".into(),
        app: "Safari".into(),
        pid: None,
        x: 0,
        y: 0,
        width: 100,
        height: 100,
        focused: true,
    }
}
#[test]
fn computer_generations_keep_current_and_previous() {
    let mut m = Mock::default();
    let mut r = AxRegistry::default();
    for g in 1..=3 {
        let generation = r.begin_snapshot("x");
        r.register(
            &mut m,
            "x",
            generation,
            AxHandle::Test(g),
            &p("button", None),
        );
        r.end_snapshot("x", generation);
    }
    assert!(r.resolve("e1").is_err());
    assert!(r.resolve("e2").is_ok());
    assert!(r.resolve("e3").is_ok());
}
#[test]
fn computer_reread_elements_keep_their_ref_and_removed_ones_expire() {
    let mut m = Mock {
        props: [
            (1, p("window", Some("Title"))),
            (2, p("button", Some("Keep"))),
            (3, p("button", Some("Gone"))),
        ]
        .into(),
        children: [(1, vec![2, 3])].into(),
        ..Default::default()
    };
    let mut registry = AxRegistry::default();
    let first = snapshot(
        &mut m,
        &mut registry,
        &window(),
        &AxSnapshotOptions::default(),
    )
    .unwrap();
    assert_eq!(
        first.text,
        "- window \"Title\" [ref=e1] app=Safari (focused)\n  - button \"Keep\" [ref=e2]\n  - \
			 button \"Gone\" [ref=e3]"
    );
    m.children.insert(1, vec![2]);
    for _ in 0..2 {
        let reread = snapshot(
            &mut m,
            &mut registry,
            &window(),
            &AxSnapshotOptions::default(),
        )
        .unwrap();
        assert_eq!(
            reread.text,
            "- window \"Title\" [ref=e1] app=Safari (focused)\n  - button \"Keep\" [ref=e2]"
        );
    }
    let found = query(
        &mut m,
        &mut registry,
        &window(),
        &AxQuery {
            role: Some("button".into()),
            title: Some("keep".into()),
            value: None,
            limit: None,
        },
    )
    .unwrap();
    assert_eq!(found[0].ref_, "e2");
    assert!(matches!(registry.resolve("e2").unwrap(), AxHandle::Test(2)));
    assert_eq!(
        registry.resolve("e3").err().map(|error| error.code),
        Some(ErrorCode::StaleRef)
    );
}
#[test]
fn computer_a_relabelled_element_keeps_its_old_ref_until_it_expires() {
    let mut m = Mock {
        props: [
            (1, p("window", Some("Title"))),
            (2, p("button", Some("Play"))),
        ]
        .into(),
        children: [(1, vec![2])].into(),
        ..Default::default()
    };
    let mut registry = AxRegistry::default();
    let options = AxSnapshotOptions::default();
    snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    m.props.insert(2, p("button", Some("Pause")));
    let relabelled = snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    assert_eq!(
        relabelled.text,
        "- window \"Title\" [ref=e1] app=Safari (focused)\n  - button \"Pause\" [ref=e3]"
    );
    let found = query(
        &mut m,
        &mut registry,
        &window(),
        &AxQuery {
            role: Some("button".into()),
            title: None,
            value: None,
            limit: None,
        },
    )
    .unwrap();
    assert_eq!(found[0].ref_, "e3");
    assert!(matches!(registry.resolve("e2").unwrap(), AxHandle::Test(2)));
    for _ in 0..2 {
        let reread = snapshot(&mut m, &mut registry, &window(), &options).unwrap();
        assert_eq!(reread.text, relabelled.text);
    }
    assert_eq!(
        registry.resolve("e2").err().map(|error| error.code),
        Some(ErrorCode::StaleRef)
    );
    assert!(matches!(registry.resolve("e3").unwrap(), AxHandle::Test(2)));
}
#[test]
fn computer_an_element_missing_from_one_snapshot_keeps_its_ref() {
    let mut m = Mock {
        props: [
            (1, p("window", Some("Title"))),
            (2, p("button", Some("Keep"))),
            (3, p("button", Some("Flicker"))),
        ]
        .into(),
        children: [(1, vec![2, 3])].into(),
        ..Default::default()
    };
    let mut registry = AxRegistry::default();
    let options = AxSnapshotOptions::default();
    let first = snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    m.children.insert(1, vec![2]);
    snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    m.children.insert(1, vec![2, 3]);
    let back = snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    assert_eq!(back.text, first.text);
    assert!(back.text.ends_with("button \"Flicker\" [ref=e3]"));
}
#[test]
fn computer_an_identity_whose_element_is_gone_gets_a_new_ref() {
    let mut m = Mock {
        props: [
            (1, p("window", Some("Title"))),
            (2, p("button", Some("Go"))),
        ]
        .into(),
        children: [(1, vec![2])].into(),
        ..Default::default()
    };
    let mut registry = AxRegistry::default();
    let options = AxSnapshotOptions::default();
    snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    m.gone.insert(2);
    let replaced = snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    assert_eq!(
        replaced.text,
        "- window \"Title\" [ref=e1] app=Safari (focused)\n  - button \"Go\" [ref=e3]"
    );
    m.gone.clear();
    let reread = snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    assert_eq!(reread.text, replaced.text);
}
#[test]
fn computer_elements_without_an_identity_get_a_new_ref_on_every_read() {
    let mut m = Mock {
        props: [
            (1, p("window", Some("Title"))),
            (2, p("button", Some("Go"))),
            (3, p("button", Some("Go"))),
        ]
        .into(),
        children: [(1, vec![2, 3])].into(),
        unidentified: [1, 2, 3].into(),
        ..Default::default()
    };
    let mut registry = AxRegistry::default();
    let options = AxSnapshotOptions::default();
    let first = snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    assert_eq!(
        first.text,
        "- window \"Title\" [ref=e1] app=Safari (focused)\n  - button \"Go\" [ref=e2]\n  - \
			 button \"Go\" [ref=e3]"
    );
    let second = snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    assert_eq!(
        second.text,
        "- window \"Title\" [ref=e4] app=Safari (focused)\n  - button \"Go\" [ref=e5]\n  - \
			 button \"Go\" [ref=e6]"
    );
    assert!(matches!(
        registry.resolve("e3").unwrap(),
        AxHandle::TestUnidentified(3)
    ));
    snapshot(&mut m, &mut registry, &window(), &options).unwrap();
    assert_eq!(
        registry.resolve("e3").err().map(|error| error.code),
        Some(ErrorCode::StaleRef)
    );
}
#[test]
fn computer_refs_are_per_target() {
    let mut m = Mock::default();
    let mut r = AxRegistry::default();
    let go = p("button", Some("Go"));
    let desktop = r.current_generation("desktop");
    assert_eq!(
        r.register(&mut m, "desktop", desktop, AxHandle::Test(5), &go),
        "e1"
    );
    for _ in 0..2 {
        let generation = r.begin_snapshot("7");
        assert_eq!(
            r.register(&mut m, "7", generation, AxHandle::Test(5), &go),
            "e2"
        );
        r.end_snapshot("7", generation);
    }
    for _ in 0..2 {
        let generation = r.begin_snapshot("7");
        r.end_snapshot("7", generation);
    }
    assert!(r.resolve("e2").is_err());
    assert!(r.resolve("e1").is_ok());
}
#[test]
fn computer_hard_cap_evicts_oldest_generation_of_largest_target() {
    let mut m = Mock::default();
    let mut r = AxRegistry::default();
    let g = r.current_generation("x");
    for n in 0..5_001 {
        r.register(&mut m, "x", g, AxHandle::Test(n), &p("button", None));
    }
    assert!(r.entries.len() <= 5_000);
    assert!(r.resolve("e1").is_err());
}
#[test]
fn computer_snapshot_text_and_filter_are_exact() {
    let mut m = Mock {
        props: [
            (1, p("window", Some("Title"))),
            (2, p("group", None)),
            (3, p("button", Some("Go"))),
        ]
        .into(),
        children: [(1, vec![2]), (2, vec![3])].into(),
        ..Default::default()
    };
    m.props.get_mut(&3).unwrap().actions.push("press".into());
    let s = snapshot(
        &mut m,
        &mut AxRegistry::default(),
        &window(),
        &AxSnapshotOptions::default(),
    )
    .unwrap();
    assert_eq!(
        s.text,
        "- window \"Title\" [ref=e1] app=Safari (focused)\n  - button \"Go\" [ref=e2]"
    );
    assert_eq!(s.node_count, 2);
}

#[test]
fn computer_role_query_does_not_confuse_tabs_with_tables_or_groups() {
    let mut backend = Mock {
        props: [
            (1, p("window", Some("Browser"))),
            (2, p("tabgroup", Some("Inventory tabs"))),
            (3, p("table", Some("Inventory table"))),
            (4, p("tab", Some("Inventory"))),
        ]
        .into(),
        children: [(1, vec![2, 3, 4])].into(),
        ..Default::default()
    };
    let found = query(
        &mut backend,
        &mut AxRegistry::default(),
        &window(),
        &AxQuery {
            role: Some("TAB".into()),
            title: Some("INV".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].role, "tab");
    assert_eq!(found[0].title.as_deref(), Some("Inventory"));
}

#[test]
fn computer_snapshot_exposes_non_press_actions_without_substituting_them() {
    let mut props = p("tabgroup", Some("Tabs"));
    props.actions = vec!["AXShowMenu".into(), "AXScrollToVisible".into()];
    let mut backend = Mock {
        props: [(1, p("window", Some("Browser"))), (2, props)].into(),
        children: [(1, vec![2])].into(),
        ..Default::default()
    };
    let result = snapshot(
        &mut backend,
        &mut AxRegistry::default(),
        &window(),
        &AxSnapshotOptions::default(),
    )
    .unwrap();
    assert!(
        result
            .text
            .contains("actions=[AXShowMenu, AXScrollToVisible]"),
        "{}",
        result.text
    );
}
#[test]
fn computer_unnamed_containers_keep_their_surviving_content() {
    // A Reminders-shaped window: an unnamed split group holding a list pane
    // and a detail pane, an unnamed splitter, and an empty wrapper.
    let mut m = Mock {
        props: [
            (1, p("window", Some("Title"))),
            (2, p("splitgroup", None)),
            (3, p("scrollarea", None)),
            (4, p("button", Some("Groceries"))),
            (5, p("splitter", None)),
            (6, p("layoutarea", None)),
            (7, p("textfield", Some("Notes"))),
            (8, p("group", None)),
        ]
        .into(),
        children: [
            (1, vec![2]),
            (2, vec![3, 5, 6, 8]),
            (3, vec![4]),
            (6, vec![7]),
        ]
        .into(),
        ..Default::default()
    };
    let s = snapshot(
        &mut m,
        &mut AxRegistry::default(),
        &window(),
        &AxSnapshotOptions::default(),
    )
    .unwrap();
    assert_eq!(
        s.text,
        "- window \"Title\" [ref=e1] app=Safari (focused)\n  - splitgroup [ref=e2]\n    - \
			 scrollarea [ref=e3]\n      - button \"Groceries\" [ref=e4]\n    - textfield \"Notes\" \
			 [ref=e5]"
    );
    assert_eq!(s.node_count, 5);
}
#[test]
fn computer_description_labels_unnamed_controls_without_changing_raw_title() {
    let mut reload = p("button", None);
    reload.description = Some("Reload".into());
    reload.actions.push("press".into());
    let mut m = Mock {
        props: [(1, p("window", Some("Title"))), (2, reload)].into(),
        children: [(1, vec![2])].into(),
        ..Default::default()
    };
    let snapshot = snapshot(
        &mut m,
        &mut AxRegistry::default(),
        &window(),
        &AxSnapshotOptions::default(),
    )
    .unwrap();
    assert!(snapshot.text.contains("- button \"Reload\""));

    let nodes = query(
        &mut m,
        &mut AxRegistry::default(),
        &window(),
        &AxQuery {
            role: Some("button".into()),
            title: Some("reload".into()),
            value: None,
            limit: None,
        },
    )
    .unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].title, None);
    assert_eq!(nodes[0].description.as_deref(), Some("Reload"));
}
#[test]
fn computer_truncation_sets_flag_and_trailer() {
    let mut m = Mock {
        props: [(1, p("window", Some("Title"))), (2, p("button", Some("A")))].into(),
        children: [(1, vec![2])].into(),
        ..Default::default()
    };
    let s = snapshot(
        &mut m,
        &mut AxRegistry::default(),
        &window(),
        &AxSnapshotOptions {
            max_nodes: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(s.truncated);
    assert!(s.text.ends_with("… truncated (1 nodes)"));
}
#[test]
fn computer_unreadable_subtree_is_skipped_with_trailer() {
    let mut m = Mock {
        props: [
            (1, p("window", Some("Title"))),
            (3, p("button", Some("Ready"))),
        ]
        .into(),
        children: [(1, vec![2, 3])].into(),
        ..Default::default()
    };
    let s = snapshot(
        &mut m,
        &mut AxRegistry::default(),
        &window(),
        &AxSnapshotOptions::default(),
    )
    .unwrap();
    assert_eq!(
        s.text,
        "- window \"Title\" [ref=e1] app=Safari (focused)\n  - button \"Ready\" [ref=e2]\n… \
			 skipped 1 unreadable nodes"
    );
    assert_eq!(s.node_count, 2);
    let limited = snapshot(
        &mut m,
        &mut AxRegistry::default(),
        &window(),
        &AxSnapshotOptions {
            max_nodes: Some(2),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(limited.truncated);
    assert!(
        limited
            .text
            .ends_with("… truncated (2 nodes)\n… skipped 1 unreadable nodes")
    );
    let nodes = query(
        &mut m,
        &mut AxRegistry::default(),
        &window(),
        &AxQuery {
            role: Some("button".into()),
            title: None,
            value: None,
            limit: None,
        },
    )
    .unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].title.as_deref(), Some("Ready"));
}
#[test]
fn computer_bounds_center_hit_test_is_global_and_frameless() {
    let bounds = AxBounds {
        x: -420.0,
        y: 75.0,
        width: 80.0,
        height: 50.0,
    };
    let mut hit = p("button", Some("Global"));
    hit.bounds = Some(bounds);
    let mut m = Mock {
        props: [(1, p("window", Some("Title"))), (2, hit)].into(),
        ..Default::default()
    };
    let mut registry = AxRegistry::default();
    let node = element_at_node(
        &mut m,
        &mut registry,
        "desktop",
        bounds.x + bounds.width / 2.0,
        bounds.y + bounds.height / 2.0,
    )
    .unwrap()
    .unwrap();
    assert_eq!(node.ref_, "e1");
    assert_eq!(
        (node.x, node.y, node.width, node.height),
        (Some(-420.0), Some(75.0), Some(80.0), Some(50.0))
    );
    assert!(matches!(registry.resolve("e1").unwrap(), AxHandle::Test(2)));
}
#[test]
fn computer_normalization_tables() {
    for (native, role) in [
        ("AXTextArea", "textarea"),
        ("AXTextField", "textfield"),
        ("AXPopUpButton", "popupbutton"),
        ("AXRadioButton", "radio"),
        ("AXCheckBox", "checkbox"),
        ("AXStaticText", "statictext"),
        ("AXScrollArea", "scrollarea"),
        ("AXTabGroup", "tabgroup"),
        ("AXWebArea", "webarea"),
        ("AXRow", "row"),
        ("AXCell", "cell"),
        ("AXOutline", "outline"),
        ("AXButton", "button"),
    ] {
        assert_eq!(normalize_role_macos(native), role);
    }
    for (native, role) in [
        ("Edit", "textfield"),
        ("Document", "textarea"),
        ("Text", "statictext"),
        ("Hyperlink", "link"),
        ("Pane", "group"),
        ("TabItem", "tab"),
        ("Tab", "tabgroup"),
        ("DataItem", "listitem"),
        ("DataGrid", "table"),
        ("SplitButton", "popupbutton"),
        ("Button", "button"),
    ] {
        assert_eq!(normalize_role_uia(native), role);
    }
    for (native, multiline, role) in [
        ("push button", false, "button"),
        ("toggle button", false, "button"),
        ("entry", false, "textfield"),
        ("text", true, "textarea"),
        ("label", false, "statictext"),
        ("page tab", false, "tab"),
        ("page tab list", false, "tabgroup"),
        ("table cell", false, "cell"),
        ("tree", false, "outline"),
        ("tree item", false, "outlineitem"),
        ("frame", false, "window"),
        ("dialog", false, "window"),
        ("list item", false, "listitem"),
    ] {
        assert_eq!(normalize_role_atspi(native, multiline), role);
    }
}
