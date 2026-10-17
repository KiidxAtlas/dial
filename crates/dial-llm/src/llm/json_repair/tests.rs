use super::*;
use serde_json::json;

fn args(text: &str) -> Option<Value> {
    parse_arguments(text)
}

#[test]
fn valid_json_is_untouched() {
    assert_eq!(
        args(r#"{"path":"a.rs","n":3,"ok":true,"x":null,"l":[1,2]}"#),
        Some(json!({"path":"a.rs","n":3,"ok":true,"x":null,"l":[1,2]}))
    );
}

#[test]
fn a_raw_newline_or_tab_inside_a_string_is_kept_as_that_character() {
    // The case that costs the most: a whole file written into "content" with real line breaks.
    let call = "{\"path\":\"a.rs\",\"content\":\"fn main() {\n\tprintln!(\\\"hi\\\");\n}\n\"}";
    assert!(
        serde_json::from_str::<Value>(call).is_err(),
        "strict JSON rejects this"
    );
    assert_eq!(
        args(call),
        Some(json!({"path":"a.rs","content":"fn main() {\n\tprintln!(\"hi\");\n}\n"}))
    );
}

#[test]
fn an_invalid_escape_keeps_its_backslash() {
    assert_eq!(
        args(r#"{"pattern":"\d+\s*\w","dir":"C:\temp\new"}"#),
        Some(json!({"pattern":"\\d+\\s*\\w","dir":"C:\temp\new"}))
    );
}

#[test]
fn single_quotes_unquoted_keys_and_trailing_commas_are_accepted() {
    assert_eq!(
        args("{path: 'src/a.rs', 'old': 'it\\'s', list: [1, 2,], }"),
        Some(json!({"path":"src/a.rs","old":"it's","list":[1,2]}))
    );
}

#[test]
fn unicode_escapes_including_surrogate_pairs_decode_and_a_broken_one_survives() {
    assert_eq!(
        args(r#"{"s":"\u00e9 \ud83d\ude00"}"#),
        Some(json!({"s":"é 😀"}))
    );
    assert_eq!(args(r#"{"s":"\u12"}"#), Some(json!({"s":"\\u12"})));
}

#[test]
fn a_double_encoded_object_is_unwrapped_once() {
    assert_eq!(args(r#""{\"path\": \"a\"}""#), Some(json!({"path":"a"})));
    assert_eq!(args(r#""just text""#), None);
}

#[test]
fn truncated_or_trailing_input_is_refused_not_guessed() {
    for bad in [
        r#"{"path":"a"#,
        r#"{"path":"a","b":"#,
        r#"{"a":[1,2"#,
        r#"{"a":1} and more"#,
        r#"{"a":1}}"#,
        "",
        "[1,2]",
        "{\"a\" 1}",
        r#"{"a":trueish}"#,
    ] {
        assert_eq!(args(bad), None, "{bad:?}");
    }
}

#[test]
fn repair_never_alters_what_a_valid_string_says() {
    for text in [
        "plain",
        "with \"quotes\"",
        "tab\there",
        "uni é 日本 😀",
        "back\\slash",
    ] {
        let strict = json!({ "v": text }).to_string();
        assert_eq!(args(&strict), Some(json!({ "v": text })));
    }
}

#[test]
fn deeply_nested_input_cannot_overflow_the_stack() {
    let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    assert!(parse(&deep).is_err());
}
