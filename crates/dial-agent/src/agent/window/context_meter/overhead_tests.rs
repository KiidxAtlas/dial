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
        .chain(request.tools.iter())
        .map(|v| v.to_string().len())
        .sum();
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
        request.messages[0].to_string().len()
    );
    assert_eq!(
        cost.request_tail_bytes,
        request.messages[5].to_string().len()
    );
    assert_eq!(
        cost.history_bytes,
        request.messages[1..5]
            .iter()
            .map(|v| v.to_string().len())
            .sum::<usize>()
    );
}
