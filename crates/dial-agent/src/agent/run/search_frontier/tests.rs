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

#[test]
fn declaration_recovery_is_specific_once_and_policy_aware() {
    for single_call in [false, true] {
        let mut memory = EmptySearches::default();
        let revision = [0, 0, 0];
        let first = json!({"pattern":"name:","path":"src/registry.rs"});
        memory.observe("grep", &first, EMPTY, false, revision);
        assert!(
            memory.advice(single_call).is_none(),
            "one absence check is legitimate"
        );
        memory.observe("grep", &first, EMPTY, false, revision);
        assert!(
            memory.advice(single_call).is_none(),
            "an identical retry is not a representation guess"
        );
        memory.observe(
            "grep",
            &json!({"pattern":"Spec::new", "path":"src/registry.rs"}),
            EMPTY,
            false,
            revision,
        );
        let note = memory
            .advice(single_call)
            .expect("distinct empty probes should suggest a declaration read");
        assert!(
            note.contains("src/registry.rs") && note.contains("intentional absence checks"),
            "{note}"
        );
        assert_eq!(note.contains("same response"), !single_call, "{note}");
        assert_eq!(note.contains("one tool call"), single_call, "{note}");
        assert!(
            memory.advice(!single_call).is_some(),
            "rendering is not delivery"
        );
        memory.delivered_advice(&crate::agent::run::guidance::note(
            crate::agent::run::guidance::Priority::Loop,
            note,
        ));
        memory.observe(
            "grep",
            &json!({"pattern":"CommandSpec", "path":"src/registry.rs"}),
            EMPTY,
            false,
            revision,
        );
        assert!(
            memory.advice(single_call).is_none(),
            "do not repeat the correction on every result"
        );
        memory.observe("read", &json!({"path":"README.md"}), FOUND, false, revision);
        memory.observe("grep", &first, EMPTY, false, revision);
        assert!(
            memory.advice(single_call).is_none(),
            "an unrelated read does not rearm advice"
        );
        memory.observe(
            "read",
            &json!({"paths":["src/registry.rs:1-40"]}),
            FOUND,
            false,
            revision,
        );
        memory.observe("grep", &first, EMPTY, false, revision);
        assert!(
            memory.advice(single_call).is_none(),
            "reading the owner retires earlier guesses"
        );
    }
}

#[test]
fn declaration_recovery_ignores_broad_absence_checks_and_stale_evidence() {
    let mut memory = EmptySearches::default();
    for pattern in ["old_api", "deprecated"] {
        memory.observe(
            "grep",
            &json!({"pattern":pattern,"path":"src"}),
            EMPTY,
            false,
            [0, 0, 0],
        );
    }
    assert!(memory.advice(false).is_none());
    for pattern in ["name:", "Spec::new"] {
        memory.observe(
            "grep",
            &json!({"pattern":pattern,"path":"src/registry.rs"}),
            EMPTY,
            false,
            [0, 0, 0],
        );
    }
    memory.observe(
        "grep",
        &json!({"pattern":"other", "path":"src/registry.rs"}),
        EMPTY,
        false,
        [1, 0, 0],
    );
    assert!(
        memory.advice(false).is_none(),
        "workspace changes invalidate pending advice"
    );
}

#[test]
fn shell_count_recovery_requires_attributable_zero_not_a_failed_script() {
    let mut memory = EmptySearches::default();
    for pattern in ["name:", "CommandSpec"] {
        memory.observe(
            "bash",
            &json!({"command":format!("grep -c '{pattern}' registry.rs"), "cwd":"src"}),
            "0\n[exit code: 1]\n",
            true,
            [0, 0, 0],
        );
    }
    let note = memory
        .advice(false)
        .expect("two explicit empty grep counts");
    assert!(note.contains("src/registry.rs"), "{note}");
    for command in [
        "grep -c name: registry.rs; echo 0",
        "grep -c name: registry.rs | wc -l",
        "grep -c name: other.rs",
        "grep -v name: registry.rs",
    ] {
        let mut memory = EmptySearches::default();
        memory.observe(
            "bash",
            &json!({"command":command}),
            "0\n[exit code: 2]\n",
            true,
            [0, 0, 0],
        );
        memory.observe(
            "bash",
            &json!({"command":"grep -c other registry.rs"}),
            "0\n[exit code: 1]\n",
            true,
            [0, 0, 0],
        );
        assert!(memory.advice(false).is_none(), "{command}");
    }
    assert!(
        empty_shell_search(
            &json!({"command":"grep -c name: registry.rs"}),
            "grep: registry.rs: No such file\n0\n[exit code: 1]\n"
        )
        .is_none()
    );
}

#[test]
fn pending_targets_survive_arbitration_and_owner_reads_use_path_identity() {
    let mut memory = EmptySearches::default();
    for file in ["first.rs", "second.rs"] {
        for pattern in ["name:", "Spec::new"] {
            memory.observe(
                "grep",
                &json!({"pattern":pattern,"path":file,"glob":"*.rs"}),
                EMPTY,
                false,
                [0, 0, 0],
            );
        }
    }
    let note = memory.advice(false).unwrap();
    assert!(note.contains("first.rs"));
    // A higher-priority note can win; observing/rendering never consumes the target.
    let mut messages = vec![
        crate::agent::run::guidance::note(
            crate::agent::run::guidance::Priority::Loop,
            note.clone(),
        ),
        crate::agent::run::guidance::note(
            crate::agent::run::guidance::Priority::Failure,
            "Resolve the failed prerequisite.",
        ),
    ];
    crate::agent::run::guidance::assemble(&mut messages, 0, &mut Default::default());
    assert!(!json!(messages).to_string().contains("Discovery recovery"));
    assert!(memory.advice(true).unwrap().contains("one tool call"));
    memory.delivered_advice(&crate::agent::run::guidance::note(
        crate::agent::run::guidance::Priority::Loop,
        note,
    ));
    assert!(
        memory.advice(false).unwrap().contains("second.rs"),
        "acknowledgement must not lose another target"
    );
    memory.observe(
        "read",
        &json!({"path":"second.rs:1-40"}),
        FOUND,
        false,
        [0, 0, 0],
    );
    assert!(
        memory.advice(false).is_none(),
        "a glob display suffix must not keep stale advice"
    );
}
