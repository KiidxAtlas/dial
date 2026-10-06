//! Cheap pre-write parsing. This is syntax evidence, never a behavioral or type-check verdict.
use ast_grep_core::{AstGrep, Language};
use ast_grep_language::SupportLang;
use serde_json::{Value, json};
use std::path::Path;

pub fn report(path: &Path, content: &str) -> Value {
    if content.len() > 4 * 1024 * 1024 {
        return json!({"supported":false,"reason":"source exceeds parser budget"});
    }
    let Some(lang) = SupportLang::from_path(path) else {
        return json!({"supported":false,"reason":"no syntax parser for this extension"});
    };
    match AstGrep::try_new(content, lang) {
        Ok(tree) => {
            let errors: Vec<_> = tree.root().dfs().filter(|node|node.is_error()||node.is_missing()).take(33)
                .map(|node| {let pos=node.start_pos();json!({"line":pos.line()+1,"column":pos.column(&node)+1,"missing":node.is_missing()})}).collect();
            json!({"supported":true,"language":lang.to_string(),"errors":errors,"valid":errors.is_empty()})
        }
        Err(_) => json!({"supported":false,"reason":"parser unavailable"}),
    }
}

pub fn preflight(path: &Path, before: &str, proposed: &str) -> Result<(), String> {
    let after = report(path, proposed);
    let new = after["errors"].as_array().map_or(0, Vec::len);
    if new == 0 {
        return Ok(());
    }
    let old = report(path, before)["errors"]
        .as_array()
        .map_or(0, Vec::len);
    if new < old {
        return Ok(());
    }
    Err(format!(
        "Syntax preflight refused {} before writing: {} parse errors (previously {}). First error: {}. Correct the proposed source or disable assistance.syntax_guard for intentional partial source. This is not a type or behavioral check.",
        path.display(),
        new,
        old,
        after["errors"][0]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_rust_is_rejected_before_a_valid_file_is_changed() {
        assert!(preflight(Path::new("a.rs"), "fn a() {}", "fn a( {").is_err());
        assert!(preflight(Path::new("a.rs"), "fn a( {", "fn a() {}").is_ok());
        assert_eq!(
            report(Path::new("notes.txt"), "fn a( {")["supported"],
            false
        );
    }
}

#[cfg(test)]
mod writes {
    use super::*;
    use crate::tools::{EditMode, Workspace, execute};
    #[test]
    fn all_write_paths_guard_source_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::new(root.path())
            .unwrap()
            .with_syntax_guard(true)
            .with_edit_mode(EditMode::Replace);
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let out = execute(&ws, "write", &json!({"path":"a.rs","content":"fn a( {"}), 5);
        assert!(out.is_error);
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.rs")).unwrap(),
            "fn a() {}\n"
        );
        let out = execute(
            &ws,
            "edit",
            &json!({"path":"a.rs","old_text":"fn a() {}","new_text":"fn a( {"}),
            5,
        );
        assert!(out.is_error);
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.rs")).unwrap(),
            "fn a() {}\n"
        );
    }
    #[test]
    fn explicit_disable_allows_intentional_partial_source() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::new(root.path())
            .unwrap()
            .with_syntax_guard(false);
        assert!(!execute(&ws, "write", &json!({"path":"a.rs","content":"fn a( {"}), 5).is_error);
    }
}
