use super::Fingerprint;

fn fingerprint(text: &str, revision: [u64; 3], inputs: Vec<Option<u8>>) -> Fingerprint<u8> {
    Fingerprint::new(&[text.to_owned()], revision, inputs)
}

#[test]
fn same_diagnosis_with_unchanged_declared_inputs_is_a_stall() {
    let before = fingerprint("test failed: a", [1, 0, 0], vec![Some(7)]);
    let after = fingerprint("test failed: a", [1, 0, 0], vec![Some(7)]);
    assert!(before.repeats(&after));
}

#[test]
fn a_no_op_or_reverted_edit_is_still_a_stall_when_every_input_is_declared() {
    // The effect epoch moved, but the declared inputs read the same bytes afterwards.
    let before = fingerprint("test failed: a", [1, 0, 0], vec![Some(7)]);
    let after = fingerprint("test failed: a", [2, 0, 0], vec![Some(7)]);
    assert!(before.repeats(&after));
}

#[test]
fn a_changed_declared_input_allows_another_round() {
    let before = fingerprint("test failed: a", [1, 0, 0], vec![Some(7)]);
    let after = fingerprint("test failed: a", [2, 0, 0], vec![Some(8)]);
    assert!(!before.repeats(&after));
}

#[test]
fn undeclared_inputs_fall_back_to_the_effect_epoch() {
    let base = fingerprint("test failed: a", [1, 0, 0], vec![None]);
    assert!(base.repeats(&fingerprint("test failed: a", [1, 0, 0], vec![None])));
    assert!(!base.repeats(&fingerprint("test failed: a", [2, 0, 0], vec![None])));
    let no_checks = fingerprint("test failed: a", [1, 0, 0], Vec::new());
    assert!(!no_checks.repeats(&fingerprint("test failed: a", [2, 0, 0], Vec::new())));
}

#[test]
fn a_different_diagnosis_or_check_set_allows_another_round() {
    let base = fingerprint("test failed: a", [1, 0, 0], vec![Some(7)]);
    assert!(!base.repeats(&fingerprint("test failed: b", [1, 0, 0], vec![Some(7)])));
    assert!(!base.repeats(&fingerprint(
        "test failed: a",
        [1, 0, 0],
        vec![Some(7), Some(7)]
    )));
}

#[test]
fn diagnosis_key_is_stable_and_distinguishes_diagnoses() {
    let a = fingerprint("x", [0, 0, 0], vec![Some(1)]);
    let same = fingerprint("x", [5, 5, 5], vec![Some(2)]);
    let b = fingerprint("y", [0, 0, 0], vec![Some(1)]);
    assert_eq!(a.key(), same.key());
    assert_ne!(a.key(), b.key());
    assert_eq!(a.key().len(), 16);
}
