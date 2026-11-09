//! Cell-source rewriting that makes top-level JavaScript declarations re-runnable.
//!
//! A cell namespace persists, so `const x = …` in an edited, re-run cell would otherwise fail
//! with "already been declared" / "redeclaration of", and a cell that threw after declaring a
//! `let`/`const` leaves that name permanently uninitialized. Notebook semantics are wanted
//! instead: top-level `let`/`const` become `var` (an uninitialized `let x;` resets to
//! `undefined`) and a top-level `class C {}` becomes `var C = class C {};`. Nested scopes,
//! loop heads and blocks keep their own declarations. A source that does not parse is returned
//! unchanged so the engine reports its own SyntaxError.

use ast_grep_core::AstGrep;
use ast_grep_language::SupportLang;

pub(super) fn redeclarable(code: &str) -> String {
    let Ok(document) = AstGrep::try_new(code, SupportLang::JavaScript) else {
        return code.to_owned();
    };
    let root = document.root();
    if root.dfs().any(|node| node.is_error() || node.is_missing()) {
        return code.to_owned();
    }
    // (byte offset, bytes to remove, replacement), applied back to front.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for statement in root.children() {
        match statement.kind().as_ref() {
            "lexical_declaration" => {
                let Some(keyword) = statement.children().next() else {
                    continue;
                };
                if !matches!(keyword.text().as_ref(), "let" | "const") {
                    continue;
                }
                let range = keyword.range();
                edits.push((range.start, range.len(), "var".into()));
                for declarator in statement
                    .children()
                    .filter(|node| node.kind() == "variable_declarator")
                {
                    if declarator.field("value").is_none()
                        && let Some(name) = declarator.field("name")
                        && name.kind() == "identifier"
                    {
                        edits.push((name.range().end, 0, " = undefined".into()));
                    }
                }
            }
            "class_declaration" => {
                let Some(name) = statement.field("name") else {
                    continue;
                };
                let range = statement.range();
                edits.push((range.start, 0, format!("var {} = ", name.text())));
                edits.push((range.end, 0, ";".into()));
            }
            _ => {}
        }
    }
    let mut source = code.to_owned();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.0));
    for (start, removed, replacement) in edits {
        source.replace_range(start..start + removed, &replacement);
    }
    source
}

#[cfg(test)]
mod tests;
