mod screen_capture_kit;

use std::ptr;

use image::{Rgba, RgbaImage, imageops::FilterType};
use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CFType,
    CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGWindowBounds, kCGWindowIsOnscreen, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID, kCGWindowSharingState,
};
use screen_capture_kit::{CaptureRequest, CaptureTarget};
use xcap::Monitor;

use super::{
    super::{
        error::{CoreResult, DesktopError},
        frame::{FrameGeometry, MAX_COMPOSITE_PIXELS, compose},
        types::{DesktopDisplay, DesktopWindow, DesktopWindowSnapshot, DisplaySelector, Target},
    },
    ax,
};

const MAX_LISTED_WINDOWS: usize = 48;
const MIN_WINDOW_EDGE: u32 = 16;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}

pub(super) fn capture_permission() -> bool {
    // SAFETY: This non-prompting TCC preflight has no arguments and is available
    // on supported macOS versions.
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub(super) fn capture_available() -> bool {
    screen_capture_kit::available()
}

#[derive(Debug, Clone)]
pub(super) struct MacCapture {
    selector: DisplaySelector,
}

impl MacCapture {
    pub(super) const fn new(selector: DisplaySelector) -> Self {
        Self { selector }
    }

    #[allow(
        clippy::unused_self,
        reason = "keeps discovery on the backend capture object"
    )]
    pub(super) fn displays(&self) -> CoreResult<Vec<DesktopDisplay>> {
        if !capture_permission() {
            return Err(DesktopError::permission_denied(
                "macOS Screen Recording permission is not granted for this process",
            ));
        }
        let monitors = Monitor::all().map_err(|error| {
            DesktopError::capture_failed(format!("Quartz monitor enumeration failed: {error}"))
        })?;
        let mut displays = Vec::with_capacity(monitors.len());
        for monitor in monitors {
            let id = monitor.id().map_err(metadata_error)?.to_string();
            let x = monitor.x().map_err(metadata_error)?;
            let y = monitor.y().map_err(metadata_error)?;
            let width = monitor.width().map_err(metadata_error)?;
            let height = monitor.height().map_err(metadata_error)?;
            let scale = f64::from(monitor.scale_factor().map_err(metadata_error)?);
            // xcap's macOS friendly_name path matches CGDirectDisplayID to
            // NSScreenNumber, then returns NSScreen.localizedName(). That binding
            // panics when AppKit returns no name (headless and virtual displays), so a
            // panic there means "no friendly name": keep the model-number fallback.
            let friendly =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| monitor.friendly_name()));
            let name = friendly
                .ok()
                .and_then(Result::ok)
                .or_else(|| monitor.name().ok())
                .unwrap_or_else(|| format!("Display {id}"));
            displays.push(DesktopDisplay {
                id,
                name,
                x,
                y,
                width,
                height,
                scale,
                pixel_x: 0,
                pixel_y: 0,
                pixel_width: scaled_edge(width, scale),
                pixel_height: scaled_edge(height, scale),
                is_primary: monitor.is_primary().map_err(metadata_error)?,
            });
        }
        if displays.is_empty() {
            return Err(DesktopError::capture_failed(
                "Quartz reported no active displays",
            ));
        }
        displays
            .sort_by(|left, right| (left.y, left.x, &left.id).cmp(&(right.y, right.x, &right.id)));
        Ok(displays)
    }

    // Discovery stays on the capture object for backend symmetry; Quartz needs
    // no selector state.
    #[allow(
        clippy::unused_self,
        reason = "keeps discovery on the backend capture object"
    )]
    pub(super) fn windows(&self) -> CoreResult<Vec<DesktopWindow>> {
        Ok(self.window_snapshot()?.windows)
    }

    #[allow(
        clippy::unused_self,
        reason = "keeps discovery on the backend capture object"
    )]
    pub(super) fn window_snapshot(&self) -> CoreResult<DesktopWindowSnapshot> {
        window_snapshot(None)
    }

    #[allow(
        clippy::unused_self,
        reason = "keeps discovery on the backend capture object"
    )]
    pub(super) fn window(&self, id: &str) -> CoreResult<DesktopWindow> {
        let missing = || {
            DesktopError::window_not_found(format!(
                "window '{id}' was not found; it may be closed or minimized"
            ))
        };
        let id = id.parse::<u32>().map_err(|_| missing())?;
        window_snapshot(Some(id))?
            .windows
            .into_iter()
            .next()
            .ok_or_else(missing)
    }

    pub(super) fn capture(
        &self,
        target: &Target,
        selector: Option<&DisplaySelector>,
    ) -> CoreResult<(RgbaImage, FrameGeometry)> {
        match target {
            Target::Desktop | Target::Display(_) => {
                let explicit = target.display_selector();
                self.capture_displays(selector.or(explicit.as_ref()).unwrap_or(&self.selector))
            }
            Target::Window(id) => self.capture_window(id),
        }
    }

    fn capture_window(&self, id: &str) -> CoreResult<(RgbaImage, FrameGeometry)> {
        let mut window = self.window(id)?;
        let window_id = id
            .parse::<u32>()
            .map_err(|_| DesktopError::invalid_target(format!("invalid macOS window id '{id}'")))?;
        let capture = screen_capture_kit::capture(vec![CaptureRequest {
            target: CaptureTarget::Window(window_id),
            bounds: capture_bounds(window.x, window.y, window.width, window.height),
        }])?
        .pop()
        .ok_or_else(|| DesktopError::capture_failed("native window capture returned no image"))?;
        (window.x, window.y, window.width, window.height) = logical_bounds(capture.bounds)?;
        let geometry =
            FrameGeometry::for_window(&window, capture.image.width(), capture.image.height());
        Ok((capture.image, geometry))
    }

    fn capture_displays(
        &self,
        selector: &DisplaySelector,
    ) -> CoreResult<(RgbaImage, FrameGeometry)> {
        let focused = if matches!(selector, DisplaySelector::Active) {
            self.windows()?.into_iter().find(|window| window.focused)
        } else {
            None
        };
        let displays = selector.select(self.displays()?, focused.as_ref())?;
        let requests = displays
            .iter()
            .map(|display| {
                let id = display
                    .id
                    .parse::<u32>()
                    .map_err(|_| DesktopError::invalid_target("invalid macOS display id"))?;
                Ok(CaptureRequest {
                    target: CaptureTarget::Display(id),
                    bounds: capture_bounds(display.x, display.y, display.width, display.height),
                })
            })
            .collect::<CoreResult<Vec<_>>>()?;
        let captures = screen_capture_kit::capture(requests)?;
        let mut regions = Vec::with_capacity(displays.len());
        let mut render_scale = 1.0f64;
        for (mut display, capture) in displays.into_iter().zip(captures) {
            (display.x, display.y, display.width, display.height) = logical_bounds(capture.bounds)?;
            let image = capture.image;
            display.pixel_width = image.width();
            display.pixel_height = image.height();
            display.scale = (f64::from(image.width()) / f64::from(display.width))
                .max(f64::from(image.height()) / f64::from(display.height));
            render_scale = render_scale.max(display.scale);
            regions.push((display, image));
        }
        if regions.len() == 1 {
            let (display, image) = regions.pop().ok_or_else(|| {
                DesktopError::capture_failed("native display capture returned no image")
            })?;
            let geometry = FrameGeometry::for_displays(std::slice::from_ref(&display));
            return Ok((image, geometry));
        }
        let min_x = regions
            .iter()
            .map(|(display, _)| i64::from(display.x))
            .min()
            .unwrap_or(0);
        let min_y = regions
            .iter()
            .map(|(display, _)| i64::from(display.y))
            .min()
            .unwrap_or(0);
        let max_x = regions
            .iter()
            .map(|(display, _)| i64::from(display.x) + i64::from(display.width))
            .max()
            .unwrap_or(0);
        let max_y = regions
            .iter()
            .map(|(display, _)| i64::from(display.y) + i64::from(display.height))
            .max()
            .unwrap_or(0);
        let logical_width = u32::try_from(max_x - min_x)
            .map_err(|_| DesktopError::capture_failed("desktop logical width overflow"))?;
        let logical_height = u32::try_from(max_y - min_y)
            .map_err(|_| DesktopError::capture_failed("desktop logical height overflow"))?;
        let target_width = scaled_edge(logical_width, render_scale).max(1);
        let target_height = scaled_edge(logical_height, render_scale).max(1);
        if u64::from(target_width) * u64::from(target_height) > MAX_COMPOSITE_PIXELS {
            return Err(DesktopError::capture_failed(format!(
                "composite {target_width}x{target_height} exceeds the native safety limit",
            )));
        }
        let mut metadata = Vec::with_capacity(regions.len());
        let composite = compose(
            target_width,
            target_height,
            Rgba([0, 0, 0, 255]),
            regions.into_iter().map(|(mut display, image)| {
                let offset_x = u32::try_from(i64::from(display.x) - min_x)
                    .map_err(|_| DesktopError::capture_failed("display x offset overflow"))?;
                let offset_y = u32::try_from(i64::from(display.y) - min_y)
                    .map_err(|_| DesktopError::capture_failed("display y offset overflow"))?;
                display.pixel_x = scaled_edge(offset_x, render_scale);
                display.pixel_y = scaled_edge(offset_y, render_scale);
                display.pixel_width = scaled_edge(display.width, render_scale).max(1);
                display.pixel_height = scaled_edge(display.height, render_scale).max(1);
                let rendered = if image.width() == display.pixel_width
                    && image.height() == display.pixel_height
                {
                    image
                } else {
                    image::imageops::resize(
                        &image,
                        display.pixel_width,
                        display.pixel_height,
                        FilterType::Triangle,
                    )
                };
                let placement = (rendered, display.pixel_x, display.pixel_y);
                metadata.push(display);
                Ok(placement)
            }),
        )?;
        let geometry = FrameGeometry::for_displays(&metadata);
        Ok((composite, geometry))
    }
}

