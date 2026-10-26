//! `[memory]`: the memory backend selection and its context limits.
use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct MemoryConfig {
    /// Legacy migration input; backend is the sole runtime selector.
    #[serde(skip_serializing)]
    pub enabled: bool,
    pub backend: crate::config::memory_backend::MemoryBackendId,
    #[serde(skip)]
    pub backend_explicit: bool,
    pub context_limit: usize,
    pub context_max_bytes: usize,
    pub request_max_bytes: usize,
    pub autolearn: bool,
    pub autolearn_auto_continue: bool,
    pub autolearn_min_tool_calls: usize,
    pub local: crate::config::memory_backend::LocalConfig,
    pub hindsight: crate::config::memory_backend::HindsightConfig,
    pub sharpshooter: crate::config::memory_backend::SharpshooterConfig,
    pub semantic: crate::config::memory_backend::SemanticConfig,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: Default::default(),
            backend_explicit: false,
            context_limit: 0,
            context_max_bytes: 0,
            request_max_bytes: 1048576,
            autolearn: false,
            autolearn_auto_continue: false,
            autolearn_min_tool_calls: 5,
            local: Default::default(),
            hindsight: Default::default(),
            sharpshooter: Default::default(),
            semantic: Default::default(),
        }
    }
}
impl<'de> Deserialize<'de> for MemoryConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct Wire {
            enabled: bool,
            backend: Option<crate::config::memory_backend::MemoryBackendId>,
            context_limit: usize,
            context_max_bytes: usize,
            request_max_bytes: usize,
            autolearn: bool,
            autolearn_auto_continue: bool,
            autolearn_min_tool_calls: usize,
            local: crate::config::memory_backend::LocalConfig,
            hindsight: crate::config::memory_backend::HindsightConfig,
            sharpshooter: crate::config::memory_backend::SharpshooterConfig,
            semantic: crate::config::memory_backend::SemanticConfig,
        }
        impl Default for Wire {
            fn default() -> Self {
                Self {
                    enabled: true,
                    backend: None,
                    context_limit: 0,
                    context_max_bytes: 0,
                    request_max_bytes: 1048576,
                    autolearn: false,
                    autolearn_auto_continue: false,
                    autolearn_min_tool_calls: 5,
                    local: Default::default(),
                    hindsight: Default::default(),
                    sharpshooter: Default::default(),
                    semantic: Default::default(),
                }
            }
        }
        let value = Wire::deserialize(deserializer)?;
        Ok(Self {
            enabled: value.enabled,
            backend_explicit: value.backend.is_some(),
            backend: value.backend.unwrap_or(if value.enabled {
                crate::config::memory_backend::MemoryBackendId::Provenance
            } else {
                crate::config::memory_backend::MemoryBackendId::Off
            }),
            context_limit: value.context_limit,
            context_max_bytes: value.context_max_bytes,
            request_max_bytes: value.request_max_bytes,
            autolearn: value.autolearn,
            autolearn_auto_continue: value.autolearn_auto_continue,
            autolearn_min_tool_calls: value.autolearn_min_tool_calls,
            local: value.local,
            hindsight: value.hindsight,
            sharpshooter: value.sharpshooter,
            semantic: value.semantic,
        })
    }
}
