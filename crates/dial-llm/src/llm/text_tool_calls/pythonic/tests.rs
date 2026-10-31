use super::*;
use serde_json::json;

fn offered() -> Vec<String> {
    ["read", "grep"].map(String::from).to_vec()
}

#[test]
fn keyword_calls_with_python_literals_are_parsed_without_evaluation() {
    let calls = parse(
        r#"[read(path="a.rs", limit=20), grep(pattern='x\ty', ignore_case=True, paths=["a", "b",], opts={"k": -1.5, "n": None})]"#,
        &offered(),
    )
    .unwrap();
    assert_eq!(
        calls,
        vec![
            ("read".into(), json!({"path": "a.rs", "limit": 20})),
            (
                "grep".into(),
                json!({"pattern": "x\ty", "ignore_case": true, "paths": ["a", "b"],
                       "opts": {"k": -1.5, "n": null}})
            ),
        ]
    );
    let wrapped = parse(
        "<|python_start|>[read(path=\"a\")]<|python_end|>\n",
        &offered(),
    );
    assert_eq!(wrapped.unwrap()[0].0, "read");
    assert_eq!(parse("[read()]", &offered()).unwrap()[0].1, json!({}));
}

#[test]
fn anything_but_a_list_of_offered_keyword_calls_is_refused() {
    for text in [
        "",
        "[]",
        "Sure: [read(path=\"a\")]",
        "[read(path=\"a\")] and more",
        "[delete(path=\"a\")]",
        "[read(\"a\")]",
        "[read(path=a)]",
        "[read(path=open(\"x\"))]",
        "[read(path=\"a\", path=\"b\")]",
        "[grep(opts={\"k\": 1, \"k\": 2})]",
        "[read(path=\"a\"",
        "[read(path=\"unterminated)]",
        "[read(path=\"bad \\q escape\")]",
        "read(path=\"a\")",
        "[read(path=[[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]])]",
    ] {
        assert_eq!(parse(text, &offered()), None, "{text}");
    }
}

#[test]
fn multibyte_text_is_kept_and_does_not_split_a_character() {
    let calls = parse("[read(path=\"距離/é.rs\")]", &offered()).unwrap();
    assert_eq!(calls[0].1, json!({"path": "距離/é.rs"}));
}
