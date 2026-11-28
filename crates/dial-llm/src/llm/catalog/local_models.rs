//! Local model discovery on disk: Hugging Face cache, LM Studio folders and checkpoint metadata.

use super::local_servers::kind_from_declared;
use super::metadata::{ModelEntry, ModelKind, ModelMetadata, metadata};
use super::probe::context;
use super::{MAX_MODELS, MAX_RESPONSE};
use crate::llm::local_format::LocalFormat;
use crate::llm::local_runtime::{GgufEngines, LocalEngine};
use crate::llm::model_config::ModelSelection;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Preliminary MLX architecture availability; this does not verify adapter training targets.
pub fn local_lora_support(user_dir: &Path, model: &Path) -> Option<bool> {
    lora_support(mlx_architectures(user_dir).as_deref(), model)
}
/// Inkling-2-Mini's vision processor ships in MLX-LM rather than MLX-VLM.
/// Select the server from checkpoint metadata, independently of image-input capability.
pub(crate) fn uses_mlx_vlm(model: &Path) -> bool {
    read_json(&model.join("config.json")).is_some_and(|config| {
        config["model_type"] != "inkling2_loop" && metadata(&config).vision == Some(true)
    })
}

/// Read local capabilities without importing Python or loading model weights. GGUF models are read
/// from their header (chat template) and a sibling `mmproj` projector, which `llama-server` loads.
pub fn local_model_metadata(model: &Path) -> ModelMetadata {
    if is_image_checkpoint(model) {
        return ModelMetadata {
            kind: ModelKind::Image,
            vision: Some(false),
            ..Default::default()
        };
    }
    if (!model.join("config.json").is_file()
        || crate::llm::local_format::detect(model) == Some(LocalFormat::Gguf))
        && let Some(info) = crate::llm::local_format::gguf_info(model)
    {
        let mut metadata = ModelMetadata {
            vision: Some(crate::llm::local_format::gguf_projector(model).is_some()),
            gguf_engines: info.engines(),
            ..Default::default()
        };
        if info
            .architecture
            .as_deref()
            .is_some_and(|arch| GGUF_EMBEDDING_ARCHITECTURES.contains(&arch))
        {
            metadata.kind = ModelKind::Embedding;
        } else if let Some(template) = &info.chat_template {
            classify_template(&mut metadata, template);
        }
        return metadata;
    }
    read_json(&model.join("config.json"))
        .map(|value| {
            let mut info = metadata(&value);
            if is_embedding_checkpoint(model) {
                info.kind = ModelKind::Embedding;
            }
            // A downloaded text-only configuration has no vision encoder.
            info.vision = Some(info.vision.unwrap_or(false));
            // These are template declarations, not inference or quality checks. Missing
            // template markers remain unknown rather than being classified by model name.
            let template = chat_template(model);
            if let Some(template) = template {
                classify_template(&mut info, &template);
            }
            // The model card's `pipeline_tag` is an explicit declaration; it fills only a
            // purpose the configuration left unknown.
            if info.kind == ModelKind::Unknown {
                info.kind = model_card_kind(model);
            }
            info
        })
        .unwrap_or_default()
}
/// The purpose a downloaded model card declares in its front matter (`pipeline_tag:
/// feature-extraction`), matched exactly like other declarations. Reads at most 16 KiB.
fn model_card_kind(model: &Path) -> ModelKind {
    let Ok(file) = fs::File::open(model.join("README.md")) else {
        return ModelKind::Unknown;
    };
    let mut text = String::new();
    if file.take(16 * 1024).read_to_string(&mut text).is_err() {
        return ModelKind::Unknown;
    }
    model_card_kind_in(&text)
}
pub(super) fn model_card_kind_in(card: &str) -> ModelKind {
    let mut lines = card.lines();
    if lines.next().map(str::trim) != Some("---") {
        return ModelKind::Unknown;
    }
    lines
        .take_while(|line| line.trim() != "---")
        .find_map(|line| line.strip_prefix("pipeline_tag:"))
        .map_or(ModelKind::Unknown, |value| {
            kind_from_declared(value.trim().trim_matches(['"', '\'']))
        })
}
/// The chat template a model directory declares: `tokenizer_config.json`'s `chat_template` (a string,
/// a list of named templates, or a map), else `chat_template.jinja`. Read as text; no weights load.
pub(crate) fn chat_template(model: &Path) -> Option<String> {
    let tokenizer = read_json(&model.join("tokenizer_config.json"));
    tokenizer
        .as_ref()
        .and_then(|value| {
            let templates = match &value["chat_template"] {
                Value::String(template) => vec![template.as_str()],
                Value::Array(items) => items
                    .iter()
                    .filter_map(|item| item["template"].as_str())
                    .collect(),
                Value::Object(items) => items.values().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            (!templates.is_empty()).then(|| templates.join("\n"))
        })
        .or_else(|| {
            let file = fs::File::open(model.join("chat_template.jinja")).ok()?;
            let bytes = crate::llm::read_within(file, MAX_RESPONSE)?;
            String::from_utf8(bytes).ok()
        })
}
/// Tool and reasoning support declared by a chat template. These are template declarations, not
/// inference or quality checks; values already known are kept.
fn classify_template(info: &mut ModelMetadata, template: &str) {
    info.tools = info.tools.or_else(|| {
        (template.contains("tools")
            && ["tool_call", "function_call", "[TOOL_CALLS]"]
                .iter()
                .any(|marker| template.contains(marker)))
        .then_some(true)
    });
    info.reasoning = info.reasoning.or_else(|| {
        (template.contains("enable_thinking")
            || (template.contains("<think>") && template.contains("</think>")))
        .then_some(true)
    });
}
/// GGUF `general.architecture` values of encoder-only embedding models (llama.cpp names).
const GGUF_EMBEDDING_ARCHITECTURES: [&str; 8] = [
    "bert",
    "nomic-bert",
    "nomic-bert-moe",
    "jina-bert-v2",
    "modern-bert",
    "neo-bert",
    "eurobert",
    "t5encoder",
];
/// Encoder architectures and model types that embed text without a language-model head.
const EMBEDDING_ARCHITECTURES: [&str; 11] = [
    "BertModel",
    "RobertaModel",
    "XLMRobertaModel",
    "DistilBertModel",
    "MPNetModel",
    "ModernBertModel",
    "NomicBertModel",
    "ElectraModel",
    "AlbertModel",
    "T5EncoderModel",
    "Qwen3ForTextEmbedding",
];
const EMBEDDING_MODEL_TYPES: [&str; 9] = [
    "bert",
    "roberta",
    "xlm-roberta",
    "distilbert",
    "mpnet",
    "modernbert",
    "nomic_bert",
    "electra",
    "albert",
];
/// Whether a local checkpoint is an embedding model, from its files: the sentence-transformers layout,
/// or an encoder `architectures`/`model_type` (causal and conditional generation models never qualify).
pub fn is_embedding_checkpoint(model: &Path) -> bool {
    if ["sentence_bert_config.json", "modules.json", "1_Pooling"]
        .iter()
        .any(|name| model.join(name).exists())
    {
        return true;
    }
    let Some(config) = read_json(&model.join("config.json")) else {
        return false;
    };
    let architectures: Vec<&str> = config
        .get("architectures")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if architectures
        .iter()
        .any(|name| name.ends_with("ForCausalLM") || name.ends_with("ForConditionalGeneration"))
    {
        return false;
    }
    if architectures
        .iter()
        .any(|name| EMBEDDING_ARCHITECTURES.contains(name))
    {
        return true;
    }
    config
        .get("model_type")
        .and_then(Value::as_str)
        .is_some_and(|kind| EMBEDDING_MODEL_TYPES.contains(&kind))
}
/// Local embedding checkpoints cannot chat and Dial cannot run them directly, so they stay
/// unavailable until an embeddings endpoint serves them. Classified from checkpoint metadata only.
pub(super) fn annotate_embedding_checkpoints(models: &mut [ModelEntry]) {
    for entry in models {
        if entry.selection.provider == "mlx"
            && entry.selection.base_url.is_none()
            && entry.is_embedding()
        {
            entry.available = false;
            entry.source = format!(
                "{} · embedding checkpoint, not served: serve it from LM Studio or an embeddings endpoint to use it",
                entry.source
            );
        }
    }
}
pub(crate) fn read_json(path: &Path) -> Option<Value> {
    crate::llm::read_json_within(path, MAX_RESPONSE)
}
/// A diffusion component layout is evidence of image weights, not of a serving transport.
/// Krea stores its transformer at the root; FLUX conversions can omit a root config entirely.
fn is_image_checkpoint(path: &Path) -> bool {
    path.join("vae").is_dir()
        && path.join("text_encoder").is_dir()
        && (path.join("transformer").is_dir()
            || path.join("unet").is_dir()
            || read_json(&path.join("config.json"))
                .is_some_and(|v| v["model_type"] == "krea2_turbo"))
}
pub(super) fn snapshot(path: &Path) -> Option<Option<u32>> {
    if is_image_checkpoint(path) {
        // A transformer layout counts as downloaded when the folder the image server reads (which
        // may be the root's original checkpoint) or its Diffusers `transformer/` is complete; whether
        // stable-diffusion.cpp can run it is reported by `annotate_image_checkpoints`.
        let source = crate::llm::server::image::transformer_source(path);
        let transformer = path.join("transformer");
        if path.join("unet").is_dir() && !transformer.is_dir() {
            complete_safetensors(&path.join("unet"))?;
        } else if complete_safetensors(&source).is_none() {
            complete_safetensors(&transformer)?;
        }
        for component in [path.join("vae"), path.join("text_encoder")] {
            complete_safetensors(&component)?;
        }
        for component in ["text_encoder_2", "text_encoder_3"] {
            if path.join(component).is_dir() {
                complete_safetensors(&path.join(component))?;
            }
        }
        return Some(None);
    }
    let config = read_json(&path.join("config.json"))?;
    config.as_object()?;
    complete_safetensors(path)?;
    Some(context(&config))
}
fn complete_safetensors(path: &Path) -> Option<()> {
    if path.join("model.safetensors.index.json").exists() {
        let index = read_json(&path.join("model.safetensors.index.json"))?;
        let weights = index.get("weight_map")?.as_object()?;
        if weights.is_empty()
            || !weights.values().all(|v| {
                v.as_str().is_some_and(|name| {
                    Path::new(name).components().count() == 1
                        && fs::metadata(path.join(name))
                            .is_ok_and(|meta| meta.is_file() && meta.len() > 0)
                })
            })
        {
            return None;
        }
    } else if !fs::read_dir(path)
        .ok()?
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "safetensors")
                && fs::metadata(entry.path()).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
        })
    {
        return None;
    }
    Some(())
}
pub(super) fn local_models(cache: &Path, trainable: Option<&Path>) -> Vec<ModelEntry> {
    local_models_where(cache, trainable, &|_| true)
}
/// [`local_models`] limited to labels `keep` accepts. Rejected repositories are skipped before their
/// weights are inspected, so resolving one model does not stat every shard in the cache.
pub(super) fn local_models_where(
    cache: &Path,
    trainable: Option<&Path>,
    keep: &dyn Fn(&str) -> bool,
) -> Vec<ModelEntry> {
    let mut models = Vec::new();
    let Ok(repos) = fs::read_dir(cache) else {
        return models;
    };
    let mut repos: Vec<_> = repos
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("models--"))
        .take(4096)
        .collect();
    repos.sort_by_key(|entry| entry.file_name());
    for repo in repos {
        if models.len() >= MAX_MODELS {
            break;
        }
        let label = repo
            .file_name()
            .to_string_lossy()
            .trim_start_matches("models--")
            .replace("--", "/");
        if !keep(&label) {
            continue;
        }
        let root = repo.path();
        let main = fs::File::open(root.join("refs/main"))
            .ok()
            .and_then(|file| {
                let mut text = String::new();
                file.take(256).read_to_string(&mut text).ok()?;
                Some(text.trim().to_owned())
            })
            .filter(|s| {
                !s.is_empty()
                    && s.len() < 256
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            });
        let mut candidates = Vec::new();
        if let Some(main) = main {
            candidates.push(root.join("snapshots").join(main));
        }
        if let Ok(entries) = fs::read_dir(root.join("snapshots")) {
            let mut revisions: Vec<_> = entries.filter_map(Result::ok).take(1024).collect();
            revisions.sort_by_key(|entry| {
                std::cmp::Reverse(entry.metadata().and_then(|m| m.modified()).ok())
            });
            candidates.extend(revisions.into_iter().map(|e| e.path()));
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut mlx = false;
        let mut quants = std::collections::BTreeMap::new();
        for path in candidates {
            if !seen.insert(path.clone()) || !path.is_dir() {
                continue;
            }
            if !mlx && let Some(window) = snapshot(&path) {
                let detected = crate::llm::local_format::detect(&path).unwrap_or(LocalFormat::Mlx);
                let is_exl3 = detected == LocalFormat::Exl3;
                let (source, available) = if is_exl3 {
                    exl3_listing("Hugging Face cache")
                } else {
                    ("Hugging Face cache (MLX)".into(), true)
                };
                models.push(ModelEntry {
                    selection: ModelSelection {
                        model: path.to_string_lossy().into_owned(),
                        context_window: window,
                        ..Default::default()
                    },
                    label: label.clone(),
                    source,
                    available,
                    metadata: ModelMetadata {
                        lora: if is_exl3 {
                            None
                        } else {
                            lora_support(trainable, &path)
                        },
                        format: Some(detected),
                        ..local_model_metadata(&path)
                    },
                });
                mlx = true;
            }
            for file in crate::llm::local_format::snapshot_ggufs(&path) {
                let relative = file
                    .strip_prefix(&path)
                    .expect("inside snapshot")
                    .to_owned();
                quants.entry(relative).or_insert(file);
            }
            if models.len() + quants.len() >= MAX_MODELS {
                break;
            }
        }
        let qualified = mlx || quants.len() > 1;
        for (relative, file) in quants
            .into_iter()
            .take(MAX_MODELS.saturating_sub(models.len()))
        {
            models.push(gguf_entry(
                "Hugging Face cache",
                &label,
                qualified.then_some(relative.as_path()),
                &file,
            ));
        }
    }
    models
}
/// A single installed quant keeps its short repository label; variants name the exact file.
fn gguf_entry(origin: &str, repo: &str, relative: Option<&Path>, file: &Path) -> ModelEntry {
    let metadata = local_model_metadata(file);
    let (source, available) = gguf_listing(origin, metadata.gguf_engines);
    ModelEntry {
        selection: ModelSelection {
            model: file.to_string_lossy().into_owned(),
            ..Default::default()
        },
        label: relative.map_or_else(|| repo.to_owned(), |r| format!("{repo} · {}", r.display())),
        source,
        available,
        metadata: ModelMetadata {
            format: Some(LocalFormat::Gguf),
            ..metadata
        },
    }
}

