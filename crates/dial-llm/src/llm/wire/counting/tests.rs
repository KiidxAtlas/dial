use super::*;
use serde_json::json;

const KNOWN: (&str, &str) = ("openai", "gpt-4o");
const UNKNOWN: (&str, &str) = ("local", "gpt-4o");

#[test]
fn only_exact_official_openai_ids_map_to_a_documented_encoding() {
    let cases = [
        ("openai", "gpt-4o", Some(Encoding::O200kBase)),
        ("openai", "gpt-5", Some(Encoding::O200kBase)),
        ("openai", "o4-mini", Some(Encoding::O200kBase)),
        ("openai", "gpt-4", Some(Encoding::Cl100kBase)),
        ("openai", "gpt-3.5-turbo", Some(Encoding::Cl100kBase)),
        // Official snapshot and variant names are prefix entries upstream; they stay unclassified.
        ("openai", "gpt-4o-2024-08-06", None),
        ("openai", "gpt-4o-mini", None),
        ("openai", "gpt-5-mini", None),
        ("openai", "o3-mini", None),
        ("openai", "gpt-4.1-preview", None),
        ("openai", "", None),
        ("anthropic", "gpt-4o", None),
        ("local", "gpt-4o", None),
        ("chatgpt", "gpt-5", None),
    ];
    for (provider, model, expected) in cases {
        assert_eq!(
            encoding_for(provider, model),
            expected,
            "{provider}/{model}"
        );
    }
}

#[test]
fn known_models_count_with_the_tokenizer_and_unknown_models_use_three_bytes_per_token() {
    // "hello world" is two tokens under both documented encodings.
    assert_eq!(text_tokens(KNOWN.0, KNOWN.1, "hello world"), 2);
    assert_eq!(text_tokens("openai", "gpt-4", "hello world"), 2);
    // Unknown: 11 bytes at three bytes per token, rounded up.
    assert_eq!(text_tokens("anthropic", "claude", "hello world"), 4);
    assert_eq!(text_tokens(KNOWN.0, KNOWN.1, ""), 0);
    assert_eq!(text_tokens(UNKNOWN.0, UNKNOWN.1, ""), 0);
}

#[test]
fn unicode_and_code_are_counted_by_bytes_when_unknown_and_by_tokens_when_known() {
    // (text, bytes div_ceil 3): multi-byte characters count their UTF-8 bytes.
    let cases = [
        ("héllo", 2),
        ("日本語", 3),
        ("fn main() {\n    println!(\"hi\");\n}\n", 12),
    ];
    for (text, unknown) in cases {
        assert_eq!(text_tokens(UNKNOWN.0, UNKNOWN.1, text), unknown, "{text:?}");
        let known = text_tokens(KNOWN.0, KNOWN.1, text);
        assert!(known > 0, "{text:?}");
        // The cache must return the same count on a repeated lookup.
        assert_eq!(text_tokens(KNOWN.0, KNOWN.1, text), known, "{text:?}");
    }
}

#[test]
fn count_cache_stays_bounded_under_many_distinct_texts() {
    for index in 0..CACHE_LIMIT + 16 {
        counted(Encoding::O200kBase, &format!("probe {index}"));
    }
    assert!(COUNTS.lock().counts.len() <= CACHE_LIMIT);
}

#[test]
fn harness_bookkeeping_never_reaches_the_count() {
    let plain = json!({"role": "user", "content": "read the file"});
    let annotated = json!({
        "role": "user",
        "content": "read the file",
        "dial_transient": true,
        "dial_id": "bookkeeping",
    });
    for (provider, model) in [KNOWN, UNKNOWN] {
        assert_eq!(
            message_tokens(provider, model, &annotated),
            message_tokens(provider, model, &plain),
            "{provider}/{model}"
        );
    }
}

#[test]
fn image_and_native_floors_hold_for_known_and_unknown_models() {
    let images = json!({
        "role": "user",
        "content": "look",
        "dial_images": [{"path": "a.png"}, {"path": "b.png"}],
    });
    let plain = json!({"role": "assistant", "content": "x"});
    let native = json!({
        "role": "assistant",
        "content": "x",
        "dial_provider_content": [{"type": "text", "text": "a longer provider-native replay"}],
    });
    let two_images = 2 * IMAGE_ESTIMATE_TOKENS as u64;
    for (provider, model) in [KNOWN, UNKNOWN] {
        assert!(message_tokens(provider, model, &images) >= two_images);
        assert!(
            message_tokens(provider, model, &native) >= message_tokens(provider, model, &plain)
        );
    }
    // Unknown models keep the byte estimate exactly, including the image and native floors.
    assert_eq!(
        message_tokens(UNKNOWN.0, UNKNOWN.1, &images),
        estimate(&images)
    );
    assert_eq!(
        message_tokens(UNKNOWN.0, UNKNOWN.1, &native),
        estimate(&native)
    );
}

#[test]
fn tool_schemas_count_in_full_and_unknown_requests_match_the_old_estimate() {
    let tool =
        json!({"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}});
    let annotated = json!({
        "type": "function",
        "function": {"name": "read", "parameters": {"type": "object"}},
        "dial_contract": "bookkeeping",
    });
    assert!(
        schema_tokens(KNOWN.0, KNOWN.1, std::slice::from_ref(&annotated))
            > schema_tokens(KNOWN.0, KNOWN.1, std::slice::from_ref(&tool))
    );

    let messages = vec![
        json!({"role": "system", "content": "fixed"}),
        json!({"role": "user", "content": "hi"}),
    ];
    let tools = vec![tool];
    assert_eq!(
        request_tokens(UNKNOWN.0, UNKNOWN.1, &messages, &tools),
        estimate_parts(&messages, &tools)
    );
    let known_sum = messages
        .iter()
        .map(|message| message_tokens(KNOWN.0, KNOWN.1, message))
        .sum::<u64>()
        + schema_tokens(KNOWN.0, KNOWN.1, &tools);
    assert_eq!(
        request_tokens(KNOWN.0, KNOWN.1, &messages, &tools),
        known_sum
    );
    assert_eq!(request_tokens(KNOWN.0, KNOWN.1, &[], &[]), 0);
    assert_eq!(request_tokens(UNKNOWN.0, UNKNOWN.1, &[], &[]), 0);
}
