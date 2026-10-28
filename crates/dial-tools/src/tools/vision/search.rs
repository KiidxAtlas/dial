//! The read-only `image_search` tool: rank workspace images by a text description (`text`), by
//! likeness to a reference image (`similar`), or score given images against a prompt (`score`),
//! with the `image_embedding` role's vectors. It is offered only while that role is assigned.
//!
//! Vectors are cached per image content (SHA-256) and embedding model in Dial's per-project state,
//! outside the workspace, so repeated searches embed only new or changed images.

use super::embedding::{self, Input};
use crate::config::types::Config;
use crate::tools::integrations::ext::{Concurrency, Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace, opt_str, tool_schema};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub const TOOL: &str = "image_search";
const MODES: &[&str] = &["text", "similar", "score"];
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];
/// Images considered in one scope; a larger tree needs a narrower `path`.
const MAX_SCOPE_IMAGES: usize = 2_000;
/// Images embedded by one call; later calls continue from the cache.
const MAX_NEW_EMBEDDINGS: usize = 256;
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_SCORE_IMAGES: usize = 32;
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
/// Images per embedding request, so a cancelled call keeps what it already embedded.
const CHUNK: usize = 8;

const DESCRIPTION: &str = "Read-only image retrieval with the configured image embedding model (text and images share one vector space). \
mode=text: rank images under `path` (default: workspace root) by how well they match `query`. \
mode=similar: rank images under `path` by likeness to the reference `image`. \
mode=score: cosine score of each of `images` against the prompt `query` (e.g. check generated images against their prompt). \
Scores are cosine similarities: compare them within one call, not as absolute thresholds. Skips .gitignored, hidden and dependency directories; vectors are cached per image content.";

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    if embedding::configured(config) {
        vec![Arc::new(ImageSearch::default())]
    } else {
        Vec::new()
    }
}

#[derive(Default)]
struct ImageSearch {
    interrupt: Arc<AtomicBool>,
}

impl Tool for ImageSearch {
    fn name(&self) -> &str {
        TOOL
    }

    fn schema(&self) -> Value {
        tool_schema(
            TOOL,
            DESCRIPTION,
            json!({
                "mode": {"type": "string", "enum": MODES},
                "query": {"type": "string", "maxLength": 1000, "description": "text to search for (text) or the prompt to score against (score)"},
                "image": {"type": "string", "description": "reference image path (similar)"},
                "images": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_SCORE_IMAGES, "description": "image paths to score (score)"},
                "path": {"type": "string", "description": "directory or image file to search (text, similar); default workspace root"},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "results to return (default 10)"},
            }),
            &["mode"],
        )
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }

    fn read_only_hint(&self) -> bool {
        true
    }

    fn may_modify_workspace(&self, _args: &Value) -> bool {
        false
    }

    fn concurrency(&self, _args: &Value) -> Concurrency {
        Concurrency::Shared
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.interrupt.store(false, Ordering::Release);
        let index = Index::open(&ctx.config.state_dir.join("image_embeddings.sqlite3"));
        match index {
            Ok(index) => run(
                ctx.workspace,
                ctx.config,
                &index,
                args,
                &Embedder::Model(Arc::clone(&self.interrupt)),
            )
            .map_or_else(ToolOutput::err, ToolOutput::ok),
            Err(error) => ToolOutput::err(format!("image index: {error:#}")),
        }
    }

    fn cancel_operation(&self) {
        self.interrupt.store(true, Ordering::Release);
    }

    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self::default()))
    }
}

/// Produces vectors; tests substitute a deterministic function for the model.
pub(crate) enum Embedder {
    Model(Arc<AtomicBool>),
    #[cfg(test)]
    Fake(fn(&Input<'_>) -> Vec<f32>),
}

impl Embedder {
    fn embed(&self, config: &Config, inputs: &[Input<'_>]) -> Result<Vec<Vec<f32>>> {
        match self {
            Self::Model(cancel) => embedding::embed(config, inputs, Arc::clone(cancel)),
            #[cfg(test)]
            Self::Fake(embed) => Ok(inputs.iter().map(embed).collect()),
        }
    }

    fn cancelled(&self) -> bool {
        match self {
            Self::Model(cancel) => cancel.load(Ordering::Acquire),
            #[cfg(test)]
            Self::Fake(_) => false,
        }
    }
}

/// Cached vectors keyed by image content hash and model identity.
pub(crate) struct Index(Connection);

impl Index {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection =
            Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(10))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS vectors(
                 sha256 TEXT NOT NULL,
                 model TEXT NOT NULL,
                 vector BLOB NOT NULL,
                 PRIMARY KEY(sha256, model)
             );",
        )?;
        Ok(Self(connection))
    }

