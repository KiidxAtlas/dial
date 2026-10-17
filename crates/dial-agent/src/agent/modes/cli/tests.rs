use super::*;

#[test]
fn tools_allowlist_keeps_finish_and_drops_others() {
    let launch = Launch {
        tools: Some(["read".to_string()].into_iter().collect()),
        ..Launch::default()
    };
    assert!(launch.allows_tool("read"));
    assert!(launch.allows_tool(FINISH));
    assert!(!launch.allows_tool("bash"));
    assert!(!launch.allows_tool("find"));
}

#[test]
fn empty_skill_allowlist_withholds_skill_tools() {
    let none = Launch {
        skills: Some(BTreeSet::new()),
        ..Launch::default()
    };
    assert!(!none.allows_tool("skill"));
    assert!(!none.allows_tool("skill_manage"));
    assert!(none.allows_tool("bash"));
    assert!(!none.skill_visible("review"));
}

#[test]
fn named_skill_allowlist_filters_catalog_and_keeps_tools() {
    let named = Launch {
        skills: Some(["review".to_string()].into_iter().collect()),
        ..Launch::default()
    };
    assert!(named.allows_tool("skill"));
    assert!(named.skill_visible("review"));
    assert!(!named.skill_visible("deploy"));
    assert!(Launch::default().skill_visible("anything"));
}

#[test]
fn deadline_expires_only_after_it_passes() {
    let past = Launch {
        deadline: Some(Instant::now()),
        ..Launch::default()
    };
    assert!(past.expired());
    let future = Launch {
        deadline: Some(Instant::now() + std::time::Duration::from_secs(60)),
        ..Launch::default()
    };
    assert!(!future.expired());
    assert!(!Launch::default().expired());
}
