use super::*;

#[test]
fn session_copies_are_session_scoped_and_named_from_the_title() {
    let tmp = tempfile::tempdir().unwrap();
    let path = save(tmp.path(), "session-1", "## A useful plan!\nDo it.").unwrap();
    assert_eq!(
        path,
        tmp.path().join("plans/session-1/a-useful-plan.md"),
        "nested heading marks are not part of the name"
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "## A useful plan!\nDo it."
    );
    assert!(save(tmp.path(), "../escape", "# x").is_err());
    assert_eq!(slug("no heading"), "plan");
    assert_eq!(slug("# ---"), "plan");
}

#[test]
fn autosave_never_overwrites_and_numbers_collisions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let first = autosave(root, "", "# Ship it\nstep one").unwrap().unwrap();
    assert_eq!(first, root.join(".dial/plans/ship-it-plan.md"));
    let second = autosave(root, "", "# Ship it\nstep two").unwrap().unwrap();
    assert_eq!(second, root.join(".dial/plans/ship-it-plan-2.md"));
    assert_eq!(fs::read_to_string(&first).unwrap(), "# Ship it\nstep one");
    assert_eq!(fs::read_to_string(&second).unwrap(), "# Ship it\nstep two");
    assert_eq!(
        autosave(root, "", "  \n").unwrap(),
        None,
        "empty plans are not saved"
    );
}

#[test]
fn autosave_directory_is_workspace_relative_unless_absolute() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("ws");
    let elsewhere = tmp.path().join("elsewhere");
    let relative = autosave(&root, "docs/plans", "# A").unwrap().unwrap();
    assert_eq!(relative, root.join("docs/plans/a-plan.md"));
    let absolute = autosave(&root, elsewhere.to_str().unwrap(), "# A")
        .unwrap()
        .unwrap();
    assert_eq!(absolute, elsewhere.join("a-plan.md"));
    let escape = autosave(&root, "../outside", "# A").unwrap_err();
    assert!(
        escape.to_string().contains("inside the workspace"),
        "{escape}"
    );
    assert!(!tmp.path().join("outside").exists());
}

#[test]
fn approved_record_bounds_the_carried_text_at_a_character_boundary() {
    let small = approved_record("# P\nstep", Some(Path::new("/s/p.md")), None);
    assert_eq!(
        small,
        json!({"text":"# P\nstep","truncated":false,"bytes":8,"saved":"/s/p.md","autosaved":null})
    );
    // A multi-byte character straddles the limit: the cut moves back to its start.
    let plan = format!("{}é tail", "a".repeat(MAX_CARRIED_PLAN_BYTES - 1));
    let large = approved_record(&plan, None, Some(Path::new("/w/.dial/plans/p-plan.md")));
    assert_eq!(large["truncated"], true);
    assert_eq!(large["bytes"], plan.len());
    assert_eq!(
        large["text"].as_str().unwrap().len(),
        MAX_CARRIED_PLAN_BYTES - 1
    );
    assert_eq!(large["autosaved"], "/w/.dial/plans/p-plan.md");
}
