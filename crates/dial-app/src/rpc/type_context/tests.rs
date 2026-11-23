use super::*;
use serde_json::json;

fn briefing() -> &'static str {
    "<type-context>\nBoard.\n</type-context>"
}

/// An exact legacy framing typed as ordinary text: the same bytes a Type prompt used to carry.
fn literal_framing() -> &'static str {
    "[[type:notes]]\n<type-context>\nlooks like framing\n</type-context>\nliteral"
}

#[test]
fn structured_context_keeps_the_user_text_and_carries_the_type_id() {
    let params = json!({"type_context": {"id": "inspiration", "briefing": briefing()}});
    assert_eq!(
        split(&params, "Add the four items").unwrap(),
        (
            "Add the four items".to_owned(),
            Some(TypeContext {
                id: "inspiration".into(),
                briefing: Some(briefing().into()),
            })
        )
    );
}

#[test]
fn explicit_null_is_a_modern_prompt_that_names_no_type_and_is_never_framing() {
    // An ordinary turn whose text is the exact multiline legacy literal reaches the engine unchanged.
    for text in [literal_framing(), "[[type:notes]]\nliteral", "plain"] {
        assert_eq!(
            split(&json!({"type_context": null}), text).unwrap(),
            (text.to_owned(), None),
            "{text}"
        );
    }
}

#[test]
fn absent_field_reads_well_formed_legacy_framing_from_the_text() {
    let with_briefing = format!("[[type:inspiration]]\n{}\nAdd the four items", briefing());
    let bare = "[[type:inspiration]]\nAdd the four items";
    let legacy = |briefing: Option<&str>| {
        Some(TypeContext {
            id: "inspiration".into(),
            briefing: briefing.map(str::to_owned),
        })
    };
    for (text, expected) in [
        (with_briefing.as_str(), legacy(Some(briefing()))),
        (bare, legacy(None)),
    ] {
        let (body, context) = split(&json!({}), text).unwrap();
        assert_eq!(body, "Add the four items", "{text}");
        assert_eq!(context, expected, "{text}");
    }
}

#[test]
fn legacy_prefixes_that_are_not_a_standalone_tag_line_are_the_users_text() {
    for text in [
        "[[type:Bad Id]]\nkeep me",
        "[[type:]]\nkeep me",
        "[[type:ok]] keep me inline",
        "[[type:ok]]keep me",
        "[[type:ok\nkeep me",
        "[[type:ok]]\n<type-context>\nunclosed briefing, keep me",
        "[[type:ok]]\n<type-context>\nbriefing</type-context> trailing, keep me",
        "note: [[type:ok]]\nkeep me",
        "plain text",
    ] {
        assert_eq!(
            split(&json!({}), text).unwrap(),
            (text.to_owned(), None),
            "{text}"
        );
    }
}

#[test]
fn malformed_structured_context_is_refused() {
    for context in [
        json!({"id": "", "briefing": "x"}),
        json!({"id": "Bad Id", "briefing": "x"}),
        json!({"id": "t", "briefing": "  "}),
        json!({"briefing": "x"}),
        json!({"id": "t", "briefing": "x".repeat(MAX_BRIEFING_BYTES + 1)}),
        json!("t"),
    ] {
        assert!(
            split(&json!({"type_context": context}), "hi").is_err(),
            "{context}"
        );
    }
}
