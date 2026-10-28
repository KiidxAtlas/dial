use super::super::Recency;
use super::super::domains::UNFILTERED;
use super::*;

fn query() -> Query<'static> {
    Query {
        text: "latest rust release",
        limit: 5,
        recency: Some(Recency::Month),
        filter: &UNFILTERED,
        today: 19_723,
    }
}

#[test]
fn body_is_a_streamed_unstored_codex_turn_forcing_web_search() {
    let sent = body(&query());
    assert_eq!(sent["model"], MODEL);
    assert!(
        sent["instructions"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["store"], false);
    assert_eq!(sent["tools"][0]["type"], "web_search");
    assert_eq!(sent["tool_choice"]["type"], "web_search");
    assert_eq!(sent["include"][0], "web_search_call.action.sources");
    assert_eq!(sent["input"][0]["role"], "user");
    let text = sent["input"][0]["content"][0]["text"].as_str().unwrap();
    assert_eq!(sent["input"][0]["content"][0]["type"], "input_text");
    assert!(text.contains("latest rust release"));
    assert!(text.contains("past month"));
}

#[test]
fn reads_sources_and_answer_from_native_items_kept_by_the_stream() {
    let content = vec![
        json!({"type": "dial_response_item", "item": {
            "type": "web_search_call",
            "action": {"sources": [{"url": "https://blog.rust-lang.org/", "title": "Blog"}]}
        }}),
        json!({"type": "dial_response_item", "item": {
            "type": "message",
            "content": [{
                "type": "output_text",
                "text": "Rust 1.80 shipped.",
                "annotations": [{
                    "type": "url_citation",
                    "url": "https://blog.rust-lang.org/2024/07/25/Rust-1.80.0.html",
                    "title": "Rust 1.80"
                }]
            }]
        }}),
    ];
    let parsed = read("streamed text", &content).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Rust 1.80 shipped."));
    let urls: Vec<&str> = parsed.results.iter().map(|r| r.url.as_str()).collect();
    // Sources the answer cites rank before sources the search merely visited.
    assert_eq!(
        urls,
        [
            "https://blog.rust-lang.org/2024/07/25/Rust-1.80.0.html",
            "https://blog.rust-lang.org/"
        ]
    );
}

#[test]
fn falls_back_to_streamed_text_when_no_message_item_was_kept() {
    let content = vec![json!({"type": "dial_response_item", "item": {
        "type": "web_search_call",
        "action": {"sources": [{"url": "https://read-only.example/"}]}
    }})];
    let parsed = read("  Rust 1.80 shipped.  ", &content).unwrap();
    assert_eq!(parsed.answer.as_deref(), Some("Rust 1.80 shipped."));
    assert_eq!(parsed.results[0].url, "https://read-only.example/");
}

#[test]
fn refuses_a_turn_that_ran_no_web_search() {
    let content = vec![json!({"type": "dial_response_item", "item": {
        "type": "message",
        "content": [{"type": "output_text", "text": "From memory."}]
    }})];
    assert!(read("From memory.", &content).is_err());
}

#[test]
fn ignores_items_that_are_not_native_response_wrappers() {
    let content = vec![json!({"type": "text", "text": "stray"})];
    assert!(read("stray", &content).is_err());
}