/// Availability is platform compatibility, not proof that the executable is installed.
fn gguf_listing(origin: &str, loaders: GgufEngines) -> (String, bool) {
    let label = format!("{origin} (GGUF)");
    if loaders == GgufEngines::AtomicOnly {
        let supported =
            LocalEngine::choices(LocalFormat::Gguf, loaders).contains(&LocalEngine::Atomic);
        let note = if supported {
            "requires Atomic Engine"
        } else {
            "not runnable locally: Atomic Engine requires Apple Silicon macOS"
        };
        return (format!("{label} · {note}"), supported);
    }
    if crate::llm::local_format::llama_server().is_some() {
        (label, true)
    } else {
        (
            format!("{label} · llama.cpp not installed; Dial offers to install it on first use"),
            true,
        )
    }
}

/// Availability for EXL3 checkpoints: requires a non-macOS host with NVIDIA CUDA + ExLlamaV3/TabbyAPI.
fn exl3_listing(origin: &str) -> (String, bool) {
    let label = format!("{origin} (EXL3)");
    if cfg!(target_os = "macos") {
        (
            format!("{label} · requires NVIDIA CUDA (ExLlamaV3/TabbyAPI); not runnable on macOS"),
            false,
        )
    } else if crate::llm::local_format::exl3_server().is_some() {
        (label, true)
    } else {
        (
            format!("{label} · ExLlamaV3/TabbyAPI server not found (set DIAL_EXL3_SERVER)"),
            true,
        )
    }
}
/// Filesystem-only availability check for a resolved route; never interpret the path as a repo id.
pub(crate) fn local_weights_present(path: &Path) -> bool {
    (path.is_dir() && snapshot(path).is_some())
        || crate::llm::local_format::gguf_file(path).is_some()
}

