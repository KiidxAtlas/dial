use std::path::Path;

use super::registered_path;

#[test]
fn computer_registered_paths_are_files_not_command_lines() {
    assert_eq!(
        registered_path(r#" "C:\Program Files\Editor\editor.exe" "#),
        Some(Path::new(r"C:\Program Files\Editor\editor.exe"))
    );
    assert_eq!(
        registered_path(r"C:\Editor\editor.exe"),
        Some(Path::new(r"C:\Editor\editor.exe"))
    );
    for value in [
        r"editor.exe",
        r#""C:\Editor\editor.exe" --new-window"#,
        r"C:\Editor\editor.exe --new-window",
        r"C:\Editor\open.cmd",
        "C:\\Editor\\editor.exe\0ignored",
    ] {
        assert!(registered_path(value).is_none(), "{value:?}");
    }
}
