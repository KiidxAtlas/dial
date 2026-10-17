use super::verified_move;

#[test]
fn computer_ignored_or_partial_native_move_is_not_success() {
    assert!(!verified_move(9, &[3]));
    assert!(!verified_move(9, &[3, 9]));
    assert!(!verified_move(9, &[]));
    assert!(!verified_move(0, &[0]));
    assert!(verified_move(9, &[9]));
}
