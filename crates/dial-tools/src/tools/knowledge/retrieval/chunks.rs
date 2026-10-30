//! Representation-local text windows and JSON pointers. Source coordinates remain in manifests.
use super::{Hit, MAX_QUOTE, check_cancel};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

const MAX_CHUNKS: usize = 4096;
const MAX_NODES: usize = 16384;

pub(super) fn parse(
    reference: &str,
    media: &str,
    bytes: &[u8],
    terms: &[String],
    cancel: &AtomicBool,
) -> Result<(Vec<Hit>, usize, bool)> {
    let text = std::str::from_utf8(bytes).context("representation is not UTF-8")?;
    check_cancel(cancel)?;
    let mut hits = Vec::new();
    let mut omitted = false;
    if media == "application/json" {
        // Parsing itself is bounded by the aggregate 2 MiB read cap and serde's depth cap.
        let value: Value = serde_json::from_str(text).context("invalid JSON representation")?;
        check_cancel(cancel)?;
        let mut stack = vec![(String::new(), &value, json!({}))];
        let mut nodes = 0;
        while let Some((pointer, value, mut coordinates)) = stack.pop() {
            check_cancel(cancel)?;
            nodes += 1;
            if nodes > MAX_NODES || hits.len() >= MAX_CHUNKS {
                omitted = true;
                break;
            }
            match value {
                Value::Object(object) => {
                    // These are exact fields in the retained representation, not a claim that
                    // arbitrary JSON metadata is an authenticated original-source mapping.
                    for key in [
                        "page",
                        "page_number",
                        "number",
                        "start_ms",
                        "end_ms",
                        "row_id",
                        "sheet",
                        "bbox",
                    ] {
                        if let Some(v) = object.get(key)
                            && serde_json::to_vec(v)?.len() <= 512
                        {
                            coordinates[key] = v.clone();
                        }
                    }
                    for (key, v) in object.iter().rev() {
                        let key = key.replace('~', "~0").replace('/', "~1");
                        stack.push((format!("{pointer}/{key}"), v, coordinates.clone()));
                    }
                }
                Value::Array(array) => {
                    // Prevent a large array from creating an unbounded traversal worklist.
                    let room = MAX_NODES.saturating_sub(nodes + stack.len());
                    if array.len() > room {
                        omitted = true;
                    }
                    for (i, v) in array.iter().enumerate().take(room).rev() {
                        stack.push((format!("{pointer}/{i}"), v, coordinates.clone()));
                    }
                }
                _ => {
                    let quote = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    if quote.len() > MAX_QUOTE {
                        omitted = true;
                        continue;
                    }
                    add(
                        &mut hits,
                        reference,
                        quote,
                        Some(value.clone()),
                        json!({"json_pointer":pointer,"ancestor_metadata":coordinates}),
                        terms,
                    );
                }
            }
        }
    } else {
        let mut offset = 0;
        'lines: for (line, text) in text.split_inclusive('\n').enumerate() {
            let mut start = 0;
            while start < text.len() {
                check_cancel(cancel)?;
                if hits.len() >= MAX_CHUNKS {
                    omitted = true;
                    break 'lines;
                }
                let mut end = (start + MAX_QUOTE).min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                add(
                    &mut hits,
                    reference,
                    text[start..end].into(),
                    None,
                    json!({"line":line+1,"byte_start":offset+start,"byte_end_exclusive":offset+end}),
                    terms,
                );
                start = end;
            }
            offset += text.len();
        }
    }
    let inspected = hits.len();
    Ok((hits, inspected, omitted))
}
fn add(
    hits: &mut Vec<Hit>,
    reference: &str,
    quote: String,
    value: Option<Value>,
    location: Value,
    terms: &[String],
) {
    let searchable = format!(
        "{} {}",
        location.get("json_pointer").unwrap_or(&Value::Null),
        quote
    )
    .to_lowercase();
    let score = terms
        .iter()
        .filter(|term| searchable.contains(term.as_str()))
        .count();
    hits.push(Hit {
        citation: reference.into(),
        representation_location: location,
        quote,
        value,
        lexical_points: score,
    });
}
