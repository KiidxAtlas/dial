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
    RECOMMENDED
        .iter()
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
mod tests {
    use super::*;
    use std::cell::Cell;

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn sized(names: &[(&str, u64)]) -> Vec<(String, u64)> {
        names
            .iter()
            .map(|(name, bytes)| ((*name).to_owned(), *bytes))
            .collect()
    }

    #[test]
    fn complete_models_never_download() {
        assert_eq!(
            prepare(
                "org/model",
                true,
                |_| Ok("/cached/model".into()),
                |_| panic!("unexpected listing"),
                |_, _| panic!("unexpected download")
            )
            .unwrap(),
            "/cached/model"
        );
    }

    #[test]
    fn missing_mlx_repository_takes_a_snapshot_and_validates() {
        let downloaded = Cell::new(false);
        let path = prepare(
            "org/model",
            true,
            |value| {
                if value == "/snapshot" {
                    Ok(value.into())
                } else {
                    bail!("missing or partial")
                }
            },
            |_| {
                Ok(files(&[
                    "config.json",
                    "model.safetensors",
                    "model-q4.gguf",
                ]))
            },
            |repo, fetch| {
                assert_eq!(repo, "org/model");
                assert_eq!(fetch, &Fetch::Snapshot);
                downloaded.set(true);
                Ok(vec!["/snapshot".into()])
            },
        )
        .unwrap();
        assert!(downloaded.get());
        assert_eq!(path, "/snapshot");
    }

    #[test]
    fn invalid_paths_do_not_list_or_download() {
        for model in [
            "/missing/model",
            "./model",
            "../model",
            "model",
            "org/model/subdir",
            "org/model/file.gguf",
            "",
        ] {
            assert!(
                prepare(
                    model,
                    true,
                    |_| bail!("missing"),
                    |_| panic!("unexpected listing"),
                    |_, _| panic!("unexpected download")
                )
                .is_err(),
                "{model:?} must be rejected"
            );
        }
    }

    #[test]
    fn download_failure_and_incomplete_snapshots_are_errors() {
        assert!(
            prepare(
                "org/model",
                true,
                |_| bail!("missing"),
                |_| Ok(files(&["model.safetensors"])),
                |_, _| bail!("network failure")
            )
            .unwrap_err()
            .to_string()
            .contains("network failure")
        );
        assert!(
            prepare(
                "org/model",
                true,
                |_| bail!("missing"),
                |_| Ok(files(&["model.safetensors"])),
                |_, _| Ok(vec!["/partial".into()])
            )
            .unwrap_err()
            .to_string()
            .contains("no complete usable weights")
        );
    }

    #[test]
    fn single_gguf_downloads_only_that_file_and_its_projector() {
        let names = files(&["README.md", "m-Q4_K_M.gguf", "mmproj-f16.gguf"]);
        assert_eq!(
            plan("org/gguf", &names).unwrap(),
            Fetch::Files(files(&["m-Q4_K_M.gguf", "mmproj-f16.gguf"]))
        );
        assert_eq!(
            plan("org/gguf", &files(&["m-Q4_K_M.gguf"])).unwrap(),
            Fetch::Files(files(&["m-Q4_K_M.gguf"]))
        );
    }

    #[test]
    fn multiple_quantizations_are_never_chosen_or_downloaded() {
        let names = files(&["m-Q4_K_M.gguf", "m-Q8_0.gguf", "mmproj-f16.gguf"]);
        let error = plan("org/gguf", &names).unwrap_err().to_string();
        assert!(error.contains("2 GGUF quantizations"), "{error}");
        assert!(error.contains("m-Q8_0.gguf"), "{error}");
        assert!(
            error.contains("huggingface-cli download org/gguf <file>"),
            "{error}"
        );
        assert!(error.contains("press / to search"), "{error}");
        let downloaded = Cell::new(false);
        assert!(
            prepare(
                "org/gguf",
                true,
                |_| bail!("missing"),
                |_| Ok(names.clone()),
                |_, _| {
                    downloaded.set(true);
                    Ok(vec![])
                }
            )
            .is_err()
        );
        assert!(!downloaded.get());
    }

