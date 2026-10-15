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
mod tests {
    use super::super::model::{Area, Kind};
    use super::*;
    use crate::config::types::Config;
    use std::path::Path;

    #[test]
    fn is_image_path_is_case_insensitive_and_excludes_svg() {
        assert!(is_image_path("assets/logo.PNG"));
        assert!(is_image_path("photo.jpeg"));
        assert!(is_image_path("dir/anim.gif"));
        assert!(is_image_path("a.webp"));
        assert!(is_image_path("scan.TIFF"));
        assert!(!is_image_path("icon.svg"));
        assert!(!is_image_path("README"));
        assert!(!is_image_path("archive.tar.gz"));
    }

    #[test]
    fn native_protocol_falls_back_without_protocol_or_decode() {
        assert!(native_protocol(None, true).is_none());
        assert!(native_protocol(Some(ImageProtocol::Kitty), false).is_none());
        assert!(matches!(
            native_protocol(Some(ImageProtocol::Kitty), true),
            Some(ImageProtocol::Kitty)
        ));
    }

    #[test]
    fn load_previews_skips_everything_without_protocol() {
        let change = Change {
            path: "logo.png".to_owned(),
            orig: None,
            kind: Kind::Added,
            area: Area::Unstaged,
        };
        assert!(
            load_previews(
                &Repo::new(Path::new("."), &Config::default()),
                &change,
                None,
                None
            )
            .is_none()
        );
    }
}
