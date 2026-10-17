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
