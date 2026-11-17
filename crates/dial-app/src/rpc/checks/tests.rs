use super::*;
use crate::config::paths::Layout;
use crate::config::settings_store::{self, Scope};
use serde_json::json;

fn code_of(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<super::super::RequestError>()
        .map_or(ErrorCode::Failed, |refused| refused.code)
}

fn layout() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    (project, home, layout)
}

/// A Git repository with no commits, so `changed_files` reads this fixture and not the checkout
/// the tests run in.
fn repo() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(project.path())
        .status()
        .unwrap();
    assert!(status.success());
    project
}

fn config_at(root: &std::path::Path) -> Config {
    Config {
        project_root: root.to_path_buf(),
        ..Config::default()
    }
}

#[test]
fn add_params_are_validated_by_the_shared_owner() {
    let cases: Vec<(Value, Option<ErrorCode>)> = vec![
        (json!({"name":"lint","command":"cargo clippy"}), None),
        (
            json!({"name":"lint","command":"c","file_extensions":[".rs"," py ",""],"timeout_secs":30}),
            None,
        ),
        (
            json!({"name":"lint","command":"c","file_extensions":null}),
            None,
        ),
        (json!({"name":"-x","command":"c"}), None),
        (
            json!({"name":"--x","command":"c"}),
            Some(ErrorCode::InvalidParams),
        ),
        (
            json!({"name":"a b","command":"c"}),
            Some(ErrorCode::InvalidParams),
        ),
        (
            json!({"name":"","command":"c"}),
            Some(ErrorCode::InvalidParams),
        ),
        (json!({"command":"c"}), Some(ErrorCode::InvalidParams)),
        (
            json!({"name":"lint","command":"  "}),
            Some(ErrorCode::InvalidParams),
        ),
        (json!({"name":"lint"}), Some(ErrorCode::InvalidParams)),
        (
            json!({"name":"lint","command":"c","file_extensions":"rs"}),
            Some(ErrorCode::InvalidParams),
        ),
        (
            json!({"name":"lint","command":"c","file_extensions":[1]}),
            Some(ErrorCode::InvalidParams),
        ),
        (
            json!({"name":"lint","command":"c","timeout_secs":-1}),
            Some(ErrorCode::InvalidParams),
        ),
        (
            json!({"name":"lint","command":"c","timeout_secs":"3"}),
            Some(ErrorCode::InvalidParams),
        ),
    ];
    for (params, expected) in cases {
        match (add_request(&params), expected) {
            (Ok(_), None) => {}
            (Err(error), Some(code)) => assert_eq!(code_of(error), code, "{params}"),
            (result, expected) => panic!("{params}: {result:?} but expected {expected:?}"),
        }
    }
}

fn lint_value(name: &str) -> toml::Value {
    toml::Value::try_from(VerificationCommand {
        name: name.into(),
        command: "cargo clippy".into(),
        file_extensions: vec!["rs".into()],
        timeout_secs: 0,
    })
    .unwrap()
}

fn lint_params() -> Value {
    json!({"name":"lint","command":"cargo clippy","file_extensions":["rs"],"confirm":true})
}

#[test]
fn unknown_keys_and_unconfirmed_writes_are_refused_before_anything_is_saved() {
    let (_project, _home, layout) = layout();
    let unknown = json!({"name":"lint","command":"c","confirm":true,"shell":"sh"});
    assert_eq!(
        code_of(add(&layout, &unknown).unwrap_err()),
        ErrorCode::InvalidParams
    );
    let unconfirmed = json!({"name":"lint","command":"c"});
    assert_eq!(
        code_of(add(&layout, &unconfirmed).unwrap_err()),
        ErrorCode::InvalidParams
    );
    let not_bool = json!({"name":"lint","command":"c","confirm":"yes"});
    assert_eq!(
        code_of(add(&layout, &not_bool).unwrap_err()),
        ErrorCode::InvalidParams
    );
    assert_eq!(
        code_of(remove(&layout, &json!({"name":"lint"})).unwrap_err()),
        ErrorCode::InvalidParams
    );
    assert!(crate::checks::project_commands(&layout).unwrap().is_empty());
}

/// Duplicates are judged against the saved files, not the session snapshot: a second add is
/// refused without anything being applied.
#[test]
fn repeated_add_without_apply_settings_is_refused_by_the_saved_file() {
    let (_project, _home, layout) = layout();
    let saved = add(&layout, &lint_params()).unwrap();
    assert_eq!(
        saved,
        json!({"name":"lint","scope":"project","saved":true,"apply_settings":true})
    );
    let repeated = add(&layout, &lint_params()).unwrap_err();
    assert_eq!(code_of(repeated), ErrorCode::Conflict);
    assert_eq!(
        crate::checks::project_commands(&layout).unwrap(),
        vec![lint_value("lint")]
    );
}