    fn get(&self, sha: &str, model: &str) -> Result<Option<Vec<f32>>> {
        let blob: Option<Vec<u8>> = self
            .0
            .query_row(
                "SELECT vector FROM vectors WHERE sha256 = ?1 AND model = ?2",
                params![sha, model],
                |row| row.get(0),
            )
            .optional()?;
        Ok(blob.map(|blob| {
            blob.as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes))
                .collect()
        }))
    }

    fn put(&self, sha: &str, model: &str, vector: &[f32]) -> Result<()> {
        let blob: Vec<u8> = vector
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        self.0.execute(
            "INSERT OR REPLACE INTO vectors(sha256, model, vector) VALUES (?1, ?2, ?3)",
            params![sha, model, blob],
        )?;
        Ok(())
    }
}

/// One image file read for embedding.
struct Image {
    path: PathBuf,
    sha: String,
    bytes: Vec<u8>,
}

pub(crate) fn run(
    ws: &Workspace,
    config: &Config,
    index: &Index,
    args: &Value,
    embedder: &Embedder,
) -> Result<String, String> {
    if !embedding::configured(config) {
        return Err("no image embedding model is configured; run `/setup image-embeddings` or assign one to the `image_embedding` role in Alt+M".into());
    }
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(value) => value
            .as_u64()
            .filter(|limit| (1..=MAX_LIMIT as u64).contains(limit))
            .ok_or(format!("limit must be an integer from 1 to {MAX_LIMIT}"))?
            as usize,
    };
    let query = opt_str(args, "query")?
        .map(str::trim)
        .filter(|query| !query.is_empty());
    let model = embedding::model_key(config);
    let error = |error: anyhow::Error| format!("{error:#}");
    match opt_str(args, "mode")? {
        Some("text") => {
            let query = query.ok_or("mode=text requires query")?;
            let scope = scope(ws, opt_str(args, "path")?)?;
            let wanted = embedder
                .embed(config, &[Input::Text(query)])
                .map_err(error)?
                .remove(0);
            let ranked = rank(ws, config, index, embedder, &model, &scope, &wanted, None)?;
            Ok(report(
                ws,
                &format!("text {query:?}"),
                &model,
                &ranked,
                limit,
            ))
        }
        Some("similar") => {
            let reference = opt_str(args, "image")?.ok_or("mode=similar requires image")?;
            let reference = read_image(ws, reference)?;
            let wanted = vectors(
                config,
                index,
                embedder,
                &model,
                std::slice::from_ref(&reference),
            )
            .map_err(error)?
            .remove(0)
            .ok_or("cancelled")?;
            let scope = scope(ws, opt_str(args, "path")?)?;
            let ranked = rank(
                ws,
                config,
                index,
                embedder,
                &model,
                &scope,
                &wanted,
                Some(&reference.sha),
            )?;
            let label = format!("similar to {}", display(ws, &reference.path));
            Ok(report(ws, &label, &model, &ranked, limit))
        }
        Some("score") => {
            let query = query.ok_or("mode=score requires query (the prompt)")?;
            let paths: Vec<&str> = match args.get("images") {
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| item.as_str().ok_or("images must be paths"))
                    .collect::<Result<_, _>>()?,
                _ => return Err("mode=score requires images".into()),
            };
            if paths.is_empty() || paths.len() > MAX_SCORE_IMAGES {
                return Err(format!("images takes 1 to {MAX_SCORE_IMAGES} paths"));
            }
            let images = paths
                .iter()
                .map(|path| read_image(ws, path))
                .collect::<Result<Vec<_>, _>>()?;
            let wanted = embedder
                .embed(config, &[Input::Text(query)])
                .map_err(error)?
                .remove(0);
            let found = vectors(config, index, embedder, &model, &images).map_err(error)?;
            let mut out = format!("image_search score {query:?} · {model}\n");
            for (image, vector) in images.iter().zip(found) {
                let vector = vector.ok_or("cancelled")?;
                let _ = writeln!(
                    out,
                    "{:.3}  {}",
                    embedding::similarity(&wanted, &vector),
                    display(ws, &image.path)
                );
            }
            Ok(out)
        }
        Some(other) => Err(format!(
            "unknown mode {other:?}; use text, similar or score"
        )),
        None => Err("mode is required: text, similar or score".into()),
    }
}

