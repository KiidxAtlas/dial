use super::*;
#[test]
fn computer_only_physical_escape_cancels() {
    assert!(physical_escape(KEY_DOWN, 53, 0, 0));
    assert!(!physical_escape(KEY_DOWN, 53, 0, SYNTHETIC_EVENT_TAG));
    assert!(!physical_escape(KEY_DOWN, 53, 42, 0));
    assert!(!physical_escape(KEY_DOWN, 0, 0, 0));
    assert!(!physical_escape(11, 53, 0, 0));
}
