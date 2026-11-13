//! Local model discovery on disk: Hugging Face cache, LM Studio folders and checkpoint metadata.

use super::local_servers::kind_from_declared;
use super::metadata::{ModelEntry, ModelKind, ModelMetadata, metadata};
use super::probe::context;
use super::{MAX_MODELS, MAX_RESPONSE};
use crate::llm::local_format::LocalFormat;
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
    if !model.join("config.json").is_file()
        && let Some(info) = crate::llm::local_format::gguf_info(model)
    {
        let mut metadata = ModelMetadata {
            vision: Some(crate::llm::local_format::gguf_projector(model).is_some()),
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
            let mut template = String::new();
            file.take(MAX_RESPONSE + 1)
                .read_to_string(&mut template)
                .ok()?;
            (template.len() as u64 <= MAX_RESPONSE).then_some(template)
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
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_RESPONSE {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_RESPONSE + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_RESPONSE {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
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
        let transformer = if path.join("transformer").is_dir() {
            path.join("transformer")
        } else if path.join("unet").is_dir() {
            path.join("unet")
        } else {
            path.to_owned()
        };
        for component in [transformer, path.join("vae"), path.join("text_encoder")] {
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
        for path in candidates {
            let (window, gguf) = match snapshot(&path) {
                Some(window) => (window, false),
                None if crate::llm::local_format::gguf_file(&path).is_some() => (None, true),
                None => continue,
            };
            models.push(ModelEntry {
                selection: ModelSelection {
                    model: path.to_string_lossy().into_owned(),
                    context_window: window,
                    ..ModelSelection::default()
                },
                label: label.clone(),
                source: if gguf {
                    gguf_source("Hugging Face cache")
                } else {
                    "Hugging Face cache (MLX)".into()
                },
                available: true,
                metadata: ModelMetadata {
                    lora: lora_support(trainable, &path),
                    format: Some(if gguf {
                        LocalFormat::Gguf
                    } else {
                        LocalFormat::Mlx
                    }),
                    ..local_model_metadata(&path)
                },
            });
            break;
        }
    }
    models
}
/// Source text for a GGUF listing. A missing `llama-server` is named here; Dial offers to install it
/// before the first turn on the model.
fn gguf_source(origin: &str) -> String {
    if crate::llm::local_format::llama_server().is_some() {
        format!("{origin} (GGUF)")
    } else {
        format!("{origin} (GGUF) · llama.cpp not installed; Dial offers to install it on first use")
    }
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
    if snapshot(path).is_some() || crate::llm::local_format::gguf_file(path).is_some() {
        return Ok(fs::canonicalize(path)?.to_string_lossy().into_owned());
    }
    if crate::llm::local_format::detect(path) == Some(crate::llm::local_format::LocalFormat::Splash)
    {
        anyhow::bail!(
            "{model:?} is a Splash bundle, which only LM Studio can run: load it in LM Studio and select it under the LM Studio provider"
        );
    }
    // Only the requested label is inspected: listing every download stats each shard of every model.
    let wanted = |label: &str| label == model;
    let mut entries = cache
        .map(|cache| local_models_where(cache, None, &wanted))
        .unwrap_or_default();
    if let Some(studio) = studio {
        entries.extend(lmstudio_mlx_models_where(studio, None, &wanted));
        entries.extend(lmstudio_gguf_models_where(studio, &wanted));
    }
    if let Some(entry) = entries.into_iter().find(|entry| entry.label == model) {
        return Ok(fs::canonicalize(entry.selection.model)?
            .to_string_lossy()
            .into_owned());
    }
    anyhow::bail!(
        "local model {model:?} has no complete downloaded weights (MLX safetensors or a GGUF file); inference is offline. Download this model first or select a downloaded local model in /models (for architecture maintenance, change the Architecture model in Settings → Model → Project understanding)"
    )
}
pub(crate) fn hf_cache() -> Option<PathBuf> {
    std::env::var_os("HF_HUB_CACHE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HF_HOME")
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(s).join("hub"))
        })
        .or_else(|| {
            crate::config::paths::home_dir().map(|home| home.join(".cache/huggingface/hub"))
        })
}
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
fn lmstudio_folder() -> Option<PathBuf> {
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
            models.push(ModelEntry {
                selection: ModelSelection {
                    model: path.to_string_lossy().into_owned(),
                    context_window: window,
                    ..ModelSelection::default()
                },
                label,
                source: "LM Studio download (MLX)".into(),
                available: true,
                metadata: ModelMetadata {
                    lora: lora_support(trainable, &path),
                    format: Some(LocalFormat::Mlx),
                    ..local_model_metadata(&path)
                },
            });
        }
    }
    models
}
/// GGUF models LM Studio downloaded (`<folder>/<publisher>/<model>/*.gguf`), listed by the file
/// `llama-server` would load. Labelled `publisher/model`.
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
            let Some(file) = crate::llm::local_format::gguf_file(&repo.path()) else {
                continue;
            };
            models.push(ModelEntry {
                selection: ModelSelection {
                    model: file.to_string_lossy().into_owned(),
                    ..ModelSelection::default()
                },
                label,
                source: gguf_source("LM Studio download"),
                available: true,
                metadata: ModelMetadata {
                    vision: Some(false),
                    format: Some(LocalFormat::Gguf),
                    ..ModelMetadata::default()
                },
            });
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
    // Native MLX-Serve uses the same publisher/repository store layout. Keep offline weights
    // visible, but never claim that Dial's chat worker can serve a diffusion checkpoint.
    if let Some(home) = crate::config::paths::home_dir() {
        for mut entry in lmstudio_mlx_models(&home.join(".mlx-serve/models"), None)
            .into_iter()
            .filter(ModelEntry::supports_images)
        {
            entry.source =
                "MLX-Serve download · start MLX-Serve and F5 rescan to select its endpoint".into();
            entry.available = false;
            if models.len() < MAX_MODELS && !models.iter().any(|known| known.label == entry.label) {
                models.push(entry);
            }
        }
    }
    for entry in &mut models {
        if entry.metadata.kind == ModelKind::Image && entry.selection.base_url.is_none() {
            entry.available = false;
            if !entry.source.starts_with("MLX-Serve download") {
                entry.source.push_str(
                    " · image checkpoint, not served: import/serve in MLX-Serve, then F5 rescan",
                );
            }
        }
    }
    models
}
