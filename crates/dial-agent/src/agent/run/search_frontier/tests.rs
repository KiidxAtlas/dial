use super::*;
use serde_json::json;

const EMPTY: &str = "no matches\n[coverage: results=0; scanned_files=3; walk complete]\n";
const FOUND: &str = "src/a.rs:1:fn main\n[coverage: results=1; walk complete]\n";

#[test]
fn empty_searches_name_earlier_overlapping_empty_searches() {
    let mut memory = EmptySearches::default();
    let revision = [0, 0, 0];
    assert_eq!(
        memory.observe(
            "grep",
            &json!({"pattern":"Foo","path":"src/net"}),
            EMPTY,
            false,
            revision
        ),
        None,
        "the first empty search has nothing earlier to name"
    );
    memory.observe(
        "glob",
        &json!({"pattern":"*.md","path":"docs"}),
        EMPTY,
        false,
        revision,
    );
    let note = memory
        .observe(
            "grep",
            &json!({"pattern":"Bar","path":"./src/"}),
            EMPTY,
            false,
            revision,
        )
        .unwrap();
    assert!(note.contains("grep `Foo` in src/net"), "{note}");
    assert!(
        !note.contains("*.md"),
        "disjoint scopes are not named: {note}"
    );
    // A search that found something adds nothing.
    assert_eq!(
        memory.observe(
            "grep",
            &json!({"pattern":"Baz","path":"src"}),
            FOUND,
            false,
            revision
        ),
        None
    );
    // The workspace root overlaps every scope.
    let note = memory
        .observe("grep", &json!({"pattern":"Qux"}), EMPTY, false, revision)
        .unwrap();
    assert!(note.contains("*.md") && note.contains("Bar") && note.contains("Foo"));
}

#[test]
fn workspace_changes_paging_and_errors_do_not_count() {
    let mut memory = EmptySearches::default();
    let args = json!({"pattern":"Foo","path":"src"});
    memory.observe("grep", &args, EMPTY, false, [0, 0, 0]);
    // An edit (or a shell or extension effect) changes the revision and forgets earlier results.
    assert_eq!(memory.observe("grep", &args, EMPTY, false, [1, 0, 0]), None);
    assert_eq!(memory.observe("grep", &args, EMPTY, false, [1, 1, 0]), None);
    let paged = json!({"pattern":"Bar","path":"src","skip":5});
    assert_eq!(
        memory.observe("grep", &paged, EMPTY, false, [1, 1, 0]),
        None
    );
    let remote = json!({"pattern":"Bar","path":"ssh://host/src"});
    assert_eq!(
        memory.observe("grep", &remote, EMPTY, false, [1, 1, 0]),
        None
    );
    assert_eq!(
        memory.observe("grep", &args, "no matches\n", true, [1, 1, 0]),
        None
    );
    // Only the original search is remembered; paged, remote and failed ones were not.
    let note = memory
        .observe("glob", &json!({"pattern":"*.rs"}), EMPTY, false, [1, 1, 0])
        .unwrap();
    assert!(note.contains("grep `Foo` in src") && !note.contains("Bar"));
    // Other tools are ignored.
    assert_eq!(
        memory.observe("read", &json!({"path":"src"}), EMPTY, false, [1, 1, 0]),
        None
    );
}
