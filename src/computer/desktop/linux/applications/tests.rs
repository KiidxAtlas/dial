use std::path::Path;

use super::exec_argv;

fn parse(value: &str) -> super::CoreResult<Vec<String>> {
    exec_argv(
        value,
        "Display Name",
        Some("icon with spaces"),
        Path::new("/apps/test app.desktop"),
    )
}

#[test]
fn expands_desktop_fields_without_splitting_values() {
    assert_eq!(
        parse("/usr/bin/example %c %i %k %U %%").unwrap(),
        [
            "/usr/bin/example",
            "Display Name",
            "--icon",
            "icon with spaces",
            "/apps/test app.desktop",
            "%",
        ]
    );
}

#[test]
fn preserves_quoted_arguments_and_empty_arguments() {
    assert_eq!(
        parse(r#""/opt/App Suite/app" "two words" """#).unwrap(),
        ["/opt/App Suite/app", "two words", ""]
    );
}

#[test]
fn applies_desktop_and_exec_backslash_layers() {
    assert_eq!(
        parse(r#"app "a\\"b" "c\\\\d" "\\$HOME""#).unwrap(),
        ["app", "a\"b", "c\\d", "$HOME"]
    );
}

#[test]
fn rejects_shell_operators_and_malformed_fields() {
    for value in [
        "app ; touch /tmp/file",
        "app $(command)",
        "app 'single quoted'",
        "app %Z",
        "app %",
        "app prefix%F",
        "app %f %U",
        "app \"%c\"",
        "app \"unterminated",
        r"app \q",
    ] {
        assert!(parse(value).is_err(), "{value}");
    }
}

#[test]
fn shell_text_is_only_a_literal_argument() {
    assert_eq!(
        parse(r#"app "$(touch /tmp/file); * | >""#).unwrap(),
        ["app", "$(touch /tmp/file); * | >"]
    );
}

#[test]
fn removes_absent_inputs_and_deprecated_codes() {
    assert_eq!(parse("app %f %d %D %n %N %v %m").unwrap(), ["app"]);
    assert_eq!(
        exec_argv("app %i", "Name", None, Path::new("/app.desktop")).unwrap(),
        ["app"]
    );
}