/// A removed name is free again at once, even while the session snapshot still lists it.
#[test]
fn remove_then_readd_without_apply_settings_saves_exactly_one_entry() {
    let (project, _home, layout) = layout();
    let mut config = config_at(project.path());
    config.verification.commands.push(VerificationCommand {
        name: "lint".into(),
        command: "cargo clippy".into(),
        file_extensions: Vec::new(),
        timeout_secs: 0,
    });
    add(&layout, &lint_params()).unwrap();
    remove(&layout, &json!({"name":"lint","confirm":true})).unwrap();
    assert!(crate::checks::project_commands(&layout).unwrap().is_empty());
    add(&layout, &lint_params()).unwrap();
    assert_eq!(
        crate::checks::project_commands(&layout).unwrap(),
        vec![lint_value("lint")]
    );
    // The session snapshot is untouched: changes reach it only through apply_settings.
    assert_eq!(config.verification.commands.len(), 1);
}

/// A name the user config already defines is a conflict: the effective configuration decides.
#[test]
fn add_is_refused_when_the_user_config_already_defines_the_name() {
    let (_project, _home, layout) = layout();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[settings_store::Change::Set {
            path: "verification.commands".into(),
            value: toml::Value::Array(vec![lint_value("lint")]),
        }],
    )
    .unwrap();
    assert_eq!(
        code_of(add(&layout, &lint_params()).unwrap_err()),
        ErrorCode::Conflict
    );
    assert!(crate::checks::project_commands(&layout).unwrap().is_empty());
}

#[test]
fn remove_deletes_a_project_check_and_refuses_an_unknown_name() {
    let (_project, _home, layout) = layout();
    add(&layout, &lint_params()).unwrap();

    let removed = remove(&layout, &json!({"name":"lint","confirm":true})).unwrap();
    assert_eq!(removed["removed"], true);
    assert!(crate::checks::project_commands(&layout).unwrap().is_empty());

    let missing = remove(&layout, &json!({"name":"lint","confirm":true})).unwrap_err();
    assert_eq!(code_of(missing), ErrorCode::NotFound);
    assert_eq!(
        code_of(remove(&layout, &json!({"confirm":true})).unwrap_err()),
        ErrorCode::InvalidParams
    );
}

/// Git reports names verbatim in NUL records: non-ASCII (quoted by default), spaces and `->` text
/// in a file name must come through unchanged, with no rename pairing leaking a second path.
#[test]
fn changed_files_are_read_verbatim_from_the_nul_status_records() {
    let project = repo();
    let names = [
        "plain.rs",
        "sp ace.rs",
        "old -> new.rs",
        "café.rs",
        "dir x/nested.rs",
    ];
    for name in names {
        let path = project.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "x").unwrap();
    }
    let report = list(&config_at(project.path()));
    let mut changed: Vec<String> = report["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap().to_owned())
        .collect();
    changed.sort();
    let mut expected: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    expected.sort();
    assert_eq!(changed, expected);
}

/// Runs `git` in `dir` with a fixed identity and no signing, so commits need no user setup.
fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=dial",
            "-c",
            "user.email=dial@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// The sorted `changed_files` the listing reports for `config`.
fn changed(config: &Config) -> Vec<String> {
    let mut paths: Vec<String> = list(config)["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap().to_owned())
        .collect();
    paths.sort();
    paths
}

/// Git reports paths relative to the repository top; a project nested below it must see only its
/// own changes, with paths relative to its root, and nothing from outside it.
#[test]
fn changes_in_a_nested_project_root_are_relative_to_that_root() {
    let repository = repo();
    let project = repository.path().join("app");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("src/x.rs"), "x").unwrap();
    std::fs::write(repository.path().join("outside.txt"), "x").unwrap();
    assert_eq!(changed(&config_at(&project)), vec!["src/x.rs"]);
}

/// With `--no-renames` a staged rename is one deletion and one addition, each a plain path.
#[test]
fn a_staged_rename_reports_the_old_path_and_the_new_path() {
    let project = repo();
    std::fs::write(project.path().join("a.rs"), "x").unwrap();
    git(project.path(), &["add", "a.rs"]);
    git(project.path(), &["commit", "-qm", "init"]);
    git(project.path(), &["mv", "a.rs", "b.rs"]);
    assert_eq!(changed(&config_at(project.path())), vec!["a.rs", "b.rs"]);
}
#[test]
fn list_reports_configured_checks_for_a_repository_without_changes() {
    let project = repo();
    let mut config = config_at(project.path());
    config.verification.commands.push(VerificationCommand {
        name: "fmt".into(),
        command: "cargo fmt --check".into(),
        file_extensions: vec!["rs".into()],
        timeout_secs: 120,
    });
    let report = list(&config);
    assert_eq!(report["changed_files"], json!([]));
    assert_eq!(
        report["configured"],
        json!([{"name":"fmt","command":"cargo fmt --check","file_extensions":["rs"],"timeout_secs":120}])
    );
    assert!(report["automatic"].is_array());
}
