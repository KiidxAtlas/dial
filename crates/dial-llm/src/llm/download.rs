//! Model preparation; catalog probes remain offline while explicit selection and backend setup may fetch weights.
//!
//! A Hugging Face repository is listed first and then fetched by one of two plans:
//! - MLX (any `.safetensors` present): a full snapshot, minus GGUF files a mixed repo may carry.
//! - GGUF-only: exactly one model file (or one complete split-shard set) plus at most one
//!   multimodal projector. Several quantizations are never chosen silently: [`Choice::Auto`] refuses
//!   them, while the interactive picker lists them ([`quant_options`]) and fetches only the one the
//!   user picked ([`Choice::Files`]).
//!
//! Discovery ([`search`], [`metadata`], [`tree`], [`recommended`]) only reads Hugging Face's JSON API
//! or the offline curated list; it never fetches weights.
use crate::llm::local_format::GgufShard as Shard;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const LIST: &str = r#"
import json, sys
from huggingface_hub import list_repo_files
print(json.dumps(list_repo_files(sys.argv[1])))
"#;

const SNAPSHOT: &str = r#"
import json, sys
from huggingface_hub import snapshot_download
print(json.dumps(snapshot_download(repo_id=sys.argv[1], ignore_patterns=["*.gguf"])))
"#;

const FILES: &str = r#"
import json, sys
from huggingface_hub import hf_hub_download
from huggingface_hub.utils import LocalEntryNotFoundError
repo, names = sys.argv[1], sys.argv[2:]
paths = []
for name in names:
    try:
        paths.append(hf_hub_download(repo_id=repo, filename=name, local_files_only=True))
    except LocalEntryNotFoundError:
        paths.append(hf_hub_download(repo_id=repo, filename=name))
print(json.dumps(paths))
"#;

const API: &str = "https://huggingface.co/api/models";
const SEARCH_LIMIT: &str = "30";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Offline starting points, available before network results or when browsing is unavailable.
/// Every id was checked against the Hugging Face model API; keep repositories carrying weights.
const RECOMMENDED: &[(Family, &str)] = &[
    (Family::Mlx, "mlx-community/Qwen2.5-Coder-7B-Instruct-4bit"),
    (Family::Mlx, "mlx-community/Qwen3-4B-4bit"),
    (Family::Mlx, "mlx-community/Llama-3.2-3B-Instruct-4bit"),
    (Family::Gguf, "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF"),
    (Family::Gguf, "unsloth/Qwen3-4B-GGUF"),
    (Family::Gguf, "bartowski/Llama-3.2-3B-Instruct-GGUF"),
];

/// Small instruction models for the memory role (fact extraction and consolidation run on every
/// saved turn, so a fast small model is preferred over a strong large one). Checked like
/// [`RECOMMENDED`].
const MEMORY_RECOMMENDED: &[(Family, &str)] = &[
    (Family::Mlx, "mlx-community/Qwen3-1.7B-4bit"),
    (Family::Mlx, "mlx-community/gemma-3-1b-it-4bit"),
    (Family::Mlx, "mlx-community/Llama-3.2-1B-Instruct-4bit"),
    (Family::Gguf, "unsloth/Qwen3-1.7B-GGUF"),
    (Family::Gguf, "unsloth/gemma-3-1b-it-GGUF"),
    (Family::Gguf, "bartowski/Llama-3.2-1B-Instruct-GGUF"),
];

/// Which weight format a search or listing covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Mlx,
    Gguf,
}

impl Family {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mlx => "MLX",
            Self::Gguf => "GGUF",
        }
    }

    pub fn other(self) -> Self {
        match self {
            Self::Mlx => Self::Gguf,
            Self::Gguf => Self::Mlx,
        }
    }

    fn filter(self) -> &'static str {
        match self {
            Self::Mlx => "mlx",
            Self::Gguf => "gguf",
        }
    }
}