    #[test]
    fn complete_split_sets_download_together_and_partial_sets_fail() {
        assert_eq!(
            plan(
                "org/big",
                &files(&["q/m-00002-of-00002.gguf", "q/m-00001-of-00002.gguf"])
            )
            .unwrap(),
            Fetch::Files(files(&[
                "q/m-00001-of-00002.gguf",
                "q/m-00002-of-00002.gguf"
            ]))
        );
        let error = plan(
            "org/big",
            &files(&["m-00001-of-00003.gguf", "m-00002-of-00003.gguf"]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("incomplete GGUF split set"), "{error}");
        assert!(plan("org/big", &files(&["m-00001-of-00002.gguf"])).is_err());
        assert!(plan("org/big", &files(&["m-00002-of-00002.gguf"])).is_err());
        // Two different split sets are two quantizations, not one model.
        assert!(
            plan(
                "org/big",
                &files(&["a-00001-of-00002.gguf", "b-00001-of-00001.gguf"])
            )
            .is_err()
        );
    }

    #[test]
    fn ambiguous_projectors_and_missing_weights_are_explained() {
        let error = plan(
            "org/vl",
            &files(&["m.gguf", "mmproj-f16.gguf", "mmproj-f32.gguf"]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("2 multimodal projectors"), "{error}");
        let error = plan("org/none", &files(&["model.bin", "README.md"]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no MLX safetensors or GGUF weights"),
            "{error}"
        );
        assert!(error.contains("docs/model-downloads.md"), "{error}");
    }

    #[test]
    fn mixed_mlx_repository_ignores_gguf_by_snapshot() {
        assert_eq!(
            plan(
                "org/mix",
                &files(&["model.safetensors", "a-Q4.gguf", "a-Q8.gguf"])
            )
            .unwrap(),
            Fetch::Snapshot
        );
    }

    #[test]
    fn picked_file_skips_the_cache_lookup_and_fetches_only_that_file() {
        // Another quantization may already be cached, so the cache must not answer for a picked file.
        let looked_up = Cell::new(0);
        let resolved = prepare(
            "org/gguf",
            false,
            |value| {
                looked_up.set(looked_up.get() + 1);
                Ok(value.into())
            },
            |_| Ok(files(&["m-Q8_0.gguf"])),
            |repo, fetch| {
                assert_eq!(repo, "org/gguf");
                assert_eq!(fetch, &Fetch::Files(files(&["m-Q8_0.gguf"])));
                Ok(vec!["/snap/m-Q8_0.gguf".into()])
            },
        )
        .unwrap();
        assert_eq!(resolved, "/snap/m-Q8_0.gguf");
        // Only the final resolve of the downloaded path ran; the repository lookup did not.
        assert_eq!(looked_up.get(), 1);
    }

    #[test]
    fn quant_options_list_each_gguf_quantization_smallest_first() {
        let options = quant_options(&sized(&[
            ("README.md", 10),
            ("m-Q8_0.gguf", 800),
            ("m-Q4_K_M.gguf", 400),
        ]));
        let labels: Vec<_> = options.iter().map(|q| q.label.as_str()).collect();
        assert_eq!(labels, ["m-Q4_K_M", "m-Q8_0"]);
        assert_eq!(options[0].files, files(&["m-Q4_K_M.gguf"]));
        assert_eq!(options[0].bytes, 400);
        assert_eq!(options[1].bytes, 800);
    }

    #[test]
    fn a_picked_quant_is_exactly_what_the_planner_accepts() {
        let listing = sized(&[
            ("m-Q4_K_M.gguf", 400),
            ("m-Q8_0.gguf", 800),
            ("mmproj-f16.gguf", 50),
        ]);
        let options = quant_options(&listing);
        let q8 = options.iter().find(|q| q.label == "m-Q8_0").unwrap();
        // The projector travels with the one attached model; the total includes it.
        assert_eq!(q8.files, files(&["m-Q8_0.gguf", "mmproj-f16.gguf"]));
        assert_eq!(q8.bytes, 850);
        assert_eq!(
            plan("org/gguf", &q8.files).unwrap(),
            Fetch::Files(files(&["m-Q8_0.gguf", "mmproj-f16.gguf"]))
        );
    }

    #[test]
    fn split_sets_are_one_quantization_and_projectors_are_not_options() {
        let options = quant_options(&sized(&[
            ("q/big-00001-of-00002.gguf", 500),
            ("q/big-00002-of-00002.gguf", 500),
            ("small.gguf", 100),
            ("mmproj-a.gguf", 5),
            ("mmproj-b.gguf", 5),
        ]));
        assert_eq!(options.len(), 2, "{options:?}");
        assert_eq!(options[0].label, "small");
        assert_eq!(
            options[0].files,
            files(&["small.gguf"]),
            "no projector when several exist"
        );
        assert_eq!(options[1].label, "big");
        assert_eq!(
            options[1].files,
            files(&["q/big-00001-of-00002.gguf", "q/big-00002-of-00002.gguf"])
        );
        assert_eq!(options[1].bytes, 1000);
    }

    #[test]
    fn repositories_without_gguf_offer_no_quantizations() {
        assert!(quant_options(&sized(&[("model.safetensors", 9), ("README.md", 1)])).is_empty());
        assert!(quant_options(&sized(&[("mmproj-f16.gguf", 9)])).is_empty());
    }

    #[test]
    fn search_results_parse_counts_and_skip_invalid_ids() {
        let json = r#"[
            {"id":"mlx-community/Qwen2.5-Coder-7B-Instruct-4bit","downloads":255850,"likes":30,"tags":["mlx"]},
            {"id":"not a repo","downloads":1},
            {"id":"org/no-counts"}
        ]"#;
        let hits = parse_search(Family::Mlx, json).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].repo, "mlx-community/Qwen2.5-Coder-7B-Instruct-4bit");
        assert_eq!(hits[0].downloads, Some(255_850));
        assert_eq!(hits[0].likes, Some(30));
        assert_eq!(hits[0].family, Family::Mlx);
        assert_eq!(hits[1].downloads, None, "missing counts stay unknown");
        assert!(parse_search(Family::Gguf, "[]").unwrap().is_empty());
        assert!(parse_search(Family::Gguf, "<html>").is_err());
    }

