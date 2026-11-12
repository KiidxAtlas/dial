//! `find` with `ranking: "embedding"`: cosine similarity between the query and overlapping line
//! windows of workspace files, embedded through the configured embedding route (the same one
//! semantic memory uses). Vectors are cached in memory by model and window content, so later calls
//! embed only new or changed windows. Each call embeds a bounded number of new windows; a larger
//! scope is ranked over what is indexed so far and says how much is left.
use super::{MAX_FILE, MAX_OUTPUT, MAX_TOTAL, Workspace, opt_str, opt_u64, req_str};
use crate::storage::memory_backend::Embedder;
use crate::tools::fs::walk::Walk;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};

/// Lines per window and the step between window starts (windows overlap by the difference).
const WINDOW: usize = 40;
const STEP: usize = 30;
/// Bytes of one window sent for embedding.
const MAX_WINDOW_BYTES: usize = 4000;
/// Windows considered per call, and new windows embedded per call.
const MAX_WINDOWS: usize = 20_000;
pub(super) const MAX_NEW: usize = 512;
const BATCH: usize = 32;
/// Cached vectors kept per process before the cache is cleared.
const MAX_CACHED: usize = 200_000;
const MAX_PER_PATH: usize = 3;

type Key = (String, [u8; 32]);

fn cache() -> &'static Mutex<HashMap<Key, Arc<[f32]>>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<Key, Arc<[f32]>>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// One window of one file.
struct Window {
    path: String,
    start: usize,
    end: usize,
    file_sha256: String,
    snippet: String,
    text: String,
    key: [u8; 32],
}

/// The windows of `text` (1-based inclusive line spans), skipping near-empty ones.
pub(super) fn windows(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut spans = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let end = (start + WINDOW).min(lines.len());
        let content: usize = lines[start..end].iter().map(|line| line.trim().len()).sum();
        if content >= 20 {
            spans.push((start + 1, end));
        }
        if end == lines.len() {
            break;
        }
        start += STEP;
    }
    spans
}

pub(super) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denominator = na.sqrt() * nb.sqrt();
    if a.len() != b.len() || denominator == 0.0 || !denominator.is_finite() {
        0.0
    } else {
        dot / denominator
    }
}

fn collect(ws: &Workspace, start: &std::path::Path) -> (Vec<Window>, usize, bool) {
    let mut windows_out = Vec::new();
    let (mut bytes, mut skipped, mut capped) = (0u64, 0usize, false);
    Walk::new(ws).run(start, &mut |path| {
        let Ok(meta) = fs::symlink_metadata(path) else {
            skipped += 1;
            return true;
        };
        if !meta.is_file()
            || meta.file_type().is_symlink()
            || meta.len() > MAX_FILE
            || !ws
                .resolve_read(&ws.rel(path))
                .is_ok_and(|resolved| resolved == path)
        {
            skipped += 1;
            return true;
        }
        if bytes + meta.len() > MAX_TOTAL || windows_out.len() >= MAX_WINDOWS {
            capped = true;
            return false;
        }
        let Ok(data) = fs::read(path) else {
            skipped += 1;
            return true;
        };
        let Ok(text) = std::str::from_utf8(&data) else {
            skipped += 1;
            return true;
        };
        if text.contains('\0') {
            skipped += 1;
            return true;
        }
        bytes += data.len() as u64;
        let rel = ws.rel(path);
        let file_sha256 = format!("{:x}", Sha256::digest(&data));
        let lines: Vec<&str> = text.lines().collect();
        for (first, last) in windows(text) {
            let body = lines[first - 1..last].join("\n");
            let mut window_text = format!("{rel}\n{body}");
            if window_text.len() > MAX_WINDOW_BYTES {
                let mut cut = MAX_WINDOW_BYTES;
                while !window_text.is_char_boundary(cut) {
                    cut -= 1;
                }
                window_text.truncate(cut);
            }
            let snippet: String = lines[first - 1..last]
                .iter()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(300)
                .collect();
            windows_out.push(Window {
                key: Sha256::digest(window_text.as_bytes()).into(),
                path: rel.clone(),
                start: first,
                end: last,
                file_sha256: file_sha256.clone(),
                snippet,
                text: window_text,
            });
            if windows_out.len() >= MAX_WINDOWS {
                capped = true;
                return false;
            }
        }
        true
    });
    (windows_out, skipped, capped)
}

