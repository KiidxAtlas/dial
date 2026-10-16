use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryBackendId {
    Off,
    #[default]
    Provenance,
    Local,
    Hindsight,
    Sharpshooter,
    Semantic,
}
impl MemoryBackendId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Provenance => "provenance",
            Self::Local => "local",
            Self::Hindsight => "hindsight",
            Self::Sharpshooter => "sharpshooter",
            Self::Semantic => "semantic",
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryScoping {
    Global,
    #[default]
    PerProject,
    PerProjectTagged,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetainMode {
    #[default]
    FullSession,
    LastTurn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub max_rollouts_per_startup: usize,
    pub max_rollout_age_days: u64,
    pub min_rollout_idle_hours: u64,
    pub thread_scan_limit: usize,
    pub max_raw_memories: usize,
    pub stage1_concurrency: usize,
    pub stage1_lease_secs: u64,
    pub stage1_retry_delay_secs: u64,
    pub consolidation_lease_secs: u64,
    pub consolidation_retry_delay_secs: u64,
    pub consolidation_heartbeat_secs: u64,
    pub rollout_payload_percent: f64,
    pub phase1_input_tokens: u32,
    pub fallback_token_limit: u32,
    pub summary_injection_tokens: u32,
    pub extraction_role: String,
    pub consolidation_role: String,
    pub timeout_secs: u64,
}
impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            max_rollouts_per_startup: 64,
            max_rollout_age_days: 30,
            min_rollout_idle_hours: 12,
            thread_scan_limit: 300,
            max_raw_memories: 200,
            stage1_concurrency: 8,
            stage1_lease_secs: 120,
            stage1_retry_delay_secs: 120,
            consolidation_lease_secs: 180,
            consolidation_retry_delay_secs: 180,
            consolidation_heartbeat_secs: 30,
            rollout_payload_percent: 0.7,
            phase1_input_tokens: 4000,
            fallback_token_limit: 16000,
            summary_injection_tokens: 5000,
            extraction_role: "memory".into(),
            consolidation_role: "memory".into(),
            timeout_secs: 120,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HindsightConfig {
    pub endpoint: String,
    pub api_key_env: String,
    pub bank: String,
    pub scoping: MemoryScoping,
    pub recall_budget: String,
    pub recall_max_tokens: u32,
    pub request_timeout_secs: u64,
    pub retain_every_turns: usize,
    pub auto_recall: bool,
    pub auto_retain: bool,
    pub retain_mode: RetainMode,
    pub retain_overlap_turns: usize,
    pub retain_context: String,
    pub recall_types: Vec<String>,
    pub recall_context_turns: usize,
    pub recall_max_query_chars: usize,
    pub reflect_timeout_secs: u64,
    pub recall_timeout_secs: u64,
    pub retain_timeout_secs: u64,
    pub mental_models_enabled: bool,
    pub mental_model_auto_seed: bool,
    pub mental_model_max_render_chars: usize,
    pub retain_mission: String,
    pub reflect_mission: String,
}
impl Default for HindsightConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            api_key_env: String::new(),
            bank: "dial".into(),
            scoping: MemoryScoping::PerProject,
            recall_budget: "mid".into(),
            recall_max_tokens: 1024,
            request_timeout_secs: 30,
            retain_every_turns: 3,
            auto_recall: true,
            retain_mission: String::new(),
            reflect_mission: String::new(),
            auto_retain: true,
            retain_mode: Default::default(),
            retain_overlap_turns: 2,
            retain_context: "dial".into(),
            recall_types: vec!["world".into(), "experience".into()],
            recall_context_turns: 1,
            recall_max_query_chars: 800,
            reflect_timeout_secs: 120,
            recall_timeout_secs: 30,
            retain_timeout_secs: 60,
            mental_models_enabled: true,
            mental_model_auto_seed: true,
            mental_model_max_render_chars: 16000,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SharpshooterConfig {
    pub model: String,
    pub model_role: String,
    pub interval_minutes: u64,
    pub injection_token_limit: u32,
    pub max_file_lines: usize,
    pub max_parallel: usize,
    pub lease_secs: u64,
    pub timeout_secs: u64,
}
impl Default for SharpshooterConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            model_role: "memory".into(),
            interval_minutes: 5,
            injection_token_limit: 15000,
            max_file_lines: 120,
            max_parallel: 1,
            lease_secs: 180,
            timeout_secs: 120,
        }
    }
}

