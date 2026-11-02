use super::super::diff_string::parse_diff_hunks;
use super::*;

fn apply(content: &str, diff: &str) -> Result<String, String> {
    let hunks = parse_diff_hunks(diff).map_err(|e| e.to_string())?;
    apply_hunks(content, "a.rs", &hunks, 0.92, true, &|_, _| Ok(()))
        .map(|(after, _)| after)
        .map_err(|e| e.to_string())
}

#[test]
fn a_line_numbered_hunk_edits_the_occurrence_at_its_line_not_the_first_duplicate() {
    let block = "fn helper() {\n    setup();\n    run();\n}\n";
    let mut content = String::from(block);
    for _ in 0..30 {
        content.push_str("// filler\n");
    }
    content.push_str(block);
    let second = content
        .lines()
        .position(|line| line == "fn helper() {")
        .unwrap();
    let second = content
        .lines()
        .skip(second + 1)
        .position(|line| line == "fn helper() {")
        .unwrap()
        + second
        + 2;
    let diff = format!(
        "@@ -{second},4 +{second},4 @@\n fn helper() {{\n     setup();\n-    run();\n+    run_twice();\n }}\n"
    );
    let after = apply(&content, &diff).unwrap();
    let mut blocks = after.split("// filler\n").filter(|part| !part.is_empty());
    assert!(
        blocks.next().unwrap().contains("    run();"),
        "first copy untouched"
    );
    assert!(after.ends_with("    run_twice();\n}\n"), "{after}");
}

#[test]
fn a_context_stripped_fallback_never_lands_on_another_function() {
    let content =
        "fn first() -> i32 {\n    return 0;\n}\n\nfn second() -> i32 {\n    return 0;\n}\n";
    // The context names `second` loosely enough that the full hunk misses; dropping the context
    // leaves `return 0;`, which matches both functions.
    let diff = "@@\n fn second() {\n-    return 0;\n+    return 2;\n }\n";
    match apply(content, diff) {
        Ok(after) => assert!(
            after.starts_with("fn first() -> i32 {\n    return 0;\n}"),
            "the first function must never be edited: {after}"
        ),
        Err(error) => assert!(!error.is_empty()),
    }
}

#[test]
fn an_intended_dedent_survives_whitespace_tolerant_matching() {
    // The file indents with 8/12 spaces; the model wrote its hunk with 4/8 (a Trim match).
    let content = "def f(xs):\n        for i in xs:\n            total += i\n            log(i)\n";
    let diff = "@@\n     for i in xs:\n-        total += i\n-        log(i)\n+        total += 2 * i\n+    log(i)\n";
    let after = apply(content, diff).unwrap();
    assert_eq!(
        after, "def f(xs):\n        for i in xs:\n            total += 2 * i\n        log(i)\n",
        "log(i) moves out of the loop, in the file's own indentation"
    );
}

#[test]
fn wrapping_lines_in_a_new_block_keeps_them_inside_it() {
    let content = "def f():\n        x = 1\n        y = 2\n";
    let diff = "@@\n-    x = 1\n-    y = 2\n+    try:\n+        x = 1\n+        y = 2\n+    except E:\n+        pass\n";
    let after = apply(content, diff).unwrap();
    assert_eq!(
        after,
        "def f():\n        try:\n            x = 1\n            y = 2\n        except E:\n            pass\n"
    );
}
