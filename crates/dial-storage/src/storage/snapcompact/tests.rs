use super::*;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut out = PNG_SIGNATURE.to_vec();
    let mut header = Vec::new();
    header.extend(width.to_be_bytes());
    header.extend(height.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(
        &mut out,
        b"IDAT",
        &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    );
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(kind);
    out.extend(data);
    out.extend(crc32(&[kind, data]).to_be_bytes());
}

fn frame_bytes(bytes: &[u8]) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", STANDARD.encode(bytes))}})
}

fn frame(width: u32, height: u32) -> Value {
    frame_bytes(&png(width, height))
}

/// Body of a single-item view: the text between the header line and the visible `next:` footer,
/// without the newline the view appends after each body.
fn record_body(text: &str) -> &str {
    let after_header = text.split_once('\n').unwrap().1;
    after_header.rsplit_once("\nnext: ").unwrap().0
}

fn archive(layers: Vec<Value>) -> Value {
    json!({"role": "user", "dial_snapcompact": {"version": VERSION, "layers": layers}})
}

fn msg(role: &str, content: &str) -> Value {
    json!({"role": role, "content": content})
}

fn layer_json(messages: Vec<Value>, source_text: &str, frames: Vec<Value>) -> Value {
    serde_json::to_value(Layer {
        id: layer_id(&messages, source_text).unwrap(),
        color: [1, 2, 3],
        source_text: source_text.into(),
        messages,
        frames,
    })
    .unwrap()
}