/// Whether recall also uses embedding vectors. `auto` uses them only when an endpoint and model are set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingMode {
    Off,
    #[default]
    Auto,
    On,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticConfig {
    pub embedding: EmbeddingMode,
    pub embedding_base_url: String,
    pub embedding_model: String,
    pub embedding_api_key_env: String,
    pub embedding_timeout_secs: u64,
    pub recall_limit: usize,
    pub token_budget: usize,
    /// Retain facts from substantive completed turns automatically, using the memory role.
    pub auto_retain: bool,
    /// Minutes between near-duplicate merge passes; 0 turns consolidation off.
    pub consolidate_interval_minutes: u64,
}
impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            embedding: EmbeddingMode::Auto,
            embedding_base_url: String::new(),
            embedding_model: String::new(),
            embedding_api_key_env: String::new(),
            embedding_timeout_secs: 20,
            recall_limit: 8,
            token_budget: 1500,
            auto_retain: true,
            consolidate_interval_minutes: 60,
        }
    }
}
fn role(value: &str) -> Result<()> {
    let roles = crate::llm::model_config::ROLES;
    ensure!(
        roles.iter().any(|(name, _)| *name == value),
        "invalid memory model role {value:?}; expected one of {}",
        roles
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}
/// Wording for one endpoint field, so a rejected embedding URL is not described as the Hindsight service.
struct EndpointLabels {
    url: &'static str,
    service: &'static str,
}
const HINDSIGHT_LABELS: EndpointLabels = EndpointLabels {
    url: "Service URL",
    service: "The memory service",
};
const EMBEDDING_LABELS: EndpointLabels = EndpointLabels {
    url: "Embedding service URL",
    service: "The embedding service",
};
fn endpoint(value: &str, credential: &str, labels: &EndpointLabels) -> Result<()> {
    let url = url::Url::parse(value).map_err(|_| {
        anyhow::anyhow!(
            "{} must be a full https:// address (http:// is allowed only on this machine)",
            labels.url
        )
    })?;
    let host = url.host_str().unwrap_or("");
    let bare_host = host.trim_start_matches('[').trim_end_matches(']');
    let local = host == "localhost"
        || bare_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && local),
        "memory endpoints require HTTPS except explicit loopback HTTP"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "{} must not contain a password, query string or fragment; put the key in the API key variable instead",
        labels.url
    );
    ensure!(
        local || !credential.is_empty(),
        "{} is not on this machine, so API key variable must name an environment variable that holds its key",
        labels.service
    );
    Ok(())
}
fn env_name(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 128
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "API key variable must be an environment variable name made of letters, digits and underscores"
    );
    Ok(())
}
fn bank(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'),
        "memory bank must contain 1..=64 ASCII letters, digits, underscores or hyphens"
    );
    Ok(())
}

/// Whether the enabled backend calls a chat model through a memory role: `memory` for local
/// extraction/consolidation and sharpshooter work (`decision` is not used by these backends).
/// Hindsight runs server-side and Provenance is deterministic, so neither needs a role. Semantic
/// calls the memory role only for automatic fact saving, which pauses (never falls back to the main
/// model) while the role is unassigned; see [`auto_retain_uses_memory_role`].
pub fn calls_model(config: &crate::config::types::MemoryConfig) -> bool {
    config.enabled
        && matches!(
            config.backend,
            MemoryBackendId::Local | MemoryBackendId::Sharpshooter
        )
}

