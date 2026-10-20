use super::super::tests::Fixture;
use super::*;

/// Mimic one tool call: observe, write, then the agent settles after the call.
fn change(cp: &Checkpoints, fx: &Fixture, rel: &str, text: &str) {
    cp.before_write(&fx.path(rel)).unwrap();
    fx.write(rel, text);
    cp.settle_changes();
}

fn digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

#[test]
fn plans_the_latest_change_and_the_turn_start() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    let cp = fx.open();
    cp.begin_turn("t").unwrap();
    change(&cp, &fx, "a.txt", "A1");
    change(&cp, &fx, "a.txt", "A2");

    let latest = cp.revert_plan(&fx.path("a.txt"), false).unwrap();
    assert_eq!(latest.shown, "a.txt");
    assert_eq!(latest.content.as_deref(), Some(&b"A1"[..]));
    assert_eq!(latest.current, Some(digest("A2")));
    let start = cp.revert_plan(&fx.path("a.txt"), true).unwrap();
    assert_eq!(start.content.as_deref(), Some(&b"A0"[..]));
}

#[test]
fn a_call_that_writes_twice_is_one_revision_and_a_no_op_keeps_the_previous_one() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    let cp = fx.open();
    cp.begin_turn("t").unwrap();
    change(&cp, &fx, "a.txt", "A1");
    // One call observing twice: the revision keeps the state before the call.
    cp.before_write(&fx.path("a.txt")).unwrap();
    fx.write("a.txt", "mid");
    cp.before_write(&fx.path("a.txt")).unwrap();
    fx.write("a.txt", "A2");
    cp.settle_changes();
    assert_eq!(
        cp.revert_plan(&fx.path("a.txt"), false)
            .unwrap()
            .content
            .as_deref(),
        Some(&b"A1"[..])
    );
    // An observed write that committed nothing does not replace the latest change.
    cp.before_write(&fx.path("a.txt")).unwrap();
    cp.settle_changes();
    assert_eq!(
        cp.revert_plan(&fx.path("a.txt"), false)
            .unwrap()
            .content
            .as_deref(),
        Some(&b"A1"[..])
    );
}

#[test]
fn refuses_outside_changes_unjournaled_files_and_identical_targets() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    fx.write("b.txt", "B0");
    let cp = fx.open();
    cp.begin_turn("t").unwrap();
    let error = cp.revert_plan(&fx.path("a.txt"), false).unwrap_err();
    assert!(
        format!("{error:#}").contains("no checkpointed change"),
        "{error:#}"
    );
    change(&cp, &fx, "a.txt", "A1");
    fx.write("a.txt", "outside");
    let error = cp.revert_plan(&fx.path("a.txt"), false).unwrap_err();
    assert!(format!("{error:#}").contains("outside"), "{error:#}");
    assert_eq!(fx.read("a.txt").as_deref(), Some("outside"));

    change(&cp, &fx, "b.txt", "B1");
    change(&cp, &fx, "b.txt", "B0");
    let error = cp.revert_plan(&fx.path("b.txt"), true).unwrap_err();
    assert!(format!("{error:#}").contains("already has"), "{error:#}");
    // A new turn starts an empty journal.
    cp.begin_turn("next").unwrap();
    assert!(cp.revert_plan(&fx.path("b.txt"), false).is_err());
}

#[test]
fn created_files_plan_deletion_and_the_revert_itself_is_undoable() {
    let fx = Fixture::new();
    fx.write("a.txt", "A0");
    let cp = fx.open();
    cp.begin_turn("t").unwrap();
    change(&cp, &fx, "new.txt", "N1");
    let created = cp.revert_plan(&fx.path("new.txt"), false).unwrap();
    assert_eq!(created.content, None);
    assert_eq!(created.current, Some(digest("N1")));

    change(&cp, &fx, "a.txt", "A1");
    let plan = cp.revert_plan(&fx.path("a.txt"), false).unwrap();
    // The revert commits through the observer like any edit.
    change(
        &cp,
        &fx,
        "a.txt",
        std::str::from_utf8(plan.content.as_deref().unwrap()).unwrap(),
    );
    assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
    // Reverting again restores the reverted change.
    let again = cp.revert_plan(&fx.path("a.txt"), false).unwrap();
    assert_eq!(again.content.as_deref(), Some(&b"A1"[..]));
    // /undo still restores the whole turn and removes its saved revisions.
    cp.rollback(1).unwrap();
    assert_eq!(fx.read("a.txt").as_deref(), Some("A0"));
    assert!(!fx.path("new.txt").exists());
    assert!(cp.revert_plan(&fx.path("a.txt"), false).is_err());
}
