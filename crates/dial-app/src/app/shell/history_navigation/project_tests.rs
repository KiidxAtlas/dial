use super::*;

#[test]
fn lookup_never_uses_another_projects_catalogue_or_an_input_path() {
    let (_home, _project, app, _shell) = super::super::testkit::shell();
    let other_project = tempfile::tempdir().unwrap();
    let mut other = app.layout.clone();
    other.project_root = other_project.path().to_owned();
    other.state_dir = other_project.path().join("private-state");
    let held = Session::create(
        &other,
        crate::storage::session::SessionMeta {
            model: "fixture".into(),
        },
    )
    .unwrap();
    assert!(resolve(&app.layout, held.id()).is_err());
    assert!(resolve(&app.layout, held.path().unwrap().to_str().unwrap()).is_err());
    assert_eq!(resolve(&other, held.id()).unwrap().id, held.id());
}
