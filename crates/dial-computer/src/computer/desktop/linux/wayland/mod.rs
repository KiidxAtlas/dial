#[cfg(feature = "wayland-pipewire")]
mod capture;
mod libei;
mod portal;
mod xkb;

use image::RgbaImage;

use crate::computer::desktop::{
    backend::{AxBackend, Backend, DeliveryMode, PointerEvent},
    control::{self, OperationToken},
    error::{CoreResult, DesktopError},
    frame::FrameGeometry,
    keys::KeyName,
    linux::ax::{AtSpiAx, AtSpiWindow},
    types::{
        CaptureCaps, DesktopCapabilities, DesktopDisplay, DesktopWindow, DisplaySelector, Target,
    },
};

/// Logical monitor geometry from the portal ScreenCast stream, paired with the
/// physical PipeWire buffer size.
///
/// The captured buffer is in physical pixels, while libei and AT-SPI address
/// the monitor in the compositor's logical coordinate space. On a scaled
/// monitor the two differ (e.g. a 2560×2880 buffer for a 1280×1440 logical
/// region at scale 2), so clicks must be mapped through the logical geometry
/// instead of treating buffer pixels as logical coordinates.
#[cfg(any(feature = "wayland-pipewire", test))]
#[derive(Debug, Clone, Copy)]
struct PortalGeometry {
    logical_x: i32,
    logical_y: i32,
    logical_width: u32,
    logical_height: u32,
    pixel_width: u32,
    pixel_height: u32,
}

#[cfg(any(feature = "wayland-pipewire", test))]
impl PortalGeometry {
    /// Build from the portal stream's `position`/`size` and the captured buffer
    /// dimensions. A missing or degenerate logical size falls back to the buffer
    /// size (scale 1), preserving behaviour on compositors that omit the
    /// mapping.
    fn new(
        position: Option<(i32, i32)>,
        size: Option<(i32, i32)>,
        pixel_width: u32,
        pixel_height: u32,
    ) -> Self {
        let (logical_x, logical_y) = position.unwrap_or((0, 0));
        let (logical_width, logical_height) = match size {
            Some((w, h)) if w > 0 && h > 0 => (w as u32, h as u32),
            _ => (pixel_width, pixel_height),
        };
        Self {
            logical_x,
            logical_y,
            logical_width,
            logical_height,
            pixel_width,
            pixel_height,
        }
    }

    /// Synthetic single-monitor display describing the captured buffer, with
    /// logical bounds and scale derived from the portal geometry.
    fn display(&self) -> DesktopDisplay {
        let scale = f64::from(self.pixel_width) / f64::from(self.logical_width.max(1));
        DesktopDisplay {
            id: "wayland-portal-0".to_string(),
            name: "Wayland portal monitor".to_string(),
            x: self.logical_x,
            y: self.logical_y,
            width: self.logical_width.max(1),
            height: self.logical_height.max(1),
            scale,
            pixel_x: 0,
            pixel_y: 0,
            pixel_width: self.pixel_width,
            pixel_height: self.pixel_height,
            is_primary: true,
        }
    }

    /// Convert a window's logical bounds (global compositor coordinates) into a
    /// pixel crop rectangle within the captured buffer.
    ///
    /// # Errors
    /// `CaptureFailed` when the toolkit could not report the window's global
    /// position (native Wayland clients answer with window-relative `0,0`, which
    /// would crop whatever sits at the monitor's top-left), or when the window
    /// lies outside the captured monitor.
    fn window_crop(&self, window: &AtSpiWindow) -> CoreResult<(u32, u32, u32, u32)> {
        let AtSpiWindow {
            window,
            position_known,
        } = window;
        if !position_known {
            return Err(DesktopError::capture_failed(format!(
                "Wayland window {} has no known screen position: its toolkit reports window-relative \
				 AT-SPI coordinates, so it cannot be located in the portal capture; capture the \
				 desktop instead",
                window.id
            )));
        }
        let outside = || {
            DesktopError::capture_failed(format!(
                "Wayland window {} is outside the selected portal monitor",
                window.id
            ))
        };
        let scale_x = f64::from(self.pixel_width) / f64::from(self.logical_width.max(1));
        let scale_y = f64::from(self.pixel_height) / f64::from(self.logical_height.max(1));
        let rel_x = window.x.checked_sub(self.logical_x).ok_or_else(outside)?;
        let rel_y = window.y.checked_sub(self.logical_y).ok_or_else(outside)?;
        if rel_x < 0 || rel_y < 0 {
            return Err(outside());
        }
        let px_x = (f64::from(rel_x) * scale_x).round() as u32;
        let px_y = (f64::from(rel_y) * scale_y).round() as u32;
        if px_x >= self.pixel_width || px_y >= self.pixel_height {
            return Err(outside());
        }
        let px_width = (f64::from(window.width) * scale_x).round().max(1.0) as u32;
        let px_height = (f64::from(window.height) * scale_y).round().max(1.0) as u32;
        let width = px_width.min(self.pixel_width - px_x);
        let height = px_height.min(self.pixel_height - px_y);
        if width == 0 || height == 0 {
            return Err(outside());
        }
        Ok((px_x, px_y, width, height))
    }
}

