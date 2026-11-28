//! On-disk formats of local models, detected from file layout alone (no weights are loaded).
//!
//! - **MLX**: a directory with `config.json` and complete safetensors; served by Dial's supervised
//!   MLX server, and trained directly on Apple Silicon (or PyTorch/PEFT on CUDA).
//! - **GGUF**: a `.gguf` file, or a directory holding one; served by a supervised `llama-server`
//!   (llama.cpp) for inference. Training prepares a safetensors base and exports GGUF LoRA or
//!   fused GGUF weights.
//! - **EXL3**: an ExLlamaV3 quantized checkpoint (`config.json` with `quant_method: "exl3"` plus
//!   safetensors); served by ExLlamaV3/TabbyAPI on NVIDIA CUDA hosts, with PEFT LoRA & EXL3 export.
//! - **Splash**: LM Studio's bundle (`manifest.json` plus `target/` and `tokenizer/`); only LM Studio
//!   can run it, so Dial recognises it to give an actionable error instead of a failed load.
use crate::platform::fs as platform_fs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalFormat {
    Mlx,
    Gguf,
    Exl3,
    Splash,
}

impl LocalFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mlx => "MLX",
            Self::Gguf => "GGUF",
            Self::Exl3 => "EXL3",
            Self::Splash => "Splash",
        }
    }

    /// The format and the runtime that serves it, as `/models` describes a local model.
    pub fn serving(self) -> &'static str {
        match self {
            Self::Mlx => "MLX, served by Dial's MLX server",
            Self::Gguf => "GGUF, served by llama.cpp",
            Self::Exl3 => "EXL3, served by ExLlamaV3 (CUDA)",
            Self::Splash => "Splash, LM Studio only",
        }
    }
}

/// Whether `path` is a regular file starting with the GGUF magic.
fn is_gguf_file(path: &Path) -> bool {
    if !fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    let mut magic = [0u8; 4];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && &magic == b"GGUF"
}

/// Standard llama.cpp split filename, shared by cache validation and download planning.
pub(crate) struct GgufShard {
    pub set: (String, u32),
    pub index: u32,
}

impl GgufShard {
    pub(crate) fn parse(path: &str) -> Option<Self> {
        let stem = path.strip_suffix(".gguf")?;
        let (head, total) = stem.rsplit_once("-of-")?;
        let (prefix, index) = head.rsplit_once('-')?;
        let digits = |text: &str| text.len() == 5 && text.bytes().all(|b| b.is_ascii_digit());
        if !digits(total) || !digits(index) {
            return None;
        }
        let (total, index): (u32, u32) = (total.parse().ok()?, index.parse().ok()?);
        (total > 0 && (1..=total).contains(&index)).then(|| Self {
            set: (prefix.to_owned(), total),
            index,
        })
    }

    fn filename(&self, index: u32) -> String {
        let (prefix, total) = &self.set;
        format!("{prefix}-{index:05}-of-{total:05}.gguf")
    }
}

fn complete_gguf_file(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if !name.ends_with(".gguf")
        || name.to_ascii_lowercase().contains("mmproj")
        || !is_gguf_file(path)
    {
        return None;
    }
    let Some(shard) = GgufShard::parse(name) else {
        return Some(path.to_path_buf());
    };
    let parent = path.parent()?;
    for index in 1..=shard.set.1 {
        if !is_gguf_file(&parent.join(shard.filename(index))) {
            return None;
        }
    }
    Some(parent.join(shard.filename(1)))
}

pub(crate) const INKLING_DRAFT: &str = "dspark-Inkling-2-Mini-Q8_0.gguf";