/// Whether the enabled backend cannot work until the `memory` model role is assigned: local
/// extraction or consolidation on that role, or sharpshooter on that role without a model override.
pub fn requires_memory_role(config: &crate::config::types::MemoryConfig) -> bool {
    calls_model(config)
        && match config.backend {
            MemoryBackendId::Local => [
                config.local.extraction_role.as_str(),
                config.local.consolidation_role.as_str(),
            ]
            .contains(&"memory"),
            MemoryBackendId::Sharpshooter => {
                config.sharpshooter.model.trim().is_empty()
                    && config.sharpshooter.model_role == "memory"
            }
            _ => false,
        }
}

/// Semantic memory saves facts automatically with the `memory` role when `auto_retain` is on. The
/// role is optional: without it automatic saving pauses while recall and the retain tool still work.
pub fn auto_retain_uses_memory_role(config: &crate::config::types::MemoryConfig) -> bool {
    config.enabled && config.backend == MemoryBackendId::Semantic && config.semantic.auto_retain
}

/// The explicit OpenAI-compatible embedding override is only usable as a pair: `Some(message)` when
/// exactly one of `embedding_base_url` and `embedding_model` is set.
pub fn half_embedding_override(semantic: &SemanticConfig) -> Option<&'static str> {
    match (
        semantic.embedding_base_url.trim().is_empty(),
        semantic.embedding_model.trim().is_empty(),
    ) {
        (true, false) => Some(
            "Embedding model override is set without Embedding service URL; set both or clear both (to use the embedding role, Alt+M, clear both)",
        ),
        (false, true) => Some(
            "Embedding service URL is set without Embedding model override; set both or clear both (to use the embedding role, Alt+M, clear both)",
        ),
        _ => None,
    }
}