pub struct WaylandBackend {
    #[cfg_attr(
        not(feature = "wayland-pipewire"),
        expect(dead_code, reason = "only read by the pipewire capture path")
    )]
    display: DisplaySelector,
    ax: Option<AtSpiAx>,
    ax_error: Option<DesktopError>,
    input: Option<libei::Libei>,
    displays: Vec<DesktopDisplay>,
}

impl WaylandBackend {
    pub fn new(display: DisplaySelector) -> Self {
        // Remove the world-readable RemoteDesktop restore token that pre-#7884
        // builds wrote during read-only calls; nothing reads it anymore (#7884).
        portal::remove_orphaned_remote_desktop_token();
        let (ax, ax_error) = match AtSpiAx::new() {
            Ok(ax) => (Some(ax), None),
            Err(err) => (None, Some(err)),
        };
        Self {
            display,
            ax,
            ax_error,
            input: None,
            displays: Vec::new(),
        }
    }

    fn window_input_error(target: &Target, kind: &str) -> CoreResult<()> {
        if let Target::Window(id) = target {
            return Err(DesktopError::background_unavailable(format!(
                "window {id} wayland-compositor-focus-only: Wayland cannot programmatically activate \
				 a non-focused window for {kind}; only the currently focused surface is reachable; \
				 use ax actions or desktop input"
            )));
        }
        Ok(())
    }

    fn run_input(
        &mut self,
        target: &Target,
        kind: &str,
        action: impl FnOnce(&mut libei::Libei) -> CoreResult<()>,
    ) -> CoreResult<()> {
        control::check()?;
        Self::window_input_error(target, kind)?;
        if self.input.is_none() {
            self.input = Some(libei::Libei::new()?);
        }
        let input = self.input.as_mut().expect("libei was initialized");
        let result = action(input);
        if input.disconnected() {
            self.input = None;
        }
        result
    }

    fn atspi_windows(&self) -> CoreResult<Vec<AtSpiWindow>> {
        self.ax
            .as_ref()
            .ok_or_else(|| {
                self.ax_error
                    .clone()
                    .unwrap_or_else(DesktopError::ax_unsupported)
            })?
            .windows()
    }
}

impl Backend for WaylandBackend {
    fn capabilities(&mut self) -> DesktopCapabilities {
        let input_permission = if self.input.is_some() {
            "granted"
        } else {
            "prompt-or-granted"
        };
        DesktopCapabilities {
            backend: "wayland".to_string(),
            display_server: Some("wayland".to_string()),
            // The PipeWire screencast path is compiled in only under the
            // wayland-pipewire feature; without it capture() hard-errors, so the
            // capability report must not advertise a capture the binary cannot do.
            capture: cfg!(feature = "wayland-pipewire"),
            input: true,
            ax: self.ax.is_some(),
            background_window_input: false,
            takeover: false,
            applications: crate::computer::desktop::applications::supported(),
            menus: self.ax.is_some(),
            held_input: true,
            spaces: false,
            global_escape: false,
            capture_permission: if cfg!(feature = "wayland-pipewire") {
                "prompt-or-granted".to_string()
            } else {
                "unavailable".to_string()
            },
            input_permission: input_permission.to_string(),
            ax_permission: if self.ax.is_some() {
                "granted".to_string()
            } else {
                "unavailable".to_string()
            },
            display_count: self.displays.len() as u32,
        }
    }

    fn displays(&mut self) -> CoreResult<Vec<DesktopDisplay>> {
        Ok(self.displays.clone())
    }

    fn validate_frame_layout(&mut self, frame: &FrameGeometry) -> CoreResult<()> {
        control::check()?;
        #[cfg(not(feature = "wayland-pipewire"))]
        {
            let _ = frame;
            Err(DesktopError::invalid_coordinate_frame(
                "validating Wayland display geometry requires the wayland-pipewire feature",
            ))
        }
        #[cfg(feature = "wayland-pipewire")]
        {
            // ScreenCast exposes geometry only when opening a stream. Enumeration
            // can use the last snapshot, but coordinate delivery needs fresh
            // bounds.
            let geometry = capture::geometry();
            control::check()?;
            let geometry = geometry.map_err(|error| {
                DesktopError::invalid_coordinate_frame(format!(
                    "could not refresh Wayland display geometry: {error}"
                ))
            })?;
            self.displays = vec![geometry.display()];
            frame.validate_layout(&self.displays)
        }
    }