/// The family to search first: MLX on Apple Silicon, where it runs natively; GGUF elsewhere.
pub fn default_family() -> Family {
    default_family_for(cfg!(all(target_os = "macos", target_arch = "aarch64")))
}

pub fn default_family_for(apple_silicon: bool) -> Family {
    if apple_silicon {
        Family::Mlx
    } else {
        Family::Gguf
    }
}

/// One search result or curated entry. Metadata is `None` when unavailable, including offline entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub repo: String,
    pub family: Family,
    pub downloads: Option<u64>,
    pub likes: Option<u64>,
    /// Hugging Face's `createdAt` string, unchanged; not the last modification date.
    pub created_at: Option<String>,
    /// Hugging Face's `usedStorage`, in bytes for the whole repository, not a chosen download.
    /// The model-list API omits this; [`metadata`] can retrieve it for one repository.
    pub size_bytes: Option<u64>,
    /// Hugging Face's `safetensors.total`; absent for repositories without that metadata.
    /// This is not inferred from a model name or a GGUF file's size.
    pub parameters: Option<u64>,
}

/// The curated list for `family`, in display order.
pub fn recommended(family: Family) -> Vec<Hit> {
    curated(RECOMMENDED, family)
}

/// The curated small models for the memory role in `family`, in display order.
pub fn recommended_memory(family: Family) -> Vec<Hit> {
    curated(MEMORY_RECOMMENDED, family)
}

fn curated(list: &[(Family, &str)], family: Family) -> Vec<Hit> {
    list.iter()
        .filter(|(kind, _)| *kind == family)
        .map(|(_, repo)| Hit {
            repo: (*repo).to_owned(),
            family,
            downloads: None,
            likes: None,
            created_at: None,
            size_bytes: None,
            parameters: None,
        })
        .collect()
}

fn get_text(url: &str, query: &[(&str, &str)]) -> Result<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .into();
    let mut request = agent.get(url);
    for &(key, value) in query {
        request = request.query(key, value);
    }
    let mut response = request
        .call()
        .with_context(|| format!("reaching Hugging Face at {url}"))?;
    ensure!(
        response.status().is_success(),
        "Hugging Face answered HTTP {} for {url}",
        response.status()
    );
    response
        .body_mut()
        .read_to_string()
        .context("reading the Hugging Face response")
}

#[derive(Deserialize)]
struct SearchItem {
    id: String,
    #[serde(default)]
    downloads: Option<u64>,
    #[serde(default)]
    likes: Option<u64>,
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
    #[serde(rename = "usedStorage", default)]
    size_bytes: Option<u64>,
    #[serde(default)]
    safetensors: Option<SafetensorsInfo>,
}

#[derive(Deserialize)]
struct SafetensorsInfo {
    #[serde(default)]
    total: Option<u64>,
}

impl SearchItem {
    fn into_hit(self, family: Family) -> Hit {
        Hit {
            repo: self.id,
            family,
            downloads: self.downloads,
            likes: self.likes,
            created_at: self.created_at.filter(|date| !date.trim().is_empty()),
            size_bytes: self.size_bytes,
            parameters: self.safetensors.and_then(|info| info.total),
        }
    }
}

