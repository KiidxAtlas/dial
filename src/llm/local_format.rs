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

/// The `llama-server` executable: `DIAL_LLAMA_SERVER`, else the first one on `PATH` or in a common
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
}

/// Header metadata of the GGUF model at `path` (a file or a directory holding one).
pub fn gguf_info(path: &Path) -> Option<GgufInfo> {
    let file = gguf_file(path)?;
    let mut reader = BufReader::new(fs::File::open(file).ok()?);
    let (mut strings, numbers) = read_gguf_metadata(&mut reader).ok()?;
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

/// Reads the key/value section: the strings in `GGUF_STRINGS` and every integer (or the maximum of
/// every integer array) outside the tokenizer, skipping everything else without buffering it.
fn read_gguf_metadata<R: Read + Seek>(
    reader: &mut R,
) -> std::io::Result<(HashMap<String, String>, HashMap<String, u64>)> {
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
    let (mut strings, mut numbers) = (HashMap::new(), HashMap::new());
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
            _ => {
                if let Some(value) = read_value(reader, kind, wanted)? {
                    numbers.insert(key, value);
                }
            }
        }
    }
    Ok((strings, numbers))
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
mod tests {
    use super::*;

    fn gguf(path: &Path) {
        fs::write(path, b"GGUF\x03\0\0\0rest").unwrap();
    }

    #[test]
    fn detects_each_format_from_layout() {
        let root = tempfile::tempdir().unwrap();
        let mlx = root.path().join("mlx");
        fs::create_dir(&mlx).unwrap();
        fs::write(mlx.join("config.json"), "{}").unwrap();
        fs::write(mlx.join("model.safetensors"), b"x").unwrap();
        assert_eq!(detect(&mlx), Some(LocalFormat::Mlx));

        let splash = root.path().join("splash");
        for dir in ["target", "tokenizer"] {
            fs::create_dir_all(splash.join(dir)).unwrap();
        }
        fs::write(splash.join("manifest.json"), "{}").unwrap();
        assert_eq!(detect(&splash), Some(LocalFormat::Splash));

        let folder = root.path().join("gguf");
        fs::create_dir(&folder).unwrap();
        gguf(&folder.join("model-Q4_K_M.gguf"));
        assert_eq!(detect(&folder), Some(LocalFormat::Gguf));
        assert_eq!(
            detect(&folder.join("model-Q4_K_M.gguf")),
            Some(LocalFormat::Gguf)
        );
        assert_eq!(detect(&root.path().join("missing")), None);
    }

    #[test]
    fn gguf_selection_skips_projectors_and_starts_at_the_first_shard() {
        let root = tempfile::tempdir().unwrap();
        gguf(&root.path().join("mmproj-model-f16.gguf"));
        gguf(&root.path().join("m-00002-of-00002.gguf"));
        gguf(&root.path().join("m-00001-of-00002.gguf"));
        assert_eq!(
            gguf_file(root.path()).unwrap().file_name().unwrap(),
            "m-00001-of-00002.gguf"
        );
    }

    #[test]
    fn incomplete_split_models_are_not_usable_cached_weights() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("m-00001-of-00002.gguf");
        let second = root.path().join("m-00002-of-00002.gguf");
        gguf(&first);
        assert_eq!(gguf_file(&first), None);
        assert_eq!(gguf_file(root.path()), None);
        fs::write(&second, b"partial").unwrap();
        assert_eq!(gguf_file(root.path()), None);
        gguf(&second);
        assert_eq!(gguf_file(&second), Some(first.clone()));
        assert_eq!(gguf_file(root.path()), Some(first));
    }

    #[test]
    fn multiple_cached_quantizations_require_an_explicit_file() {
        let root = tempfile::tempdir().unwrap();
        let selected = root.path().join("m-Q4.gguf");
        gguf(&selected);
        gguf(&root.path().join("m-Q8.gguf"));
        assert_eq!(gguf_file(root.path()), None);
        assert_eq!(gguf_file(&selected), Some(selected));
        gguf(&root.path().join("mmproj-f16.gguf"));
        assert_eq!(gguf_file(&root.path().join("mmproj-f16.gguf")), None);
    }

    #[test]
    fn a_gguf_extension_without_the_magic_is_not_a_model() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("notes.gguf"), b"not a model").unwrap();
        assert_eq!(detect(root.path()), None);
    }
}

