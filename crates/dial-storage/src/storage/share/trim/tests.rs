use super::*;
use crate::storage::share::decrypt_share;

/// Incompressible-enough text so the sealed size tracks the plaintext size.
fn noisy(len: usize, seed: u64) -> String {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(
                b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
                    [(state % 62) as usize],
            )
        })
        .collect()
}

fn transcript(records: &[Value]) -> Vec<u8> {
    serde_json::to_vec(records).expect("records serialize")
}

fn opened(share: &EncryptedShare) -> Vec<Value> {
    let plain = decrypt_share(&share.bytes, &share.key_fragment).expect("share opens");
    serde_json::from_slice(&plain).expect("share is a record array")
}

#[test]
fn share_within_the_cap_is_sealed_untouched() {
    let records = vec![
        json!({"type": "session", "id": "s"}),
        json!({"type": "message", "seq": 0, "content": "hello"}),
    ];
    let fitted = fit_to_cap(&transcript(&records), 1 << 20).expect("fits");
    assert!(fitted.trims.is_empty());
    assert_eq!(opened(&fitted.share), records);
}

#[test]
fn oversized_share_fits_the_cap_and_keeps_header_and_newest_records() {
    let mut records = vec![json!({"type": "session", "id": "s"})];
    for seq in 0..200 {
        records.push(json!({"type": "message", "seq": seq, "content": noisy(4096, seq)}));
    }
    let cap = 64 * 1024;
    let fitted = fit_to_cap(&transcript(&records), cap).expect("fits after trimming");
    assert!(fitted.share.bytes.len() <= cap);
    assert!(
        fitted
            .trims
            .iter()
            .any(|trim| matches!(trim, Trim::OldestRecords { dropped } if *dropped > 0))
    );
    let kept = opened(&fitted.share);
    assert_eq!(kept[0]["type"], "session");
    assert_eq!(kept.last().expect("newest kept")["seq"], 199);
    assert!(kept.len() < records.len());
}

#[test]
fn inline_images_are_removed_before_any_text_is_cut() {
    let records = vec![
        json!({"type": "session", "id": "s"}),
        json!({
            "type": "message",
            "content": "keep me",
            "dial_images": [{"snapshot": {"inline": noisy(40_000, 7)}}],
        }),
    ];
    let fitted = fit_to_cap(&transcript(&records), 8 * 1024).expect("fits");
    assert_eq!(fitted.trims, vec![Trim::InlineImages { count: 1 }]);
    let kept = opened(&fitted.share);
    assert_eq!(kept[1]["content"], "keep me");
    assert_eq!(kept[1]["dial_images"][0]["unavailable"], IMAGE_REMOVED);
}

#[test]
fn long_strings_step_down_the_ladder_before_records_are_dropped() {
    let records = vec![
        json!({"type": "session", "id": "s"}),
        json!({"type": "message", "seq": 0, "content": noisy(40_000, 3)}),
    ];
    let fitted = fit_to_cap(&transcript(&records), 4 * 1024).expect("fits");
    assert!(
        fitted
            .trims
            .iter()
            .any(|trim| matches!(trim, Trim::LongStrings { .. }))
    );
    assert!(
        !fitted
            .trims
            .iter()
            .any(|trim| matches!(trim, Trim::OldestRecords { .. }))
    );
    let kept = opened(&fitted.share);
    assert_eq!(kept.len(), 2);
    assert!(
        kept[1]["content"]
            .as_str()
            .expect("string")
            .ends_with(TRUNCATED)
    );
}

#[test]
fn header_alone_over_the_cap_is_an_error() {
    let records = vec![json!({"type": "session", "id": "s"})];
    assert!(fit_to_cap(&transcript(&records), 16).is_err());
}
