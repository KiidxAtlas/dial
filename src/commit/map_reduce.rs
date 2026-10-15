//! Map-reduce analysis for diffs too large for one prompt.
//!
//! Each non-binary file is summarized into factual observations by batched map calls, then a
//! single reduce call synthesizes the conventional analysis from those observations.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::commit_types::{ConventionalAnalysis, DEFAULT_COMMIT_TYPE, types_description};
use super::config::GenerationConfig;
use super::diff::{ConventionalFileDiff, FileStatus, is_excluded, parse_prompt_diff};
use super::inference::{Inference, Tier};
use super::markdown::{
    FileObservations, parse_conventional_analysis_markdown, parse_file_observations_markdown,
};
use super::prompts::{PromptContext, PromptFamily, render_conventional_prompt};
use crate::llm::Reply;

const MAX_FILE_TOKENS: usize = 50_000;
const MAP_PHASE_CONCURRENCY: usize = 16;
const MIN_MAP_BATCH_TOKENS: usize = 4_000;
const MAX_CONTEXT_FILES: usize = 20;

/// Factual observations extracted for one changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedFile {
    file: String,
    observations: Vec<String>,
    additions: usize,
    deletions: usize,
    status: FileStatus,
}

/// Return whether a diff crosses the exact map-reduce threshold.
pub(crate) fn should_use_map_reduce(diff: &str, config: &GenerationConfig) -> bool {
    if !config.map_reduce_enabled {
        return false;
    }
    let mut total_tokens = 0usize;
    let mut has_included_file = false;
    for file in included_files(parse_prompt_diff(diff)) {
        has_included_file = true;
        let tokens = file.token_estimate();
        if tokens > MAX_FILE_TOKENS {
            return true;
        }
        total_tokens += tokens;
        if total_tokens >= config.map_reduce_threshold {
            return true;
        }
    }
    has_included_file && total_tokens >= config.map_reduce_threshold
}

/// Run per-file observation mapping followed by one reduce synthesis call.
pub(crate) fn run_map_reduce(
    inference: &Inference<'_>,
    config: &GenerationConfig,
    stat: &str,
    diff: &str,
    scope_candidates: &str,
    user_context: Option<&str>,
) -> Result<ConventionalAnalysis, String> {
    let files = included_files(parse_prompt_diff(diff));
    if files.is_empty() {
        return Err("No relevant files to summarize after filtering".to_owned());
    }
    let observations = map_phase(&files, inference, config)?;
    let prompt = render_conventional_prompt(
        PromptFamily::Reduce,
        &PromptContext::default()
            .with_text("types_description", types_description())
            .with_text("observations", render_observations_markdown(&observations))
            .with_text("stat", stat)
            .with_text("scope_candidates", scope_candidates),
    );
    let user = match user_context.filter(|value| !value.is_empty()) {
        Some(value) => format!(
            "{}\n\n<user_context>\n{value}\n</user_context>",
            prompt.user
        ),
        None => prompt.user,
    };
    inference.complete(Tier::Primary, &prompt.system, &user, |reply| {
        parse_conventional_analysis_markdown(&reply.text, DEFAULT_COMMIT_TYPE)
    })
}

fn included_files(files: Vec<ConventionalFileDiff>) -> Vec<ConventionalFileDiff> {
    files
        .into_iter()
        .filter(|file| !is_excluded(&file.filename))
        .collect()
}

/// Render status and change-count annotations into reduce-phase markdown.
fn render_observations_markdown(observations: &[ObservedFile]) -> String {
    let sections: Vec<String> = observations
        .iter()
        .map(|item| {
            let mut annotations: Vec<String> = if item.status == FileStatus::Modified {
                Vec::new()
            } else {
                vec![item.status.label().to_owned()]
            };
            if item.additions > 0 || item.deletions > 0 {
                annotations.push(format!("+{}/-{}", item.additions, item.deletions));
            }
            let suffix = if annotations.is_empty() {
                String::new()
            } else {
                format!(" ({})", annotations.join(", "))
            };
            let bullets: Vec<String> = item
                .observations
                .iter()
                .map(|text| format!("- {text}"))
                .collect();
            format!("# {}{suffix}\n{}", item.file, bullets.join("\n"))
        })
        .collect();
    sections.join("\n\n")
}