type WindowDictionary = CFDictionary<CFString, CFType>;

/// Reads each window from one immutable Quartz snapshot; individual xcap
/// property getters would re-enumerate the whole desktop for every field.
fn window_snapshot(target: Option<u32>) -> CoreResult<DesktopWindowSnapshot> {
    if !capture_permission() {
        return Err(DesktopError::permission_denied(
            "macOS Screen Recording permission is not granted for this process",
        ));
    }
    let options = if target.is_some() {
        CGWindowListOption::OptionIncludingWindow
    } else {
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements
    };
    let snapshot = CGWindowListCopyWindowInfo(options, target.unwrap_or(0))
        .ok_or_else(|| DesktopError::capture_failed("Quartz window enumeration failed"))?;
    // SAFETY: CoreGraphics returns an immutable array of dictionaries whose
    // documented window keys are CFStrings and whose values are CFTypes.
    let snapshot = unsafe { CFRetained::cast_unchecked::<CFArray<WindowDictionary>>(snapshot) };
    let active_pid = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|app| u32::try_from(app.processIdentifier()).ok());
    // SAFETY: This copy-rule snapshot remains alive and is never mutated.
    let metadata =
        unsafe { snapshot.iter_unchecked() }.filter_map(|dictionary| window_metadata(dictionary));
    let (mut listing, active) = collect_windows(metadata, target, active_pid);
    let focused = active_pid
        .and_then(|pid| libc::pid_t::try_from(pid).ok())
        .and_then(ax::focused_window_id);
    // A targeted snapshot cannot infer front-to-back ordering of sibling
    // windows when AX cannot identify the application's focused window.
    let focused = if target.is_some() {
        focused
    } else {
        key_window(&active, focused)
    };
    if let Some(focused) = focused {
        let id = focused.to_string();
        if let Some(window) = listing.windows.iter_mut().find(|window| window.id == id) {
            window.focused = true;
        }
    }
    Ok(listing)
}

