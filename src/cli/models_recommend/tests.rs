use super::*;
use crate::llm::local_format::gguf_metadata_tests::{header, string, uint32};

#[test]
fn gguf_models_are_sized_from_their_header_and_file() {
    let root = tempfile::tempdir().unwrap();
    let mut bytes = header(&[
        ("general.architecture", string("llama")),
        ("llama.block_count", uint32(32)),
        ("llama.attention.head_count", uint32(32)),
        ("llama.attention.head_count_kv", uint32(8)),
        ("llama.embedding_length", uint32(4096)),
        ("llama.context_length", uint32(131_072)),
    ]);
    bytes.resize(bytes.len() + 4096, 0);
    let weights = bytes.len() as u64;
    fs::write(root.path().join("model-Q4_K_M.gguf"), bytes).unwrap();
    let row = row(
        Candidate {
            label: "llama-gguf".into(),
            source: "test".into(),
            path: root.path().to_path_buf(),
            favorite: false,
        },
        8192,
        None,
        Some(64 * GIB),
        &[],
    );
    assert_eq!(row.note, None);
    assert_eq!(row.weights_bytes, Some(weights));
    let estimate = row.estimate.unwrap();
    // 32 layers x 8 KV heads x (128 + 128) values x 2 bytes per token.
    assert_eq!(estimate.kv_bytes, 8192 * 32 * 8 * 256 * 2);
    assert_eq!(row.fit, Fit::Fits);
    assert!(row.max_context_tokens > 8192);
}
