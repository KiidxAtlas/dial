use super::{ImageRequest, output_name, parse};

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
        output_name(root, "A Red Fox, at dusk! over the snowy hills", "png"),
        "image-a-red-fox-at-dusk-over.png"
    );
    assert_eq!(output_name(root, "日本の猫", "jpg"), "image.jpg");

    std::fs::write(root.join("image-fox.png"), b"").unwrap();
    std::fs::write(root.join("image-fox-2.png"), b"").unwrap();
    assert_eq!(output_name(root, "fox", "png"), "image-fox-3.png");
}
