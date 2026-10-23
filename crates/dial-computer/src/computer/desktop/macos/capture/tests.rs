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

fn metadata(count: u32) -> impl Iterator<Item = (u32, super::DesktopWindow)> {
    (1..=count).map(|id| {
        (
            id,
            super::DesktopWindow {
                id: id.to_string(),
                title: String::new(),
                app: "fixture".into(),
                pid: Some(1),
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                focused: false,
            },
        )
    })
}

#[test]
fn computer_window_listing_reports_actual_truncation_not_just_a_full_page() {
    for count in [0, 47, 48, 49, 60] {
        let (listing, active) = super::collect_windows(metadata(count), None, Some(1));
        assert_eq!(
            listing.windows.len(),
            (count as usize).min(super::MAX_LISTED_WINDOWS)
        );
        assert_eq!(listing.truncated, count > 48);
        assert_eq!(listing.limit, Some(48));
        assert_eq!(active.len(), listing.windows.len());
    }
}

#[test]
fn computer_explicit_window_lookup_bypasses_discovery_cap_and_rejects_other_ids() {
    let (listing, _) = super::collect_windows(metadata(60), Some(60), None);
    assert_eq!(listing.windows.len(), 1);
    assert_eq!(listing.windows[0].id, "60");
    assert!(!listing.truncated);
    assert_eq!(listing.limit, None);
    let (missing, _) = super::collect_windows(metadata(60), Some(61), None);
    assert!(missing.windows.is_empty());
}
