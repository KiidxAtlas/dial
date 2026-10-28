//! macOS clipboard readers: pasteboard image data first, then Finder file selections.

use super::{check_size, file_url_paths, image_file};
use anyhow::{Context, Result};
use objc2::{rc::Retained, runtime::AnyObject};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSPasteboard,
    NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSDictionary, NSString};

/// Reads image data from the general pasteboard. PNG and JPEG bytes are returned as-is for
/// normalization; TIFF (what macOS screenshots and "Copy Image" often produce) is converted.
pub(super) fn pasteboard_image() -> Result<Option<Vec<u8>>> {
    let pasteboard = NSPasteboard::generalPasteboard();
    // SAFETY: AppKit exports these pasteboard type names as immutable process-wide statics.
    let png = unsafe { NSPasteboardTypePNG };
    // SAFETY: as above.
    let tiff = unsafe { NSPasteboardTypeTIFF };
    let jpeg = NSString::from_str("public.jpeg");
    if let Some(data) = pasteboard.dataForType(png) {
        return data_bytes(&data).map(Some);
    }
    if let Some(data) = pasteboard.dataForType(&jpeg) {
        return data_bytes(&data).map(Some);
    }
    if let Some(data) = pasteboard.dataForType(tiff) {
        return tiff_to_png(&data).map(Some);
    }
    Ok(None)
}

/// Reads the first image file among the pasteboard's file URLs (Finder "Copy" on a file).
pub(super) fn finder_image() -> Result<Option<Vec<u8>>> {
    let pasteboard = NSPasteboard::generalPasteboard();
    let Some(items) = pasteboard.pasteboardItems() else {
        return Ok(None);
    };
    // SAFETY: AppKit exports this pasteboard type name as an immutable process-wide static.
    let file_url = unsafe { NSPasteboardTypeFileURL };
    let urls: Vec<String> = items
        .to_vec()
        .iter()
        .filter_map(|item| item.stringForType(file_url))
        .map(|url| url.to_string())
        .collect();
    for path in file_url_paths(&urls) {
        if let Some(bytes) = image_file(&path)? {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

fn data_bytes(data: &NSData) -> Result<Vec<u8>> {
    check_size(data.len())?;
    Ok(data.to_vec())
}

fn tiff_to_png(tiff: &NSData) -> Result<Vec<u8>> {
    check_size(tiff.len())?;
    let rep = NSBitmapImageRep::imageRepWithData(tiff)
        .context("pasteboard TIFF data is not a readable bitmap")?;
    let properties: Retained<NSDictionary<NSBitmapImageRepPropertyKey, AnyObject>> =
        NSDictionary::new();
    // SAFETY: `rep` is a live bitmap representation and `properties` is a valid (empty)
    // dictionary, as the AppKit method requires.
    let png =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties) }
            .context("pasteboard TIFF could not be converted to PNG")?;
    data_bytes(&png)
}