/// The qualified DSpark preset supports at most 4K. Select it *after* fitting the target's
/// window, never by shrinking that window or assuming an unknown server default fits.
pub fn inkling_dspark(target: &Path, serving_context: Option<u64>) -> Option<PathBuf> {
    if !matches!(serving_context, Some(1..=4096)) {
        return None;
    }
    if !matches!(
        target.file_name()?.to_str()?,
        "Inkling-2-Mini-AD-IQ2_M.gguf"
            | "Inkling-2-Mini-AD-IQ3_S.gguf"
            | "Inkling-2-Mini-AD-Q4_K.gguf"
    ) {
        return None;
    }
    let draft = target.with_file_name(INKLING_DRAFT);
    is_gguf_file(&draft).then_some(draft)
}

/// The GGUF file `llama-server` should load for `path`: the file itself, or the weights inside a
/// directory. Multimodal projectors (`mmproj*`) are never the model; for split models the first shard
/// is chosen only when every sibling shard is present. Ambiguous directories require an explicit file.
pub fn gguf_file(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return complete_gguf_file(path);
    }
    let mut models = fs::read_dir(path)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| gguf_target(&entry.path()));
    let first = models.next()?;
    models.next().is_none().then_some(first)
}

fn gguf_target(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if name == INKLING_DRAFT || GgufShard::parse(name).is_some_and(|s| s.index != 1) {
        return None;
    }
    complete_gguf_file(path)
}

/// Discover independently selectable targets in a snapshot. Follow file links to Hub blobs
/// for validation, never descend through directory links, and bound work per snapshot.
pub(super) fn snapshot_ggufs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_owned()];
    let mut models = Vec::new();
    let mut remaining = 8192;
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok).take(remaining) {
            remaining -= 1;
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(entry.path());
            } else if let Some(model) = gguf_target(&entry.path()) {
                models.push(model);
            }
        }
        if remaining == 0 {
            break;
        }
    }
    models.sort();
    models
}

/// The multimodal projector (`mmproj*.gguf`) next to the GGUF model at `path`, if any.
pub fn gguf_projector(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() { path } else { path.parent()? };
    let mut projectors: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|file| {
            file.extension().is_some_and(|ext| ext == "gguf")
                && file
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().to_lowercase().contains("mmproj"))
                && is_gguf_file(file)
        })
        .collect();
    projectors.sort();
    projectors.into_iter().next()
}

fn is_splash(path: &Path) -> bool {
    path.join("manifest.json").is_file()
        && path.join("target").is_dir()
        && path.join("tokenizer").is_dir()
}

/// Whether `path/config.json` declares an ExLlamaV3 (EXL3) quantized checkpoint.
fn is_exl3_config(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path.join("config.json")) else {
        return false;
    };
    let Some(bytes) = crate::llm::read_within(file, 1024 * 1024) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let quant = value.get("quantization_config");
    let method_is_exl3 = quant
        .and_then(|q| q.get("quant_method"))
        .and_then(|v| v.as_str())
        .is_some_and(|m| m.eq_ignore_ascii_case("exl3") || m.eq_ignore_ascii_case("exllamav3"));
    let has_exl3_section = value.get("exl3_config").is_some()
        || quant.and_then(|q| q.get("exl3_config")).is_some();
    method_is_exl3 || has_exl3_section
}

/// Detect the format of a local model path. EXL3 directories also contain `config.json` and
/// `.safetensors`, so EXL3 metadata is checked before claiming MLX.
pub fn detect(path: &Path) -> Option<LocalFormat> {
    if path.is_dir() {
        if is_splash(path) {
            return Some(LocalFormat::Splash);
        }
        let has_safetensors = path.join("config.json").is_file()
            && fs::read_dir(path)
                .ok()?
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == "safetensors")
                });
        if has_safetensors {
            if is_exl3_config(path) {
                return Some(LocalFormat::Exl3);
            }
            return Some(LocalFormat::Mlx);
        }
    }
    gguf_file(path).map(|_| LocalFormat::Gguf)
}

