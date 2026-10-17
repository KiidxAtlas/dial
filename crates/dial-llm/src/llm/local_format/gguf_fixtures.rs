//! GGUF header builders shared by the local-format tests of this crate and its dependents
//! (`test-support`).
/// A minimal GGUF v3 header with the given key/value section (no tensors).
pub fn header(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
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

pub fn string(text: &str) -> Vec<u8> {
    let mut bytes = 8u32.to_le_bytes().to_vec();
    bytes.extend((text.len() as u64).to_le_bytes());
    bytes.extend(text.as_bytes());
    bytes
}

pub fn uint32(value: u32) -> Vec<u8> {
    let mut bytes = 4u32.to_le_bytes().to_vec();
    bytes.extend(value.to_le_bytes());
    bytes
}
