use super::*;

fn version(path: &str, before: Option<&str>, after: &str) -> TurnVersion {
    TurnVersion {
        path: path.into(),
        in_project: true,
        before: before.map(|text| text.as_bytes().to_vec()),
        after: Some(after.as_bytes().to_vec()),
    }
}

#[test]
fn line_mutations_flip_operators_words_and_integers_outside_strings() {
    assert_eq!(
        line_mutations("    if a >= 10 && ok == True:"),
        [
            "    if a >= 10 && ok != True:",
            "    if a > 10 && ok == True:",
            "    if a >= 10 || ok == True:",
            "    if a >= 10 && ok == False:",
            "    if a >= 11 && ok == True:",
        ]
    );
    assert!(line_mutations("    // if a == b").is_empty());
    assert!(line_mutations("print(\"a == b, true\")").is_empty());
    assert!(line_mutations("let value_1 = x.v2;").is_empty());
    assert_eq!(line_mutations("return truex || y"), ["return truex && y"]);
    assert_eq!(line_mutations("x = 1.5"), Vec::<String>::new());
}

#[test]
fn plan_mutates_only_changed_lines_and_spreads_the_budget() {
    let before = "def f(a):\n    return a == 1\n";
    let after = "def f(a):\n    return a == 1\n\ndef g(b):\n    return b >= 3\n";
    let mutants = plan(&[version("m.py", Some(before), after)], 4);
    assert_eq!(mutants.len(), 1, "{mutants:?}");
    assert_eq!(mutants[0].line, 5);
    assert_eq!(mutants[0].mutated, "return b > 3");
    assert_eq!(
        String::from_utf8(mutants[0].content.clone()).unwrap(),
        "def f(a):\n    return a == 1\n\ndef g(b):\n    return b > 3\n"
    );

    let many: String = (0..10).map(|i| format!("x{i} = {i}\n")).collect();
    let spread = plan(&[version("n.py", None, &many)], 3);
    assert_eq!(spread.iter().map(|m| m.line).collect::<Vec<_>>(), [1, 4, 7]);
    assert!(plan(&[version("same.py", Some(after), after)], 3).is_empty());
}
