//! Image checkpoints Dial can run itself on stable-diffusion.cpp (see `llm::server::image`).
//!
//! Discovery lists Diffusers text-to-image repositories, and a download fetches only the files
//! `sd-server` loads: each component's default weights and its small config files, never the
//! repository's sample images or other precision variants. A root checkpoint that duplicates the
//! transformer in its original format is fetched in place of the Diffusers transformer weights.
//! An MLX-quantized pack (mflux, MLX-Serve) cannot run there; [`runnable_original`] finds the
//! Diffusers repository it was converted from, through the pack's declared `base_model`.
use super::{API, FILES, Family, Hit, get_text, is_repo_id, parse_search, run_python};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Diffusers component folders stable-diffusion.cpp reads (tokenizer and scheduler files are small
/// and kept so the snapshot stays a valid Diffusers layout).
const COMPONENTS: [&str; 9] = [
    "unet",
    "transformer",
    "vae",
    "text_encoder",
    "text_encoder_2",
    "text_encoder_3",
    "tokenizer",
    "tokenizer_2",
    "scheduler",
];
/// Small files kept from each component folder.
const CONFIG_EXTENSIONS: [&str; 4] = [".json", ".txt", ".model", ".jinja"];

fn image_search_query(query: &str) -> Vec<(&str, &str)> {
    let mut params = vec![
        ("filter", "diffusers"),
        ("pipeline_tag", "text-to-image"),
        ("sort", "downloads"),
        ("direction", "-1"),
        ("limit", super::SEARCH_LIMIT),
        ("expand", "downloads"),
        ("expand", "likes"),
        ("expand", "createdAt"),
        ("expand", "safetensors"),
    ];
    let query = query.trim();
    if !query.is_empty() {
        params.push(("search", query));
    }
    params
}

/// Diffusers text-to-image repositories, most downloaded first: the layouts Dial runs on
/// stable-diffusion.cpp. Network metadata only; weights are fetched after an explicit selection.
pub fn search(query: &str) -> Result<Vec<Hit>> {
    let text = get_text(API, &image_search_query(query))?;
    parse_search(Family::Mlx, &text)
}

/// The files of a Diffusers repository listing that `sd-server` needs, and their total size.
/// `None` when the listing is not a Diffusers layout (no `model_index.json`).
pub fn files(listing: &[(String, u64)]) -> Option<(Vec<String>, u64)> {
    if !listing.iter().any(|(path, _)| path == "model_index.json") {
        return None;
    }
    let mut picked = vec!["model_index.json".to_owned()];
    let mut indexes: Vec<&str> = Vec::new();
    let mut weights: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (path, _) in listing {
        let Some((folder, name)) = path.split_once('/') else {
            continue;
        };
        if !COMPONENTS.contains(&folder) || name.contains('/') {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".safetensors") {
            weights.entry(folder).or_default().push(path);
        } else if lower.contains(".safetensors.index") {
            indexes.push(path);
        } else if CONFIG_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
            picked.push(path.clone());
        }
    }
    if weights.is_empty() {
        return None;
    }
    let original = root_original(listing, weights.get("transformer").map(Vec::as_slice));
    for (folder, paths) in weights {
        if folder == "transformer" && original.is_some() {
            continue;
        }
        // Half precision when offered (half the download, and what stable-diffusion.cpp computes
        // in), else the default weights (no precision suffix), else another precision variant.
        let variants: Vec<&str> = paths.iter().filter_map(|path| variant(path)).collect();
        let chosen = if variants.contains(&"fp16") {
            Some("fp16")
        } else if paths.iter().any(|path| variant(path).is_none()) {
            None
        } else {
            ["bf16", "fp32"]
                .into_iter()
                .find(|name| variants.contains(name))
                .or_else(|| variants.iter().copied().min())
        };
        picked.extend(
            paths
                .iter()
                .filter(|path| variant(path) == chosen)
                .map(|path| (*path).to_owned()),
        );
        picked.extend(
            indexes
                .iter()
                .filter(|path| {
                    path.split_once('/').is_some_and(|(dir, _)| dir == folder)
                        && index_variant(path) == chosen
                })
                .map(|path| (*path).to_owned()),
        );
    }
    picked.extend(original);
    picked.sort();
    picked.dedup();
    let bytes = listing
        .iter()
        .filter(|(path, _)| picked.contains(path))
        .map(|(_, size)| size)
        .sum();
    Some((picked, bytes))
}

/// The repository-root checkpoint that holds the transformer in its original (non-Diffusers)
/// format: the only root weight file, within 0.1% of the size of the Diffusers transformer weights
/// (the two differ only in tensor names). stable-diffusion.cpp reads that format natively, and
/// misreads some Diffusers transformers (FLUX.2 klein's `flux-2-klein-4b.safetensors`; see
/// `server::image::transformer_source`). Repositories with several root files (FLUX.1's
/// transformer beside `ae.safetensors`) or a full single-file checkpoint keep the Diffusers copy.
fn root_original(listing: &[(String, u64)], transformer: Option<&[&str]>) -> Option<String> {
    let transformer = transformer?;
    let default: u64 = listing
        .iter()
        .filter(|(path, _)| transformer.contains(&path.as_str()) && variant(path).is_none())
        .map(|(_, size)| size)
        .sum();
    let mut roots = listing.iter().filter(|(path, _)| {
        let lower = path.to_ascii_lowercase();
        !path.contains('/') && (lower.ends_with(".safetensors") || lower.ends_with(".sft"))
    });
    let (path, size) = roots.next()?;
    if roots.next().is_some() || default == 0 {
        return None;
    }
    (size.abs_diff(default) <= default / 1000).then(|| path.clone())
}

