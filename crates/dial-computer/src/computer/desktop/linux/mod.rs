pub mod ax;
mod menus;
pub mod wayland;
pub mod x11;

use crate::computer::desktop::{
    backend::Backend,
    error::{CoreResult, DesktopError},
    types::DisplaySelector,
};

pub fn new_backend(display: DisplaySelector) -> CoreResult<Box<dyn Backend>> {
    use crate::computer::linux_display::DisplayServer;
    match DisplayServer::current() {
        DisplayServer::Wayland => Ok(Box::new(wayland::WaylandBackend::new(display))),
        DisplayServer::X11 => Ok(Box::new(x11::X11Backend::new(display)?)),
        DisplayServer::Missing => Err(DesktopError::capture_failed(
            "no display server (neither WAYLAND_DISPLAY nor DISPLAY is set)",
        )),
    }
}