fn search_query(family: Family, query: &str) -> Vec<(&str, &str)> {
    let mut params = vec![
        ("filter", family.filter()),
        ("sort", "downloads"),
        ("direction", "-1"),
        ("limit", SEARCH_LIMIT),
        // Supported list-model expansions. `usedStorage` is detail-only: expanding it
        // here returns HTTP 400. Repeated `expand` keys match huggingface_hub.list_models.
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

/// Repositories of `family` matching `query`, most downloaded first. Network only; no weights.
/// An empty query browses popular repositories. [`recommended`] remains available offline.
pub fn search(family: Family, query: &str) -> Result<Vec<Hit>> {
    let text = get_text(API, &search_query(family, query))?;
    parse_search(family, &text)
}

/// Embedding-model repositories matching `query`, most downloaded first. Network only; no weights.
/// An empty query browses popular embedders. Uses the `feature-extraction` pipeline tag so results
/// are embedders rather than chat models; [`search`] remains for the weight-family shelves.
pub fn search_embedding(query: &str) -> Result<Vec<Hit>> {
    let mut params = vec![
        ("pipeline_tag", "feature-extraction"),
        ("sort", "downloads"),
        ("direction", "-1"),
        ("limit", SEARCH_LIMIT),
        ("expand", "downloads"),
        ("expand", "likes"),
        ("expand", "createdAt"),
    ];
    let query = query.trim();
    if !query.is_empty() {
        params.push(("search", query));
    }
    let text = get_text(API, &params)?;
    parse_search(Family::Gguf, &text)
}

fn image_search_query(query: &str) -> Vec<(&str, &str)> {
    let mut params = search_query(Family::Mlx, query);
    params.push(("pipeline_tag", "text-to-image"));
    params
}

/// MLX text-to-image repositories, using declared Hub tags rather than repository names.
/// Network metadata only; weights are fetched only after an explicit model selection.
pub fn search_images(query: &str) -> Result<Vec<Hit>> {
    let text = get_text(API, &image_search_query(query))?;
    parse_search(Family::Mlx, &text)
}

/// Metadata for one repository, including its whole-repository storage when reported.
/// Network only; no file listing or weights are fetched.
pub fn metadata(family: Family, repo: &str) -> Result<Hit> {
    ensure!(
        is_repo_id(repo),
        "{repo:?} is not a Hugging Face repository id"
    );
    let text = get_text(&format!("{API}/{repo}"), &[])?;
    parse_metadata(family, &text)
}

fn parse_metadata(family: Family, json: &str) -> Result<Hit> {
    let item: SearchItem =
        serde_json::from_str(json).context("reading the Hugging Face model metadata")?;
    ensure!(
        is_repo_id(&item.id),
        "invalid repository id in model metadata"
    );
    Ok(item.into_hit(family))
}

/// Parses the model-list JSON. Entries whose id is not a valid repository id are dropped.
pub fn parse_search(family: Family, json: &str) -> Result<Vec<Hit>> {
    let items: Vec<SearchItem> =
        serde_json::from_str(json).context("reading the Hugging Face search response")?;
    Ok(items
        .into_iter()
        .filter(|item| is_repo_id(&item.id))
        .map(|item| item.into_hit(family))
        .collect())
}

#[derive(Deserialize)]
struct TreeItem {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    lfs: Option<LfsInfo>,
}

#[derive(Deserialize)]
struct LfsInfo {
    size: u64,
}

/// Files on the main branch of `repo` with their sizes in bytes. Network only; no weights.
pub fn tree(repo: &str) -> Result<Vec<(String, u64)>> {
    ensure!(
        is_repo_id(repo),
        "{repo:?} is not a Hugging Face repository id"
    );
    let text = get_text(&format!("{API}/{repo}/tree/main"), &[("recursive", "true")])?;
    parse_tree(&text)
}

/// Parses the tree JSON into `(path, bytes)` for regular files; LFS-stored files report their LFS size.
pub fn parse_tree(json: &str) -> Result<Vec<(String, u64)>> {
    let items: Vec<TreeItem> =
        serde_json::from_str(json).context("reading the repository file list")?;
    Ok(items
        .into_iter()
        .filter(|item| item.kind == "file")
        .map(|item| {
            let size = item.lfs.map_or(item.size, |lfs| lfs.size);
            (item.path, size)
        })
        .collect())
}

/// One GGUF quantization the user can download: a single file or a complete split-shard set, plus
/// the projector when the repository has exactly one. `bytes` counts every file in `files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quant {
    pub label: String,
    pub files: Vec<String>,
    pub bytes: u64,
}

/// The GGUF quantizations of a repository from its file list, smallest first. A repository without
/// GGUF weights yields none. When there are several projectors none is attached; fetch the wanted
/// one manually.
pub fn quant_options(files: &[(String, u64)]) -> Vec<Quant> {
    let size = |name: &str| {
        files
            .iter()
            .find(|(file, _)| file == name)
            .map_or(0, |(_, bytes)| *bytes)
    };
    let gguf: Vec<&String> = files
        .iter()
        .map(|(file, _)| file)
        .filter(|file| file.to_ascii_lowercase().ends_with(".gguf"))
        .collect();
    let (projectors, models): (Vec<&String>, Vec<&String>) = gguf
        .into_iter()
        .partition(|file| file_name(file).to_ascii_lowercase().contains("mmproj"));
    // Shards of one set share a directory and set prefix; each complete set is one quantization.
    let mut groups: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for file in models {
        let (key, label) = match Shard::parse(file) {
            Some(shard) => {
                let (prefix, total) = shard.set;
                // The shared parser retains the directory in the set identity; only the
                // display label drops it. Identically named sets in different dirs stay distinct.
                let label = file_name(&prefix).to_owned();
                (format!("{prefix}-of-{total}"), label)
            }
            None => {
                let name = file_name(file);
                (
                    file.clone(),
                    name.strip_suffix(".gguf").unwrap_or(name).to_owned(),
                )
            }
        };
        groups
            .entry(key)
            .or_insert_with(|| (label, Vec::new()))
            .1
            .push(file.clone());
    }
    let projector = (projectors.len() == 1).then(|| projectors[0].to_string());
    let mut quants: Vec<Quant> = groups
        .into_values()
        .map(|(label, mut names)| {
            names.sort();
            let mut bytes: u64 = names.iter().map(|name| size(name)).sum();
            if let Some(projector) = &projector {
                bytes += size(projector);
                names.push(projector.clone());
            }
            Quant {
                label,
                files: names,
                bytes,
            }
        })
        .collect();
    quants.sort_by(|a, b| a.bytes.cmp(&b.bytes).then_with(|| a.label.cmp(&b.label)));
    quants
}

/// Bytes an MLX snapshot download fetches: every listed file except GGUF weights.
pub fn snapshot_bytes(files: &[(String, u64)]) -> u64 {
    files
        .iter()
        .filter(|(file, _)| !file.to_ascii_lowercase().ends_with(".gguf"))
        .map(|(_, bytes)| *bytes)
        .sum()
}

/// Bytes in the Hugging Face cache for `repo`, complete and partial blobs alike. Progress display only.
pub fn cached_bytes(repo: &str) -> u64 {
    let Some(cache) = crate::llm::catalog::hf_cache() else {
        return 0;
    };
    let blobs = cache
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("blobs");
    let Ok(entries) = fs::read_dir(blobs) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// Parameter count written in a repository name (`Qwen3-4B-4bit` gives `4B`). Names often omit it.
pub fn params_hint(repo: &str) -> Option<String> {
    repo.rsplit('/')
        .next()?
        .split(['-', '_'])
        .find_map(|segment| {
            let count = segment.strip_suffix(['B', 'b'])?;
            (count.chars().any(|c| c.is_ascii_digit())
                && count.chars().all(|c| c.is_ascii_digit() || c == '.'))
            .then(|| format!("{count}B"))
        })
}

/// Reuse complete local weights, otherwise download a Hugging Face repository.
/// Local paths are never interpreted as repository ids.
pub fn ensure_local_model(python: &Path, model: &str) -> Result<String> {
    ensure_local_model_with(
        python,
        model,
        &Choice::Auto,
        &AtomicBool::new(false),
        &|_: &str| {},
    )
}

/// How an explicit download picks its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Plan from the repository listing; refuses GGUF repositories with several quantizations.
    Auto,
    /// Exactly these repository files, as picked from [`quant_options`]. The cache and listing are
    /// bypassed for the choice, so the picked quantization is what gets fetched.
    Files(Vec<String>),
}

/// Lists the files of a Hugging Face repository.
type Listing<'a> = Box<dyn Fn(&str) -> Result<Vec<String>> + 'a>;

/// [`ensure_local_model`] with a file choice, a cancel flag and progress lines. A cancelled download
/// fails with "download cancelled". Files Hugging Face already cached are reused by the next attempt.
pub fn ensure_local_model_with(
    python: &Path,
    model: &str,
    choice: &Choice,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<String> {
    ensure!(!cancel.load(Ordering::SeqCst), "download cancelled");
    let listing: Listing<'_> = match choice {
        Choice::Auto => Box::new(|repo: &str| -> Result<Vec<String>> {
            progress(&format!("listing the files of {repo}"));
            let output = run_python(python, LIST, &[repo.to_owned()], 600, cancel, progress).with_context(
                || {
                    format!("listing files of {repo:?}; check the repository id, network access, and Hugging Face access")
                },
            )?;
            serde_json::from_str::<Vec<String>>(output.trim())
                .context("reading the repository file list")
        }),
        Choice::Files(files) => {
            let files = files.clone();
            Box::new(move |_: &str| -> Result<Vec<String>> { Ok(files.clone()) })
        }
    };
    let fetch = |repo: &str, plan: &Fetch| -> Result<Vec<String>> {
        progress(&format!("downloading {repo}"));
        let (script, args) = match plan {
            Fetch::Snapshot => (SNAPSHOT, vec![repo.to_owned()]),
            Fetch::Files(files) => {
                let mut args = vec![repo.to_owned()];
                args.extend(files.iter().cloned());
                (FILES, args)
            }
        };
        let output = run_python(python, script, &args, 3600, cancel, progress).with_context(|| {
            format!("downloading local model {repo:?}; check network access, Hugging Face access, and the local Python runtime")
        })?;
        let paths: Vec<String> = match plan {
            Fetch::Snapshot => vec![serde_json::from_str(output.trim())?],
            Fetch::Files(_) => serde_json::from_str(output.trim())?,
        };
        progress(&format!("checking {repo}"));
        Ok(paths)
    };
    prepare(
        model,
        matches!(choice, Choice::Auto),
        crate::llm::catalog::resolve_local_model,
        &*listing,
        fetch,
    )
    .map_err(|error| {
        if cancel.load(Ordering::SeqCst) {
            anyhow::anyhow!("download cancelled")
        } else {
            error
        }
    })
}

/// What an explicit download fetches, decided from the repository listing before any weights move.
#[derive(Debug, PartialEq, Eq)]
enum Fetch {
    /// MLX repository: the whole snapshot except GGUF files.
    Snapshot,
    /// Exactly these repository-relative files; the first is the model.
    Files(Vec<String>),
}

fn run_python(
    python: &Path,
    script: &str,
    args: &[String],
    timeout: u64,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<String> {
    let mut argv = vec!["-c".to_owned(), script.to_owned()];
    argv.extend(args.iter().cloned());
    {
        let mut command = std::process::Command::new(python);
        command.args(&argv);
        let timeout = Duration::from_secs(timeout);
        let output = crate::runtime::process::run(
            command,
            None,
            timeout,
            8 * 1024 * 1024,
            cancel,
            &mut |stream, bytes| {
                if stream == crate::runtime::process::Stream::Stderr {
                    progress(&String::from_utf8_lossy(bytes));
                }
            },
        )
        .map_err(anyhow::Error::msg)?;
        ensure!(
            output.success() && !output.output_incomplete,
            "{}",
            output.failure("model acquisition", timeout)
        );
        String::from_utf8(output.stdout).context("model acquisition output is not UTF-8")
    }
}

/// `reuse_cache` asks `resolve` for a complete local copy of `model` first. A picked file must skip
/// that lookup: another quantization of the same repository may already be cached.
fn prepare(
    model: &str,
    reuse_cache: bool,
    resolve: impl Fn(&str) -> Result<String>,
    list: impl FnOnce(&str) -> Result<Vec<String>>,
    fetch: impl FnOnce(&str, &Fetch) -> Result<Vec<String>>,
) -> Result<String> {
    let error = if reuse_cache {
        match resolve(model) {
            Ok(path) => return Ok(path),
            Err(error) => error,
        }
    } else {
        anyhow::anyhow!("{model:?} is a picked file, not a cached model")
    };
    if Path::new(model).exists() {
        return Err(error);
    }
    if !is_repo_id(model) {
        bail!(
            "{model:?} is neither an existing local model path nor a Hugging Face repository id (owner/name); pass a complete local MLX or GGUF model, or a repository id"
        );
    }
    let plan = plan(model, &list(model)?)?;
    let paths = fetch(model, &plan)?;
    let snapshot = paths.first().context("download returned no model path")?;
    ensure!(
        Path::new(snapshot).is_absolute(),
        "download returned a non-absolute snapshot path"
    );
    resolve(snapshot).context("downloaded model has no complete usable weights")
}

fn is_repo_id(model: &str) -> bool {
    let parts: Vec<_> = model.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && *part != "."
                && *part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
}

/// Choose the files to fetch for `repo` from its listing. Never picks between GGUF quantizations.
fn plan(repo: &str, files: &[String]) -> Result<Fetch> {
    if files.iter().any(|file| file.ends_with(".safetensors")) {
        return Ok(Fetch::Snapshot);
    }
    let gguf: Vec<&String> = files
        .iter()
        .filter(|file| file.to_ascii_lowercase().ends_with(".gguf"))
        .collect();
    let (projectors, mut models): (Vec<&String>, Vec<&String>) = gguf
        .into_iter()
        .partition(|file| file_name(file).to_ascii_lowercase().contains("mmproj"));
    if models.is_empty() {
        bail!(
            "{repo:?} has no MLX safetensors or GGUF weights; Dial downloads only those formats (see docs/model-downloads.md)"
        );
    }
    models.sort();
    let mut selected: Vec<String> = if models.len() == 1 && Shard::parse(models[0]).is_none() {
        vec![models[0].clone()]
    } else {
        let shards: Vec<Option<Shard>> = models.iter().map(|file| Shard::parse(file)).collect();
        match shards.first().and_then(Option::as_ref) {
            Some(first)
                if shards
                    .iter()
                    .all(|shard| shard.as_ref().is_some_and(|s| s.set == first.set)) =>
            {
                let total = first.set.1;
                let mut indices: Vec<u32> = shards.iter().flatten().map(|s| s.index).collect();
                indices.sort_unstable();
                let complete = indices.len() == total as usize
                    && indices
                        .iter()
                        .zip(1..)
                        .all(|(index, expected)| *index == expected);
                ensure!(
                    complete,
                    "{repo:?} has an incomplete GGUF split set (expected {total} shards); download the missing shards manually"
                );
                models.iter().map(|file| (*file).clone()).collect()
            }
            _ => bail!(
                "{repo:?} has {} GGUF quantizations ({}); Dial will not choose one. Pick one in /models → Get models (press / to search), or download the one you want with `huggingface-cli download {repo} <file>` and set the model to its local path",
                models.len(),
                models
                    .iter()
                    .map(|file| file.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    };
    if projectors.len() > 1 {
        bail!(
            "{repo:?} has {} multimodal projectors ({}); Dial will not choose one. Download the wanted projector manually next to the model",
            projectors.len(),
            projectors
                .iter()
                .map(|file| file.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    selected.extend(projectors.into_iter().cloned());
    Ok(Fetch::Files(selected))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests;
