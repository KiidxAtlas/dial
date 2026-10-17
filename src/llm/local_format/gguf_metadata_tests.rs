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

#[test]
fn sampling_defaults_are_read_from_float_and_integer_keys() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("m.gguf");
    let mut temp = 6u32.to_le_bytes().to_vec();
    temp.extend(0.7f32.to_le_bytes());
    let mut top_p = 12u32.to_le_bytes().to_vec();
    top_p.extend(0.95f64.to_le_bytes());
    let mut min_p = 6u32.to_le_bytes().to_vec();
    min_p.extend(0.05f32.to_le_bytes());
    let mut other = 6u32.to_le_bytes().to_vec();
    other.extend(0.5f32.to_le_bytes());
    fs::write(
        &file,
        header(&[
            ("general.architecture", string("llama")),
            ("general.sampling.temp", temp),
            ("general.sampling.top_p", top_p),
            ("general.sampling.top_k", uint32(20)),
            ("general.sampling.min_p", min_p),
            ("general.sampling.xtc_probability", other),
        ]),
    )
    .unwrap();
    let info = gguf_info(&file).unwrap();
    assert_eq!(info.sampling_temp, Some(f64::from(0.7f32)));
    assert_eq!(info.sampling_top_p, Some(0.95));
    assert_eq!(info.sampling_top_k, Some(20));
    assert_eq!(info.sampling_min_p, Some(f64::from(0.05f32)));
}