/// The `llama-server` executable: `DIAL_LLAMA_SERVER`, else Dial's own install (`/setup llama-cpp`,
/// at `tools/llama-cpp` as `setup::deps::install_dir` lays it out), else `PATH` or a common
/// package-manager location.
pub fn llama_server() -> Option<PathBuf> {
    find_server("DIAL_LLAMA_SERVER", "llama-cpp", "llama-server")
}

/// Never fall back to stock `llama-server` for an explicit Atomic selection.
pub fn atomic_server() -> Option<PathBuf> {
    find_server("DIAL_ATOMIC_SERVER", "atomic-engine", "atomic-llama-server")
}

/// stable-diffusion.cpp's `sd-server`, which serves downloaded image checkpoints:
/// `DIAL_SD_SERVER`, else Dial's own install (`/setup sd-cpp`, at `tools/sd-cpp`), else `PATH` or a
/// common package-manager location.
pub fn sd_server() -> Option<PathBuf> {
    find_server("DIAL_SD_SERVER", "sd-cpp", "sd-server")
}

/// MLX-Serve's `mlx-serve`, which serves downloaded MLX chat models and image packs on Apple
/// Silicon: `DIAL_MLX_SERVE`, else Dial's own install (`/setup mlx-serve`, at `tools/mlx-serve`),
/// else `PATH` or a common package-manager location.
pub fn mlx_serve() -> Option<PathBuf> {
    find_server("DIAL_MLX_SERVE", "mlx-serve", "mlx-serve")
}

/// vLLM-MLX (`vllm-mlx`), a continuous-batching OpenAI-compatible MLX server for Apple Silicon:
/// `DIAL_VLLM_MLX_SERVER`, else Dial's install under `tools/vllm-mlx`, else `PATH`.
pub fn vllm_mlx_server() -> Option<PathBuf> {
    find_server("DIAL_VLLM_MLX_SERVER", "vllm-mlx", "vllm-mlx")
}

/// ik_llama.cpp's `ik-llama-server` executable (IQK quantization & MoE GGUF fork):
/// `DIAL_IK_LLAMA_SERVER`, else Dial's install under `tools/ik-llama`, else `PATH`.
pub fn ik_llama_server() -> Option<PathBuf> {
    find_server("DIAL_IK_LLAMA_SERVER", "ik-llama", "ik-llama-server")
}

/// mistral.rs's `mistralrs-server` executable (Rust Metal/CUDA/CPU server for GGUF & Safetensors):
/// `DIAL_MISTRALRS_SERVER`, else Dial's install under `tools/mistral-rs`, else `PATH`.
pub fn mistralrs_server() -> Option<PathBuf> {
    find_server("DIAL_MISTRALRS_SERVER", "mistral-rs", "mistralrs-server")
}

/// vLLM's `vllm` CLI server (Linux CUDA/ROCm/CPU server for HuggingFace Safetensors + LoRA):
/// `DIAL_VLLM_SERVER`, else Dial's install under `tools/vllm`, else `PATH`.
pub fn vllm_server() -> Option<PathBuf> {
    find_server("DIAL_VLLM_SERVER", "vllm", "vllm")
}

/// SGLang's `sglang` server CLI (Linux CUDA RadixAttention server for Safetensors + LoRA):
/// `DIAL_SGLANG_SERVER`, else Dial's install under `tools/sglang`, else `PATH`.
pub fn sglang_server() -> Option<PathBuf> {
    find_server("DIAL_SGLANG_SERVER", "sglang", "sglang")
}

/// ExLlamaV3 / TabbyAPI local server executable (NVIDIA CUDA hosts):
/// `DIAL_EXL3_SERVER`, else Dial's install under `tools/exl3`, else `PATH`.
pub fn exl3_server() -> Option<PathBuf> {
    find_server("DIAL_EXL3_SERVER", "exl3", "exllamav3-server")
        .or_else(|| find_server("DIAL_EXL3_SERVER", "tabbyapi", "tabbyapi"))
}

