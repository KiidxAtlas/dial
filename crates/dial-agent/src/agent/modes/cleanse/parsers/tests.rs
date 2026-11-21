use super::*;

fn ctx<'a>(root: &'a Path) -> Ctx<'a> {
    Ctx {
        checker: "test",
        cwd: root,
        root,
    }
}

#[test]
fn normalizes_paths_into_the_project_only() {
    let root = Path::new("/proj");
    assert_eq!(
        normalize_file("./src/a.ts", root, root).as_deref(),
        Some("src/a.ts")
    );
    assert_eq!(
        normalize_file("file:///proj/src/a%20b.ts", root, root).as_deref(),
        Some("src/a b.ts")
    );
    assert_eq!(normalize_file("/elsewhere/x.ts", root, root), None);
    assert_eq!(normalize_file("<stdin>", root, root), None);
}

#[test]
fn generic_parses_gcc_and_msvc_lines() {
    let root = Path::new("/proj");
    let out = "src/a.c:4:2: error: boom [-Werror]\nsrc\\b.cs(7,3): warning CS1234: odd";
    let diagnostics = parse(Kind::Generic, &ctx(root), out, "");
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].severity, Severity::Error);
    assert_eq!(diagnostics[0].line, Some(4));
    assert_eq!(diagnostics[1].code.as_deref(), Some("CS1234"));
}

#[test]
fn ruff_json_becomes_located_warnings_with_fix_hints() {
    let root = Path::new("/proj");
    let out = r#"[{"filename":"/proj/a.py","message":"unused import","code":"F401",
            "location":{"row":1,"column":8},"end_location":{"row":1,"column":12},
            "fix":{"message":"Remove import"}}]"#;
    let diagnostics = parse(Kind::Ruff, &ctx(root), out, "");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].file.as_deref(), Some("a.py"));
    assert_eq!(diagnostics[0].suggestion.as_deref(), Some("Remove import"));
    assert_eq!(diagnostics[0].severity, Severity::Warning);
}

#[test]
fn swiftlint_json_keeps_rule_severity_and_optional_column() {
    let root = Path::new("/repo");
    let out = r#"[{"file":"/repo/Sources/App.swift","line":10,"character":5,"rule_id":"force_cast","severity":"Error","reason":"Force casts should be avoided","type":"Force Cast"},{"file":"/repo/Sources/App.swift","line":12,"character":null,"rule_id":"line_length","severity":"Warning","reason":"Line should be 120 characters or less"},{"file":"/elsewhere/X.swift","line":1,"rule_id":"x","severity":"Warning","reason":"outside"}]"#;
    let diagnostics = parse(Kind::Swiftlint, &ctx(root), out, "");
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].file.as_deref(), Some("Sources/App.swift"));
    assert_eq!(
        (diagnostics[0].line, diagnostics[0].column),
        (Some(10), Some(5))
    );
    assert_eq!(diagnostics[0].code.as_deref(), Some("force_cast"));
    assert_eq!(diagnostics[0].severity, Severity::Error);
    assert_eq!(diagnostics[1].column, None);
    assert_eq!(diagnostics[1].severity, Severity::Warning);
    assert_eq!(Kind::from_id("swiftlint"), Some(Kind::Swiftlint));
}

#[test]
fn ansi_and_whitespace_are_cleaned_from_messages() {
    let root = Path::new("/proj");
    let out = "a.py:1:1: E999 \u{1b}[31mbad\u{1b}[0m   syntax";
    let diagnostics = parse(Kind::Flake8, &ctx(root), out, "");
    assert_eq!(diagnostics[0].message, "bad syntax");
    assert_eq!(diagnostics[0].severity, Severity::Error);
}

#[test]
fn balanced_json_slices_are_found_inside_noise() {
    let values = json_values("progress...\n{\"a\":\"}\"}\nnoise [1,2]");
    assert_eq!(values.len(), 2);
}

#[test]
fn stray_unbalanced_bracket_does_not_hide_later_json() {
    let values = json_values("warning: stray [\n{\"a\":1}");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["a"], 1);
}
