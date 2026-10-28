use super::*;
use crate::tools::integrations::mcp::tests::{config_with, fake_config};
use crate::tools::integrations::mcp::{McpLoad, load};

fn remote(name: &str, version: u64) -> RemoteTool {
    RemoteTool {
        name: name.to_owned(),
        description: format!("tool {name}"),
        input_schema: json!({"type": "object", "version": version}),
        read_only_hint: false,
    }
}

fn connected() -> McpLoad {
    let loaded = load(&config_with(vec![("fake", fake_config("", 30))]));
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    loaded
}

/// Applies a sync the way the registry does: removals first, then upserts.
fn apply(registry: &mut BTreeMap<String, Arc<dyn Tool>>, change: Change) {
    for name in change.removed {
        registry.remove(&name);
    }
    for (name, tool) in change.upserted {
        registry.insert(name, tool);
    }
}

fn fake_names(registry: &BTreeMap<String, Arc<dyn Tool>>) -> BTreeSet<String> {
    registry
        .keys()
        .filter(|name| name.starts_with("mcp__fake__"))
        .cloned()
        .collect()
}

#[test]
fn diff_reports_added_removed_and_redefined_names() {
    let old = vec![remote("a", 1), remote("b", 1), remote("c", 1)];
    let new = vec![remote("b", 1), remote("c", 2), remote("d", 1)];
    assert_eq!(
        diff(&old, &new),
        CatalogDiff {
            added: vec!["d".into()],
            removed: vec!["a".into()],
            changed: vec!["c".into()],
        }
    );
    assert!(diff(&old, &old).is_empty());
}

#[test]
fn added_removed_and_redefined_tools_reach_the_next_sync() {
    let mut loaded = connected();
    let session = loaded.sessions[0].clone();
    let mut registry = BTreeMap::new();

    session.apply_catalog(vec![
        remote("keep", 1),
        remote("drop", 1),
        remote("edit", 1),
    ]);
    apply(&mut registry, loaded.live.sync());
    assert_eq!(
        fake_names(&registry),
        ["mcp__fake__drop", "mcp__fake__edit", "mcp__fake__keep"]
            .map(str::to_owned)
            .into_iter()
            .collect()
    );
    let kept = registry["mcp__fake__keep"].clone();
    let edited = registry["mcp__fake__edit"].clone();

    session.apply_catalog(vec![remote("keep", 1), remote("edit", 2), remote("new", 1)]);
    apply(&mut registry, loaded.live.sync());
    assert_eq!(
        fake_names(&registry),
        ["mcp__fake__edit", "mcp__fake__keep", "mcp__fake__new"]
            .map(str::to_owned)
            .into_iter()
            .collect()
    );
    assert!(
        Arc::ptr_eq(&kept, &registry["mcp__fake__keep"]),
        "an unchanged tool keeps its registered instance"
    );
    assert!(
        !Arc::ptr_eq(&edited, &registry["mcp__fake__edit"]),
        "a redefined tool is registered anew"
    );
    assert!(loaded.live.is_retired("mcp__fake__drop"));
    assert!(
        !session.offers(&remote("edit", 1)),
        "old definition is withdrawn"
    );
    assert!(session.offers(&remote("edit", 2)));

    assert!(
        loaded.live.sync().upserted.is_empty(),
        "an unchanged catalog syncs to nothing"
    );

    session.apply_catalog(vec![remote("drop", 1)]);
    apply(&mut registry, loaded.live.sync());
    assert!(
        !loaded.live.is_retired("mcp__fake__drop"),
        "a returning tool is no longer retired"
    );
}