    #[test]
    fn model_browse_and_search_request_supported_metadata_fields() {
        for family in [Family::Mlx, Family::Gguf] {
            let browse = search_query(family, " \t ");
            assert_eq!(browse, search_query(family, ""));
            assert!(browse.contains(&("filter", family.filter())));
            assert!(browse.contains(&("sort", "downloads")));
            assert!(browse.contains(&("direction", "-1")));
            assert!(browse.contains(&("limit", "30")));
            assert!(!browse.iter().any(|(key, _)| *key == "search"));
            let expansions: Vec<_> = browse
                .iter()
                .filter_map(|(key, value)| (*key == "expand").then_some(*value))
                .collect();
            assert_eq!(
                expansions,
                ["downloads", "likes", "createdAt", "safetensors"]
            );
            let mut expected = browse;
            expected.push(("search", "Qwen coder"));
            assert_eq!(search_query(family, "  Qwen coder  "), expected);
        }
    }

    #[test]
    fn model_list_and_detail_share_exact_metadata_parsing() {
        // Shape verified against /api/models/mlx-community/Qwen3-4B-4bit.
        let json = r#"{"id":"mlx-community/Qwen3-4B-4bit","downloads":12,"likes":3,
            "createdAt":"2025-04-28T21:28:51.000Z","usedStorage":2274445183,
            "safetensors":{"parameters":{"U32":4022272000,"BF16":196096},"total":4022468096}}"#;
        let detail = parse_metadata(Family::Mlx, json).unwrap();
        let list = parse_search(Family::Mlx, &format!("[{json}]")).unwrap();
        assert_eq!(list, vec![detail.clone()]);
        assert_eq!(
            detail.created_at.as_deref(),
            Some("2025-04-28T21:28:51.000Z")
        );
        assert_eq!(detail.size_bytes, Some(2_274_445_183));
        assert_eq!(detail.parameters, Some(4_022_468_096));
        assert_eq!(detail.downloads, Some(12));
        assert_eq!(detail.likes, Some(3));
    }

    #[test]
    fn absent_metadata_is_not_invented_from_names_dates_or_tensor_types() {
        for fields in [
            "",
            r#", "createdAt":null,"usedStorage":null,"safetensors":null"#,
            r#", "createdAt":" ","safetensors":{}"#,
            r#", "lastModified":"2025-04-28T21:28:51.000Z","size":10,
                "safetensors":{"parameters":{"F16":4000000000}}"#,
        ] {
            let json = format!(r#"{{"id":"org/model-4B"{fields}}}"#);
            let hit = parse_metadata(Family::Gguf, &json).unwrap();
            assert_eq!(hit.created_at, None);
            assert_eq!(hit.size_bytes, None);
            assert_eq!(hit.parameters, None);
        }
        let zero = parse_metadata(
            Family::Mlx,
            r#"{"id":"org/model","usedStorage":0,"safetensors":{"total":0}}"#,
        )
        .unwrap();
        assert_eq!(zero.size_bytes, Some(0));
        assert_eq!(zero.parameters, Some(0));
    }

    #[test]
    fn invalid_metadata_types_and_out_of_range_counts_are_rejected() {
        for fields in [
            r#""createdAt":42"#,
            r#""usedStorage":-1"#,
            r#""usedStorage":1.5"#,
            r#""usedStorage":"1024""#,
            r#""usedStorage":18446744073709551616"#,
            r#""safetensors":{"total":-1}"#,
            r#""safetensors":{"total":"4000"}"#,
            r#""safetensors":false"#,
        ] {
            let json = format!(r#"{{"id":"org/model",{fields}}}"#);
            assert!(parse_metadata(Family::Mlx, &json).is_err(), "{json}");
            assert!(parse_search(Family::Mlx, &format!("[{json}]")).is_err());
        }
        assert!(parse_metadata(Family::Mlx, r#"{"id":"not a repo"}"#).is_err());
        assert!(parse_metadata(Family::Mlx, "[]").is_err());
        assert!(metadata(Family::Mlx, "../invalid/repo").is_err());
    }

    #[test]
    fn tree_listing_keeps_files_and_prefers_lfs_sizes() {
        let json = r#"[
            {"type":"directory","path":"q","size":0},
            {"type":"file","path":"q/m-Q4.gguf","size":135,"lfs":{"size":4000,"oid":"x"}},
            {"type":"file","path":"README.md","size":12}
        ]"#;
        assert_eq!(
            parse_tree(json).unwrap(),
            sized(&[("q/m-Q4.gguf", 4000), ("README.md", 12)])
        );
        assert!(parse_tree("{}").is_err());
    }

    #[test]
    fn snapshot_bytes_leave_gguf_weights_out() {
        assert_eq!(
            snapshot_bytes(&sized(&[
                ("model.safetensors", 900),
                ("config.json", 1),
                ("m.gguf", 500)
            ])),
            901
        );
    }

    #[test]
    fn platform_choice_prefers_mlx_only_on_apple_silicon() {
        assert_eq!(default_family_for(true), Family::Mlx);
        assert_eq!(default_family_for(false), Family::Gguf);
        assert_eq!(Family::Mlx.other(), Family::Gguf);
        assert_eq!(Family::Gguf.label(), "GGUF");
    }

    #[test]
    fn curated_entries_are_valid_ids_in_their_family() {
        for family in [Family::Mlx, Family::Gguf] {
            let list = recommended(family);
            assert!(!list.is_empty());
            for hit in list {
                assert!(is_repo_id(&hit.repo), "{}", hit.repo);
                assert_eq!(hit.family, family);
                assert_eq!(hit.downloads, None);
                assert_eq!(hit.likes, None);
                assert_eq!(hit.created_at, None);
                assert_eq!(hit.size_bytes, None);
                assert_eq!(hit.parameters, None);
                if family == Family::Gguf {
                    assert!(
                        hit.repo.to_ascii_uppercase().ends_with("GGUF"),
                        "{}",
                        hit.repo
                    );
                }
            }
        }
    }

    #[test]
    fn parameter_hints_come_from_the_name_only() {
        assert_eq!(
            params_hint("mlx-community/Qwen3-4B-4bit").as_deref(),
            Some("4B")
        );
        assert_eq!(
            params_hint("mlx-community/Qwen2.5-Coder-7B-Instruct-4bit").as_deref(),
            Some("7B")
        );
        assert_eq!(params_hint("org/Llama-3.2-1.5b").as_deref(), Some("1.5B"));
        assert_eq!(params_hint("org/plain-model"), None);
        assert_eq!(params_hint("org/4bit-tool"), None);
    }

    #[test]
    fn a_cancelled_listing_is_reported_as_cancelled() {
        // The interpreter cannot start, and the cancel flag is already set: the error names the cancel.
        let cancel = AtomicBool::new(true);
        let error = ensure_local_model_with(
            Path::new("/definitely/not/python"),
            "org/model",
            &Choice::Auto,
            &cancel,
            &|_: &str| {},
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("download cancelled"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_plans_gguf_files_and_validates_them_before_caching() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let snapshot = temp.path().join("snapshot");
        fs::create_dir(&snapshot).unwrap();
        let model = snapshot.join("m-Q4_K_M.gguf");
        fs::write(&model, b"GGUFweights").unwrap();
        let marker = temp.path().join("fetched");
        let python = temp.path().join("python");
        let json = serde_json::to_string(&model.to_string_lossy()).unwrap();
        // The fake interpreter answers the listing script and records any file fetch.
        fs::write(
            &python,
            format!(
                "#!/bin/sh\n[ \"$1\" = '-c' ] || exit 2\ncase \"$2\" in\n*list_repo_files*) printf '%s\\n' '[\"m-Q4_K_M.gguf\",\"README.md\"]' ;;\n*hf_hub_download*) touch '{marker}'; printf '[%s]\\n' '{json}' ;;\n*) exit 4 ;;\nesac\n",
                marker = marker.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).unwrap();
        let cancel = AtomicBool::new(false);
        let fetch = |repo: &str, fetch: &Fetch| -> Result<Vec<String>> {
            let output = match fetch {
                Fetch::Snapshot => bail!("unexpected snapshot"),
                Fetch::Files(files) => {
                    let mut args = vec![repo.to_owned()];
                    args.extend(files.iter().cloned());
                    run_python(&python, FILES, &args, 10, &cancel, &|_| {})?
                }
            };
            Ok(serde_json::from_str(output.trim())?)
        };
        let resolved = prepare(
            "org/gguf",
            true,
            |value| {
                if Path::new(value).is_absolute() && value.ends_with(".gguf") {
                    crate::llm::local_format::gguf_file(Path::new(value))
                        .map(|path| path.to_string_lossy().into_owned())
                        .context("not a GGUF")
                } else {
                    bail!("missing")
                }
            },
            |repo| {
                let output = run_python(&python, LIST, &[repo.to_owned()], 10, &cancel, &|_| {})?;
                Ok(serde_json::from_str(output.trim())?)
            },
            fetch,
        )
        .unwrap();
        assert_eq!(
            fs::canonicalize(&resolved).unwrap(),
            fs::canonicalize(&model).unwrap()
        );
        assert!(marker.exists());

        // Multi-quant repositories are rejected by the listing alone: no fetch marker is created.
        fs::remove_file(&marker).unwrap();
        fs::write(
            &python,
            "#!/bin/sh\necho '[\"m-Q4.gguf\",\"m-Q8.gguf\"]'\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&python, fs::Permissions::from_mode(0o700)).unwrap();
        let error = ensure_local_model(&python, "org/multi-quant").unwrap_err();
        assert!(
            format!("{error:#}").contains("2 GGUF quantizations"),
            "{error:#}"
        );
        assert!(!marker.exists());
    }
}
