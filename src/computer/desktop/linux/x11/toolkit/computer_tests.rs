use super::*;

#[test]
fn computer_kde_x11_sessions_disable_uinput_hotplug() {
    let unsafe_for = |session, desktop, full, display, wayland| {
        kde_x11_uinput_hotplug_is_unsafe(session, [desktop, None, None], full, display, wayland)
    };
    assert!(unsafe_for(Some("x11"), Some("KDE"), None, None, None));
    assert!(unsafe_for(
        None,
        Some("ubuntu:plasma"),
        None,
        Some(":0"),
        None
    ));
    assert!(unsafe_for(None, None, Some("true"), Some(":0"), None));
    // Explicit X11 wins over a stray WAYLAND_DISPLAY (nested compositor).
    assert!(unsafe_for(
        Some("x11"),
        Some("KDE"),
        None,
        Some(":0"),
        Some("wayland-0")
    ));
    assert!(!unsafe_for(
        Some("wayland"),
        Some("KDE"),
        None,
        Some(":0"),
        None
    ));
    assert!(!unsafe_for(
        None,
        Some("KDE"),
        None,
        Some(":0"),
        Some("wayland-0")
    ));
    assert!(!unsafe_for(
        Some("x11"),
        Some("GNOME"),
        None,
        Some(":0"),
        None
    ));
    assert!(!unsafe_for(None, Some("KDE"), None, None, None));
}