/// Resolve local inference without asking Hugging Face to look up a revision.
pub fn resolve_local_model(model: &str) -> Result<String> {
    resolve_local_model_in(model, hf_cache().as_deref(), lmstudio_folder().as_deref())
}
pub(super) fn resolve_local_model_in(
    model: &str,
    cache: Option<&Path>,
    studio: Option<&Path>,
) -> Result<String> {
    let path = Path::new(model);
    if path.is_file()
        && let Some(file) = crate::llm::local_format::gguf_file(path)
    {
        // HF snapshot files can be symlinks to extensionless blobs. Keep the filename and its
        // siblings (projector/shards) while anchoring a relative selection outside the cwd.
        return Ok(std::path::absolute(file)?.to_string_lossy().into_owned());
    }
    if path.is_dir() && local_weights_present(path) {
        return Ok(fs::canonicalize(path)?.to_string_lossy().into_owned());
    }
    if crate::llm::local_format::detect(path) == Some(crate::llm::local_format::LocalFormat::Splash)
    {
        anyhow::bail!(
            "{model:?} is a Splash bundle, which only LM Studio can run: load it in LM Studio and select it under the LM Studio provider"
        );
    }
    // Only the requested label is inspected: listing every download stats each shard of every model.
    let wanted = |repo: &str| {
        repo == model
            || model
                .strip_prefix(repo)
                .is_some_and(|s| s.starts_with(" · "))
    };
    let mut entries = cache
        .map(|cache| local_models_where(cache, None, &wanted))
        .unwrap_or_default();
    if let Some(studio) = studio {
        entries.extend(lmstudio_mlx_models_where(studio, None, &wanted));
        entries.extend(lmstudio_gguf_models_where(studio, &wanted));
    }
    if let Some(entry) = entries.iter().find(|entry| entry.label == model) {
        let local = Path::new(&entry.selection.model);
        let resolved = if local.is_file() {
            std::path::absolute(local)?
        } else {
            fs::canonicalize(local)?
        };
        return Ok(resolved.to_string_lossy().into_owned());
    }
    if entries.iter().any(|entry| {
        entry
            .label
            .strip_prefix(model)
            .is_some_and(|s| s.starts_with(" · "))
    }) {
        anyhow::bail!(
            "local model {model:?} has multiple installed variants; choose a file in /models or specify its exact path"
        );
    }
    anyhow::bail!(
        "local model {model:?} has no complete downloaded weights (MLX safetensors or a GGUF file); inference is offline. Download this model first or select a downloaded local model in /models (for architecture maintenance, change the Architecture model in Settings → Model → Project understanding)"
    )
}
pub(crate) use crate::config::paths::hf_cache;
/// The `mlx_lm/models` directory of dial's Python environment (`dial setup` creates it under the user dir).
pub(super) fn mlx_architectures(user_dir: &Path) -> Option<PathBuf> {
    let lib = crate::config::paths::python_dir(user_dir).join(".venv/lib");
    fs::read_dir(&lib)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("python"))
        .map(|entry| entry.path().join("site-packages/mlx_lm/models"))
        .find(|dir| dir.is_dir())
}
/// Whether mlx-lm implements the architecture in `model/config.json` (`model_type`, or the text model's
/// for multimodal configs). A static check: no weights are loaded.
pub(super) fn lora_support(architectures: Option<&Path>, model: &Path) -> Option<bool> {
    let architectures = architectures?;
    let config = read_json(&model.join("config.json"))?;
    let kind = config
        .get("model_type")
        .or_else(|| config.pointer("/text_config/model_type"))
        .and_then(Value::as_str)?;
    if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Some(false);
    }
    // MLX also resolves architecture aliases (for example gemma4_unified).
    // Absence of a same-named module cannot prove incompatibility.
    architectures
        .join(format!("{kind}.py"))
        .is_file()
        .then_some(true)
}
/// LM Studio's download folder: its `downloadsFolder` setting, else `~/.lmstudio/models`.
pub(super) fn lmstudio_folder() -> Option<PathBuf> {
    let home = crate::config::paths::home_dir()?;
    let configured = read_json(&home.join(".lmstudio/settings.json")).and_then(|settings| {
        settings["downloadsFolder"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    });
    Some(configured.unwrap_or_else(|| home.join(".lmstudio/models")))
}
/// MLX models LM Studio downloaded, laid out as `<folder>/<publisher>/<model>/`. Only folders that
/// pass the same check as Hugging Face snapshots (config plus complete safetensors) are listed, so GGUF
/// downloads and LM Studio-specific bundles are skipped. Entries are labelled `publisher/model`.
pub(super) fn lmstudio_mlx_models(folder: &Path, trainable: Option<&Path>) -> Vec<ModelEntry> {
    lmstudio_mlx_models_where(folder, trainable, &|_| true)
}
/// [`lmstudio_mlx_models`] limited to labels `keep` accepts, checked before the weights are read.
pub(super) fn lmstudio_mlx_models_where(
    folder: &Path,
    trainable: Option<&Path>,
    keep: &dyn Fn(&str) -> bool,
) -> Vec<ModelEntry> {
    let sorted = |dir: &Path| -> Vec<fs::DirEntry> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .map(|it| it.filter_map(Result::ok).take(4096).collect())
            .unwrap_or_default();
        entries.sort_by_key(|entry| entry.file_name());
        entries
    };
    let mut models = Vec::new();
    for publisher in sorted(folder) {
        for repo in sorted(&publisher.path()) {
            if models.len() >= MAX_MODELS {
                return models;
            }
            let label = format!(
                "{}/{}",
                publisher.file_name().to_string_lossy(),
                repo.file_name().to_string_lossy()
            );
            if !keep(&label) {
                continue;
            }
            let path = repo.path();
            let Some(window) = snapshot(&path) else {
                continue;
            };
            let detected = crate::llm::local_format::detect(&path).unwrap_or(LocalFormat::Mlx);
            let is_exl3 = detected == LocalFormat::Exl3;
            let (source, available) = if is_exl3 {
                exl3_listing("LM Studio download")
            } else {
                ("LM Studio download (MLX)".into(), true)
            };
            models.push(ModelEntry {
                selection: ModelSelection {
                    model: path.to_string_lossy().into_owned(),
                    context_window: window,
                    ..ModelSelection::default()
                },
                label,
                source,
                available,
                metadata: ModelMetadata {
                    lora: if is_exl3 {
                        None
                    } else {
                        lora_support(trainable, &path)
                    },
                    format: Some(detected),
                    ..local_model_metadata(&path)
                },
            });
        }
    }
    models
}
/// Independently selectable GGUF targets in LM Studio's `<folder>/<publisher>/<model>` store.
pub(super) fn lmstudio_gguf_models(folder: &Path) -> Vec<ModelEntry> {
    lmstudio_gguf_models_where(folder, &|_| true)
}
/// [`lmstudio_gguf_models`] limited to labels `keep` accepts, checked before the folder is scanned.
pub(super) fn lmstudio_gguf_models_where(
    folder: &Path,
    keep: &dyn Fn(&str) -> bool,
) -> Vec<ModelEntry> {
    let sorted = |dir: &Path| -> Vec<fs::DirEntry> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .map(|it| it.filter_map(Result::ok).take(4096).collect())
            .unwrap_or_default();
        entries.sort_by_key(|entry| entry.file_name());
        entries
    };
    let mut models = Vec::new();
    for publisher in sorted(folder) {
        for repo in sorted(&publisher.path()) {
            if models.len() >= MAX_MODELS {
                return models;
            }
            let label = format!(
                "{}/{}",
                publisher.file_name().to_string_lossy(),
                repo.file_name().to_string_lossy()
            );
            if !keep(&label) {
                continue;
            }
            let root = repo.path();
            let files = crate::llm::local_format::snapshot_ggufs(&root);
            let qualified = files.len() > 1 || (!files.is_empty() && snapshot(&root).is_some());
            for file in files.into_iter().take(MAX_MODELS - models.len()) {
                let relative = file.strip_prefix(&root).expect("inside model folder");
                models.push(gguf_entry(
                    "LM Studio download",
                    &label,
                    qualified.then_some(relative),
                    &file,
                ));
            }
        }
    }
    models
}
/// Downloaded local models (MLX checkpoints and GGUF files) from the Hugging Face cache and LM
/// Studio. Makes no network calls and loads no models; `discover` and `dial models recommend` share
/// this listing.
pub fn downloaded_local_models(user_dir: &Path) -> Vec<ModelEntry> {
    local_model_entries(mlx_architectures(user_dir).as_deref())
}
/// The listing behind [`downloaded_local_models`]; `trainable` is the mlx-lm model directory that
/// LoRA support is checked against.
pub(crate) fn local_model_entries(trainable: Option<&Path>) -> Vec<ModelEntry> {
    let mut models = Vec::new();
    if let Some(cache) = hf_cache() {
        models = local_models(&cache, trainable);
    }
    if let Some(folder) = lmstudio_folder() {
        let gguf = lmstudio_gguf_models(&folder);
        // The same repository downloaded by both keeps its Hugging Face entry.
        for entry in lmstudio_mlx_models(&folder, trainable)
            .into_iter()
            .chain(gguf)
        {
            if models.len() < MAX_MODELS && !models.iter().any(|known| known.label == entry.label) {
                models.push(entry);
            }
        }
    }
    // Native MLX-Serve uses the same publisher/repository store layout. Dial runs these image
    // models on MLX-Serve itself where it can (macOS on Apple Silicon); elsewhere they stay visible
    // but unavailable, since Dial's chat worker cannot serve a diffusion checkpoint.
    if let Some(home) = crate::config::paths::home_dir() {
        for mut entry in lmstudio_mlx_models(&home.join(".mlx-serve/models"), None)
            .into_iter()
            .filter(ModelEntry::supports_images)
        {
            entry.source = if crate::llm::server::image::mlx_serve_supported() {
                entry.available = true;
                format!("MLX-Serve download · {}", mlx_serve_runner())
            } else {
                entry.available = false;
                "MLX-Serve download · MLX-Serve runs only on macOS with Apple Silicon".into()
            };
            if models.len() < MAX_MODELS && !models.iter().any(|known| known.label == entry.label) {
                models.push(entry);
            }
        }
    }
    annotate_image_checkpoints(&mut models);
    models
}

