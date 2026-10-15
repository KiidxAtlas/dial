//! Tunables for commit-message generation.

/// Lock and generated files that never describe a change.
pub(crate) const EXCLUDED_FILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "shrinkwrap.yaml",
    "bun.lock",
    "bun.lockb",
    "deno.lock",
    "composer.lock",
    "Gemfile.lock",
    "poetry.lock",
    "Pipfile.lock",
    "pdm.lock",
    "uv.lock",
    "go.sum",
    "flake.lock",
    "pubspec.lock",
    "Podfile.lock",
    "Packages.resolved",
    "mix.lock",
    "packages.lock.json",
    "gradle.lockfile",
];

/// Extensions ranked last when a diff must be truncated (leading dot stripped).
pub(crate) const LOW_PRIORITY_EXTENSIONS: &[&str] = &[
    "lock", "snap", "sum", "toml", "yaml", "yml", "json", "md", "txt", "log", "tmp", "bak",
];

/// Maximum size of one file's diff before the prompt scrubber truncates it.
pub(crate) const MAX_FILE_BYTES: usize = 100_000;

/// Generation limits and thresholds.
#[derive(Debug, Clone)]
pub struct GenerationConfig {
    pub summary_guideline: usize,
    pub summary_soft_limit: usize,
    pub summary_hard_limit: usize,
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
    pub auto_fast_threshold_lines: usize,
    pub max_diff_length: usize,
    pub max_diff_tokens: usize,
    pub wide_change_threshold: f64,
    pub max_detail_tokens: usize,
    pub wide_change_abstract: bool,
    pub map_reduce_threshold: usize,
    pub map_batch_token_budget: usize,
    pub map_reduce_enabled: bool,
    /// Persistent reply cache (`commit.cache_enabled`).
    pub cache_enabled: bool,
    /// Cache entry lifetime in days (`commit.cache_ttl_days`).
    pub cache_ttl_days: f64,
}

impl Default for GenerationConfig {
    fn default() -> Self {
        Self {
            summary_guideline: 72,
            summary_soft_limit: 96,
            summary_hard_limit: 128,
            max_retries: 3,
            initial_backoff_ms: 1000,
            auto_fast_threshold_lines: 200,
            max_diff_length: 100_000,
            max_diff_tokens: 25_000,
            wide_change_threshold: 0.5,
            max_detail_tokens: 200,
            wide_change_abstract: true,
            map_reduce_threshold: 5000,
            map_batch_token_budget: 16_000,
            map_reduce_enabled: true,
            cache_enabled: true,
            cache_ttl_days: 14.0,
        }
    }
}

impl GenerationConfig {
    /// Defaults with the `[commit]` cache settings applied.
    pub(crate) fn from_settings(commit: &crate::config::types::CommitConfig) -> Self {
        Self {
            cache_enabled: commit.cache_enabled,
            cache_ttl_days: commit.cache_ttl_days,
            ..Self::default()
        }
    }
}
