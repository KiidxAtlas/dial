use super::*;

#[test]
fn everyday_commands_live_in_visible_sections() {
    for name in EVERYDAY {
        assert!(
            !category(name).advanced(),
            "/{name} is everyday but its section is collapsed"
        );
    }
}

#[test]
fn every_category_has_a_palette_tab() {
    let tabs = [
        "Sessions", "Models", "Changes", "Tasks", "Modes", "Context", "Tools", "Settings",
        "Upkeep", "Learn",
    ];
    for c in Category::ALL {
        assert!(tabs.contains(&c.tab()), "{:?}", c);
    }
}

#[test]
fn titles_and_blurbs_are_present_and_distinct() {
    let mut titles = std::collections::HashSet::new();
    for c in Category::ALL {
        assert!(!c.title().is_empty() && !c.blurb().is_empty());
        assert!(titles.insert(c.title()), "duplicate {}", c.title());
    }
}

#[test]
fn availability_uses_known_context_and_preserves_deferred_commands() {
    let idle = AvailabilityContext {
        git: Some(true),
        busy: false,
        session: true,
    };
    let missing = AvailabilityContext {
        git: Some(false),
        busy: true,
        session: false,
    };
    for (name, args, reason) in [
        ("commit", "", "needs a git repository"),
        ("changes", "", "needs a git repository"),
        ("watch", "", "needs an active session"),
        ("pack", "", "needs an active session"),
        (
            "models",
            "restart",
            "needs an idle session; finish or cancel the current turn",
        ),
    ] {
        assert_eq!(unavailable_reason(name, args, idle), None);
        assert_eq!(unavailable_reason(name, args, missing), Some(reason));
    }
    assert_eq!(
        unavailable_reason("commit", "", AvailabilityContext { git: None, ..idle }),
        None
    );
    assert_eq!(unavailable_reason("review", "files", missing), None);
    assert_eq!(unavailable_reason("model", "", missing), None);
    assert_eq!(unavailable_reason("compact", "", missing), None);
}
