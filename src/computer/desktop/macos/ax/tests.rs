use objc2_core_foundation::CFNumber;

use super::{AttachedCandidate, replace_utf16_selection, select_attached, stringify_value};

#[test]
fn computer_only_missing_attributes_read_as_absent_not_failed() {
    use objc2_application_services::AXError;
    assert!(super::is_absent_attribute(AXError::NoValue));
    assert!(super::is_absent_attribute(AXError::AttributeUnsupported));
    assert!(super::is_absent_attribute(AXError::NotImplemented));
    assert!(!super::is_absent_attribute(AXError::CannotComplete));
    assert!(!super::is_absent_attribute(AXError::InvalidUIElement));
}

#[test]
fn computer_numeric_values_render_as_numbers_at_stored_precision() {
    assert_eq!(stringify_value(&CFNumber::new_i32(1)), "1");
    assert_eq!(stringify_value(&CFNumber::new_i64(-3)), "-3");
    assert_eq!(stringify_value(&CFNumber::new_f64(0.185)), "0.185");
    assert_eq!(stringify_value(&CFNumber::new_f32(0.185)), "0.185");
}

#[test]
fn computer_selected_text_replaces_utf16_selection_without_losing_surrounding_text() {
    assert_eq!(
        replace_utf16_selection("a😀bc", 1, 2, "é").as_deref(),
        Some("aébc")
    );
    assert_eq!(
        replace_utf16_selection("a😀bc", 3, 0, "X").as_deref(),
        Some("a😀Xbc")
    );
    assert_eq!(
        replace_utf16_selection("a😀bc", 0, 5, "").as_deref(),
        Some("")
    );
    assert_eq!(
        replace_utf16_selection("", 0, 0, "hi").as_deref(),
        Some("hi")
    );
}

#[test]
fn computer_invalid_or_surrogate_splitting_selections_never_become_writes() {
    for (start, length) in [(-1, 0), (0, -1), (2, 0), (1, 1), (4, 9), (6, 0)] {
        assert_eq!(replace_utf16_selection("a😀bc", start, length, "X"), None);
    }
}

#[test]
fn computer_sheet_or_popover_window_id_beats_a_menu_at_the_same_frame() {
    let candidates = [
        AttachedCandidate::Menu {
            frame_matches: true,
        },
        AttachedCandidate::OwnWindow(Some(51)),
    ];
    assert_eq!(select_attached(&candidates, 51).unwrap(), Some(1));
}

#[test]
fn computer_open_menu_resolves_by_its_unique_matching_frame() {
    let candidates = [
        AttachedCandidate::OwnWindow(Some(69)),
        AttachedCandidate::Menu {
            frame_matches: false,
        },
        AttachedCandidate::Menu {
            frame_matches: true,
        },
    ];
    assert_eq!(select_attached(&candidates, 57).unwrap(), Some(2));
}

#[test]
fn computer_sheet_reporting_another_window_id_never_inherits_the_target() {
    let candidates = [
        AttachedCandidate::OwnWindow(Some(60)),
        AttachedCandidate::OwnWindow(None),
    ];
    assert_eq!(select_attached(&candidates, 51).unwrap(), None);
}

#[test]
fn computer_ambiguous_menu_frames_are_refused() {
    let candidates = [
        AttachedCandidate::Menu {
            frame_matches: true,
        },
        AttachedCandidate::Menu {
            frame_matches: true,
        },
    ];
    assert!(select_attached(&candidates, 57).is_err());
}
