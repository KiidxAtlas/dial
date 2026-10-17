use super::*;

fn request(messages: Vec<Value>) -> Chat {
    Chat {
        messages,
        tools: vec![json!({"type":"function","function":{"name":"read"}})],
        temperature: 0.2,
        max_tokens: 64,
        seed: Some(3),
    }
}

#[test]
fn unchanged_projection_keeps_the_borrowed_request() {
    let original = request(vec![json!({"role":"user","content":"hi"})]);
    assert!(projected_request(&original, None).is_none());
}

#[test]
fn projected_history_replaces_only_messages() {
    let original = request(vec![json!({"role":"user","content":"old"})]);
    let projected = projected_request(
        &original,
        Some(vec![json!({"role":"user","content":"new"})]),
    )
    .expect("a changed projection produces a request");
    assert_eq!(
        projected.messages,
        vec![json!({"role":"user","content":"new"})]
    );
    assert_eq!(projected.tools, original.tools);
    assert_eq!(projected.temperature, original.temperature);
    assert_eq!(projected.max_tokens, original.max_tokens);
    assert_eq!(projected.seed, original.seed);
    assert_eq!(original.messages[0]["content"], "old");
}
