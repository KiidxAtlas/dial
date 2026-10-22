use super::*;
use serde_json::json;
#[test]
fn categories_are_disjoint_and_preserve_the_total_estimate() {
    let mut request = Chat {
        messages: vec![],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 8192,
        seed: None,
    };
    request.messages = vec![
        json!({"role":"system","content":"rules \"quoted\"\n\\escaped 雪","dial_transient":true}),
        json!({"role":"developer","content":"developer café"}),
        json!({"role":"tool","content":"evidence\t\u{0001}"}),
        json!({"role":"user","content":"[Current task evidence packet; harness context]\n{}"}),
        json!({"role":"user","content":"task 日本語"}),
        json!({"role":"user","content":"transient 尾","dial_transient":true}),
    ];
    request.tools = vec![json!({"type":"function","description":"\"雪\"\n\\"})];
    let cost = overhead(&request);
    let total: usize = request
        .messages
        .iter()
        .map(wire_message_bytes)
        .sum::<usize>()
        + request.tools.iter().map(json_bytes).sum::<usize>();
    assert_eq!(cost.total(), total);
    assert_eq!(
        cost.economy_system_bytes
            + cost.history_bytes
            + cost.request_tail_bytes
            + cost.tool_schema_bytes,
        total
    );
    assert_eq!(
        cost.economy_system_bytes,
        request.messages[..2]
            .iter()
            .map(wire_message_bytes)
            .sum::<usize>()
    );
    assert_eq!(
        cost.request_tail_bytes,
        wire_message_bytes(&request.messages[5])
    );
    assert_eq!(
        cost.history_bytes,
        request.messages[2..5]
            .iter()
            .map(wire_message_bytes)
            .sum::<usize>()
    );
}

#[test]
fn internal_evidence_metadata_does_not_inflate_request_economy() {
    let mut request = Chat {
        messages: vec![json!({"role":"tool","content":"source evidence"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 512,
        seed: None,
    };
    let before = overhead(&request);
    request.messages[0]["dial_evidence"] =
        json!({"artifact":"artifact://source", "internal":"x".repeat(40_000)});
    let after = overhead(&request);
    assert_eq!(before.total(), after.total());
    assert_eq!(before.history_bytes, after.history_bytes);
    assert_eq!(before.tool_result_bytes, after.tool_result_bytes);
}
