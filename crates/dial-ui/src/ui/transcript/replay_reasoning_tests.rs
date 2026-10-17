use super::*;
use serde_json::json;

fn thinking_texts(transcript: &Transcript) -> Vec<String> {
    transcript
        .blocks
        .iter()
        .filter_map(|block| match &block.kind {
            Kind::Thinking(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn replay_shows_reasoning_from_content_and_from_provider_thinking_blocks() {
    let mut transcript = Transcript::default();
    let mut replay = Replay::default();
    replay.push(
        &mut transcript,
        &json!({"role":"assistant","content":"done","reasoning_content":"plain reasoning"}),
    );
    replay.push(
        &mut transcript,
        &json!({"role":"assistant","content":"again",
            "dial_provider_content":[
                {"type":"thinking","thinking":"typed ","signature":"s"},
                {"type":"thinking","thinking":"reasoning","signature":"s"},
                {"type":"text","text":"again"}]}),
    );
    replay.push(
        &mut transcript,
        &json!({"role":"assistant","content":"no thoughts"}),
    );
    assert_eq!(
        thinking_texts(&transcript),
        vec!["plain reasoning".to_owned(), "typed reasoning".to_owned()]
    );
}
