use super::*;
use crate::collab::protocol::{SNAPSHOT_CHUNK_BYTES, parse_link};

#[test]
fn relayed_admission_proofs_cannot_forge_or_replay_mutations() {
    let token = [3; WRITE_TOKEN_BYTES];
    let mut guest = GuestPeer {
        name: "writer".into(),
        writable: true,
        challenge: "fresh".into(),
        request_sequence: 0,
    };
    let mut prompt = json!({"t":"prompt","text":"safe","requestSeq":1,
        "authorization":request_proof(&token,"fresh",1,"prompt","safe")});
    let mut forged = prompt.clone();
    forged["text"] = json!("unsafe");
    assert!(!guest.authorize(Some(&token), &forged));
    forged = prompt.clone();
    forged["authorization"] = json!(write_proof(&token, "fresh"));
    assert!(
        !guest.authorize(Some(&token), &forged),
        "an admission proof is not a request proof"
    );
    assert!(guest.authorize(Some(&token), &prompt));
    assert!(
        !guest.authorize(Some(&token), &prompt),
        "the same signed action runs at most once"
    );
    prompt["requestSeq"] = json!(2);
    assert!(
        !guest.authorize(Some(&token), &prompt),
        "a view holder cannot advance a captured action"
    );
    prompt["authorization"] = json!(request_proof(&token, "previous", 2, "prompt", "safe"));
    assert!(
        !guest.authorize(Some(&token), &prompt),
        "old connections cannot authorize new actions"
    );
    prompt["authorization"] = json!(request_proof(&token, "fresh", 2, "prompt", "safe"));
    assert!(guest.authorize(Some(&token), &prompt));
}

#[test]
fn link_round_trips_room_key_and_write_token() {
    let room = generate_room_id().unwrap();
    let key = [7u8; 32];
    let token = [9u8; WRITE_TOKEN_BYTES];
    let link = format_link(
        "https://relay.example.com",
        &room,
        &key,
        &[8; 32],
        Some(&token),
    )
    .unwrap();
    let parsed = parse_link(&link, "").unwrap();
    assert_eq!(parsed.room_id, room);
    assert_eq!(parsed.key, key);
    assert_eq!(parsed.write_token, Some(token));
    assert!(parsed.ws_url.ends_with(&format!("/r/{room}")));

    let view = format_link("https://relay.example.com", &room, &key, &[8; 32], None).unwrap();
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
    let presented = write_proof(&token, "fresh-challenge");
    assert!(write_grant(
        Some(&token),
        "fresh-challenge",
        Some(&presented)
    ));
    assert!(!write_grant(
        Some(&token),
        "different-peer",
        Some(&presented)
    ));
    assert!(!write_grant(Some(&token), "fresh-challenge", None));
    assert!(!write_grant(Some(&token), "fresh-challenge", Some("bogus")));
    assert!(!write_grant(None, "fresh-challenge", Some(&presented)));
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

#[test]
fn publishing_is_incremental_and_resets_rewritten_history() {
    let mut session = Session::ephemeral();
    session
        .append(json!({"role":"user","content":"first"}))
        .unwrap();
    let mut entries = Arc::new(vec![Arc::from(bounded_json(
        &session.archived_messages()[0],
    ))]);
    let mut cursor = None;
    let mut frames = Vec::new();
    publish_entries(&mut entries, &mut cursor, &session, |text| {
        frames.push(text)
    });
    assert!(frames.is_empty(), "startup snapshot must not be replayed");
    let snapshot = entries.clone();
    for n in 0..128 {
        session
            .append(json!({"role":"assistant","content":format!("answer {n}")}))
            .unwrap();
        publish_entries(&mut entries, &mut cursor, &session, |text| {
            frames.push(text)
        });
        assert_eq!(frames.len(), n + 1);
        assert_eq!(
            serde_json::from_str::<Value>(&frames[n]).unwrap()["seq"],
            n + 1
        );
    }
    assert_eq!(snapshot.len(), 1, "queued snapshots remain immutable");
    assert!(
        Arc::ptr_eq(&snapshot[0], &entries[0]),
        "old serialized bodies are shared"
    );
    session.compact("summary".into(), 1).unwrap();
    publish_entries(&mut entries, &mut cursor, &session, |text| {
        frames.push(text)
    });
    assert_eq!(frames.len(), 128, "compaction does not rewrite the archive");
    session
        .append(json!({"role":"user","content":"abandoned"}))
        .unwrap();
    publish_entries(&mut entries, &mut cursor, &session, |_| {});
    session.rewind(1).unwrap();
    session
        .append(json!({"role":"user","content":"replacement"}))
        .unwrap();
    frames.clear();
    publish_entries(&mut entries, &mut cursor, &session, |text| {
        frames.push(text)
    });
    assert_eq!(
        serde_json::from_str::<Value>(&frames[0]).unwrap()["t"],
        "reset"
    );
    assert_eq!(
        entries
            .iter()
            .map(|text| serde_json::from_str::<Value>(text).unwrap())
            .collect::<Vec<_>>(),
        session.archived_messages()
    );
}