/// Whether Dial's private Python MLX environment exists under `user_dir`.
pub fn mlx_python_ready(user_dir: &Path) -> bool {
    let venv = crate::config::paths::python_dir(user_dir).join(".venv");
    let bin = if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    };
    platform_fs::is_executable(&bin)
}
/// llama.cpp's `llama-quantize` executable for quantizing exported F16 GGUF checkpoints:
/// `DIAL_LLAMA_QUANTIZE`, else Dial's install under `tools/llama-cpp`, else `PATH`.
pub fn llama_quantize() -> Option<PathBuf> {
    find_server("DIAL_LLAMA_QUANTIZE", "llama-cpp", "llama-quantize")
}

/// llama.cpp's `convert_hf_to_gguf.py` script if installed alongside llama.cpp or via env.
pub fn convert_hf_to_gguf() -> Option<PathBuf> {
    find_helper_script("DIAL_CONVERT_HF_TO_GGUF", "convert_hf_to_gguf.py")
}

/// llama.cpp's `convert_lora_to_gguf.py` script if installed alongside llama.cpp or via env.
pub fn convert_lora_to_gguf() -> Option<PathBuf> {
    find_helper_script("DIAL_CONVERT_LORA_TO_GGUF", "convert_lora_to_gguf.py")
}

fn find_helper_script(variable: &str, name: &str) -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(variable).filter(|v| !v.is_empty()) {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let user_dir = crate::config::types::default_user_dir();
    if !user_dir.as_os_str().is_empty() {
        let managed = crate::config::paths::tools_dir(&user_dir)
            .join("llama-cpp")
            .join(name);
        if managed.is_file() {
            return Some(managed);
        }
    }
    if let Some(server) = llama_server()
        && let Some(dir) = server.parent()
    {
        for candidate in [dir.join(name), dir.join("../share/llama.cpp").join(name)] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    find_server_on_path(name)
}

/// A server program: the explicit `variable`, Dial's install under `tools/<install>` (or its
/// `bin`/`Scripts` venv directory), then `PATH`.
fn find_server(variable: &str, install: &str, program: &str) -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(variable).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let name = format!("{program}{}", std::env::consts::EXE_SUFFIX);
    let user_dir = crate::config::types::default_user_dir();
    if !user_dir.as_os_str().is_empty() {
        let root = crate::config::paths::tools_dir(&user_dir).join(install);
        for candidate in [
            root.join(&name),
            root.join("bin").join(&name),
            root.join("Scripts").join(&name),
        ] {
            if platform_fs::is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    find_server_on_path(&name)
}
/// Search PATH first; graphical launches may omit the package-manager directories.
pub(super) fn find_server_on_path(name: &str) -> Option<PathBuf> {
    platform_fs::find_program(Path::new(name), std::env::var_os("PATH").as_deref()).or_else(|| {
        package_bin_dirs(std::env::consts::OS)
            .iter()
            .find_map(|dir| platform_fs::executable_candidate(&Path::new(dir).join(name)))
    })
}

/// A slash-rooted Unix path is relative to the current drive on Windows.
fn package_bin_dirs(os: &str) -> &'static [&'static str] {
    match os {
        "macos" => &["/opt/homebrew/bin", "/usr/local/bin"],
        "linux" => &["/usr/local/bin"],
        _ => &[],
    }
}

/// Whether the local model `model` (a path) is served by a Dial-launched `llama-server`.
pub fn serves_with_llama_cpp(model: &str) -> bool {
    !model.is_empty() && detect(Path::new(model)) == Some(LocalFormat::Gguf)
}

/// Hyperparameters read from a GGUF header. Only metadata is parsed; tensor data is never read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GgufInfo {
    pub architecture: Option<String>,
    /// `tokenizer.chat_template`, the Jinja template `llama-server --jinja` renders.
    pub chat_template: Option<String>,
    /// Native training context (`<arch>.context_length`).
    pub context_length: Option<u64>,
    pub block_count: Option<u64>,
    pub head_count: Option<u64>,
    /// KV heads; per-layer arrays report their maximum.
    pub head_count_kv: Option<u64>,
    pub embedding_length: Option<u64>,
    pub key_length: Option<u64>,
    pub value_length: Option<u64>,
    /// MLA latent rank (DeepSeek-style attention), which replaces per-head K/V in the cache.
    pub kv_lora_rank: Option<u64>,
    pub rope_dimension_count: Option<u64>,
    /// Hybrid recurrent models: every Nth layer is full attention and the rest keep no KV cache.
    pub full_attention_interval: Option<u64>,
    /// `general.sampling.temp`: the sampler temperature the model was published with.
    pub sampling_temp: Option<f64>,
    /// `general.sampling.top_p`, `general.sampling.top_k` and `general.sampling.min_p`: the other
    /// sampler defaults llama.cpp converters write into the header.
    pub sampling_top_p: Option<f64>,
    pub sampling_top_k: Option<u64>,
    pub sampling_min_p: Option<f64>,
}