/// `fp16` for `vae/diffusion_pytorch_model.fp16.safetensors` and
/// `text_encoder/model.fp16-00001-of-00002.safetensors`; `None` for default weights.
fn variant(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?.strip_suffix(".safetensors")?;
    let (_, rest) = name.split_once('.')?;
    let rest = rest.split_once('-').map_or(rest, |(variant, _)| variant);
    (!rest.is_empty()).then_some(rest)
}

/// `fp16` for `unet/diffusion_pytorch_model.safetensors.index.fp16.json`.
fn index_variant(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    let rest = name.split(".safetensors.index.").nth(1)?;
    rest.strip_suffix(".json")
}

/// Downloads `files` of `repo` into the Hugging Face cache and returns the snapshot directory.
/// A cancelled download fails with "download cancelled"; cached files are reused.
pub fn download(
    python: &Path,
    repo: &str,
    files: &[String],
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<String> {
    ensure!(
        is_repo_id(repo),
        "{repo:?} is not a Hugging Face repository id"
    );
    let first = files
        .first()
        .context("no image checkpoint files to download")?;
    ensure!(!cancel.load(Ordering::SeqCst), "download cancelled");
    progress(&format!("downloading {repo}"));
    let mut args = vec![repo.to_owned()];
    args.extend(files.iter().cloned());
    let output = run_python(python, FILES, &args, 6 * 3600, cancel, progress).map_err(|error| {
        if cancel.load(Ordering::SeqCst) {
            anyhow::anyhow!("download cancelled")
        } else {
            error.context(format!(
                "downloading image checkpoint {repo:?}; check network access, Hugging Face access, and the local Python runtime"
            ))
        }
    })?;
    let paths: Vec<String> =
        serde_json::from_str(output.trim()).context("reading the downloaded file paths")?;
    snapshot_root(paths.first().context("download returned no paths")?, first)
}

/// The snapshot directory holding `downloaded`, the cached copy of repository file `relative`.
fn snapshot_root(downloaded: &str, relative: &str) -> Result<String> {
    let mut root = Path::new(downloaded);
    ensure!(root.is_absolute(), "download returned a non-absolute path");
    for _ in Path::new(relative).components() {
        root = root
            .parent()
            .context("download path is shorter than the file")?;
    }
    Ok(root.display().to_string())
}

/// The Hugging Face repository an installed snapshot came from (`…/models--owner--name/…`).
pub fn repo_of(path: &Path) -> Option<String> {
    path.components().find_map(|part| {
        let repo = part.as_os_str().to_str()?.strip_prefix("models--")?;
        let repo = repo.replacen("--", "/", 1);
        is_repo_id(&repo).then_some(repo)
    })
}

/// A Diffusers original Dial can download and run in place of an MLX-quantized pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Original {
    pub repo: String,
    pub files: Vec<String>,
    pub bytes: u64,
}

/// The Diffusers repository `pack` declares as its `base_model`, with the files to fetch.
/// Network only; no weights.
///
/// # Errors
/// The pack declares no base model, or the base model is gated or not a Diffusers layout.
pub fn runnable_original(pack: &str) -> Result<Original> {
    ensure!(
        is_repo_id(pack),
        "{pack:?} is not a Hugging Face repository id"
    );
    let info: Value = serde_json::from_str(&get_text(&format!("{API}/{pack}"), &[])?)
        .context("reading the Hugging Face model metadata")?;
    let repo = base_models(&info)
        .into_iter()
        .next()
        .with_context(|| format!("{pack} does not declare the model it was converted from"))?;
    let original: Value = serde_json::from_str(&get_text(&format!("{API}/{repo}"), &[])?)
        .context("reading the Hugging Face model metadata")?;
    if original["gated"].as_str().is_some() || original["gated"] == Value::Bool(true) {
        bail!(
            "{repo} is gated: accept its license on huggingface.co and log in with `hf auth login`, then download it from /models → Get models → Images"
        );
    }
    let listing = super::tree(&repo)?;
    let (files, bytes) = files(&listing).with_context(|| {
        format!("{repo} (the original of {pack}) is not a Diffusers repository Dial can run")
    })?;
    Ok(Original { repo, files, bytes })
}

/// `cardData.base_model`, a string or a list, keeping valid repository ids.
fn base_models(info: &Value) -> Vec<String> {
    let declared = &info["cardData"]["base_model"];
    let names: Vec<&str> = match declared {
        Value::String(name) => vec![name.as_str()],
        Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    names
        .into_iter()
        .map(str::trim)
        .filter(|name| is_repo_id(name))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests;
