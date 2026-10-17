use super::*;

#[test]
fn envelope_codec_roundtrip_and_rewrite() {
    let packed = pack_envelope(0xDEAD_BEEF, b"sealed");
    assert_eq!(&packed[..4], &[0xDE, 0xAD, 0xBE, 0xEF]);
    let (peer, payload) = unpack_envelope(&packed).unwrap();
    assert_eq!(peer, 0xDEAD_BEEF);
    assert_eq!(payload, b"sealed");
    let mut packed = pack_envelope(0, b"x");
    assert!(rewrite_envelope_peer(&mut packed, 7));
    assert_eq!(unpack_envelope(&packed).unwrap(), (7, &b"x"[..]));
    assert!(unpack_envelope(&[1, 2, 3]).is_none());
    assert_eq!(unpack_envelope(&[0, 0, 0, 1]).unwrap(), (1, &b""[..]));
    assert!(!rewrite_envelope_peer(&mut [0u8; 2], 1));
}

#[test]
fn control_messages_roundtrip() {
    for control in [
        Control::PeerJoined(3),
        Control::PeerLeft(9),
        Control::RoomClosed,
    ] {
        assert_eq!(Control::parse(&control.encode()), Some(control));
    }
    assert_eq!(Control::parse("{\"t\":\"other\"}"), None);
    assert_eq!(Control::parse("not json"), None);
}

#[test]
fn links_roundtrip_full_view_web_and_legacy_forms() {
    let key = [5u8; 32];
    let token = [9u8; 16];
    let id = "AAAAAAAAAAAAAAAAAAAAAA";
    let full = format_link("wss://relay.example.com", id, &key, Some(&token)).unwrap();
    assert!(full.starts_with("relay.example.com/r/AAAAAAAAAAAAAAAAAAAAAA."));
    let parsed = parse_link(&full, "").unwrap();
    assert_eq!(
        parsed.ws_url,
        "wss://relay.example.com/r/AAAAAAAAAAAAAAAAAAAAAA"
    );
    assert_eq!(parsed.key, key);
    assert_eq!(parsed.write_token, Some(token));

    let view = format_link("https://relay.example.com:8443", id, &key, None).unwrap();
    let parsed = parse_link(&view, "").unwrap();
    assert_eq!(
        parsed.ws_url,
        "wss://relay.example.com:8443/r/AAAAAAAAAAAAAAAAAAAAAA"
    );
    assert_eq!(parsed.write_token, None);

    let local = format_link("http://127.0.0.1:7466", id, &key, Some(&token)).unwrap();
    assert!(local.starts_with("ws://127.0.0.1:7466/r/"));
    assert_eq!(
        parse_link(&local, "").unwrap().ws_url,
        "ws://127.0.0.1:7466/r/AAAAAAAAAAAAAAAAAAAAAA"
    );

    let legacy = format!(
        "wss://relay.example.com/r/{id}%23{}",
        URL_SAFE_NO_PAD.encode(key)
    );
    assert_eq!(parse_link(&legacy, "").unwrap().key, key);

    let bare = format!("{id}.{}", URL_SAFE_NO_PAD.encode(key));
    assert!(parse_link(&bare, "").is_err());
    assert_eq!(
        parse_link(&bare, "wss://relay.example.com").unwrap().ws_url,
        "wss://relay.example.com/r/AAAAAAAAAAAAAAAAAAAAAA"
    );
}

#[test]
fn links_refuse_insecure_remote_and_malformed_keys() {
    let key = [1u8; 32];
    assert!(format_link("ws://relay.example.com", "AAAAAAAAAAAA", &key, None).is_err());
    assert!(parse_link("ws://relay.example.com/r/AAAAAAAAAAAA.AAAA", "").is_err());
    assert!(parse_link("wss://relay.example.com/r/AAAAAAAAAAAA.AAAA", "").is_err());
    assert!(parse_link("wss://relay.example.com/x/AAAAAAAAAAAA", "").is_err());
    assert!(parse_link("wss://relay.example.com/r/short.AAAA", "").is_err());
}

#[test]
fn snapshot_chunks_are_bounded_and_terminated() {
    let empty: Vec<String> = snapshot_chunks(Arc::new(Vec::new())).collect();
    assert_eq!(
        empty,
        vec!["{\"t\":\"snapshot-chunk\",\"entries\":[],\"final\":true}"]
    );
    let big = "x".repeat(SNAPSHOT_CHUNK_BYTES / 2 + 1);
    let entries = vec![format!("\"{big}\""), format!("\"{big}\""), "1".into()];
    let chunks: Vec<Value> =
        snapshot_chunks(Arc::new(entries.into_iter().map(Arc::from).collect()))
            .map(|c| serde_json::from_str(&c).unwrap())
            .collect();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(chunks[0]["final"], false);
    assert_eq!(chunks[1]["entries"].as_array().unwrap().len(), 2);
    assert_eq!(chunks[1]["final"], true);
}

#[test]
fn bounded_json_strips_images_then_clips_then_placeholders() {
    let image = format!("data:image/png;base64,{}", "A".repeat(4000));
    let value = json!({"type":"message","id":"m1","message":{"role":"user","content":[{"type":"image_url","image_url":{"url":image}}]}});
    let text = bounded_json_with(&value, 1000);
    assert!(text.len() <= 1000);
    assert!(text.contains("image omitted"));
    let long =
        json!({"type":"message","id":"m2","message":{"role":"tool","content":"y".repeat(100_000)}});
    let text = bounded_json_with(&long, 20_000);
    assert!(text.len() <= 20_000);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert!(
        parsed["message"]["content"]
            .as_str()
            .unwrap()
            .contains("clipped for collab")
    );
    let wide = Value::Array((0..5000).map(|i| json!(i)).collect());
    let text = bounded_json_with(&wide, 200);
    assert!(text.contains("placeholder"));
    assert!(clip_middle("héllo wörld", 4).chars().count() > 0);
}
