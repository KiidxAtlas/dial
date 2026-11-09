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

fn lean(root: &Path, mode: EditMode) -> Workspace {
    Workspace::new(root)
        .unwrap()
        .with_syntax_guard(true)
        .with_syntax_block(true)
        .with_edit_mode(mode)
}

/// The small/lean local tiers refuse an edit that adds parser errors before commit, write nothing,
/// keep witnesses, and accept a corrected retry without a reread.
#[test]
fn lean_tier_refuses_breaking_edits_and_a_corrected_retry_needs_no_reread() {
    let root = tempfile::tempdir().unwrap();
    let ws = lean(root.path(), EditMode::Replace);
    let original = "fn a() {\n    let x = 1;\n    x\n}\n";
    std::fs::write(root.path().join("a.rs"), original).unwrap();
    execute(&ws, "read", &json!({"path":"a.rs"}), 5);
    let out = execute(
        &ws,
        "edit",
        &json!({"path":"a.rs","old_string":"    let x = 1;","new_string":"    let x = (1;"}),
        5,
    );
    assert!(out.is_error, "{}", out.text);
    assert!(out.text.contains("Edit refused"), "{}", out.text);
    assert!(out.text.contains("nothing was written"), "{}", out.text);
    assert!(out.text.contains("2:    let x = (1;"), "{}", out.text);
    assert!(out.text.contains("allow_syntax_errors"), "{}", out.text);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.rs")).unwrap(),
        original
    );
    let out = execute(
        &ws,
        "edit",
        &json!({"path":"a.rs","old_string":"    let x = 1;","new_string":"    let x = (1);"}),
        5,
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(
        std::fs::read_to_string(root.path().join("a.rs"))
            .unwrap()
            .contains("(1);")
    );
}

#[test]
fn lean_tier_refusal_keeps_the_hashline_tag_valid() {
    let root = tempfile::tempdir().unwrap();
    let ws = lean(root.path(), EditMode::Hashline);
    std::fs::write(root.path().join("a.py"), "def a():\n    return 1\n").unwrap();
    let read = execute(&ws, "read", &json!({"path":"a.py"}), 5);
    let header = read.text.lines().next().unwrap().to_owned();
    let out = execute(
        &ws,
        "edit",
        &json!({"input":format!("{header}\nPUT 2.=2:\n+    return (1\n")}),
        5,
    );
    assert!(out.is_error, "{}", out.text);
    assert!(out.text.contains("Edit refused"), "{}", out.text);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.py")).unwrap(),
        "def a():\n    return 1\n"
    );
    let out = execute(
        &ws,
        "edit",
        &json!({"input":format!("{header}\nPUT 2.=2:\n+    return (2)\n")}),
        5,
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(
        std::fs::read_to_string(root.path().join("a.py"))
            .unwrap()
            .contains("return (2)")
    );
}

#[test]
fn lean_tier_allows_unchanged_error_counts_the_escape_hatch_and_other_grammars() {
    let root = tempfile::tempdir().unwrap();
    let ws = lean(root.path(), EditMode::Replace);
    // Already broken: an edit that does not add errors passes with the advisory.
    std::fs::write(root.path().join("a.rs"), "fn a( {\nfn b() {}\n").unwrap();
    execute(&ws, "read", &json!({"path":"a.rs"}), 5);
    let out = execute(
        &ws,
        "edit",
        &json!({"path":"a.rs","old_string":"fn b()","new_string":"fn c()"}),
        5,
    );
    assert!(!out.is_error, "{}", out.text);
    // The escape hatch commits a breaking edit, still with the advisory.
    std::fs::write(root.path().join("b.go"), "package b\n\nfunc B() {}\n").unwrap();
    execute(&ws, "read", &json!({"path":"b.go"}), 5);
    let breaking = json!({"path":"b.go","old_string":"func B() {}","new_string":"func B( {}"});
    assert!(execute(&ws, "edit", &breaking, 5).is_error);
    let mut allowed = breaking.clone();
    allowed["allow_syntax_errors"] = json!(true);
    let out = execute(&ws, "edit", &allowed, 5);
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("Syntax advisory"), "{}", out.text);
    // Grammars outside the high-confidence set (and unparsed files) only get the advisory.
    for (path, original, updated) in [
        ("c.json", "{\"a\": 1}\n", "{\"a\": 1,,}\n"),
        ("d.txt", "x\n", "x (\n"),
    ] {
        std::fs::write(root.path().join(path), original).unwrap();
        execute(&ws, "read", &json!({"path":path}), 5);
        let out = execute(
            &ws,
            "edit",
            &json!({"path":path,"old_string":original,"new_string":updated}),
            5,
        );
        assert!(!out.is_error, "{path}: {}", out.text);
    }
    // Full tier: the same breaking edit is only reported.
    let full = Workspace::new(root.path())
        .unwrap()
        .with_syntax_guard(true)
        .with_edit_mode(EditMode::Replace);
    std::fs::write(root.path().join("e.ts"), "const a = 1;\n").unwrap();
    execute(&full, "read", &json!({"path":"e.ts"}), 5);
    let out = execute(
        &full,
        "edit",
        &json!({"path":"e.ts","old_string":"const a = 1;","new_string":"const a = (1;"}),
        5,
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("Syntax advisory"), "{}", out.text);
}