fn map_phase(
    files: &[ConventionalFileDiff],
    inference: &Inference<'_>,
    config: &GenerationConfig,
) -> Result<Vec<ObservedFile>, String> {
    let headers = ContextHeaders::new(files);
    let total_tokens: usize = files
        .iter()
        .map(|file| {
            if file.is_binary {
                0
            } else {
                file.token_estimate()
            }
        })
        .sum();
    let budget = effective_map_budget(total_tokens, config.map_batch_token_budget);
    let batches = build_llm_file_batches(files, budget);

    let mut observations: Vec<Option<ObservedFile>> = files
        .iter()
        .map(|file| {
            file.is_binary.then(|| ObservedFile {
                file: file.filename.clone(),
                observations: vec!["Binary file changed.".to_owned()],
                additions: file.additions,
                deletions: file.deletions,
                status: file.status,
            })
        })
        .collect();

    let batch_results = map_with_concurrency(
        &batches,
        MAP_PHASE_CONCURRENCY,
        |batch, _batch_index| -> Result<Vec<(usize, ObservedFile)>, String> {
            let batch_files: Vec<&ConventionalFileDiff> =
                batch.iter().map(|&index| &files[index]).collect();
            let names: Vec<&str> = batch_files
                .iter()
                .map(|file| file.filename.as_str())
                .collect();
            let header = headers.header_for_files(&names);
            let mapped = map_file_batch(&batch_files, &header, inference, budget)?;
            Ok(batch
                .iter()
                .copied()
                .zip(mapped)
                .collect::<Vec<(usize, ObservedFile)>>())
        },
    );
    for batch in batch_results {
        for (file_index, observation) in batch? {
            observations[file_index] = Some(observation);
        }
    }
    observations
        .into_iter()
        .enumerate()
        .map(|(index, observation)| {
            observation
                .ok_or_else(|| format!("Missing map observation for {}", files[index].filename))
        })
        .collect()
}

fn map_file_batch(
    files: &[&ConventionalFileDiff],
    context_header: &str,
    inference: &Inference<'_>,
    budget: usize,
) -> Result<Vec<ObservedFile>, String> {
    let prompt_files: Vec<HashMap<String, String>> = files
        .iter()
        .map(|file| {
            HashMap::from([
                ("path".to_owned(), file.filename.clone()),
                ("diff".to_owned(), render_file_diff_for_batch(file, budget)),
            ])
        })
        .collect();
    let prompt = render_conventional_prompt(
        PromptFamily::Map,
        &PromptContext::default()
            .with_list("files", prompt_files)
            .with_text("context_header", context_header),
    );
    inference.complete(Tier::Fast, &prompt.system, &prompt.user, |reply: &Reply| {
        Ok(map_response_to_observations(
            files,
            &parse_file_observations_markdown(&reply.text),
            &reply.finish_reason,
        ))
    })
}

fn map_response_to_observations(
    files: &[&ConventionalFileDiff],
    entries: &[FileObservations],
    finish_reason: &str,
) -> Vec<ObservedFile> {
    if entries.is_empty() {
        return files
            .iter()
            .map(|file| fallback_file_observation(file))
            .collect();
    }
    let mut used = vec![false; entries.len()];
    let stopped_at_max_tokens = matches!(finish_reason, "max_tokens" | "length");
    files
        .iter()
        .map(|file| {
            let Some(entry_index) = find_observation_entry(&file.filename, entries, &used, files)
            else {
                return fallback_file_observation(file);
            };
            used[entry_index] = true;
            let observations: Vec<String> = entries[entry_index]
                .observations
                .iter()
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .collect();
            let observations = if observations.is_empty() && stopped_at_max_tokens {
                vec![fallback_observation_text(&file.filename)]
            } else {
                observations
            };
            ObservedFile {
                file: file.filename.clone(),
                observations,
                additions: file.additions,
                deletions: file.deletions,
                status: file.status,
            }
        })
        .collect()
}

