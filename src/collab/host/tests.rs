use super::*;
use crate::collab::protocol::{SNAPSHOT_CHUNK_BYTES, parse_link};

#[test]
fn link_round_trips_room_key_and_write_token() {
    let room = generate_room_id().unwrap();
    let key = [7u8; 32];
    let token = [9u8; WRITE_TOKEN_BYTES];
    let link = format_link("https://relay.example.com", &room, &key, Some(&token)).unwrap();
    let parsed = parse_link(&link, "").unwrap();
    assert_eq!(parsed.room_id, room);
    assert_eq!(parsed.key, key);
    assert_eq!(parsed.write_token, Some(token));
    assert!(parsed.ws_url.ends_with(&format!("/r/{room}")));

    let view = format_link("https://relay.example.com", &room, &key, None).unwrap();
    let parsed = parse_link(&view, "").unwrap();
    assert_eq!(parsed.write_token, None);
    assert_eq!(parsed.key, key);
}

#[test]
fn snapshot_splits_into_bounded_chunks_with_one_final_marker() {
    let big = format!("\"{}\"", "x".repeat(SNAPSHOT_CHUNK_BYTES / 2 + 1024));
    let entries: Vec<Arc<str>> = [big.clone(), big.clone(), big]
        .into_iter()
        .map(Arc::from)
        .collect();
    let chunks: Vec<Value> = snapshot_chunks(Arc::new(entries.clone()))
        .map(|text| serde_json::from_str(&text).unwrap())
        .collect();
    assert!(
        chunks.len() >= 2,
        "entries larger than one chunk must split"
    );
    let finals = chunks.iter().filter(|c| c["final"] == true).count();
    assert_eq!(finals, 1);
    assert_eq!(chunks.last().unwrap()["final"], true);
    let recovered: usize = chunks
        .iter()
        .map(|c| c["entries"].as_array().unwrap().len())
        .sum();
    assert_eq!(recovered, entries.len());
}

#[test]
fn write_token_grant_requires_exact_match() {
    let token = [3u8; WRITE_TOKEN_BYTES];
    let presented = URL_SAFE_NO_PAD.encode(token);
    assert!(write_grant(Some(&token), Some(&presented)));
    assert!(!write_grant(Some(&token), None));
    assert!(!write_grant(Some(&token), Some("bogus")));
    assert!(!write_grant(None, Some(&presented)));
}

#[test]
fn steering_queue_bounds_prompts_and_coalesces_aborts() {
    let mut queue = Vec::new();
    for _ in 0..3 {
        assert_eq!(queue_steer(&mut queue, Steer::Abort), Ok(()));
    }
    assert_eq!(queue, vec![Steer::Abort]);
    for _ in 0..MAX_QUEUED_PROMPTS {
        assert_eq!(queue_steer(&mut queue, Steer::Prompt("go".into())), Ok(()));
    }
    assert_eq!(
        queue_steer(&mut queue, Steer::Prompt("go".into())),
        Err(Refusal::Busy)
    );
    assert_eq!(queue_steer(&mut queue, Steer::Abort), Ok(()));
    assert_eq!(queue.iter().filter(|s| **s == Steer::Abort).count(), 1);

    let mut fresh = Vec::new();
    let oversized = "x".repeat(MAX_PROMPT_BYTES + 1);
    assert_eq!(
        queue_steer(&mut fresh, Steer::Prompt(oversized)),
        Err(Refusal::PromptTooLarge)
    );
    let full = "y".repeat(MAX_PROMPT_BYTES);
    for _ in 0..MAX_QUEUED_PROMPT_BYTES / MAX_PROMPT_BYTES {
        assert_eq!(queue_steer(&mut fresh, Steer::Prompt(full.clone())), Ok(()));
    }
    assert_eq!(
        queue_steer(&mut fresh, Steer::Prompt("z".into())),
        Err(Refusal::Busy)
    );
}
