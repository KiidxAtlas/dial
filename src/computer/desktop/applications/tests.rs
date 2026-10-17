use super::*;

fn app(id: &str, name: &str, path: &str) -> Application {
    Application {
        id: id.into(),
        name: name.into(),
        path: path.into(),
        running: false,
        pid: None,
    }
}

#[test]
fn computer_identity_precedes_name_and_names_are_case_insensitive() {
    let apps = [
        app("org.example.Editor", "Edit", "/A.app"),
        app("other", "org.example.Editor", "/B.app"),
    ];
    assert_eq!(
        resolve(&apps, "org.example.Editor").unwrap().unwrap().path,
        "/A.app"
    );
    assert_eq!(
        resolve(&apps, "eDiT").unwrap().unwrap().id,
        "org.example.Editor"
    );
    assert!(resolve(&apps, "missing").unwrap().is_none());
}

#[test]
fn computer_duplicate_names_and_bundle_id_copies_require_path() {
    let apps = [
        app("same", "Editor", "/A.app"),
        app("same", "editor", "/B.app"),
    ];
    assert!(resolve(&apps, "EDITOR").is_err());
    assert!(resolve(&apps, "same").is_err());
    assert_eq!(resolve(&apps, "/B.app").unwrap().unwrap().path, "/B.app");
}
