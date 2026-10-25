use super::*;

#[test]
fn failing_test_names_are_parsed_from_common_runners() {
    let text = "test parser::tests::empty ... FAILED\ntest parser::tests::ok ... ok\nFAILED tests/test_x.py::test_y - AssertionError\n--- FAIL: TestThing (0.00s)\n  ✕ renders the header (5 ms)\n";
    assert_eq!(
        failing_tests(text),
        BTreeSet::from([
            "parser::tests::empty".to_owned(),
            "tests/test_x.py::test_y".to_owned(),
            "TestThing".to_owned(),
            "renders the header (5 ms)".to_owned(),
        ])
    );
    assert!(failing_tests("all good").is_empty());
}

#[test]
fn notes_after_the_configured_flat_rounds_and_resets_on_improvement() {
    let mut off = Gradient::default();
    assert!((0..10).all(|_| off.observe(3, 0).is_none()), "0 disables");

    let mut gradient = Gradient::default();
    assert!(
        gradient.observe(5, 2).is_none(),
        "first score sets the baseline"
    );
    assert!(gradient.observe(5, 2).is_none());
    let note = gradient.observe(6, 2).expect("two flat rounds");
    assert!(
        note.contains("stayed at 6") && note.contains("best of 5"),
        "{note}"
    );
    assert!(
        gradient.observe(5, 2).is_none(),
        "the count restarts after a note"
    );
    assert!(gradient.observe(4, 2).is_none(), "improvement");
    assert!(gradient.observe(4, 2).is_none());
    assert!(gradient.observe(4, 2).is_some());

    let mut solved = Gradient::default();
    assert!(solved.observe(0, 1).is_none());
    assert!(
        solved.observe(0, 1).is_none(),
        "nothing failing is never a stall"
    );
}
