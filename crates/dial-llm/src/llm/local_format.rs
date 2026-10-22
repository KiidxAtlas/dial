//! On-disk formats of local models, detected from file layout alone (no weights are loaded).
//!
//! - **MLX**: a directory with `config.json` and complete safetensors; served by Dial's supervised
//!   MLX server, and the only format LoRA training supports.
//! - **GGUF**: a `.gguf` file, or a directory holding one; served by a supervised `llama-server`
//!   (llama.cpp) for inference.
//! - **Splash**: LM Studio's bundle (`manifest.json` plus `target/` and `tokenizer/`); only LM Studio
//!   can run it, so Dial recognises it to give an actionable error instead of a failed load.

use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalFormat {
    Mlx,
    Gguf,
    Splash,
}

impl LocalFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mlx => "MLX",
            Self::Gguf => "GGUF",
            Self::Splash => "Splash",
        }
    }
}

/// Whether `path` is a regular file starting with the GGUF magic.
fn is_gguf_file(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && &magic == b"GGUF"
        && fs::metadata(path).is_ok_and(|meta| meta.is_file())
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
    let (prefix, total) = shard.set;
    let parent = path.parent()?;
    for index in 1..=total {
        if !is_gguf_file(&parent.join(format!("{prefix}-{index:05}-of-{total:05}.gguf"))) {
            return None;
        }
    }
    Some(parent.join(format!("{prefix}-00001-of-{total:05}.gguf")))
}

/// The GGUF file `llama-server` should load for `path`: the file itself, or the weights inside a
/// directory. Multimodal projectors (`mmproj*`) are never the model; for split models the first shard
/// is chosen only when every sibling shard is present. Ambiguous directories require an explicit file.
pub fn gguf_file(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return complete_gguf_file(path);
    }
    let mut models = std::collections::BTreeSet::new();
    for entry in fs::read_dir(path).ok()?.filter_map(Result::ok) {
        let file = entry.path();
        if file
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(GgufShard::parse)
            .is_some_and(|shard| shard.index != 1)
        {
            continue;
        }
        if let Some(model) = complete_gguf_file(&file) {
            models.insert(model);
            if models.len() > 1 {
                return None;
            }
        }
    }
    models.into_iter().next()
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

/// Detect the format of a local model path. MLX is checked by the caller's snapshot rules, so this
/// only claims MLX for a directory that has `config.json` next to a safetensors file.
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
            return Some(LocalFormat::Mlx);
        }
    }
    gguf_file(path).map(|_| LocalFormat::Gguf)
}

/// The `llama-server` executable: `DIAL_LLAMA_SERVER`, else Dial's own install (`/setup llama-cpp`,
/// at `tools/llama-cpp` as `setup::deps::install_dir` lays it out), else `PATH` or a common
/// package-manager location.
pub fn llama_server() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("DIAL_LLAMA_SERVER").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let name = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    let user_dir = crate::config::types::default_user_dir();
    if !user_dir.as_os_str().is_empty() {
        let managed = user_dir.join("tools").join("llama-cpp").join(name);
        if managed.is_file() {
            return Some(managed);
        }
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
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
    let name = file.file_name()?.to_string_lossy().into_owned();
    let Some(prefix) = name.find("-00001-of-").map(|at| name[..at].to_owned()) else {
        return Some(fs::metadata(&file).ok()?.len());
    };
    let mut total = 0;
    for entry in fs::read_dir(file.parent()?).ok()?.filter_map(Result::ok) {
        let shard = entry.file_name().to_string_lossy().into_owned();
        if shard.starts_with(&format!("{prefix}-"))
            && shard.contains("-of-")
            && shard.ends_with(".gguf")
        {
            total += entry.metadata().ok()?.len();
        }
    }
    Some(total)
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
            return Some(
                hasher
                    .finalize()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            );
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
