use super::key_window;

#[test]
fn computer_key_window_is_the_accessibility_focused_window_not_the_frontmost() {
    assert_eq!(key_window(&[41, 42, 43], Some(42)), Some(42));
}

#[test]
fn computer_key_window_falls_back_to_the_frontmost_window_without_accessibility() {
    assert_eq!(key_window(&[41, 42], None), Some(41));
    assert_eq!(key_window(&[], None), None);
}

#[test]
fn computer_key_window_marks_nothing_when_the_focused_window_is_not_listed() {
    assert_eq!(key_window(&[41, 42], Some(99)), None);
}