/// How Dial runs an MLX image pack, for a listing's source line.
fn mlx_serve_runner() -> &'static str {
    if crate::llm::local_format::mlx_serve().is_some() {
        "MLX image pack, run by Dial on MLX-Serve"
    } else {
        "MLX image pack, run by Dial on MLX-Serve (installed on first use)"
    }
}

/// Downloaded diffusion checkpoints run on a Dial-launched stable-diffusion.cpp `sd-server` when
/// their files are a layout it loads. MLX-quantized packs run on a Dial-launched MLX-Serve on
/// macOS with Apple Silicon; elsewhere they stay listed but unavailable, with the reason.
pub(super) fn annotate_image_checkpoints(models: &mut [ModelEntry]) {
    for entry in models {
        if entry.metadata.kind != ModelKind::Image
            || entry.selection.base_url.is_some()
            || entry.source.starts_with("MLX-Serve download")
        {
            continue;
        }
        let model = Path::new(&entry.selection.model);
        if crate::llm::server::image::mlx_serve_supported()
            && crate::llm::server::image::is_mlx_pack(model)
        {
            entry.available = true;
            entry.source.push_str(" · ");
            entry.source.push_str(mlx_serve_runner());
            continue;
        }
        match crate::llm::server::image::components(model) {
            Ok(_) => {
                entry.available = true;
                entry.source.push_str(if crate::llm::local_format::sd_server().is_some() {
                    " · image checkpoint, run by Dial on stable-diffusion.cpp"
                } else {
                    " · image checkpoint, run by Dial on stable-diffusion.cpp (installs with /setup sd-cpp)"
                });
            }
            Err(error) => {
                entry.available = false;
                let reason = format!("{error:#}");
                let reason = if reason.contains("MLX-quantized") {
                    "MLX-quantized pack: it runs on MLX-Serve, which needs macOS with Apple Silicon; elsewhere download the original safetensors".to_owned()
                } else {
                    reason
                };
                entry
                    .source
                    .push_str(&format!(" · image checkpoint, not runnable: {reason}"));
            }
        }
    }
}
