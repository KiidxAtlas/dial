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
fn endpoint(value: &str, credential: &str) -> Result<()> {
    let url = url::Url::parse(value).map_err(|_| {
        anyhow::anyhow!(
            "Service URL must be a full https:// address (http:// is allowed only on this machine)"
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
        "Service URL must not contain a password, query string or fragment; put the key in the API key variable instead"
    );
    ensure!(
        local || !credential.is_empty(),
        "The memory service is not on this machine, so API key variable must name an environment variable that holds its key"
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
/// Hindsight runs server-side and Provenance is deterministic, so neither needs a role.
pub fn calls_model(config: &crate::config::types::MemoryConfig) -> bool {
    config.enabled
        && matches!(
            config.backend,
            MemoryBackendId::Local | MemoryBackendId::Sharpshooter
        )
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
        endpoint(&hindsight.endpoint, &hindsight.api_key_env)?;
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
    if !semantic.embedding_base_url.trim().is_empty() {
        endpoint(
            semantic.embedding_base_url.trim(),
            &semantic.embedding_api_key_env,
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
}