impl GgufInfo {
    pub fn engines(&self) -> super::local_runtime::GgufEngines {
        self.architecture
            .as_deref()
            .map(super::local_runtime::GgufEngines::for_architecture)
            .unwrap_or_default()
    }
}

/// Header metadata of the GGUF model at `path` (a file or a directory holding one).
pub fn gguf_info(path: &Path) -> Option<GgufInfo> {
    let file = gguf_file(path)?;
    let mut reader = BufReader::new(fs::File::open(file).ok()?);
    let (mut strings, numbers, floats) = read_gguf_metadata(&mut reader).ok()?;
    let architecture = strings.remove("general.architecture");
    let arch = architecture.clone()?;
    let number = |key: &str| numbers.get(&format!("{arch}.{key}")).copied();
    Some(GgufInfo {
        architecture,
        chat_template: strings.remove("tokenizer.chat_template"),
        context_length: number("context_length"),
        block_count: number("block_count"),
        head_count: number("attention.head_count"),
        head_count_kv: number("attention.head_count_kv"),
        embedding_length: number("embedding_length"),
        key_length: number("attention.key_length"),
        value_length: number("attention.value_length"),
        kv_lora_rank: number("attention.kv_lora_rank"),
        rope_dimension_count: number("rope.dimension_count"),
        full_attention_interval: number("full_attention_interval"),
        sampling_temp: floats.get("general.sampling.temp").copied(),
        sampling_top_p: floats.get("general.sampling.top_p").copied(),
        sampling_top_k: numbers.get("general.sampling.top_k").copied(),
        sampling_min_p: floats.get("general.sampling.min_p").copied(),
    })
}

/// Bytes of GGUF weights `llama-server` loads for `path`: the selected file plus its sibling shards.
pub fn gguf_weight_bytes(path: &Path) -> Option<u64> {
    let file = gguf_file(path)?;
    let Some(shard) = GgufShard::parse(file.file_name()?.to_str()?) else {
        return Some(fs::metadata(&file).ok()?.len());
    };
    let parent = file.parent()?;
    (1..=shard.set.1).try_fold(0u64, |total, index| {
        total.checked_add(fs::metadata(parent.join(shard.filename(index))).ok()?.len())
    })
}

const GGUF_MAX_KEYS: u64 = 1 << 16;
const GGUF_MAX_KEY_BYTES: u64 = 1 << 16;
/// Chat templates beyond this are skipped rather than buffered.
const GGUF_MAX_TEMPLATE_BYTES: u64 = 1 << 20;
/// String values kept from the header; every other string is skipped without buffering.
const GGUF_STRINGS: [&str; 2] = ["general.architecture", "tokenizer.chat_template"];
/// Float keys kept from the header (llama.cpp sampler defaults); other floats are skipped.
const GGUF_SAMPLING_FLOATS: [&str; 3] = [
    "general.sampling.temp",
    "general.sampling.top_p",
    "general.sampling.min_p",
];

