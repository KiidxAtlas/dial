use super::redeclarable;

#[test]
fn top_level_declarations_become_re_runnable() {
    assert_eq!(redeclarable("const a = 1;"), "var a = 1;");
    assert_eq!(
        redeclarable("let x, y = 2;\nx"),
        "var x = undefined, y = 2;\nx"
    );
    assert_eq!(
        redeclarable("const {p, q} = o; let [r] = s;"),
        "var {p, q} = o; var [r] = s;"
    );
    assert_eq!(
        redeclarable("class C { m() {} }\nnew C()"),
        "var C = class C { m() {} };\nnew C()"
    );
    assert_eq!(
        redeclarable("const fs = await import('node:fs');"),
        "var fs = await import('node:fs');"
    );
}

#[test]
fn nested_scopes_strings_and_parse_errors_are_left_alone() {
    for code in [
        "for (const k of ks) { let v = k; }",
        "function f() { const z = 1; return z; }",
        "{ let block = 1; }",
        "console.log('const a = 1')",
        "// let comment = 1\nvar kept = 1;",
        "const broken = ;",
        "function g() {}",
    ] {
        assert_eq!(redeclarable(code), code);
    }
}
