use super::*;
use crate::config::types::EditMode;
use crate::tools::{Workspace, execute};
#[test]
fn write_and_replace_allow_invalid_source_with_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path())
        .unwrap()
        .with_syntax_guard(true)
        .with_edit_mode(EditMode::Replace);
    std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
    execute(&ws, "read", &json!({"path":"a.rs"}), 5);
    let out = execute(&ws, "write", &json!({"path":"a.rs","content":"fn a( {"}), 5);
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("Syntax advisory"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.rs")).unwrap(),
        "fn a( {"
    );
    let out = execute(
        &ws,
        "edit",
        &json!({"path":"a.rs","old_string":"fn a( {","new_string":"fn b( {"}),
        5,
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("Syntax advisory"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.rs")).unwrap(),
        "fn b( {"
    );
}
#[test]
fn hashline_edits_do_not_reject_existing_or_new_parser_errors() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap().with_syntax_guard(true);
    for (path, original, updated) in [
        ("a.rs", "fn a() {}\n", "fn a( {"),
        (
            "functions.zsh",
            "values=(one two)\nprint ${^values}\n",
            "print ${^values} # edited",
        ),
    ] {
        std::fs::write(root.path().join(path), original).unwrap();
        let read = execute(&ws, "read", &json!({"path":path}), 5);
        assert!(!read.is_error, "{}", read.text);
        let header = read.text.lines().next().unwrap();
        let line = if path.ends_with("zsh") { 2 } else { 1 };
        let out = execute(
            &ws,
            "edit",
            &json!({"input":format!("{header}\nPUT {line}.={line}:\n+{updated}\n")}),
            5,
        );
        assert!(!out.is_error, "{}", out.text);
        if path.ends_with("rs") {
            assert!(out.text.contains("Syntax advisory"), "{}", out.text);
        }
        assert!(
            std::fs::read_to_string(root.path().join(path))
                .unwrap()
                .contains(updated)
        );
    }
}
#[test]
fn explicit_disable_omits_advisories() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path())
        .unwrap()
        .with_syntax_guard(false);
    let out = execute(&ws, "write", &json!({"path":"a.rs","content":"fn a( {"}), 5);
    assert!(!out.is_error, "{}", out.text);
    assert!(!out.text.contains("Syntax advisory"));
}

#[test]
fn zsh_is_unsupported_so_its_syntax_never_produces_advisories() {
    let zsh = "autoload -Uz compinit\nprint ${(j:,:)array}\n";
    assert_eq!(report(Path::new("rc.zsh"), zsh)["supported"], false);
    assert_eq!(report(Path::new(".zshrc"), zsh)["supported"], false);
    assert!(advisory(Path::new("rc.zsh"), "", zsh).is_none());
}