#[cfg(test)]
pub(crate) mod gguf_metadata_tests {
    use super::*;

    /// A minimal GGUF v3 header with the given key/value section (no tensors).
    pub(crate) fn header(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = b"GGUF".to_vec();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.extend((entries.len() as u64).to_le_bytes());
        for (key, value) in entries {
            bytes.extend((key.len() as u64).to_le_bytes());
            bytes.extend(key.as_bytes());
            bytes.extend(value);
        }
        bytes
    }

    pub(crate) fn string(text: &str) -> Vec<u8> {
        let mut bytes = 8u32.to_le_bytes().to_vec();
        bytes.extend((text.len() as u64).to_le_bytes());
        bytes.extend(text.as_bytes());
        bytes
    }

    pub(crate) fn uint32(value: u32) -> Vec<u8> {
        let mut bytes = 4u32.to_le_bytes().to_vec();
        bytes.extend(value.to_le_bytes());
        bytes
    }

    fn string_array(items: &[&str]) -> Vec<u8> {
        let mut bytes = 9u32.to_le_bytes().to_vec();
        bytes.extend(8u32.to_le_bytes());
        bytes.extend((items.len() as u64).to_le_bytes());
        for item in items {
            bytes.extend((item.len() as u64).to_le_bytes());
            bytes.extend(item.as_bytes());
        }
        bytes
    }

    fn int32_array(items: &[i32]) -> Vec<u8> {
        let mut bytes = 9u32.to_le_bytes().to_vec();
        bytes.extend(5u32.to_le_bytes());
        bytes.extend((items.len() as u64).to_le_bytes());
        for item in items {
            bytes.extend(item.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn header_metadata_is_read_past_tokenizer_arrays_and_mixed_value_types() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("m.gguf");
        let mut float = 6u32.to_le_bytes().to_vec();
        float.extend(1.5f32.to_le_bytes());
        fs::write(
            &file,
            header(&[
                ("general.architecture", string("qwen3")),
                ("general.name", string("Qwen3 test")),
                ("tokenizer.ggml.tokens", string_array(&["a", "bc", "def"])),
                ("tokenizer.ggml.token_type", int32_array(&[1, 1, 3])),
                ("qwen3.rope.freq_base", float),
                ("qwen3.context_length", uint32(40960)),
                ("qwen3.block_count", uint32(36)),
                ("qwen3.attention.head_count", uint32(32)),
                ("qwen3.attention.head_count_kv", int32_array(&[8, 0, 8])),
                ("qwen3.attention.key_length", uint32(128)),
                ("qwen3.attention.value_length", uint32(128)),
            ]),
        )
        .unwrap();
        let info = gguf_info(&file).unwrap();
        assert_eq!(info.architecture.as_deref(), Some("qwen3"));
        assert_eq!(info.context_length, Some(40960));
        assert_eq!(info.block_count, Some(36));
        assert_eq!(info.head_count, Some(32));
        assert_eq!(info.head_count_kv, Some(8));
        assert_eq!((info.key_length, info.value_length), (Some(128), Some(128)));
        assert_eq!(info.kv_lora_rank, None);
    }

    #[test]
    fn truncated_or_oversized_headers_are_rejected_without_panicking() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("m.gguf");
        let full = header(&[("general.architecture", string("llama"))]);
        fs::write(&file, &full[..full.len() - 2]).unwrap();
        assert_eq!(gguf_info(&file), None);

        let mut huge = b"GGUF".to_vec();
        huge.extend(3u32.to_le_bytes());
        huge.extend(0u64.to_le_bytes());
        huge.extend(u64::MAX.to_le_bytes());
        fs::write(&file, huge).unwrap();
        assert_eq!(gguf_info(&file), None);
    }

    #[test]
    fn split_models_count_every_shard() {
        let root = tempfile::tempdir().unwrap();
        let first = header(&[("general.architecture", string("llama"))]);
        fs::write(root.path().join("m-00001-of-00002.gguf"), &first).unwrap();
        let mut second = vec![0u8; 100];
        second[..4].copy_from_slice(b"GGUF");
        fs::write(root.path().join("m-00002-of-00002.gguf"), second).unwrap();
        fs::write(root.path().join("mmproj-f16.gguf"), vec![0u8; 1000]).unwrap();
        assert_eq!(
            gguf_weight_bytes(root.path()),
            Some(first.len() as u64 + 100)
        );
    }
}
