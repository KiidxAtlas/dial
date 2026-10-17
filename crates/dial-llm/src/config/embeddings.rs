//! Local embedding model choices offered by the settings panel.

/// Picker suggestions for `memory.semantic.embedding_model`, `(name, label)` in catalog order.
/// A unit test keeps this list equal to [`CATALOG`].
pub const CHOICES: &[(&str, &str)] = &[
    ("nomic-embed-text", "nomic-embed-text · 768 dims · 274 MB"),
    ("all-minilm", "all-minilm · 384 dims · 46 MB"),
    (
        "mxbai-embed-large",
        "mxbai-embed-large · 1024 dims · 670 MB",
    ),
    ("embeddinggemma", "embeddinggemma · 768 dims · 622 MB"),
    ("bge-m3", "bge-m3 · 1024 dims · 1200 MB"),
    (
        "snowflake-arctic-embed2",
        "snowflake-arctic-embed2 · 1024 dims · 1200 MB",
    ),
];
