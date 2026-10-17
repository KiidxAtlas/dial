use super::*;

#[test]
fn whitespace_only_diff_is_style() {
    let diff = "@@ -1,2 +1,2 @@\n-  x\n+ x\n";
    let trivial = detect_trivial_change(diff).expect("trivial");
    assert_eq!(trivial.summary, "formatted code");
    assert_eq!(trivial.commit_type, "style");
}

#[test]
fn import_only_diff_is_reorganized_imports() {
    let diff = "@@ -1 +1 @@\n-import a from 'a';\n+import b from 'b';\n";
    let trivial = detect_trivial_change(diff).expect("trivial");
    assert_eq!(trivial.summary, "reorganized imports");
}

#[test]
fn real_change_is_not_trivial() {
    assert!(detect_trivial_change("@@ -1 +1 @@\n-let x = 1;\n+let x = 2;\n").is_none());
    assert!(detect_trivial_change("").is_none());
}
