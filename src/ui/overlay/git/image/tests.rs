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