/// Run an embedding search. `endpoint` describes where inputs are sent, for the result header.
pub(super) fn search(
    ws: &Workspace,
    args: &Value,
    embedder: &mut dyn Embedder,
    endpoint: &str,
) -> Result<String, String> {
    let query = req_str(args, "query")?;
    if query.trim().is_empty() || query.len() > 4096 {
        return Err("query must be 1..=4096 bytes and nonblank".into());
    }
    if opt_str(args, "mode")?.is_some_and(|mode| mode != "behavior") {
        return Err("ranking=embedding works in behavior mode only".into());
    }
    let limit = opt_u64(args, "limit")?.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err("limit must be 1..=100".into());
    }
    let start = ws.resolve_read(opt_str(args, "path")?.unwrap_or("."))?;
    if !start.exists() {
        return Err("search scope does not exist".into());
    }
    let model = embedder.model().to_owned();
    let (windows, skipped, capped) = collect(ws, &start);
    let missing: Vec<usize> = {
        let cached = cache().lock().unwrap_or_else(|e| e.into_inner());
        windows
            .iter()
            .enumerate()
            .filter(|(_, window)| !cached.contains_key(&(model.clone(), window.key)))
            .map(|(index, _)| index)
            .collect()
    };
    let embedding_now = &missing[..missing.len().min(MAX_NEW)];
    for batch in embedding_now.chunks(BATCH) {
        let inputs: Vec<String> = batch.iter().map(|&i| windows[i].text.clone()).collect();
        let vectors = embedder
            .embed(&inputs)
            .map_err(|error| format!("embedding search failed: {error:#}"))?;
        if vectors.len() != inputs.len() {
            return Err("embedding endpoint returned the wrong number of vectors".into());
        }
        let mut cached = cache().lock().unwrap_or_else(|e| e.into_inner());
        if cached.len() + vectors.len() > MAX_CACHED {
            cached.clear();
        }
        for (&index, vector) in batch.iter().zip(vectors) {
            cached.insert((model.clone(), windows[index].key), vector.into());
        }
    }
    let query_vector = embedder
        .embed(&[query.to_owned()])
        .map_err(|error| format!("embedding the query failed: {error:#}"))?
        .into_iter()
        .next()
        .ok_or("embedding endpoint returned no query vector")?;
    let mut scored: Vec<(f32, &Window)> = {
        let cached = cache().lock().unwrap_or_else(|e| e.into_inner());
        windows
            .iter()
            .filter_map(|window| {
                cached
                    .get(&(model.clone(), window.key))
                    .map(|vector| (cosine(&query_vector, vector), window))
            })
            .collect()
    };
    let indexed = scored.len();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| (&a.1.path, a.1.start).cmp(&(&b.1.path, b.1.start)))
    });
    let remaining = missing.len() - embedding_now.len();
    let mut out = format!(
        "Retrieval: embedding similarity ({model} via {endpoint}) over {WINDOW}-line windows; cosine scores, not probabilities.\n"
    );
    let mut shown: Vec<&Window> = Vec::new();
    for (score, window) in scored {
        if shown.len() as u64 >= limit {
            break;
        }
        let same_path: Vec<&&Window> = shown.iter().filter(|w| w.path == window.path).collect();
        if same_path.len() >= MAX_PER_PATH
            || same_path
                .iter()
                .any(|w| w.start <= window.end && window.start <= w.end)
        {
            continue;
        }
        let row = format!(
            "{}:{}-{} similarity={score:.3} [embedding] source_sha256={} {}\n",
            window.path, window.start, window.end, window.file_sha256, window.snippet
        );
        if out.len() + row.len() > MAX_OUTPUT {
            out.push_str("[output byte cap reached]\n");
            break;
        }
        out.push_str(&row);
        shown.push(window);
    }
    if shown.is_empty() {
        out.push_str("No indexed windows.\n");
    }
    out.push_str(&format!(
        "[{} results; {indexed} of {} windows indexed ({} embedded this call); {skipped} files skipped{}]\n",
        shown.len(),
        windows.len(),
        embedding_now.len(),
        if capped {
            "; scope capped, narrow `path` for full coverage"
        } else {
            ""
        }
    ));
    if remaining > 0 {
        out.push_str(&format!(
            "[{remaining} windows not yet embedded (at most {MAX_NEW} per call); run the same find again to extend the index]\n"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
