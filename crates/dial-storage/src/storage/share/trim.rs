//! Size-bounded sharing. The ladder applies the least lossy trim first and measures the sealed
//! (encrypted, compressed) payload after each step, so the cap applies to what is uploaded:
//! 1. inline images are replaced by an `unavailable` marker;
//! 2. long strings are truncated at 32768, 8192, 2048, then 512 characters;
//! 3. the oldest non-header records are dropped, the fewest that fit.
//!
//! The session header always stays, so the share still names its session. Every trim applied is
//! reported. Sealing goes through `encrypt_transcript`, so redaction and encryption are unchanged.
use super::{EncryptedShare, encrypt_transcript};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fmt;

/// Default ceiling for an uploaded share payload, in bytes after encryption.
pub const SHARE_CAP_BYTES: usize = 8 << 20;

const STRING_LIMITS: [usize; 4] = [32_768, 8_192, 2_048, 512];
const IMAGE_REMOVED: &str = "image removed to fit the share size cap";
const TRUNCATED: &str = " …[truncated to fit the share size cap]";

/// One trim the ladder applied, with how much it touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trim {
    InlineImages { count: usize },
    LongStrings { limit: usize, count: usize },
    OldestRecords { dropped: usize },
}

impl fmt::Display for Trim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InlineImages { count } => write!(f, "removed {count} inline image(s)"),
            Self::LongStrings { limit, count } => {
                write!(f, "truncated {count} string(s) to {limit} characters")
            }
            Self::OldestRecords { dropped } => write!(f, "dropped {dropped} oldest record(s)"),
        }
    }
}

/// A sealed share that fits the cap, plus the trims needed to get there (empty when none were).
#[derive(Debug)]
pub struct Fitted {
    pub share: EncryptedShare,
    pub trims: Vec<Trim>,
}

/// Seals the JSON-array `transcript` under `cap` bytes by applying the ladder until it fits.
pub fn fit_to_cap(transcript: &[u8], cap: usize) -> Result<Fitted> {
    let mut records: Vec<Value> = serde_json::from_slice(transcript)
        .context("share input must be a JSON array of records")?;
    let mut trims = Vec::new();
    if let Some(share) = seal_within(&records, cap)? {
        return Ok(Fitted { share, trims });
    }

    let count: usize = records.iter_mut().map(strip_inline_images).sum();
    if count > 0 {
        trims.push(Trim::InlineImages { count });
    }
    if let Some(share) = seal_within(&records, cap)? {
        return Ok(Fitted { share, trims });
    }

    for limit in STRING_LIMITS {
        let count: usize = records
            .iter_mut()
            .map(|record| truncate_strings(record, limit))
            .sum();
        if count > 0 {
            trims.push(Trim::LongStrings { limit, count });
        }
        if let Some(share) = seal_within(&records, cap)? {
            return Ok(Fitted { share, trims });
        }
    }

    let header = usize::from(records.first().is_some_and(is_header));
    let body = records.len() - header;
    let fits = |dropped: usize| seal_within(&without_oldest(&records, header, dropped), cap);
    if body == 0 || fits(body)?.is_none() {
        bail!(
            "session share stays over {cap} bytes after every trim; it cannot be shared under this cap"
        );
    }
    // Sealed size falls as records are dropped, so the smallest count that fits is found by bisection.
    let (mut low, mut high) = (1, body);
    while low < high {
        let mid = low + (high - low) / 2;
        if fits(mid)?.is_some() {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    let Some(share) = fits(low)? else {
        bail!("session share no longer fits the cap after dropping records");
    };
    trims.push(Trim::OldestRecords { dropped: low });
    Ok(Fitted { share, trims })
}

fn seal_within(records: &[Value], cap: usize) -> Result<Option<EncryptedShare>> {
    let share = encrypt_transcript(&serde_json::to_vec(records)?)?;
    Ok((share.bytes.len() <= cap).then_some(share))
}

fn is_header(record: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("session")
}

fn without_oldest(records: &[Value], header: usize, dropped: usize) -> Vec<Value> {
    records[..header]
        .iter()
        .chain(&records[header + dropped..])
        .cloned()
        .collect()
}

/// Replaces inline image snapshots and `data:image/` strings. Returns how many were replaced.
fn strip_inline_images(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let mut count = 0;
            if let Some(Value::Array(images)) = map.get_mut("dial_images") {
                for image in images.iter_mut() {
                    if image
                        .get("snapshot")
                        .and_then(|s| s.get("inline"))
                        .is_some()
                    {
                        *image = json!({"unavailable": IMAGE_REMOVED});
                        count += 1;
                    }
                }
            }
            for child in map.values_mut() {
                count += strip_inline_images(child);
            }
            count
        }
        Value::Array(items) => items.iter_mut().map(strip_inline_images).sum(),
        Value::String(text) if text.starts_with("data:image/") => {
            *text = format!("[{IMAGE_REMOVED}]");
            1
        }
        _ => 0,
    }
}

/// Truncates every string longer than `limit` characters. Returns how many were truncated.
fn truncate_strings(value: &mut Value, limit: usize) -> usize {
    match value {
        Value::String(text) => match text.char_indices().nth(limit) {
            Some((cut, _)) => {
                text.truncate(cut);
                text.push_str(TRUNCATED);
                1
            }
            None => 0,
        },
        Value::Array(items) => items.iter_mut().map(|v| truncate_strings(v, limit)).sum(),
        Value::Object(map) => map.values_mut().map(|v| truncate_strings(v, limit)).sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests;