fn find_observation_entry(
    filename: &str,
    entries: &[FileObservations],
    used: &[bool],
    files: &[&ConventionalFileDiff],
) -> Option<usize> {
    let basename = base_name(filename);
    let basename_unique = files
        .iter()
        .filter(|file| base_name(&file.filename) == basename)
        .count()
        == 1;
    for pass in 0..3 {
        for (index, entry) in entries.iter().enumerate() {
            if used[index] {
                continue;
            }
            let candidate = entry.path.as_str();
            if pass == 0 && candidate == filename {
                return Some(index);
            }
            if pass == 1 && basename_unique && base_name(candidate) == basename {
                return Some(index);
            }
            if pass == 2 && path_suffix_matches(candidate, filename) {
                return Some(index);
            }
        }
    }
    None
}

fn effective_map_budget(total_tokens: usize, cap: usize) -> usize {
    let ideal = (total_tokens * 5).div_ceil(MAP_PHASE_CONCURRENCY * 4);
    cap.max(1).min(MIN_MAP_BATCH_TOKENS.max(ideal))
}

/// Group non-binary file indices into greedy token- and byte-budgeted batches.
fn build_llm_file_batches(files: &[ConventionalFileDiff], budget: usize) -> Vec<Vec<usize>> {
    let token_budget = budget.max(1);
    let byte_budget = token_budget * 4;
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_tokens = 0usize;
    let mut current_bytes = 0usize;
    for (index, file) in files.iter().enumerate() {
        if file.is_binary {
            continue;
        }
        let tokens = file.token_estimate();
        let bytes = file.size() + usize::from(!file.content.is_empty());
        if tokens > token_budget || bytes > byte_budget {
            if !current.is_empty() {
                batches.push(std::mem::take(&mut current));
            }
            batches.push(vec![index]);
            current_tokens = 0;
            current_bytes = 0;
            continue;
        }
        if !current.is_empty()
            && (current_tokens + tokens > token_budget || current_bytes + bytes > byte_budget)
        {
            batches.push(std::mem::take(&mut current));
            current_tokens = 0;
            current_bytes = 0;
        }
        current.push(index);
        current_tokens += tokens;
        current_bytes += bytes;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

fn render_file_diff_for_batch(file: &ConventionalFileDiff, budget: usize) -> String {
    let max_bytes = (budget * 4).max(1);
    let rendered_bytes = file.size() + usize::from(!file.content.is_empty());
    if file.token_estimate() <= budget && rendered_bytes <= max_bytes {
        return reconstruct_file_diff(file);
    }
    let mut clone = file.clone();
    clone.truncate(max_bytes.saturating_sub(1).max(1));
    reconstruct_file_diff(&clone)
}

fn reconstruct_file_diff(file: &ConventionalFileDiff) -> String {
    if file.content.is_empty() {
        file.header.clone()
    } else {
        format!("{}\n{}", file.header, file.content)
    }
}

fn fallback_file_observation(file: &ConventionalFileDiff) -> ObservedFile {
    ObservedFile {
        file: file.filename.clone(),
        observations: vec![fallback_observation_text(&file.filename)],
        additions: file.additions,
        deletions: file.deletions,
        status: file.status,
    }
}

fn fallback_observation_text(filename: &str) -> String {
    format!("Updated {}.", base_name(filename))
}

/// Basename of a path, falling back to the whole value.
fn base_name(value: &str) -> &str {
    Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(value)
}

fn path_suffix_matches(left: &str, right: &str) -> bool {
    path_has_suffix(left, right) || path_has_suffix(right, left)
}

fn path_has_suffix(value: &str, suffix: &str) -> bool {
    value == suffix
        || value.ends_with(&format!("/{suffix}"))
        || value.ends_with(&format!("\\{suffix}"))
}

/// Per-file context lines describing the other files in the same commit.
struct ContextHeaders {
    large_commit_header: Option<String>,
    files: Vec<(String, usize, &'static str)>,
}

impl ContextHeaders {
    fn new(files: &[ConventionalFileDiff]) -> Self {
        let large_commit_header =
            (files.len() > 100).then(|| format!("(Large commit with {} total files)", files.len()));
        let entries = if large_commit_header.is_some() {
            Vec::new()
        } else {
            files
                .iter()
                .map(|file| {
                    (
                        file.filename.clone(),
                        file.additions + file.deletions,
                        infer_file_description(&file.filename, &file.content),
                    )
                })
                .collect()
        };
        Self {
            large_commit_header,
            files: entries,
        }
    }

    fn header_for_files(&self, current_files: &[&str]) -> String {
        if let Some(header) = &self.large_commit_header {
            return header.clone();
        }
        let others: Vec<&(String, usize, &'static str)> = self
            .files
            .iter()
            .filter(|(name, _, _)| !current_files.contains(&name.as_str()))
            .collect();
        if others.is_empty() {
            return String::new();
        }
        let mut shown = others.clone();
        shown.sort_by(|left, right| right.1.cmp(&left.1));
        shown.truncate(MAX_CONTEXT_FILES);
        let mut lines = vec!["OTHER FILES IN THIS CHANGE:".to_owned()];
        lines.extend(
            shown
                .iter()
                .map(|(file, size, description)| format!("- {file} ({size} lines): {description}")),
        );
        if shown.len() < others.len() {
            lines.push(format!("... and {} more files", others.len() - shown.len()));
        }
        lines.join("\n")
    }
}

fn infer_file_description(filename: &str, content: &str) -> &'static str {
    let lower = filename.to_lowercase();
    let suffix = Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{}", ext.to_lowercase()))
        .unwrap_or_default();
    if lower.contains("test") {
        return "test file";
    }
    if lower.contains("prompt") || lower.contains("system") {
        return "prompt template";
    }
    if suffix == ".md" {
        return "documentation";
    }
    if lower.contains("config") || [".toml", ".yaml", ".yml"].contains(&suffix.as_str()) {
        return "configuration";
    }
    if lower.contains("error") {
        return "error definitions";
    }
    if lower.contains("type") {
        return "type definitions";
    }
    if lower.ends_with("mod.rs") || lower.ends_with("lib.rs") {
        return "module exports";
    }
    if lower.ends_with("main.rs") || lower.ends_with("main.go") || lower.ends_with("main.py") {
        return "entry point";
    }
    if content.contains("class ") || content.contains("def ") || content.contains("fn ") {
        return "implementation";
    }
    if content.contains("struct ") || content.contains("enum ") {
        return "type definitions";
    }
    if content.contains("async ") || content.contains("await") {
        return "async code";
    }
    "source code"
}

/// Apply `worker` to every item with at most `limit` concurrent workers; results keep item order.
fn map_with_concurrency<T: Sync, R: Send>(
    items: &[T],
    limit: usize,
    worker: impl Fn(&T, usize) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<R>>> =
        Mutex::new(std::iter::repeat_with(|| None).take(items.len()).collect());
    std::thread::scope(|scope| {
        for _ in 0..limit.min(items.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let value = worker(item, index);
                    slots.lock()[index] = Some(value);
                }
            });
        }
    });
    slots
        .into_inner()
        .into_iter()
        .map(|slot| slot.expect("every worker slot is filled before the scope ends"))
        .collect()
}
