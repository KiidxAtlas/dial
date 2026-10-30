use super::{ImageRequest, output_name, parse};
use std::path::Path;

#[test]
fn generate_and_edit_need_a_prompt() {
    assert_eq!(
        parse("generate  a red fox at dusk "),
        Some(ImageRequest {
            prompt: "a red fox at dusk",
            source: None
        })
    );
    assert_eq!(parse("generate"), None);
    assert_eq!(parse("generate   "), None);
    assert_eq!(parse(""), None);
    assert_eq!(
        parse("edit fox.png  make it snow "),
        Some(ImageRequest {
            prompt: "make it snow",
            source: Some("fox.png")
        })
    );
    assert_eq!(parse("edit fox.png"), None);
    assert_eq!(parse("edit fox.png   "), None);
    assert_eq!(parse("edit"), None);
    assert_eq!(parse("settings now"), None);
}

#[test]
fn output_names_come_from_the_prompt_and_skip_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert_eq!(
        output_name(
            root,
            "A Red Fox, at dusk! over the snowy hills",
            "png",
            None
        ),
        "image-a-red-fox-at-dusk-over.png"
    );
    assert_eq!(output_name(root, "日本の猫", "jpg", None), "image.jpg");

    std::fs::write(root.join("image-fox.png"), b"").unwrap();
    std::fs::write(root.join("image-fox-2.png"), b"").unwrap();
    assert_eq!(output_name(root, "fox", "png", None), "image-fox-3.png");
}

#[test]
fn edit_outputs_take_the_source_stem_and_sit_beside_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("kite.png"), b"").unwrap();
    assert_eq!(
        output_name(root, "add a sun", "png", Some("kite.png")),
        "kite-add-a-sun.png"
    );
    assert_eq!(
        output_name(root, "sun", "png", Some("art/kite.jpg")),
        Path::new("art").join("kite-sun.png").to_string_lossy()
    );
    // Never overwrite: an existing edit output (or the source itself) gets the next number.
    std::fs::create_dir(root.join("art")).unwrap();
    std::fs::write(root.join("art/kite-sun.png"), b"").unwrap();
    assert_eq!(
        output_name(root, "sun", "png", Some("art/kite.jpg")),
        Path::new("art").join("kite-sun-2.png").to_string_lossy()
    );
    assert_eq!(
        output_name(root, "日本の猫", "png", Some("kite.png")),
        "kite-2.png"
    );
}
