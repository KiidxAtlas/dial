//! `find` with `ranking: "embedding"`: cosine similarity between the query and overlapping line
//! windows of workspace files, embedded through the configured embedding route (the same one
//! semantic memory uses). Vectors are cached in memory by route and window content. Each call
//! embeds a bounded number of missing windows, rotating through repeated scopes independently of
//! vector residency. Results report this call's coverage, including any cache eviction gaps.
use super::{MAX_FILE, MAX_OUTPUT, MAX_TOTAL, Workspace, opt_str, opt_u64, req_str};
use crate::storage::memory_backend::Embedder;
use crate::tools::fs::walk::Walk;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Read;
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
/// Bounds both vector payloads and per-entry bookkeeping, independently of dimensions.
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHED: usize = 200_000;
const MAX_PER_PATH: usize = 3;
/// Independently bounded progress metadata for recently searched route/scope snapshots.
const MAX_SCOPES: usize = 128;

type Key = (String, [u8; 32]);

#[derive(Default)]
struct VectorCache {
    entries: HashMap<Key, Arc<[f32]>>,
    order: VecDeque<Key>,
    bytes: usize,
    progress: VecDeque<([u8; 32], usize)>,
}
impl VectorCache {
    fn select_missing(&mut self, scope: [u8; 32], vectors: &[Option<Arc<[f32]>>]) -> Vec<usize> {
        if vectors.is_empty() {
            return Vec::new();
        }
        let cursor = self
            .progress
            .iter()
            .position(|(key, _)| key == &scope)
            .and_then(|position| self.progress.remove(position))
            .map_or(0, |(_, cursor)| cursor);
        let selected: Vec<usize> = (0..vectors.len())
            .map(|offset| (cursor + offset) % vectors.len())
            .filter(|&index| vectors[index].is_none())
            .take(MAX_NEW)
            .collect();
        let next = selected
            .last()
            .map_or(cursor, |index| (index + 1) % vectors.len());
        if self.progress.len() >= MAX_SCOPES {
            self.progress.pop_front();
        }
        // Reserve progress before endpoint work, without retaining per-window history.
        // Failed batches are retried on a later circuit rather than blocking the tail.
        self.progress.push_back((scope, next));
        selected
    }
    fn weight(key: &Key, vector: &[f32]) -> usize {
        // Includes both owned route keys and conservative map/queue/Arc overhead.
        256usize
            .saturating_add(key.0.len().saturating_mul(2))
            .saturating_add(std::mem::size_of_val(vector))
    }
    fn insert(&mut self, key: Key, vector: Arc<[f32]>, budget: usize) {
        let weight = Self::weight(&key, &vector);
        if weight > budget || self.entries.contains_key(&key) {
            return;
        }
        while self.bytes.saturating_add(weight) > budget || self.entries.len() >= MAX_CACHED {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(value) = self.entries.remove(&old) {
                self.bytes -= Self::weight(&old, &value);
            }
        }
        self.bytes += weight;
        self.order.push_back(key.clone());
        self.entries.insert(key, vector);
    }
}
fn cache() -> &'static Mutex<VectorCache> {
    static CACHE: std::sync::OnceLock<Mutex<VectorCache>> = std::sync::OnceLock::new();
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
    let mut walk = Walk::new(ws);
    walk.run(start, &mut |path| {
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
        if bytes >= MAX_TOTAL || bytes + meta.len() > MAX_TOTAL || windows_out.len() >= MAX_WINDOWS
        {
            capped = true;
            return false;
        }
        let allowance = MAX_FILE.min(MAX_TOTAL - bytes);
        let Ok(data) = read_source(path, allowance) else {
            // A failed/growing read may have consumed its entire allowance. Charge it
            // conservatively so repeated failures cannot bypass the per-call I/O budget.
            bytes += allowance;
            capped |= bytes >= MAX_TOTAL;
            skipped += 1;
            return true;
        };
        bytes += data.len() as u64;
        let Ok(text) = std::str::from_utf8(&data) else {
            skipped += 1;
            return true;
        };
        if text.contains('\0') {
            skipped += 1;
            return true;
        }
        let rel = ws.rel(path);
        let file_sha256 = format!("{:x}", Sha256::digest(&data));
        let lines: Vec<&str> = text.lines().collect();
        for (first, last) in windows(text) {
            let body = lines[first - 1..last].join("\n");
            let mut window_text = format!("{rel}\n{body}");
            if window_text.len() > MAX_WINDOW_BYTES {
                window_text.truncate(window_text.floor_char_boundary(MAX_WINDOW_BYTES));
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
    (windows_out, skipped, capped || walk.truncated)
}

/// Read through one non-following, nonblocking descriptor; growth cannot bypass the cap.
fn read_source(path: &std::path::Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let file =
        crate::platform::fs::open_nofollow_nonblocking(fs::OpenOptions::new().read(true), path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > limit {
        return Err(std::io::Error::other("not an admitted regular source file"));
    }
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(std::io::Error::other("source grew beyond read budget"));
    }
    Ok(data)
}

pub(super) struct Prepared {
    query: String,
    limit: u64,
    windows: Vec<Window>,
    skipped: usize,
    capped: bool,
}
pub(super) fn prepare(ws: &Workspace, args: &Value) -> Result<Prepared, String> {
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
    let (windows, skipped, capped) = collect(ws, &start);
    Ok(Prepared {
        query: query.to_owned(),
        limit,
        windows,
        skipped,
        capped,
    })
}

pub(super) struct Indexed {
    prepared: Prepared,
    model: String,
    query_vector: Vec<f32>,
    vectors: Vec<Option<Arc<[f32]>>>,
    embedded: usize,
    remaining: usize,
}
impl Prepared {
    pub(super) fn embed(self, embedder: &mut dyn Embedder) -> Result<Indexed, String> {
        self.embed_with_cache(embedder, cache(), MAX_CACHE_BYTES)
    }

    fn embed_with_cache(
        self,
        embedder: &mut dyn Embedder,
        cache: &Mutex<VectorCache>,
        budget: usize,
    ) -> Result<Indexed, String> {
        let model = embedder.model().to_owned();
        let route = embedder.route();
        let windows = &self.windows;
        let mut scope = Sha256::new();
        scope.update((route.len() as u64).to_le_bytes());
        scope.update(route.as_bytes());
        for window in windows {
            scope.update(window.key);
        }
        let (mut call_vectors, embedding_now) = {
            let mut cached = cache.lock().unwrap_or_else(|e| e.into_inner());
            let vectors: Vec<Option<Arc<[f32]>>> = windows
                .iter()
                .map(|window| cached.entries.get(&(route.clone(), window.key)).cloned())
                .collect();
            let selected = cached.select_missing(scope.finalize().into(), &vectors);
            (vectors, selected)
        };
        let missing = call_vectors
            .iter()
            .filter(|vector| vector.is_none())
            .count();
        for batch in embedding_now.chunks(BATCH) {
            let inputs: Vec<String> = batch.iter().map(|&i| windows[i].text.clone()).collect();
            let vectors = embedder
                .embed(&inputs)
                .map_err(|error| format!("embedding search failed: {error:#}"))?;
            if vectors.len() != inputs.len() {
                return Err("embedding endpoint returned the wrong number of vectors".into());
            }
            let mut cached = cache.lock().unwrap_or_else(|e| e.into_inner());
            for (&index, vector) in batch.iter().zip(vectors) {
                let vector: Arc<[f32]> = vector.into();
                cached.insert(
                    (route.clone(), windows[index].key),
                    Arc::clone(&vector),
                    budget,
                );
                // Pin this call's vectors even when the bounded shared cache evicts them.
                call_vectors[index] = Some(vector);
            }
        }
        let query_vector = embedder
            .embed(std::slice::from_ref(&self.query))
            .map_err(|error| format!("embedding the query failed: {error:#}"))?
            .into_iter()
            .next()
            .ok_or("embedding endpoint returned no query vector")?;
        Ok(Indexed {
            prepared: self,
            model,
            query_vector,
            vectors: call_vectors,
            embedded: embedding_now.len(),
            remaining: missing - embedding_now.len(),
        })
    }
}
impl Indexed {
    pub(super) fn render(self, endpoint: &str) -> Result<String, String> {
        let Prepared {
            windows,
            skipped,
            capped,
            limit,
            ..
        } = self.prepared;
        let Self {
            model,
            query_vector,
            vectors,
            embedded,
            remaining,
            ..
        } = self;
        let mut scored: Vec<(f32, &Window)> = windows
            .iter()
            .zip(&vectors)
            .filter_map(|(window, vector)| {
                vector
                    .as_ref()
                    .map(|vector| (cosine(&query_vector, vector), window))
            })
            .collect();
        let indexed = scored.len();
        scored.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| (&a.1.path, a.1.start).cmp(&(&b.1.path, b.1.start)))
        });
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
        embedded,
        if capped {
            "; scope capped, narrow `path` for full coverage"
        } else {
            ""
        }
    ));
        if remaining > 0 {
            out.push_str(&format!(
            "[{remaining} windows not indexed this call (at most {MAX_NEW} embedded per call); repeat find to rotate through missing windows; narrow `path` if cache eviction prevents full coverage]\n"
        ));
        }
        Ok(out)
    }
}

#[cfg(test)]
fn search(
    ws: &Workspace,
    args: &Value,
    embedder: &mut dyn Embedder,
    endpoint: &str,
) -> Result<String, String> {
    prepare(ws, args)?.embed(embedder)?.render(endpoint)
}

#[cfg(test)]
mod tests;
