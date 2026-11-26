use super::*;

#[test]
fn display_routing_and_physical_key_observation_distinguish_empty_socket_names() {
    use DisplayServer::{Missing, Wayland, X11};
    for (wayland, x11, session, server, is_wayland) in [
        (Some(""), Some(":0"), Some("x11"), X11, false),
        (None, Some(":0"), None, X11, false),
        (Some(""), Some(":0"), Some("Wayland"), X11, true),
        (Some("wayland-1"), Some(":0"), None, Wayland, true),
        (Some(""), Some(""), None, Missing, false),
    ] {
        let selected = DisplayServer::detect(wayland.map(OsStr::new), x11.map(OsStr::new));
        assert_eq!(selected, server);
        assert_eq!(selected.wayland_session(session), is_wayland);
    }
}
