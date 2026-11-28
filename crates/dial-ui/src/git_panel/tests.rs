//! Which repository the `/git` panel works on when the project is a folder of nested repositories.
use super::Repo;
use super::testkit::git;
use crate::config::types::Config;
use std::path::{Path, PathBuf};

/// A project folder that is not a repository, holding real nested checkouts and a plain folder.
fn project(nested: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    // A broken gitfile stops Git from finding an ancestor checkout when TMPDIR sits inside one.
    std::fs::write(dir.path().join(".git"), "gitdir: missing\n").expect("gitfile");
    for name in nested {
        let child = dir.path().join(name);
        std::fs::create_dir_all(&child).expect("child folder");
        git(&child, &["init", "-q"]);
    }
    std::fs::create_dir_all(dir.path().join("plain/inner")).expect("plain folder");
    dir
}

fn resolve(root: &Path, folder: Option<&str>) -> Result<PathBuf, String> {
    let mut repo = Repo::new(root, &Config::default());
    if let Some(folder) = folder {
        repo = repo.select(folder);
    }
    repo.at_repository_root().map(|repo| repo.root)
}

fn canon(path: &Path) -> PathBuf {
    path.canonicalize().expect("canonical path")
}

#[test]
fn a_project_outside_git_uses_its_only_nested_repository() {
    let dir = project(&["only"]);
    assert_eq!(
        resolve(dir.path(), None).unwrap(),
        canon(&dir.path().join("only"))
    );
}

#[test]
fn a_project_with_several_nested_repositories_is_never_guessed() {
    let dir = project(&["alpha", "beta"]);
    let error = resolve(dir.path(), None).unwrap_err();
    assert!(
        error.contains("alpha") && error.contains("beta") && error.contains("/git repo <folder>"),
        "the choices are named: {error}"
    );
}

#[test]
fn an_explicit_choice_selects_one_of_several_nested_repositories() {
    let dir = project(&["alpha", "beta"]);
    assert_eq!(
        resolve(dir.path(), Some("beta")).unwrap(),
        canon(&dir.path().join("beta"))
    );
}

#[test]
fn an_explicit_choice_must_name_a_repository_of_its_own_inside_the_project() {
    let dir = project(&["alpha"]);
    let outside = tempfile::tempdir().expect("outside");
    git(outside.path(), &["init", "-q"]);
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).expect("link");
    let absolute = outside.path().display().to_string();
    let cases = [
        ("plain", "is not a Git repository"),
        ("plain/inner", "is not a Git repository"),
        ("missing", "is not a folder in the project"),
        ("../elsewhere", "must name a folder inside the project"),
        ("", "must name a folder inside the project"),
        (absolute.as_str(), "must name a folder inside the project"),
    ];
    for (folder, expect) in cases {
        let error = resolve(dir.path(), Some(folder)).unwrap_err();
        assert!(error.contains(expect), "{folder:?}: {error}");
    }
    #[cfg(unix)]
    {
        let error = resolve(dir.path(), Some("escape")).unwrap_err();
        assert!(
            error.contains("outside the project"),
            "a link out of the project is refused: {error}"
        );
    }
}

#[test]
fn a_project_that_is_itself_a_repository_keeps_its_own_root() {
    let dir = project(&["nested"]);
    std::fs::remove_file(dir.path().join(".git")).expect("drop the blocker");
    git(dir.path(), &["init", "-q"]);
    assert_eq!(resolve(dir.path(), None).unwrap(), canon(dir.path()));
}

#[test]
fn a_folder_without_any_repository_keeps_git_s_own_error() {
    let dir = project(&[]);
    assert!(resolve(dir.path(), None).is_err());
}