/// Strings, integers and the `GGUF_SAMPLING_FLOATS` floats of a header's key/value section.
type GgufMetadata = (
    HashMap<String, String>,
    HashMap<String, u64>,
    HashMap<String, f64>,
);

/// Reads the key/value section: the strings in `GGUF_STRINGS` and every integer (or the maximum of
/// every integer array) outside the tokenizer, skipping everything else without buffering it.
fn read_gguf_metadata<R: Read + Seek>(reader: &mut R) -> std::io::Result<GgufMetadata> {
    let invalid =
        |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        return Err(invalid("not a GGUF file"));
    }
    let version = read_u32(reader)?;
    if !(2..=3).contains(&version) {
        return Err(invalid("unsupported GGUF version"));
    }
    let _tensors = read_u64(reader)?;
    let keys = read_u64(reader)?;
    if keys > GGUF_MAX_KEYS {
        return Err(invalid("too many GGUF metadata keys"));
    }
    let (mut strings, mut numbers, mut floats) = (HashMap::new(), HashMap::new(), HashMap::new());
    for _ in 0..keys {
        let length = read_u64(reader)?;
        if length > GGUF_MAX_KEY_BYTES {
            return Err(invalid("GGUF key too long"));
        }
        let mut key = vec![0u8; length as usize];
        reader.read_exact(&mut key)?;
        let key = String::from_utf8_lossy(&key).into_owned();
        let kind = read_u32(reader)?;
        let wanted = !key.starts_with("tokenizer.");
        match kind {
            8 if GGUF_STRINGS.contains(&key.as_str()) => {
                let length = read_u64(reader)?;
                if length > GGUF_MAX_TEMPLATE_BYTES {
                    seek_forward(reader, length)?;
                    continue;
                }
                let mut text = vec![0u8; length as usize];
                reader.read_exact(&mut text)?;
                strings.insert(key, String::from_utf8_lossy(&text).into_owned());
            }
            9 => {
                let item = read_u32(reader)?;
                let count = read_u64(reader)?;
                match integer_width(item) {
                    // Unwanted or non-integer fixed-width arrays are skipped in one seek.
                    Some(width) if !wanted || matches!(item, 6 | 7 | 12) => {
                        seek_forward(reader, count.saturating_mul(width))?;
                    }
                    Some(_) => {
                        let mut largest = None;
                        for _ in 0..count {
                            largest = largest.max(read_value(reader, item, true)?);
                        }
                        if let Some(value) = largest {
                            numbers.insert(key, value);
                        }
                    }
                    // Strings vary in size and are skipped one by one; nested arrays are rejected.
                    None => {
                        for _ in 0..count {
                            read_value(reader, item, false)?;
                        }
                    }
                }
            }
            6 | 12 if GGUF_SAMPLING_FLOATS.contains(&key.as_str()) => {
                let value = if kind == 6 {
                    f64::from(f32::from_bits(read_u32(reader)?))
                } else {
                    f64::from_bits(read_u64(reader)?)
                };
                floats.insert(key, value);
            }
            _ => {
                if let Some(value) = read_value(reader, kind, wanted)? {
                    numbers.insert(key, value);
                }
            }
        }
    }
    Ok((strings, numbers, floats))
}