#[test]
fn allow_syntax_errors_is_valid_in_every_edit_grammar() {
    for mode in [
        EditMode::Hashline,
        EditMode::Replace,
        EditMode::Patch,
        EditMode::ApplyPatch,
        EditMode::Sloppy,
    ] {
        let schema = crate::tools::edit::editing::schema(mode);
        assert_eq!(
            schema["function"]["parameters"]["properties"]["allow_syntax_errors"]["type"],
            "boolean",
            "{mode:?}"
        );
        let mut args = match mode {
            EditMode::Replace => json!({"path":"a","old_string":"x","new_string":"y"}),
            EditMode::Patch => json!({"path":"a","edits":[]}),
            _ => json!({"input":""}),
        };
        args["allow_syntax_errors"] = json!(true);
        crate::tools::edit::editing::validate_shape(mode, &args).unwrap();
    }
}

#[test]
fn lean_write_refuses_new_errors_before_commit_and_preserves_the_witness() {
    let root = tempfile::tempdir().unwrap();
    let ws = lean(root.path(), EditMode::Hashline);
    let path = root.path().join("a.rs");
    let original = "fn a() {}\n";
    std::fs::write(&path, original).unwrap();
    let read = execute(&ws, "read", &json!({"path":"a.rs"}), 5);
    assert!(!read.is_error, "{}", read.text);
    let breaking = json!({"path":"a.rs","content":"fn a( {"});
    let out = execute(&ws, "write", &breaking, 5);
    assert!(out.is_error, "{}", out.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert!(out.execution.changed_paths.is_empty());
    let corrected = execute(
        &ws,
        "write",
        &json!({"path":"a.rs","content":"fn b() {}\n"}),
        5,
    );
    assert!(!corrected.is_error, "{}", corrected.text);
    let mut allowed = breaking;
    allowed["allow_syntax_errors"] = json!(true);
    let out = execute(&ws, "write", &allowed, 5);
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a( {");
    let out = execute(
        &ws,
        "write",
        &json!({"path":"new.py","content":"def broken(:\n"}),
        5,
    );
    assert!(out.is_error, "{}", out.text);
    assert!(!root.path().join("new.py").exists());
}

#[test]
fn python_string_round_trip_and_focused_repair_preserve_saved_bytes() {
    let root = tempfile::tempdir().unwrap();
    let ws = Workspace::new(root.path()).unwrap().with_syntax_guard(true);
    let source = "room = '洞窟'\nprint(\"\\n\")\nprint(f\"{room}: \\\"shield\\\" \\\\ path\")\n";
    let arguments = json!({"path":"dungeon_game.py","content":source});
    let wire = serde_json::to_vec(&arguments).unwrap();
    let decoded: Value = serde_json::from_slice(&wire).unwrap();
    assert_eq!(
        decoded["content"].as_str().unwrap().as_bytes(),
        source.as_bytes()
    );
    let written = execute(&ws, "write", &decoded, 5);
    assert!(!written.is_error, "{}", written.text);
    assert_eq!(
        std::fs::read(root.path().join("dungeon_game.py")).unwrap(),
        source.as_bytes()
    );
    assert_eq!(report(Path::new("dungeon_game.py"), source)["valid"], true);

    let read = execute(&ws, "read", &json!({"path":"dungeon_game.py"}), 5);
    let header = read.text.lines().next().unwrap();
    let broken = execute(
        &ws,
        "edit",
        &json!({"input":format!("{header}\nPUT 2.=2:\n+print(\"\n+\")")}),
        5,
    );
    assert!(!broken.is_error, "{}", broken.text);
    assert!(broken.text.contains("Syntax advisory"), "{}", broken.text);
    let saved = std::fs::read_to_string(root.path().join("dungeon_game.py")).unwrap();
    assert!(
        saved.contains("print(\"\n\")"),
        "malformed source must not be silently fixed"
    );
    assert_eq!(report(Path::new("dungeon_game.py"), &saved)["valid"], false);
    let read = execute(&ws, "read", &json!({"path":"dungeon_game.py"}), 5);
    let header = read.text.lines().next().unwrap();
    let repaired = execute(
        &ws,
        "edit",
        &json!({"input":format!("{header}\nPUT 2.=3:\n+print()")}),
        5,
    );
    assert!(!repaired.is_error, "{}", repaired.text);
    let expected = source.replacen("print(\"\\n\")", "print()", 1);
    let saved = std::fs::read_to_string(root.path().join("dungeon_game.py")).unwrap();
    assert_eq!(saved, expected);
    assert_eq!(report(Path::new("dungeon_game.py"), &saved)["valid"], true);
}