pub fn validate(config: &crate::config::types::MemoryConfig) -> Result<()> {
    ensure!(
        config.enabled || !config.backend_explicit || config.backend == MemoryBackendId::Off,
        "memory.enabled=false conflicts with explicit non-off memory.backend; remove legacy enabled or choose off"
    );
    ensure!(
        config.context_limit <= 50
            && (config.context_max_bytes == 0 || (256..=24000).contains(&config.context_max_bytes)),
        "invalid memory context bounds"
    );
    ensure!(
        (1024..=16 * 1024 * 1024).contains(&config.request_max_bytes),
        "invalid memory request_max_bytes (1024..=16MiB)"
    );
    ensure!(
        (1..=1000).contains(&config.autolearn_min_tool_calls),
        "invalid autolearn tool threshold"
    );
    let local = &config.local;
    ensure!(
        (1..=1024).contains(&local.max_rollouts_per_startup)
            && (1..=3650).contains(&local.max_rollout_age_days)
            && local.min_rollout_idle_hours <= 8760
            && (1..=10000).contains(&local.thread_scan_limit)
            && (1..=2000).contains(&local.max_raw_memories)
            && (1..=32).contains(&local.stage1_concurrency),
        "invalid local memory rollout bounds"
    );
    ensure!(
        (0.05..=0.9).contains(&local.rollout_payload_percent)
            && (256..=1000000).contains(&local.phase1_input_tokens)
            && (1024..=1000000).contains(&local.fallback_token_limit)
            && (256..=1000000).contains(&local.summary_injection_tokens),
        "invalid local memory token bounds"
    );
    ensure!(
        (1..=3600).contains(&local.timeout_secs)
            && local.stage1_lease_secs >= local.timeout_secs
            && local.consolidation_lease_secs >= local.timeout_secs
            && (1..local.consolidation_lease_secs).contains(&local.consolidation_heartbeat_secs)
            && local.stage1_retry_delay_secs <= 86400
            && local.consolidation_retry_delay_secs <= 86400,
        "invalid local memory lease, heartbeat or deadline"
    );
    ensure!(
        local.stage1_lease_secs <= 86400 && local.consolidation_lease_secs <= 86400,
        "local memory lease exceeds allowed bound"
    );
    role(&local.extraction_role)?;
    role(&local.consolidation_role)?;
    let hindsight = &config.hindsight;
    env_name(&hindsight.api_key_env)?;
    bank(&hindsight.bank)?;
    ensure!(
        ["low", "mid", "high"].contains(&hindsight.recall_budget.as_str())
            && (256..=100000).contains(&hindsight.recall_max_tokens)
            && (1..=3600).contains(&hindsight.request_timeout_secs)
            && (1..=1000).contains(&hindsight.retain_every_turns),
        "invalid hindsight recall or retention bounds"
    );
    ensure!(
        hindsight.retain_mission.len() <= 8192 && hindsight.reflect_mission.len() <= 8192,
        "hindsight mission exceeds bounds"
    );
    ensure!(
        hindsight.retain_overlap_turns <= 1000
            && hindsight.retain_context.len() <= 8192
            && hindsight.recall_types.len() <= 16
            && hindsight
                .recall_types
                .iter()
                .all(|value| !value.is_empty() && value.len() <= 64)
            && (1..=32).contains(&hindsight.recall_context_turns)
            && (256..=65536).contains(&hindsight.recall_max_query_chars)
            && [
                hindsight.reflect_timeout_secs,
                hindsight.recall_timeout_secs,
                hindsight.retain_timeout_secs
            ]
            .iter()
            .all(|value| (1..=3600).contains(value))
            && (256..=65536).contains(&hindsight.mental_model_max_render_chars),
        "invalid hindsight context, deadline or synthesis bounds"
    );
    if !hindsight.endpoint.is_empty() {
        endpoint(
            &hindsight.endpoint,
            &hindsight.api_key_env,
            &HINDSIGHT_LABELS,
        )?;
    }
    if config.backend == MemoryBackendId::Hindsight {
        ensure!(
            !hindsight.endpoint.is_empty(),
            "hindsight backend requires memory.hindsight.endpoint"
        );
    }
    let sharp = &config.sharpshooter;
    role(&sharp.model_role)?;
    ensure!(
        sharp.model.len() <= 1024
            && (1..=1440).contains(&sharp.interval_minutes)
            && (256..=1000000).contains(&sharp.injection_token_limit)
            && (1..=120).contains(&sharp.max_file_lines)
            && (1..=32).contains(&sharp.max_parallel)
            && (1..=3600).contains(&sharp.timeout_secs)
            && sharp.lease_secs >= sharp.timeout_secs
            && sharp.lease_secs <= 86400,
        "invalid sharpshooter bounds"
    );
    let semantic = &config.semantic;
    ensure!(
        (1..=120).contains(&semantic.embedding_timeout_secs)
            && (1..=50).contains(&semantic.recall_limit)
            && (64..=16000).contains(&semantic.token_budget)
            && semantic.consolidate_interval_minutes <= 10080
            && semantic.embedding_base_url.len() <= 2048
            && semantic.embedding_model.len() <= 256,
        "invalid semantic memory bounds"
    );
    // Overrides matter only to the semantic backend, and not at all when embeddings are off.
    let overrides_used =
        config.backend == MemoryBackendId::Semantic && semantic.embedding != EmbeddingMode::Off;
    if overrides_used {
        if let Some(message) = half_embedding_override(semantic) {
            anyhow::bail!("{message}");
        }
    }
    if overrides_used && !semantic.embedding_base_url.trim().is_empty() {
        endpoint(
            semantic.embedding_base_url.trim(),
            &semantic.embedding_api_key_env,
            &EMBEDDING_LABELS,
        )?;
    }
    if !semantic.embedding_api_key_env.is_empty() {
        env_name(&semantic.embedding_api_key_env)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        let mut memory = crate::config::types::MemoryConfig::default();
        memory.backend = MemoryBackendId::Semantic;
        memory.backend_explicit = true;
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
        let mut memory = crate::config::types::MemoryConfig::default();
        memory.backend = MemoryBackendId::Semantic;
        memory.backend_explicit = true;
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
        let mut memory = crate::config::types::MemoryConfig::default();
        memory.backend = MemoryBackendId::Sharpshooter;
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
}