/// SHA-256 (lowercase hex) of the `tokenizer.ggml.tokens` array in a GGUF header: the vocabulary a
/// draft model must share with its target for speculative decoding. Only the header is read.
pub(crate) fn gguf_tokenizer_digest(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    // A single token longer than this is not a real vocabulary entry; refuse the file.
    const MAX_TOKEN_BYTES: u64 = 1 << 16;
    let mut reader = BufReader::new(fs::File::open(gguf_file(path)?).ok()?);
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).ok()?;
    let version = read_u32(&mut reader).ok()?;
    if &magic != b"GGUF" || !(2..=3).contains(&version) {
        return None;
    }
    read_u64(&mut reader).ok()?;
    let keys = read_u64(&mut reader).ok()?;
    if keys > GGUF_MAX_KEYS {
        return None;
    }
    for _ in 0..keys {
        let length = read_u64(&mut reader).ok()?;
        if length > GGUF_MAX_KEY_BYTES {
            return None;
        }
        let mut key = vec![0u8; length as usize];
        reader.read_exact(&mut key).ok()?;
        let kind = read_u32(&mut reader).ok()?;
        if key == b"tokenizer.ggml.tokens" && kind == 9 {
            if read_u32(&mut reader).ok()? != 8 {
                return None;
            }
            let count = read_u64(&mut reader).ok()?;
            let mut hasher = Sha256::new();
            hasher.update(count.to_le_bytes());
            for _ in 0..count {
                let length = read_u64(&mut reader).ok()?;
                if length > MAX_TOKEN_BYTES {
                    return None;
                }
                let mut token = vec![0u8; length as usize];
                reader.read_exact(&mut token).ok()?;
                hasher.update(length.to_le_bytes());
                hasher.update(&token);
            }
            return Some(crate::hex(&hasher.finalize()));
        }
        skip_gguf_value(&mut reader, kind).ok()?;
    }
    None
}

/// Skips one header value of `kind` without buffering it; arrays of any element type are skipped.
fn skip_gguf_value<R: Read + Seek>(reader: &mut R, kind: u32) -> std::io::Result<()> {
    if kind != 9 {
        return read_value(reader, kind, false).map(|_| ());
    }
    let item = read_u32(reader)?;
    let count = read_u64(reader)?;
    match integer_width(item) {
        Some(width) => seek_forward(reader, count.saturating_mul(width)),
        None => {
            for _ in 0..count {
                skip_gguf_value(reader, item)?;
            }
            Ok(())
        }
    }
}

/// Byte width of a fixed-size GGUF scalar type (integers, floats and bool).
fn integer_width(kind: u32) -> Option<u64> {
    match kind {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

/// Reads one scalar or string value; returns non-negative integers when `keep` is set.
fn read_value<R: Read + Seek>(
    reader: &mut R,
    kind: u32,
    keep: bool,
) -> std::io::Result<Option<u64>> {
    let mut buffer = [0u8; 8];
    let value = match kind {
        0 => {
            reader.read_exact(&mut buffer[..1])?;
            Some(u64::from(buffer[0]))
        }
        1 => {
            reader.read_exact(&mut buffer[..1])?;
            u64::try_from(buffer[0] as i8).ok()
        }
        2 => {
            reader.read_exact(&mut buffer[..2])?;
            Some(u64::from(u16::from_le_bytes([buffer[0], buffer[1]])))
        }
        3 => {
            reader.read_exact(&mut buffer[..2])?;
            u64::try_from(i16::from_le_bytes([buffer[0], buffer[1]])).ok()
        }
        4 => Some(u64::from(read_u32(reader)?)),
        5 => u64::try_from(read_u32(reader)? as i32).ok(),
        10 => Some(read_u64(reader)?),
        11 => u64::try_from(read_u64(reader)? as i64).ok(),
        6 | 7 | 12 => {
            seek_forward(reader, integer_width(kind).unwrap_or(0))?;
            None
        }
        8 => {
            let length = read_u64(reader)?;
            seek_forward(reader, length)?;
            None
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unsupported GGUF value type",
            ));
        }
    };
    Ok(value.filter(|_| keep))
}

fn seek_forward<R: Seek>(reader: &mut R, bytes: u64) -> std::io::Result<()> {
    let bytes = i64::try_from(bytes).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "GGUF length out of range")
    })?;
    reader.seek(std::io::SeekFrom::Current(bytes)).map(|_| ())
}

fn read_u32<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "test-support"))]
pub mod gguf_fixtures;

#[cfg(test)]
pub(crate) mod gguf_metadata_tests;
