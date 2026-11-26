//! An empty socket name is unset, not a Wayland (or X11) connection.
use std::ffi::OsStr;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum DisplayServer {
    Wayland,
    X11,
    Missing,
}

impl DisplayServer {
    fn detect(wayland: Option<&OsStr>, x11: Option<&OsStr>) -> Self {
        if wayland.is_some_and(|v| !v.is_empty()) {
            Self::Wayland
        } else if x11.is_some_and(|v| !v.is_empty()) {
            Self::X11
        } else {
            Self::Missing
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn current() -> Self {
        Self::detect(
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            std::env::var_os("DISPLAY").as_deref(),
        )
    }

    #[cfg(any(dial_desktop_linux, test))]
    fn wayland_session(self, session_type: Option<&str>) -> bool {
        self == Self::Wayland || session_type.is_some_and(|s| s.eq_ignore_ascii_case("wayland"))
    }

    /// An XWayland connection does not authorize physical key observation or activation.
    #[cfg(dial_desktop_linux)]
    pub(super) fn in_wayland_session() -> bool {
        Self::current().wayland_session(std::env::var("XDG_SESSION_TYPE").ok().as_deref())
    }
}

#[cfg(test)]
mod tests;
