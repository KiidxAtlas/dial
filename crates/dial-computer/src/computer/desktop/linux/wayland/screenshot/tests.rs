use super::*;

#[test]
fn portal_png_supplies_pixels_but_non_images_and_large_files_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("portal.png");
    let image = RgbaImage::from_pixel(2, 3, image::Rgba([1, 2, 3, 255]));
    image.save(&path).unwrap();
    assert_eq!(load_image(&path).unwrap(), image);
    std::fs::write(&path, b"not a PNG").unwrap();
    assert!(load_image(&path).is_err());
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    assert!(load_image(&path).unwrap_err().message.contains("64 MiB"));
}
