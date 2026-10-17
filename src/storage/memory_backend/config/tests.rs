use super::*;

#[test]
fn memory_model_roles_default_to_memory_decision_and_accept_only_role_names() {
    assert_eq!(LocalConfig::default().extraction_role, "memory");
    assert_eq!(LocalConfig::default().consolidation_role, "memory");
    assert_eq!(SharpshooterConfig::default().model_role, "memory");
    for name in [
        "main",
        "small",
        "coach",
        "tester",
        "planner",
        "memory",
        "decision",
        "embedding",
    ] {
        assert!(role(name).is_ok(), "{name} should be accepted");
    }
    for name in ["", "smol", "slow", "default", "bad role", "reranker"] {
        assert!(role(name).is_err(), "{name:?} should be rejected");
    }
}

#[test]
fn off_and_non_semantic_backends_do_not_validate_embedding_overrides() {
    let mut memory = crate::config::types::MemoryConfig {
        backend: MemoryBackendId::Semantic,
        backend_explicit: true,
        ..Default::default()
    };
    memory.semantic.embedding_base_url = "http://remote.example/v1".into();
    memory.semantic.embedding_model = "embedder".into();
    assert!(validate(&memory).is_err(), "auto still validates overrides");
    memory.semantic.embedding = EmbeddingMode::Off;
    assert!(validate(&memory).is_ok(), "off ignores the override");
    memory.semantic.embedding = EmbeddingMode::Auto;
    memory.backend = MemoryBackendId::Local;
    assert!(
        validate(&memory).is_ok(),
        "other backends ignore the override"
    );
}

#[test]
fn half_embedding_override_is_rejected_with_embedding_wording() {
    let mut memory = crate::config::types::MemoryConfig {
        backend: MemoryBackendId::Semantic,
        backend_explicit: true,
        ..Default::default()
    };
    memory.semantic.embedding_model = "nomic-embed-text".into();
    let error = validate(&memory).unwrap_err().to_string();
    assert!(error.contains("Embedding service URL"), "{error}");
    assert!(error.contains("set both or clear both"), "{error}");
    assert!(!error.contains("Service URL must"), "{error}");
    memory.semantic.embedding_model.clear();
    memory.semantic.embedding_base_url = "http://127.0.0.1:11434/v1".into();
    let error = validate(&memory).unwrap_err().to_string();
    assert!(error.contains("Embedding model override"), "{error}");
    memory.semantic.embedding_model = "nomic-embed-text".into();
    validate(&memory).unwrap();
    // A remote URL without a key names the embedding service, not the Hindsight one.
    memory.semantic.embedding_base_url = "https://remote.example/v1".into();
    let error = validate(&memory).unwrap_err().to_string();
    assert!(error.starts_with("The embedding service"), "{error}");
    // Embeddings off or another backend: the half pair is never used, so it is not rejected.
    memory.semantic.embedding_base_url.clear();
    memory.semantic.embedding = EmbeddingMode::Off;
    validate(&memory).unwrap();
}

#[test]
fn memory_role_requirement_follows_backend_and_overrides() {
    let mut memory = crate::config::types::MemoryConfig {
        backend: MemoryBackendId::Sharpshooter,
        ..Default::default()
    };
    assert!(requires_memory_role(&memory));
    memory.sharpshooter.model = "ollama/qwen3".into();
    assert!(
        !requires_memory_role(&memory),
        "the override replaces the role"
    );
    memory.backend = MemoryBackendId::Local;
    assert!(requires_memory_role(&memory));
    memory.backend = MemoryBackendId::Semantic;
    assert!(!requires_memory_role(&memory));
    assert!(auto_retain_uses_memory_role(&memory));
    memory.semantic.auto_retain = false;
    assert!(!auto_retain_uses_memory_role(&memory));
}