/// Layer 0 (older) with a tool call and its result; layer 1 (distant) mentions the same path.
fn fixture() -> Vec<Value> {
    let older = layer_json(
        vec![
            json!({"role": "assistant", "tool_calls": [{"id": "call_9", "function": {"name": "read", "arguments": "{\"path\":\"src/lib.rs\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "call_9", "content": "needle in src/lib.rs"}),
            msg("user", "unrelated words"),
        ],
        "source zero",
        vec![frame(4, 4)],
    );
    let distant = layer_json(
        vec![
            msg("assistant", "later note about src/lib.rs"),
            msg("user", "nothing here"),
        ],
        "source one",
        vec![frame(4, 4)],
    );
    vec![archive(vec![older]), archive(vec![distant])]
}

fn run(messages: &[Value], args: Value) -> ToolOutput {
    view(messages, &args).unwrap()
}

/// The visible `next:` cursor (JSON, or `null`) printed in the footer.
fn footer_next(text: &str) -> Value {
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("next: "))
        .expect("visible next line");
    serde_json::from_str(line).unwrap()
}

/// Sums the item spans of a paged view, following the visible footer cursor to the end.
fn covered_bytes(messages: &[Value], mut args: Value) -> usize {
    let mut covered = 0usize;
    loop {
        let out = run(messages, args.clone());
        assert!(out.text.len() <= MAX_OUTPUT_BYTES);
        let data = out.data.unwrap();
        for item in data["items"].as_array().unwrap() {
            covered +=
                item["end"].as_u64().unwrap() as usize - item["start"].as_u64().unwrap() as usize;
        }
        let next = footer_next(&out.text);
        assert_eq!(next, data["next"]);
        if next.is_null() {
            return covered;
        }
        args["offset"] = next["offset"].clone();
        args["start"] = next["start"].clone();
    }
}

#[test]
fn layer_id_is_stable_and_covers_messages_and_source() {
    let messages = vec![msg("user", "hi")];
    let id = layer_id(&messages, "text").unwrap();
    assert_eq!(id, layer_id(&messages, "text").unwrap());
    assert_eq!(id.len(), 64);
    assert_ne!(id, layer_id(&messages, "other").unwrap());
    assert_ne!(id, layer_id(&[msg("user", "bye")], "text").unwrap());
}

#[test]
fn repeated_identical_archive_is_deduplicated_and_v1_is_ignored() {
    let layer = layer_json(vec![msg("user", "a")], "a", vec![frame(2, 2)]);
    let messages = vec![
        archive(vec![layer.clone()]),
        archive(vec![layer]),
        json!({"role": "user", "dial_snapcompact": {"version": 1, "source_text": "legacy"}}),
    ];
    let found = layers(&messages).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].source_text, "a");
    assert_eq!(found[0].color, [1, 2, 3]);
}

#[test]
fn conflicting_frames_for_one_layer_id_fail_closed() {
    let messages = vec![
        archive(vec![layer_json(
            vec![msg("user", "a")],
            "a",
            vec![frame(2, 2)],
        )]),
        archive(vec![layer_json(
            vec![msg("user", "a")],
            "a",
            vec![frame(3, 3)],
        )]),
    ];
    assert!(layers(&messages).is_err());
}

#[test]
fn malformed_layer_metadata_is_refused() {
    let good = layer_json(vec![msg("user", "a")], "a", vec![frame(2, 2)]);
    let mut tampered = good.clone();
    tampered["source_text"] = json!("edited after digest");
    let mut bad_id = good.clone();
    bad_id["id"] = json!("not-a-digest");
    let mut extra = good.clone();
    extra["surprise"] = json!(true);
    let mut nested = good.clone();
    nested["messages"] = json!([{"role": "user", "dial_snapcompact": {}}]);
    let mut bad_color = good.clone();
    bad_color["color"] = json!([1, 2]);
    let cases = [
        ("digest mismatch", tampered),
        ("malformed id", bad_id),
        ("unknown field", extra),
        ("nested archive", nested),
        ("bad color", bad_color),
    ];
    for (name, layer) in cases {
        assert!(layers(&[archive(vec![layer])]).is_err(), "{name}");
    }
    assert!(
        layers(&[json!({"role":"user","dial_snapcompact":{"version":3,"layers":[]}})]).is_err()
    );
    assert!(layers(&[json!({"role":"user","dial_snapcompact":{"version":2}})]).is_err());
}

#[test]
fn png_structure_table_rejects_corruption_and_oversize() {
    let valid = png(4, 4);
    let mut bad_signature = valid.clone();
    bad_signature[0] ^= 1;
    let mut bad_crc = valid.clone();
    bad_crc[16] ^= 1;
    let truncated = valid[..valid.len() - 1].to_vec();
    let mut trailing = valid.clone();
    trailing.push(0);
    let mut no_end = PNG_SIGNATURE.to_vec();
    let mut header = Vec::new();
    header.extend(4u32.to_be_bytes());
    header.extend(4u32.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]);
    chunk(&mut no_end, b"IHDR", &header);
    chunk(&mut no_end, b"IDAT", &[0]);
    let oversize = png(MAX_EDGE + 1, 1);
    let cases = [
        ("signature", bad_signature),
        ("crc", bad_crc),
        ("truncated", truncated),
        ("trailing bytes", trailing),
        ("missing IEND", no_end),
        ("oversize edge", oversize),
    ];
    for (name, bytes) in cases {
        assert!(validate_png(&bytes).is_err(), "{name}");
        assert!(
            decode_frame(&frame_bytes(&bytes)).is_err(),
            "{name} via frame"
        );
    }
    validate_png(&png(MAX_EDGE, MAX_EDGE)).unwrap();
}

#[test]
fn frame_shape_and_frame_count_are_bounded() {
    let mut extra_field = frame(2, 2);
    extra_field["cache"] = json!(true);
    let not_png = json!({"type": "image_url", "image_url": {"url": "https://example.test/a.png"}});
    assert!(decode_frame(&extra_field).is_err());
    assert!(decode_frame(&not_png).is_err());
    let frames = vec![frame(2, 2); MAX_FRAMES + 1];
    let layer = layer_json(vec![msg("user", "a")], "a", frames);
    assert!(layers(&[archive(vec![layer])]).is_err());
}

#[test]
fn layer_count_budget_is_enforced() {
    let many: Vec<Value> = (0..=MAX_LAYERS)
        .map(|i| layer_json(vec![msg("user", &i.to_string())], "s", vec![frame(1, 1)]))
        .collect();
    assert!(layers(&[archive(many.clone())]).is_err());
    assert_eq!(
        layers(&[archive(many[..MAX_LAYERS].to_vec())])
            .unwrap()
            .len(),
        MAX_LAYERS
    );
}

#[test]
fn readable_is_deterministic_and_keeps_tool_fields() {
    let messages = fixture();
    let layer = layers(&messages).unwrap().remove(0);
    let first = readable(&layer.messages).unwrap();
    assert_eq!(first, readable(&layer.messages).unwrap());
    assert!(first.contains("TOOL CALL id=call_9 name=read"));
    assert!(first.contains("TOOL RESULT tool_call_id=call_9"));
    assert!(first.contains("needle in src/lib.rs"));
}

#[test]
fn index_lists_layers_and_empty_archive_reports_none() {
    let messages = fixture();
    let out = run(&messages, json!({"view": "layers", "query": ""}));
    let data = out.data.clone().unwrap();
    assert_eq!(data["items"].as_array().unwrap().len(), 2);
    assert_eq!(data["layers_total"], 2);
    assert!(data["items"][0]["ref"].as_str().unwrap().len() == 64);
    assert_eq!(footer_next(&out.text), Value::Null);
    assert!(out.text.contains("coverage complete"));
    let empty = run(&[], json!({"view": "layers", "query": ""}));
    assert!(empty.text.contains("no snapcompact records selected"));
    assert_eq!(empty.data.unwrap()["next"], Value::Null);
}

#[test]
fn visible_footer_carries_cursor_totals_and_history_caution() {
    let messages = fixture();
    let out = run(
        &messages,
        json!({"view": "layers", "query": "", "limit": 1}),
    );
    assert_eq!(footer_next(&out.text), json!({"offset": 1, "start": 0}));
    assert_eq!(
        out.data.as_ref().unwrap()["next"],
        json!({"offset": 1, "start": 0})
    );
    assert!(
        out.text
            .contains("shown 1 of 2 items from 2 layers; coverage partial, continue at next")
    );
    assert!(out.text.contains("history: archived compaction history"));
    let last = run(
        &messages,
        json!({"view": "layers", "query": "", "offset": 1}),
    );
    assert_eq!(footer_next(&last.text), Value::Null);
    assert!(last.text.contains("coverage complete"));
}

#[test]
fn query_links_tool_pairs_and_distant_path_references() {
    let messages = fixture();
    let out = run(&messages, json!({"view": "layers", "query": "NEEDLE"}));
    let items = out.data.unwrap()["items"].clone();
    let items = items.as_array().unwrap();
    let matched = items.iter().find(|i| i["kind"] == "match").unwrap();
    assert_eq!(matched["record"], 1);
    let layers_all = layers(&messages).unwrap();
    assert_eq!(matched["layer"], layers_all[0].id);
    let call = items
        .iter()
        .find(|i| i["kind"] == "related" && i["record"] == 0)
        .expect("tool call linked by shared id");
    assert!(call["reasons"].to_string().contains("tool_call_id call_9"));
    let distant = items
        .iter()
        .find(|i| i["layer"] == layers_all[1].id)
        .expect("distant record linked by path token");
    assert_eq!(distant["kind"], "related");
    assert!(
        distant["reasons"]
            .to_string()
            .contains("reference src/lib.rs")
    );
    assert!(
        !items
            .iter()
            .any(|i| i["layer"] == layers_all[1].id && i["record"] == 1)
    );
}

#[test]
fn record_selector_returns_exact_json_of_the_selected_record() {
    let messages = fixture();
    let all = layers(&messages).unwrap();
    let id = all[0].id.clone();
    for (index, expected) in all[0].messages.iter().enumerate() {
        let out = run(
            &messages,
            json!({"view": "layers", "query": "", "layer": id, "record": index}),
        );
        let data = out.data.clone().unwrap();
        assert_eq!(data["items_total"], 1);
        assert_eq!(data["items"][0]["record"], index);
        assert_eq!(data["items"][0]["ref"], format!("{id}#{index}"));
        let body = record_body(&out.text);
        assert_eq!(body, serde_json::to_string(expected).unwrap());
        assert_eq!(serde_json::from_str::<Value>(body).unwrap(), *expected);
        let digest = digest_hex(body.as_bytes());
        assert_eq!(data["items"][0]["digest"], digest);
        assert!(
            out.text
                .lines()
                .next()
                .unwrap()
                .contains(&format!("digest={digest}"))
        );
    }
}

#[test]
fn message_records_are_recovered_as_exact_canonical_json() {
    let messages_in = vec![
        json!({"role": "user", "content": 1, "meta": {"s": "1", "n": null, "esc": "a\"b\nc"}}),
        json!({"role": "assistant", "content": [{"type": "text", "text": "line\nbreak"}]}),
    ];
    let archived = vec![archive(vec![layer_json(
        messages_in.clone(),
        "src",
        vec![frame(2, 2)],
    )])];
    let id = layers(&archived).unwrap()[0].id.clone();
    for (index, message) in messages_in.iter().enumerate() {
        let out = run(
            &archived,
            json!({"view": "layers", "query": "", "layer": id, "record": index}),
        );
        let body = record_body(&out.text);
        assert_eq!(body, serde_json::to_string(message).unwrap());
        assert_eq!(serde_json::from_str::<Value>(body).unwrap(), *message);
    }
}

#[test]
fn record_selector_requires_layer_empty_query_and_an_existing_index() {
    let messages = fixture();
    let id = layers(&messages).unwrap()[0].id.clone();
    let cases = [
        json!({"view": "layers", "query": "", "record": 0}),
        json!({"view": "layers", "query": "needle", "layer": id, "record": 1}),
        json!({"view": "layers", "query": "", "layer": id, "record": 3}),
        json!({"view": "layers", "query": "", "layer": id, "record": -1}),
        json!({"view": "layers", "query": "", "layer": id, "record": "0"}),
        json!({"view": "layers", "query": "", "layer": "0".repeat(64), "record": 0}),
    ];
    for args in cases {
        assert!(view(&messages, &args).is_err(), "{args}");
    }
}

#[test]
fn legacy_source_text_layer_is_returned_literally_as_its_only_record() {
    let messages = vec![archive(vec![layer_json(
        vec![],
        "legacy source",
        vec![frame(2, 2)],
    )])];
    let id = layers(&messages).unwrap()[0].id.clone();
    let out = run(
        &messages,
        json!({"view": "layers", "query": "", "layer": id}),
    );
    assert_eq!(record_body(&out.text), "legacy source");
    let record = run(
        &messages,
        json!({"view": "layers", "query": "", "layer": id, "record": 0}),
    );
    assert_eq!(record_body(&record.text), "legacy source");
    assert!(
        view(
            &messages,
            &json!({"view": "layers", "query": "", "layer": id, "record": 1})
        )
        .is_err()
    );
}

#[test]
fn unknown_layer_and_bad_arguments_fail_closed() {
    let messages = fixture();
    let id = layers(&messages).unwrap()[0].id.clone();
    let unknown = json!({"view": "layers", "query": "", "layer": "0".repeat(64)});
    assert!(view(&messages, &unknown).is_err());
    let cases = [
        json!({"query": ""}),
        json!({"query": "", "layer": id}),
        json!({"view": "other", "query": "x"}),
        json!({"view": "layers", "layer": "x"}),
        json!({"view": "layers", "query": "x", "limit": 0}),
        json!({"view": "layers", "query": "x", "limit": 9}),
        json!({"view": "layers", "query": "x", "offset": -1}),
        json!({"view": "layers", "query": "x", "start": "1"}),
        json!({"view": "layers", "query": "x", "format": "png"}),
        json!({"view": "layers", "query": "x", "extra": 1}),
        json!({"view": "layers", "query": 5}),
    ];
    for args in cases {
        assert!(view(&messages, &args).is_err(), "{args}");
    }
}

#[test]
fn layer_view_pages_exact_spans_within_output_budget() {
    let big = "x".repeat(40_000);
    let source = "é".repeat(9_000);
    let big_message = msg("user", &big);
    let messages = vec![archive(vec![
        layer_json(vec![big_message.clone()], "big", vec![frame(2, 2)]),
        layer_json(vec![], &source, vec![frame(2, 2)]),
    ])];
    let all = layers(&messages).unwrap();
    let source_id = all[1].id.clone();
    let big_id = all[0].id.clone();
    assert!(
        view(
            &messages,
            &json!({"view": "layers", "query": "", "layer": source_id, "start": 1})
        )
        .is_err()
    );
    assert_eq!(
        covered_bytes(
            &messages,
            json!({"view": "layers", "query": "", "layer": source_id, "limit": 8})
        ),
        source.len()
    );
    assert_eq!(
        covered_bytes(
            &messages,
            json!({"view": "layers", "query": "", "layer": big_id, "record": 0})
        ),
        serde_json::to_string(&big_message).unwrap().len()
    );
}

#[test]
fn schema_adds_view_fields_without_changing_existing_contract() {
    let mut base = json!({
        "type": "object",
        "properties": {"query": {"type": "string"}, "limit": {"type": "integer", "default": 10}},
        "required": ["query"]
    });
    schema(&mut base);
    assert_eq!(base["properties"]["limit"]["default"], 10);
    assert_eq!(base["properties"]["query"], json!({"type": "string"}));
    assert_eq!(base["required"], json!(["query"]));
    assert_eq!(base["properties"]["view"]["enum"], json!(["layers"]));
    for name in ["view", "layer", "record", "offset", "start", "format"] {
        assert!(base["properties"].get(name).is_some(), "{name}");
    }
}

#[test]
fn path_links_match_whole_reference_tokens_not_substrings() {
    let layer = layer_json(
        vec![
            msg("user", "needle a.rs"),
            msg("assistant", "data.rs"),
            msg("assistant", "a.rs"),
        ],
        "paths",
        vec![frame(1, 1)],
    );
    let out = run(
        &[archive(vec![layer])],
        json!({"view":"layers","query":"needle"}),
    );
    let data = out.data.unwrap();
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|item| item["record"] == 2));
    assert!(!items.iter().any(|item| item["record"] == 1));
    assert!(out.text.contains("at most 8 reference tokens"));
}