/// The line `image_gen` appends after writing `path`: the cosine similarity of the image to its
/// prompt under the `image_embedding` role. `None` while the role is unassigned; a failure is
/// reported on the line instead of failing the generation.
pub fn prompt_similarity_line(config: &Config, prompt: &str, path: &Path) -> Option<String> {
    if !embedding::configured(config) || prompt.trim().is_empty() {
        return None;
    }
    let score = || -> Result<f32> {
        let bytes = std::fs::read(path)?;
        let vectors = embedding::embed(
            config,
            &[Input::Text(prompt.trim()), Input::Image(&bytes)],
            Arc::new(AtomicBool::new(false)),
        )?;
        if let Ok(index) = Index::open(&config.state_dir.join("image_embeddings.sqlite3")) {
            let sha = format!("{:x}", Sha256::digest(&bytes));
            let _ = index.put(&sha, &embedding::model_key(config), &vectors[1]);
        }
        Ok(embedding::similarity(&vectors[0], &vectors[1]))
    };
    Some(match score() {
        Ok(score) => format!(
            "Prompt similarity {score:.3} ({}; cosine, compare across candidates with image_search mode=score).",
            config.for_role(embedding::ROLE).model
        ),
        Err(error) => format!("Prompt similarity unavailable: {error:#}"),
    })
}

/// Ranked `(score, path)` pairs plus how the scope was covered.
struct Ranked {
    hits: Vec<(f32, PathBuf)>,
    images: usize,
    embedded: usize,
    pending: usize,
    skipped: usize,
    capped: bool,
}

#[allow(clippy::too_many_arguments)]
fn rank(
    ws: &Workspace,
    config: &Config,
    index: &Index,
    embedder: &Embedder,
    model: &str,
    scope: &Path,
    wanted: &[f32],
    exclude: Option<&str>,
) -> Result<Ranked, String> {
    let (paths, capped) = collect(ws, scope);
    if paths.is_empty() {
        return Err(format!(
            "no images (.png, .jpg, .webp, .gif) under {}",
            display(ws, scope)
        ));
    }
    let mut ranked = Ranked {
        hits: Vec::new(),
        images: paths.len(),
        embedded: 0,
        pending: 0,
        skipped: 0,
        capped,
    };
    let mut missing = Vec::new();
    for path in paths {
        let Some(bytes) = std::fs::read(&path).ok().filter(|bytes| {
            bytes.len() as u64 <= MAX_IMAGE_BYTES && crate::llm::images::dimensions(bytes).is_ok()
        }) else {
            ranked.skipped += 1;
            continue;
        };
        let sha = format!("{:x}", Sha256::digest(&bytes));
        if exclude == Some(sha.as_str()) {
            continue;
        }
        match index
            .get(&sha, model)
            .map_err(|error| format!("{error:#}"))?
        {
            Some(vector) => ranked
                .hits
                .push((embedding::similarity(wanted, &vector), path)),
            None if missing.len() < MAX_NEW_EMBEDDINGS => missing.push(Image { path, sha, bytes }),
            None => ranked.pending += 1,
        }
    }
    let found =
        vectors(config, index, embedder, model, &missing).map_err(|error| format!("{error:#}"))?;
    for (image, vector) in missing.into_iter().zip(found) {
        match vector {
            Some(vector) => {
                ranked.embedded += 1;
                ranked
                    .hits
                    .push((embedding::similarity(wanted, &vector), image.path));
            }
            None => ranked.pending += 1,
        }
    }
    ranked
        .hits
        .sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(ranked)
}

