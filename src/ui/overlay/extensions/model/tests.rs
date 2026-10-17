use super::*;

fn row(id: &str, title: &str, summary: &str, tags: &[&str]) -> Row {
    Row {
        id: id.into(),
        title: title.into(),
        summary: summary.into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        origin: Origin::Curated,
        state: State::Available,
        details: Vec::new(),
    }
}

fn sample() -> Browser {
    let mut b = Browser::new();
    b.set_rows(
        Tab::Mcp,
        vec![
            row(
                "filesystem",
                "Filesystem",
                "Read and write project files",
                &["files"],
            ),
            row("memory", "Memory", "Key-value notes", &["notes"]),
            row("fetch", "Fetch", "Download web pages", &["web"]),
        ],
    );
    b
}

#[test]
fn tabs_wrap_both_ways() {
    let mut b = Browser::new();
    b.prev_tab();
    assert_eq!(b.tab(), Tab::Marketplaces);
    b.next_tab();
    assert_eq!(b.tab(), Tab::Mcp);
}

#[test]
fn catalog_lifecycle_covers_every_tab() {
    let mut b = Browser::new();
    for tab in Tab::ALL {
        assert_eq!(b.catalog(tab), &CatalogState::default());
        assert_eq!(
            b.begin_catalog(tab, String::new(), 1),
            Intent::CatalogBrowse {
                tab,
                query: String::new(),
                page: 1
            }
        );
        assert!(b.catalog(tab).loading);
        b.apply_catalog(CatalogUpdate {
            tab,
            query: String::new(),
            page: 1,
            rows: vec![row(tab.label(), tab.label(), "", &[])],
            more: false,
            error: None,
            message: String::new(),
        });
        assert!(!b.catalog(tab).loading);
        assert_eq!(b.catalog(tab).page, 1);
        assert_eq!(b.rows(tab).len(), 1);
    }
}

#[test]
fn registry_search_results_are_not_filtered_a_second_time() {
    let mut b = sample();
    for c in "nuxt".chars() {
        b.push_query(c);
    }
    let mut remote = row("framework-docs", "Framework docs", "Documentation", &[]);
    remote.origin = Origin::Remote;
    b.apply_catalog(CatalogUpdate {
        tab: Tab::Mcp,
        query: "nuxt".into(),
        page: 1,
        rows: vec![remote.clone()],
        more: false,
        error: None,
        message: String::new(),
    });
    assert_eq!(b.selected(), Some(&remote));
    b.push_query('x');
    assert!(b.visible(Tab::Mcp).is_empty());
    b.pop_query();
    assert_eq!(b.selected(), Some(&remote));
    // A plugin listing is not a server-side search response.
    b.set_rows(Tab::Plugins, vec![remote]);
    assert!(b.visible(Tab::Plugins).is_empty());
    b.clear_search();
    assert_eq!(b.visible(Tab::Mcp).len(), 4);
}

#[test]
fn cursor_clamps_to_visible_rows() {
    let mut b = sample();
    b.move_cursor(10);
    assert_eq!(b.selected().map(|r| r.id.as_str()), Some("fetch"));
    b.move_cursor(-10);
    assert_eq!(b.cursor(), 0);
}

#[test]
fn search_filters_title_summary_and_tags_case_insensitively() {
    let mut b = sample();
    for c in "WEB".chars() {
        b.push_query(c);
    }
    let ids: Vec<&str> = b.visible(Tab::Mcp).iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["fetch"]);
    b.clear_search();
    assert_eq!(b.visible(Tab::Mcp).len(), 3);
}

#[test]
fn match_summary_counts_each_tab_while_searching() {
    let mut b = sample();
    b.set_rows(
        Tab::Plugins,
        vec![row("hello", "Hello plugin", "Greets", &["web"])],
    );
    assert_eq!(b.match_summary(), "");
    for c in "web".chars() {
        b.push_query(c);
    }
    assert_eq!(
        b.match_summary(),
        "MCP servers 1 · Plugins 1 · Skills 0 · Marketplaces 0"
    );
}

#[test]
fn search_keeps_cursor_in_range_when_results_shrink() {
    let mut b = sample();
    b.move_cursor(2);
    b.push_query('n');
    b.push_query('o');
    b.push_query('t');
    assert!(b.cursor() < b.visible(Tab::Mcp).len().max(1));
}

#[test]
fn set_rows_keeps_selection_by_id() {
    let mut b = sample();
    b.move_cursor(1);
    b.set_rows(
        Tab::Mcp,
        vec![
            row("fetch", "Fetch", "", &[]),
            row("memory", "Memory", "", &[]),
        ],
    );
    assert_eq!(b.selected().map(|r| r.id.as_str()), Some("memory"));
}

#[test]
fn installs_wait_for_confirmation_and_declines_clear_it() {
    let mut b = sample();
    let intent = Intent::InstallCuratedMcp {
        id: "memory".into(),
    };
    assert_eq!(b.request(intent.clone()), None);
    assert_eq!(b.confirm.as_ref(), Some(&intent));
    b.decline();
    assert!(b.confirm.is_none());
    b.request(intent.clone());
    assert_eq!(b.accept(), Some(intent));
    assert!(b.confirm.is_none());
}

#[test]
fn enable_runs_at_once_but_disable_and_remove_wait_for_confirmation() {
    let mut b = sample();
    let enable = Intent::ToggleMcp {
        id: "memory".into(),
        enable: true,
    };
    assert_eq!(b.request(enable.clone()), Some(enable));
    assert!(
        b.request(Intent::ToggleMcp {
            id: "memory".into(),
            enable: false,
        })
        .is_none()
    );
    assert!(b.confirm.is_some());
    b.decline();
    assert!(
        b.request(Intent::RemoveMcp {
            id: "memory".into(),
        })
        .is_none()
    );
    assert!(b.confirm.is_some());
}

#[test]
fn switching_tabs_drops_pending_confirm() {
    let mut b = sample();
    b.request(Intent::InstallCuratedMcp {
        id: "memory".into(),
    });
    b.next_tab();
    assert!(b.confirm.is_none());
}

#[test]
fn intent_round_trips_through_json_for_the_action_channel() {
    let intent = Intent::InstallCuratedMcp {
        id: "memory".into(),
    };
    let text = serde_json::to_string(&intent).unwrap();
    assert_eq!(text, r#"{"op":"install_curated_mcp","id":"memory"}"#);
    let back: Intent = serde_json::from_str(&text).unwrap();
    assert_eq!(back, intent);
}

#[test]
fn job_progress_goes_running_then_done() {
    let mut b = Browser::new();
    b.start_job("Installing memory");
    b.push_job_line("npm fetched");
    assert!(b.job_running());
    b.finish_job(true, "Connected with 9 tools");
    assert_eq!(
        b.job,
        Job::Done {
            label: "Installing memory".into(),
            ok: true,
            message: "Connected with 9 tools".into(),
        }
    );
}

#[test]
fn marketplace_and_skill_changes_wait_for_confirmation() {
    let mut b = Browser::new();
    for intent in [
        Intent::RemoveMarketplace { name: "x".into() },
        Intent::ToggleMarketplace {
            name: "x".into(),
            enable: false,
        },
        Intent::InstallSkill {
            source: "https://example.com/org/skills.git#pdf".into(),
        },
    ] {
        assert!(b.request(intent).is_none());
        b.decline();
    }
    // Enabling a marketplace is reversible and runs at once.
    assert!(
        b.request(Intent::ToggleMarketplace {
            name: "x".into(),
            enable: true,
        })
        .is_some()
    );
}