#[test]
fn a_refresh_applies_the_servers_current_list_and_reports_the_diff() {
    let mut loaded = connected();
    let session = loaded.sessions[0].clone();
    let mut registry = BTreeMap::new();
    session.apply_catalog(vec![remote("only", 1)]);
    apply(&mut registry, loaded.live.sync());
    assert!(loaded.live.take_events().is_empty());

    session.refresh_tools();

    let events = loaded.live.take_events();
    assert!(
        events
            .iter()
            .any(|event| event.kind == McpEventKind::ToolsChanged),
        "{events:?}"
    );
    let refreshed = events.iter().find_map(|event| match &event.kind {
        McpEventKind::ToolsRefreshed { added, removed, .. } => {
            Some((added.clone(), removed.clone()))
        }
        _ => None,
    });
    let (added, removed) = refreshed.expect("the diff is reported");
    assert_eq!(removed, vec!["only".to_owned()]);
    assert!(added.iter().any(|name| name == "echo"), "{added:?}");
    assert!(!session.offers(&remote("only", 1)));

    apply(&mut registry, loaded.live.sync());
    assert!(!registry.contains_key("mcp__fake__only"));
    assert!(registry.contains_key("mcp__fake__echo"));

    session.refresh_tools();
    assert_eq!(
        loaded.live.take_events(),
        vec![McpEvent {
            server: "fake".into(),
            kind: McpEventKind::ToolsChanged,
        }],
        "an unchanged list only announces the refresh"
    );
}

#[test]
fn events_and_notices_are_bounded_and_drain_once() {
    let loaded = connected();
    let session = loaded.sessions[0].clone();
    for _ in 0..EVENT_QUEUE_LIMIT + 5 {
        session.push_event(McpEventKind::PromptsChanged);
    }
    assert_eq!(loaded.live.take_events().len(), EVENT_QUEUE_LIMIT);
    assert!(loaded.live.take_events().is_empty());

    for index in 0..NOTICE_LIMIT + 3 {
        session.push_notice(format!("notice {index}"));
    }
    let mut live = loaded.live.clone();
    let notices = live.sync().notices;
    assert_eq!(notices.len(), NOTICE_LIMIT);
    assert_eq!(notices.last().map(String::as_str), Some("notice 18"));
}

#[test]
fn event_payloads_name_the_server_and_kind() {
    let event = McpEvent {
        server: "fake".into(),
        kind: McpEventKind::ToolsRefreshed {
            added: vec!["x".into()],
            removed: vec![],
            changed: vec!["y".into()],
        },
    };
    assert_eq!(
        event.to_json(),
        json!({
            "server": "fake",
            "kind": "tools_refreshed",
            "added": ["x"],
            "removed": [],
            "changed": ["y"],
        })
    );
    assert_eq!(
        McpEvent {
            server: "fake".into(),
            kind: McpEventKind::ToolsRefreshFailed {
                error: "boom".into()
            },
        }
        .to_json(),
        json!({"server": "fake", "kind": "tools_refresh_failed", "error": "boom"})
    );
}

/// Polls `done` until it holds, failing the test rather than hanging when a refresh never lands.
fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the MCP tool refresh"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn catalog_names(session: &Session) -> Vec<String> {
    session
        .catalog_snapshot()
        .1
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

#[test]
fn a_burst_of_list_changed_signals_refreshes_once() {
    let loaded = load(&config_with(vec![(
        "fake",
        fake_config("announce-burst,list-counts", 30),
    )]));
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let session = loaded.sessions[0].clone();
    wait_until(|| catalog_names(&session).contains(&"seen-2".to_owned()));
    // The server announced three changes; the debounce must not turn them into a third listing.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let names = catalog_names(&session);
    assert!(names.contains(&"seen-2".to_owned()), "{names:?}");
    assert!(!names.contains(&"seen-3".to_owned()), "{names:?}");
}

#[test]
fn a_failed_refresh_keeps_the_previous_tools_and_reports_a_notice() {
    let mut loaded = load(&config_with(vec![(
        "fake",
        fake_config("announce-burst,list-fails-after-first", 30),
    )]));
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
    let session = loaded.sessions[0].clone();
    let mut notices = Vec::new();
    wait_until(|| {
        notices.extend(loaded.live.sync().notices);
        notices
            .iter()
            .any(|notice| notice.contains("tools/list refresh failed"))
    });
    assert!(!session.catalog_current(), "a failed refresh is stale");
    let names = catalog_names(&session);
    assert!(names.contains(&"echo".to_owned()), "{names:?}");
}
