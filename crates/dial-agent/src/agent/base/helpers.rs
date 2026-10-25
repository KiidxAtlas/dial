//! Small shared helpers: workspace configuration, text utilities and history thresholds.

use crate::agent::*;

pub(in crate::agent) struct CheckpointObserver(pub(in crate::agent) Arc<Checkpoints>);

impl tools::WriteObserver for CheckpointObserver {
    fn before_write(&self, abs: &Path) -> Result<()> {
        self.0.before_write(abs)
    }
    fn scoped(&self, owner: &str) -> Option<Arc<dyn tools::WriteObserver>> {
        Some(Arc::new(WorkerObserver(self.0.clone(), owner.to_owned())))
    }
}

/// Records a worker's writes in the shared checkpoint under the worker's label.
struct WorkerObserver(Arc<Checkpoints>, String);

impl tools::WriteObserver for WorkerObserver {
    fn before_write(&self, abs: &Path) -> Result<()> {
        self.0.before_write_as(abs, &self.1)
    }
    fn scoped(&self, owner: &str) -> Option<Arc<dyn tools::WriteObserver>> {
        Some(Arc::new(WorkerObserver(self.0.clone(), owner.to_owned())))
    }
}

pub(in crate::agent) fn result_clone(report: &TurnReport) -> TurnReport {
    TurnReport {
        text: report.text.clone(),
        outcome: report.outcome,
        steps: report.steps,
        interrupted: report.interrupted,
        trajectory: report.trajectory,
    }
}

pub(in crate::agent) fn configure(
    ws: Workspace,
    config: &Config,
    mode: crate::config::types::EditMode,
    layout: Option<&Layout>,
    checkpoints: Option<Arc<Checkpoints>>,
) -> Result<Workspace> {
    let mut ws = ws
        .with_edit_settings(config.edit.clone())
        .with_ssh_config(config.ssh.clone())
        .with_ssh_hosts(&config.ssh_hosts)
        .with_edit_mode(mode)
        .with_syntax_guard(config.assistance.enabled && config.assistance.syntax_guard);
    if let Some(layout) = layout {
        ws = ws.with_edit_blackbox_dir(layout.state_dir.join("edit-blackbox"));
    }
    if let Some(checkpoints) = checkpoints {
        ws = ws.with_observer(Arc::new(CheckpointObserver(checkpoints)));
    }
    configure_sandbox(ws, config, layout)
}

pub(in crate::agent) fn configure_strict(
    ws: Workspace,
    config: &Config,
    mode: crate::config::types::EditMode,
) -> Result<Workspace> {
    configure_sandbox(
        ws.with_edit_settings(config.edit.clone())
            .with_ssh_config(config.ssh.clone())
            .with_ssh_hosts(&config.ssh_hosts)
            .with_edit_mode(mode),
        config,
        None,
    )
}

pub(in crate::agent) fn configure_sandbox(
    ws: Workspace,
    config: &Config,
    layout: Option<&Layout>,
) -> Result<Workspace> {
    let ws = tools::apply_sandbox_config(
        ws,
        &config.sandbox,
        layout.map(|l| (l.state_dir.as_path(), l.user_dir.as_path())),
    )?;
    Ok(ws.with_shell_config(&config.shell))
}

/// A superseded read is replaced only if no more than this many estimated tokens follow it (or the context is
/// under pressure): replacing one buried deeper would force the provider to prefill everything after it again.
pub(in crate::agent) const CACHE_WARM_SUFFIX_TOKENS: usize = 6_000;
/// Old observation results are dropped under context pressure, except in the newest part of the conversation:
/// a fifth of the window, never less than this, so the model keeps the evidence it is working from right now.
pub(in crate::agent) const PRUNE_PROTECT_MIN_TOKENS: usize = 4_000;
/// Longest search query sent to memory recall, in characters; the store itself refuses queries over 8 KiB.
pub(in crate::agent) const RECALL_QUERY_CHARS: usize = 2_000;

pub fn json_text(text: &str) -> &str {
    let text = text.trim();
    if let Some(body) = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
    {
        body.trim().strip_suffix("```").unwrap_or(body).trim()
    } else {
        text
    }
}

pub use crate::runtime::presentation::truncate;

pub(in crate::agent) fn looks_like_overflow(error: &anyhow::Error) -> bool {
    crate::llm::errors::is_context_overflow(error)
}

/// Merge this request's plain harness notes (transient user messages with text content and no
/// other Dial marker than the advisory priority) from `start` on into one message at the first
/// note's position, so the model reads one harness note per request instead of a stack of
/// competing instructions. Advisory notes are capped first by `run::guidance::cap`.
pub(in crate::agent) fn coalesce_harness_notes(messages: &mut Vec<Value>, start: usize) {
    let plain = crate::agent::run::guidance::is_plain_note;
    let Some(first) = (start..messages.len()).find(|&index| plain(&messages[index])) else {
        return;
    };
    let mut texts = vec![
        messages[first]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    ];
    let mut index = first + 1;
    while index < messages.len() {
        if plain(&messages[index]) {
            texts.push(
                messages.remove(index)["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
        } else {
            index += 1;
        }
    }
    if texts.len() > 1 {
        messages[first]["content"] = json!(texts.join("\n\n"));
    }
}

/// A reply that is just a JSON-looking object naming an offered tool. Small models often emit
/// slightly broken ones (`{{"name": ...}` with unbalanced braces), so this matches the shape
/// rather than requiring valid JSON.
pub(in crate::agent) fn looks_like_text_call(text: &str, names: &[String]) -> bool {
    let t = text.trim();
    if !t.starts_with('{')
        || t.len() > 8000
        || !t.contains("\"arguments\"") && !t.contains("\"parameters\"")
    {
        return false;
    }
    names.iter().any(|n| {
        t.contains(&format!("\"name\": \"{n}\"")) || t.contains(&format!("\"name\":\"{n}\""))
    })
}

/// Window that history thresholds are planned against: the model window, limited by the operator's
/// absolute history ceiling when the economy policy is on (a ceiling of 0 means no ceiling). Never
/// below 8192 tokens.
pub(in crate::agent) fn capped_window(window: u32, cap: u32, economy: bool) -> u32 {
    if economy && cap > 0 {
        window.min(cap.max(8192))
    } else {
        window
    }
}

#[cfg(test)]
mod history_cap_tests;
