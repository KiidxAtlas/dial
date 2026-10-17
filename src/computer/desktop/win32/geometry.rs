//! Win32 uses physical desktop pixels end-to-end. Monitor DPI scale is
//! metadata, not a transform of the global coordinate origin or capture atlas.

use super::super::{
    error::{CoreResult, DesktopError},
    frame::MAX_COMPOSITE_PIXELS,
    types::DesktopDisplay,
};

#[derive(Debug)]
pub(super) struct PhysicalLayout {
    left: i32,
    top: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl PhysicalLayout {
    pub(super) fn new<'a>(displays: impl Iterator<Item = &'a DesktopDisplay>) -> CoreResult<Self> {
        let mut left = i32::MAX;
        let mut top = i32::MAX;
        let mut right = i64::MIN;
        let mut bottom = i64::MIN;
        for display in displays {
            if display.width == 0 || display.height == 0 {
                return Err(DesktopError::capture_failed(
                    "Win32 returned an empty display rectangle",
                ));
            }
            left = left.min(display.x);
            top = top.min(display.y);
            right = right.max(i64::from(display.x) + i64::from(display.width));
            bottom = bottom.max(i64::from(display.y) + i64::from(display.height));
        }
        if right == i64::MIN {
            return Err(DesktopError::capture_failed(
                "Win32 reported no active displays",
            ));
        }
        let width = u32::try_from(right - i64::from(left)).map_err(|_| {
            DesktopError::capture_failed("Win32 desktop width exceeds the native range")
        })?;
        let height = u32::try_from(bottom - i64::from(top)).map_err(|_| {
            DesktopError::capture_failed("Win32 desktop height exceeds the native range")
        })?;
        if u64::from(width) * u64::from(height) > MAX_COMPOSITE_PIXELS {
            return Err(DesktopError::capture_failed(format!(
                "Win32 composite {width}x{height} exceeds the native safety limit"
            )));
        }
        Ok(Self {
            left,
            top,
            width,
            height,
        })
    }

    /// Places a display from the same enumeration in the normalized atlas.
    pub(super) fn place(&self, display: &mut DesktopDisplay) {
        // Layout construction validated the complete bounding box before any
        // image allocation. Subtract in i64 to support negative desktop origins.
        display.pixel_x = (i64::from(display.x) - i64::from(self.left)) as u32;
        display.pixel_y = (i64::from(display.y) - i64::from(self.top)) as u32;
        display.pixel_width = display.width;
        display.pixel_height = display.height;
    }
}

#[cfg(test)]
mod tests;
