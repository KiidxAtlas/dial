use super::*;

#[test]
fn split_word_separates_subcommand_and_rest() {
    assert_eq!(split_word("switch  feat/x "), ("switch", "feat/x"));
    assert_eq!(split_word("list"), ("list", ""));
    assert_eq!(split_word(""), ("", ""));
}

#[test]
fn resolve_dir_joins_relative_paths_and_expands_home() {
    assert_eq!(resolve_dir(Path::new("/r"), "x/y"), PathBuf::from("/r/x/y"));
    assert_eq!(resolve_dir(Path::new("/r"), "/abs"), PathBuf::from("/abs"));
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let home = home.canonicalize().unwrap_or(home);
    assert_eq!(resolve_dir(Path::new("/r"), "~"), home);
    assert!(resolve_dir(Path::new("/r"), "~/x").starts_with(&home));
    assert_eq!(resolve_dir(Path::new("/r"), "~x"), PathBuf::from("/r/~x"));
}

#[test]
fn label_prefers_branch_then_detached_head() {
    let mut tree = Worktree {
        path: PathBuf::from("/w"),
        branch: Some("feat/x".into()),
        head: "0123456789".into(),
        bare: false,
        detached: false,
        locked: false,
    };
    assert_eq!(label(&tree), "feat/x");
    tree.branch = None;
    tree.detached = true;
    assert_eq!(label(&tree), "detached at 0123456");
}

#[test]
fn worktree_clean_source_defaults_off() {
    assert!(
        !crate::config::types::Config::default()
            .worktree
            .clean_source
    );
}
