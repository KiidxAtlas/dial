//! Image previews for binary changes in the git overlay. Only raster formats the terminal graphics
//! protocols can draw are previewed; SVG is text and keeps the binary placeholder.

use super::model::{Change, Previews};
use crate::git_panel::Repo;
use crate::ui::images::{ImageProtocol, Thumbnail};

/// Whether `path` names a raster image format the git overlay previews (case-insensitive).
pub(super) fn is_image_path(path: &str) -> bool {
    let Some((_, ext)) = path.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
    )
}

/// The protocol used to draw a native preview, or None when the text placeholder must be shown instead.
/// A native preview needs a terminal that speaks a graphics protocol and an image that decoded.
pub(super) fn native_protocol(
    protocol: Option<ImageProtocol>,
    decoded: bool,
) -> Option<ImageProtocol> {
    if decoded { protocol } else { None }
}

/// Decodes both sides of an image change for the terminal's graphics protocol. Returns None when the
/// file is not a previewable image, the terminal has no protocol, or neither side decodes.
pub(super) fn load_previews(
    repo: &Repo,
    change: &Change,
    rev: Option<&str>,
    protocol: Option<ImageProtocol>,
) -> Option<Previews> {
    // Without a graphics protocol there is nothing to draw, so skip reading and decoding.
    let protocol = protocol?;
    if !is_image_path(&change.path) {
        return None;
    }
    let (before, after) = repo.image_sides(change, rev)?;
    let decode = |bytes: Option<Vec<u8>>| {
        bytes.and_then(|bytes| Thumbnail::from_bytes(&change.path, &bytes, true).ok())
    };
    let (before, after) = (decode(before), decode(after));
    native_protocol(Some(protocol), before.is_some() || after.is_some())?;
    Some(Previews { before, after })
}

#[cfg(test)]
mod tests;