/// Vectors for `images` in order: cached, or embedded in chunks and cached. `None` marks images
/// left unembedded because the call was cancelled.
fn vectors(
    config: &Config,
    index: &Index,
    embedder: &Embedder,
    model: &str,
    images: &[Image],
) -> Result<Vec<Option<Vec<f32>>>> {
    let mut out: Vec<Option<Vec<f32>>> = images
        .iter()
        .map(|image| index.get(&image.sha, model))
        .collect::<Result<_>>()?;
    let missing: Vec<usize> = (0..images.len()).filter(|i| out[*i].is_none()).collect();
    for chunk in missing.chunks(CHUNK) {
        if embedder.cancelled() {
            break;
        }
        let inputs: Vec<Input<'_>> = chunk
            .iter()
            .map(|i| Input::Image(&images[*i].bytes))
            .collect();
        let found = embedder.embed(config, &inputs)?;
        anyhow::ensure!(found.len() == chunk.len(), "embedding count mismatch");
        for (i, vector) in chunk.iter().zip(found) {
            index.put(&images[*i].sha, model, &vector)?;
            out[*i] = Some(vector);
        }
    }
    Ok(out)
}

fn scope(ws: &Workspace, path: Option<&str>) -> Result<PathBuf, String> {
    match path.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => {
            let resolved = ws.resolve(path)?;
            if resolved.exists() {
                Ok(resolved)
            } else {
                Err(format!("{path}: no such file or directory"))
            }
        }
        None => Ok(ws.root().to_path_buf()),
    }
}

fn read_image(ws: &Workspace, input: &str) -> Result<Image, String> {
    let path = ws.resolve(input.trim())?;
    let meta = std::fs::metadata(&path).map_err(|error| format!("{input}: {error}"))?;
    if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES {
        return Err(format!("{input} is not an image file of at most 20 MiB"));
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("{input}: {error}"))?;
    crate::llm::images::dimensions(&bytes)
        .map_err(|error| format!("{input} is not a readable image: {error}"))?;
    let sha = format!("{:x}", Sha256::digest(&bytes));
    Ok(Image { path, sha, bytes })
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Image files under `scope` in path order, honoring ignore files and skipping hidden,
/// dependency and protected paths. The flag reports a capped scan.
fn collect(ws: &Workspace, scope: &Path) -> (Vec<PathBuf>, bool) {
    if scope.is_file() {
        return (
            is_image(scope)
                .then(|| scope.to_path_buf())
                .into_iter()
                .collect(),
            false,
        );
    }
    let mut walker = ignore::WalkBuilder::new(scope);
    walker
        .follow_links(false)
        .require_git(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| SKIPPED_DIRS.contains(&name))
        });
    let mut found = Vec::new();
    for entry in walker.build().flatten() {
        let path = entry.path();
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || !is_image(path)
            || ws.is_protected(path)
        {
            continue;
        }
        if found.len() == MAX_SCOPE_IMAGES {
            return (found, true);
        }
        found.push(path.to_path_buf());
    }
    (found, false)
}

fn display(ws: &Workspace, path: &Path) -> String {
    let relative = path.strip_prefix(ws.root()).unwrap_or(path);
    let text = relative.to_string_lossy();
    if text.is_empty() {
        ".".into()
    } else {
        text.into_owned()
    }
}

fn report(ws: &Workspace, label: &str, model: &str, ranked: &Ranked, limit: usize) -> String {
    let mut out = format!(
        "image_search {label} · {} images, {} newly embedded · {model}\n",
        ranked.images, ranked.embedded
    );
    for (score, path) in ranked.hits.iter().take(limit) {
        let _ = writeln!(out, "{score:.3}  {}", display(ws, path));
    }
    if ranked.hits.is_empty() {
        out.push_str("no embedded images to rank yet\n");
    }
    if ranked.pending > 0 {
        let _ = writeln!(
            out,
            "[{} images not embedded yet (at most {MAX_NEW_EMBEDDINGS} per call, or cancelled); repeat the call to include them]",
            ranked.pending
        );
    }
    if ranked.skipped > 0 {
        let _ = writeln!(
            out,
            "[{} files skipped: unreadable, not an image, or larger than 20 MiB]",
            ranked.skipped
        );
    }
    if ranked.capped {
        let _ = writeln!(
            out,
            "[scope capped at {MAX_SCOPE_IMAGES} images; narrow `path`]"
        );
    }
    out
}

#[cfg(test)]
mod tests;