/// Applies the discovery cap only after metadata safety filtering, and only
/// for enumeration. Keeping this policy independent of Quartz allows fixtures
/// to exercise exact-cap and explicit-ID behavior without desktop access.
fn collect_windows(
    metadata: impl IntoIterator<Item = (u32, DesktopWindow)>,
    target: Option<u32>,
    active_pid: Option<u32>,
) -> (DesktopWindowSnapshot, Vec<u32>) {
    let mut windows = Vec::new();
    let mut active = Vec::new();
    let mut truncated = false;
    for (id, window) in metadata {
        if target.is_some_and(|target| target != id) {
            continue;
        }
        if target.is_none() && windows.len() == MAX_LISTED_WINDOWS {
            truncated = true;
            break;
        }
        if window.pid.is_some() && window.pid == active_pid {
            active.push(id);
        }
        windows.push(window);
    }
    (
        DesktopWindowSnapshot {
            windows,
            truncated,
            limit: target.is_none().then_some(MAX_LISTED_WINDOWS),
        },
        active,
    )
}

fn window_value<'a>(dictionary: &'a WindowDictionary, key: &CFString) -> Option<&'a CFType> {
    // SAFETY: All callers borrow an immutable copy-rule Quartz snapshot.
    unsafe { dictionary.get_unchecked(key) }
}