    fn windows(&mut self) -> CoreResult<Vec<DesktopWindow>> {
        Ok(self
            .atspi_windows()?
            .into_iter()
            .map(|entry| entry.window)
            .collect())
    }

    fn focused_keyboard_window(&mut self) -> CoreResult<DesktopWindow> {
        let entry = self
            .atspi_windows()?
            .into_iter()
            .find(|entry| entry.window.focused)
            .ok_or_else(|| DesktopError::invalid_target("no focused Wayland window was found"))?;
        if !entry.position_known {
            return Err(DesktopError::invalid_target(
                "the focused Wayland surface has no known global position; cannot safely target \
				 display keyboard input",
            ));
        }
        Ok(entry.window)
    }

    fn capture(
        &mut self,
        target: &Target,
        _caps: &CaptureCaps,
        selector: Option<&DisplaySelector>,
    ) -> CoreResult<(RgbaImage, FrameGeometry)> {
        #[cfg(not(feature = "wayland-pipewire"))]
        {
            let _ = (target, selector);
            Err(DesktopError::capture_failed(
                "Wayland capture requires the wayland-pipewire feature",
            ))
        }
        #[cfg(feature = "wayland-pipewire")]
        {
            let (image, geometry) = capture::capture()?;
            self.displays = vec![geometry.display()];

            let explicit = target.display_selector();
            let selected = selector
                .or(explicit.as_ref())
                .unwrap_or(&self.display)
                .select(self.displays.clone(), None)?;
            match target {
                Target::Desktop | Target::Display(_) => {
                    Ok((image, FrameGeometry::for_displays(&selected)))
                }
                Target::Window(id) => {
                    let entry = self
                        .atspi_windows()?
                        .into_iter()
                        .find(|entry| &entry.window.id == id)
                        .ok_or_else(|| {
                            DesktopError::window_not_found(format!("Wayland window {id} not found"))
                        })?;
                    let (x, y, width, height) = geometry.window_crop(&entry)?;
                    let cropped = image::imageops::crop_imm(&image, x, y, width, height).to_image();
                    let frame =
                        FrameGeometry::for_window(&entry.window, cropped.width(), cropped.height());
                    Ok((cropped, frame))
                }
            }
        }
    }

    fn pointer(
        &mut self,
        target: &Target,
        ev: PointerEvent,
        _frame: &FrameGeometry,
        _mode: DeliveryMode,
        token: &OperationToken,
    ) -> CoreResult<()> {
        token.check()?;
        self.run_input(target, "pointer input", |input| input.pointer(ev))
    }

    fn type_text(
        &mut self,
        target: &Target,
        text: &str,
        _mode: DeliveryMode,
        token: &OperationToken,
    ) -> CoreResult<()> {
        token.check()?;
        self.run_input(target, "keyboard input", |input| input.type_text(text))
    }

    fn key_chord(
        &mut self,
        target: &Target,
        keys: &[KeyName],
        _mode: DeliveryMode,
        token: &OperationToken,
    ) -> CoreResult<()> {
        token.check()?;
        self.run_input(target, "keyboard input", |input| input.key_chord(keys))
    }

    fn hold_keys(
        &mut self,
        target: &Target,
        keys: &[KeyName],
        duration: std::time::Duration,
        _mode: DeliveryMode,
        token: &OperationToken,
    ) -> CoreResult<()> {
        token.check()?;
        self.run_input(target, "held keyboard input", |input| {
            input.hold_keys(keys, duration)
        })
    }

    fn menu_items(
        &mut self,
        window: &DesktopWindow,
        path: &[String],
    ) -> CoreResult<Vec<crate::computer::desktop::menus::DesktopMenuItem>> {
        crate::computer::desktop::linux::menus::items(window, path)
    }

    fn menu_select(
        &mut self,
        window: &DesktopWindow,
        path: &[String],
        _mode: DeliveryMode,
        token: &OperationToken,
    ) -> CoreResult<()> {
        token.check()?;
        crate::computer::desktop::linux::menus::select(window, path)
    }

    fn raise_window(&mut self, id: &str, token: &OperationToken) -> CoreResult<()> {
        token.check()?;
        Err(DesktopError::background_unavailable(format!(
            "window {id} wayland-compositor-focus-only: Wayland cannot programmatically activate a \
			 non-focused window; only the currently focused surface is reachable"
        )))
    }

    fn ax(&mut self) -> Option<&mut dyn AxBackend> {
        self.ax.as_mut().map(|ax| ax as &mut dyn AxBackend)
    }
}

#[cfg(test)]
mod tests;
