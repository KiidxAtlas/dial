//! `[task]` settings for subagent workers.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `[task]`: subagent orchestration limits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskConfig {
    /// How many levels deep subagents may start their own subagents: -1 unlimited, 0 none.
    /// The main agent's children are depth 1; with 2 they may spawn one more level.
    pub max_recursion_depth: i32,
    /// Most active worker branches per project in this process (1..=64).
    pub max_concurrency: usize,
    pub isolation_dependencies: bool,
    pub isolation_git: bool,
    pub isolation_dependency_bytes: u64,
    pub isolation_source_bytes: usize,
    pub isolation_source_files: usize,
    /// Per-provider ceiling on in-flight subagent model requests (provider id → limit; 0 or
    /// absent = unlimited). A slot is held only while a request streams, never across a child's
    /// whole run, so deep spawn trees cannot deadlock. A local model server is always 1.
    pub provider_concurrency: BTreeMap<String, usize>,
    /// Load Markdown agent definitions from `.dial/agents` and `~/.dial/agents`.
    pub discover_agents: bool,
    /// Also load Claude Code definitions from `.claude/agents` and `~/.claude/agents`.
    pub claude_agents: bool,
    /// Rejected structured results (`yield` or a JSON answer) a child may correct before failing.
    pub max_schema_retries: u32,
    /// Keep finished subagent transcripts (outside the project) so they can be resumed by id.
    pub persist: bool,
    /// Pending steering messages a running or parked agent may hold (1..=4096).
    pub inbox_limit: usize,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            max_recursion_depth: 2,
            max_concurrency: 32,
            isolation_dependencies: true,
            isolation_git: true,
            isolation_dependency_bytes: 4 * 1024 * 1024 * 1024,
            isolation_source_bytes: 512 * 1024 * 1024,
            isolation_source_files: 100_000,
            provider_concurrency: BTreeMap::new(),
            discover_agents: true,
            claude_agents: true,
            max_schema_retries: 3,
            persist: true,
            inbox_limit: 256,
        }
    }
}

impl TaskConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=1_000_000).contains(&self.isolation_source_files)
                && (1..=2_147_483_648).contains(&self.isolation_source_bytes),
            "invalid isolation source limits"
        );
        anyhow::ensure!(
            self.isolation_dependency_bytes <= 64 * 1024 * 1024 * 1024,
            "isolation dependency limit exceeds 64 GiB"
        );
        anyhow::ensure!(
            (-1..=16).contains(&self.max_recursion_depth),
            "task.max_recursion_depth must be -1 (unlimited) or 0..16"
        );
        anyhow::ensure!(
            (1..=64).contains(&self.max_concurrency),
            "task.max_concurrency must be 1..64"
        );
        anyhow::ensure!(
            self.max_schema_retries <= 10,
            "task.max_schema_retries must be 0..10"
        );
        anyhow::ensure!(
            (1..=4096).contains(&self.inbox_limit),
            "task.inbox_limit must be 1..4096"
        );
        for (provider, limit) in &self.provider_concurrency {
            anyhow::ensure!(
                !provider.trim().is_empty() && *limit <= 1024,
                "task.provider_concurrency.{provider:?} must be 0 (unlimited)..1024"
            );
        }
        Ok(())
    }

    /// Whether an agent at `depth` (main agent 0, its children 1) may still start subagents.
    pub fn can_spawn_at(&self, depth: u32) -> bool {
        self.max_recursion_depth < 0 || i64::from(depth) < i64::from(self.max_recursion_depth)
    }
}