fn window_number(dictionary: &WindowDictionary, key: &CFString) -> Option<i64> {
    let number = window_value(dictionary, key)?.downcast_ref::<CFNumber>()?;
    let mut value = 0i64;
    // SAFETY: The output is writable storage for the requested signed 64-bit
    // type.
    unsafe { number.value(CFNumberType::SInt64Type, ptr::from_mut(&mut value).cast()) }
        .then_some(value)
}

fn window_string(dictionary: &WindowDictionary, key: &CFString) -> String {
    window_value(dictionary, key)
        .and_then(CFType::downcast_ref::<CFString>)
        .map(ToString::to_string)
        .unwrap_or_default()
}

fn window_metadata(dictionary: &WindowDictionary) -> Option<(u32, DesktopWindow)> {
    // SAFETY: The CoreGraphics key constants are process-lived. Typed
    // downcasts below reject absent or malformed window metadata.
    unsafe {
        let onscreen = window_value(dictionary, kCGWindowIsOnscreen)?
            .downcast_ref::<CFBoolean>()?
            .value();
        if !onscreen || window_number(dictionary, kCGWindowSharingState)? == 0 {
            return None;
        }
        let id = u32::try_from(window_number(dictionary, kCGWindowNumber)?).ok()?;
        let bounds = window_value(dictionary, kCGWindowBounds)?.downcast_ref::<CFDictionary>()?;
        let mut rect = CGRect::default();
        if !CGRectMakeWithDictionaryRepresentation(Some(bounds), &mut rect) {
            return None;
        }
        let (x, y, width, height) = logical_bounds(rect).ok()?;
        if width < MIN_WINDOW_EDGE || height < MIN_WINDOW_EDGE {
            return None;
        }
        let title = window_string(dictionary, kCGWindowName);
        let app = window_string(dictionary, kCGWindowOwnerName);
        if (title.is_empty() && app.is_empty())
            || (title == "StatusIndicator" && app == "Window Server")
        {
            return None;
        }
        let pid =
            window_number(dictionary, kCGWindowOwnerPID).and_then(|pid| u32::try_from(pid).ok());
        Some((
            id,
            DesktopWindow {
                id: id.to_string(),
                title,
                app,
                pid,
                x,
                y,
                width,
                height,
                focused: false,
            },
        ))
    }
}

fn metadata_error(error: impl std::fmt::Display) -> DesktopError {
    DesktopError::capture_failed(format!("failed to read native display metadata: {error}"))
}

fn scaled_edge(value: u32, scale: f64) -> u32 {
    (f64::from(value) * scale)
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

/// The window holding input focus among the active application's listed
/// windows (front to back): its `AXFocusedWindow`, or the frontmost when
/// accessibility cannot name one (for example without the Accessibility
/// permission). A focused window that was not listed marks nothing.
fn key_window(active: &[u32], ax_focused: Option<u32>) -> Option<u32> {
    match ax_focused {
        Some(id) => active.contains(&id).then_some(id),
        None => active.first().copied(),
    }
}

fn capture_bounds(x: i32, y: i32, width: u32, height: u32) -> CGRect {
    CGRect::new(
        CGPoint::new(f64::from(x), f64::from(y)),
        CGSize::new(f64::from(width), f64::from(height)),
    )
}

fn logical_bounds(bounds: CGRect) -> CoreResult<(i32, i32, u32, u32)> {
    let x = bounds.origin.x.round();
    let y = bounds.origin.y.round();
    let width = bounds.size.width.round();
    let height = bounds.size.height.round();
    if !x.is_finite()
        || !y.is_finite()
        || !width.is_finite()
        || !height.is_finite()
        || x < f64::from(i32::MIN)
        || x > f64::from(i32::MAX)
        || y < f64::from(i32::MIN)
        || y > f64::from(i32::MAX)
        || width < 1.0
        || width > f64::from(u32::MAX)
        || height < 1.0
        || height > f64::from(u32::MAX)
    {
        return Err(DesktopError::capture_failed(
            "native capture returned invalid logical bounds",
        ));
    }
    Ok((x as i32, y as i32, width as u32, height as u32))
}

#[cfg(test)]
mod tests;
