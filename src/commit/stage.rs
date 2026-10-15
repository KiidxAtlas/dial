//! Staged conventional-commit generation.
//!
//! Flow: whitespace-only shortcut, then either the fast single-prompt path (with analysis
//! fallback) or the standard analysis path (direct or map-reduce). Summaries are generated
//! and salvaged from the analysis, then the final message is validated with up to three repair
//! attempts.

use std::path::Path;

use super::commit_types::{
    CommitMessage, ConventionalAnalysis, DEFAULT_COMMIT_TYPE, format_conventional_commit,
    types_description,
};
use super::config::{GenerationConfig, MAX_FILE_BYTES};
use super::diff::{
    condense_stat, is_all_whitespace_diff, parse_prompt_diff, scrub_diff_for_prompt,
    smart_truncate_diff, strip_whitespace_only_files, truncate_diff_by_lines,
};
use super::inference::{Inference, Tier};
use super::map_reduce::{run_map_reduce, should_use_map_reduce};
use super::markdown::{
    fallback_summary, parse_conventional_analysis_markdown, parse_fast_commit_markdown,
    parse_summary_markdown, strip_type_prefix,
};
use super::normalization::post_process_commit_message;
use super::prompts::{PromptContext, PromptFamily, render_conventional_prompt};
use super::scope::{count_changed_lines, extract_scope_candidates};
use super::text::{cp_len, first_word, take_chars};
use super::validation::{
    is_past_tense_first_word, repair_summary_tense, validate_commit_message,
    validate_summary_quality,
};

/// Diff budget, in lines, for the single fast-path prompt.
const FAST_DIFF_MAX_LINES: usize = 10_000;
/// Smallest summary budget handed to the summary prompt.
const MIN_SUMMARY_CHARS: usize = 20;
/// Files shown in the stat block of the summary prompt.
const SUMMARY_STAT_FILES: usize = 30;
/// Validation attempts before the last message is returned with its error.
const VALIDATION_ATTEMPTS: usize = 3;

/// Repository context supplied to analysis and final project-scope validation.
#[derive(Debug, Clone, Default)]
pub(crate) struct GenerationContext {
    pub user_context: Option<String>,
    pub recent_commits: Option<String>,
    pub common_scopes: Option<String>,
    pub project_context: Option<String>,
    pub project_names: Vec<String>,
}

/// Git inputs captured from one immutable staged-tree view.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GenerationInput<'a> {
    pub diff: &'a str,
    pub stat: &'a str,
    pub numstat: &'a str,
    pub context: &'a GenerationContext,
}

/// Generated commit plus any final validation failure left for manual correction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationResult {
    pub commit: CommitMessage,
    pub validation_error: Option<String>,
}

/// Run the full generation algorithm over captured Git inputs.
pub(crate) fn generate_conventional_commit(
    inference: &Inference<'_>,
    config: &GenerationConfig,
    input: GenerationInput<'_>,
) -> Result<GenerationResult, String> {
    let commit = if is_all_whitespace_diff(input.diff) {
        let files = parse_prompt_diff(input.diff);
        let summary = match files.as_slice() {
            [only] => format!("reformatted {}", base_name(&only.filename)),
            _ => format!("reformatted {} files", files.len()),
        };
        post_process_commit_message(style_commit(summary), config)?
    } else if count_auto_fast_lines(input.numstat, config).is_some() {
        generate_fast_workflow(inference, config, &input)?
    } else {
        generate_standard_workflow(inference, config, &input)?
    };
    let detail_points = commit.body.clone();
    validate_and_process(commit, &detail_points, inference, config, &input)
}

fn generate_fast_workflow(
    inference: &Inference<'_>,
    config: &GenerationConfig,
    input: &GenerationInput<'_>,
) -> Result<CommitMessage, String> {
    let stripped = strip_whitespace_only_files(input.diff).unwrap_or_else(|| input.diff.to_owned());
    let scrubbed = scrub_diff_for_prompt(&stripped, MAX_FILE_BYTES);
    let diff = truncate_diff_by_lines(&scrubbed, FAST_DIFF_MAX_LINES);
    let scope_candidates = extract_scope_candidates(input.numstat, config).scope_candidates;
    let user_context = input.context.user_context.as_deref();
    if let Ok(commit) = generate_fast_commit(
        inference,
        config,
        input.stat,
        &diff,
        &scope_candidates,
        user_context,
    ) {
        if validate_commit_message(&commit, config, input.stat, &[]).ok() {
            return Ok(commit);
        }
    }
    let analysis = analysis_with_map_reduce(
        inference,
        config,
        input.stat,
        &diff,
        &scope_candidates,
        input.context,
    )?;
    message_from_analysis(analysis, config, input.stat, user_context, inference)
}

fn generate_standard_workflow(
    inference: &Inference<'_>,
    config: &GenerationConfig,
    input: &GenerationInput<'_>,
) -> Result<CommitMessage, String> {
    let stripped = strip_whitespace_only_files(input.diff).unwrap_or_else(|| input.diff.to_owned());
    let diff = scrub_diff_for_prompt(&stripped, MAX_FILE_BYTES);
    let scope_candidates = extract_scope_candidates(input.numstat, config).scope_candidates;
    let map_reduce = should_use_map_reduce(&diff, config);
    let analysis_diff = if map_reduce || cp_len(&diff) <= config.max_diff_length {
        diff
    } else {
        smart_truncate_diff(&diff, config.max_diff_length, config)
    };
    let analysis = if map_reduce {
        run_map_reduce(
            inference,
            config,
            input.stat,
            &analysis_diff,
            &scope_candidates,
            input.context.user_context.as_deref(),
        )?
    } else {
        generate_direct_analysis(
            inference,
            input.stat,
            &analysis_diff,
            &scope_candidates,
            input.context,
        )?
    };
    message_from_analysis(
        analysis,
        config,
        input.stat,
        input.context.user_context.as_deref(),
        inference,
    )
}

fn analysis_with_map_reduce(
    inference: &Inference<'_>,
    config: &GenerationConfig,
    stat: &str,
    diff: &str,
    scope_candidates: &str,
    context: &GenerationContext,
) -> Result<ConventionalAnalysis, String> {
    if should_use_map_reduce(diff, config) {
        return run_map_reduce(
            inference,
            config,
            stat,
            diff,
            scope_candidates,
            context.user_context.as_deref(),
        );
    }
    generate_direct_analysis(inference, stat, diff, scope_candidates, context)
}

fn generate_direct_analysis(
    inference: &Inference<'_>,
    stat: &str,
    diff: &str,
    scope_candidates: &str,
    context: &GenerationContext,
) -> Result<ConventionalAnalysis, String> {
    let prompt = render_conventional_prompt(
        PromptFamily::Analysis,
        &PromptContext::default()
            .with_text(
                "project_context",
                context.project_context.as_deref().unwrap_or(""),
            )
            .with_text("types_description", types_description())
            .with_text("stat", stat)
            .with_text("scope_candidates", scope_candidates)
            .with_text(
                "common_scopes",
                context.common_scopes.as_deref().unwrap_or(""),
            )
            .with_text(
                "recent_commits",
                context.recent_commits.as_deref().unwrap_or(""),
            )
            .with_text("diff", diff),
    );
    let user = match context
        .user_context
        .as_deref()
        .filter(|value| !value.is_empty())
    {
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

fn generate_fast_commit(
    inference: &Inference<'_>,
    config: &GenerationConfig,
    stat: &str,
    diff: &str,
    scope_candidates: &str,
    user_context: Option<&str>,
) -> Result<CommitMessage, String> {
    let prompt = render_conventional_prompt(
        PromptFamily::Fast,
        &PromptContext::default()
            .with_text("stat", stat)
            .with_text("diff", diff)
            .with_text("scope_candidates", scope_candidates)
            .with_text("user_context", user_context.unwrap_or(""))
            .with_text("types_description", types_description()),
    );
    let commit = inference.complete(Tier::Fast, &prompt.system, &prompt.user, |reply| {
        parse_fast_commit_markdown(&reply.text, DEFAULT_COMMIT_TYPE)
    })?;
    post_process_commit_message(commit, config)
}

/// Build the final message from an analysis, generating and salvaging its summary.
fn message_from_analysis(
    analysis: ConventionalAnalysis,
    config: &GenerationConfig,
    stat: &str,
    user_context: Option<&str>,
    inference: &Inference<'_>,
) -> Result<CommitMessage, String> {
    let mut summary = analysis.summary.clone().unwrap_or_default();
    for attempt in 0..config.max_retries.max(1) {
        if summary.is_empty() || attempt > 0 {
            summary =
                generate_summary_from_analysis(inference, config, &analysis, stat, user_context);
        }
        if validate_summary_quality(&summary, &analysis.commit_type, stat).ok() {
            break;
        }
        summary.clear();
    }
    if summary.is_empty() {
        summary = fallback_summary(
            stat,
            &analysis.details,
            "",
            Some(config.summary_guideline),
            &analysis.commit_type,
        );
    }
    post_process_commit_message(
        CommitMessage {
            commit_type: analysis.commit_type,
            scope: analysis.scope,
            summary,
            body: analysis.details,
            footers: Vec::new(),
        },
        config,
    )
}

/// Generate and salvage a compliant summary from one holistic analysis. Infallible: model
/// failures fall back to the analysis summary, then to a deterministic summary.
fn generate_summary_from_analysis(
    inference: &Inference<'_>,
    config: &GenerationConfig,
    analysis: &ConventionalAnalysis,
    stat: &str,
    user_context: Option<&str>,
) -> String {
    let prefix_length = cp_len(&analysis.commit_type)
        + 2
        + analysis
            .scope
            .as_deref()
            .map_or(0, |scope| cp_len(scope) + 2);
    let chars = config
        .summary_guideline
        .saturating_sub(prefix_length)
        .max(MIN_SUMMARY_CHARS);
    let details = if analysis.details.is_empty() {
        format!("- {}", analysis.summary.as_deref().unwrap_or(""))
    } else {
        analysis
            .details
            .iter()
            .map(|detail| format!("- {detail}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let prompt = render_conventional_prompt(
        PromptFamily::Summary,
        &PromptContext::default()
            .with_text("commit_type", analysis.commit_type.as_str())
            .with_text("scope", analysis.scope.as_deref().unwrap_or(""))
            .with_text("chars", chars.to_string())
            .with_text("user_context", user_context.unwrap_or(""))
            .with_text("details", details)
            .with_text("stat", condense_stat(stat, SUMMARY_STAT_FILES)),
    );
    let generated = inference
        .complete(Tier::Primary, &prompt.system, &prompt.user, |reply| {
            parse_summary_markdown(&reply.text)
        })
        .unwrap_or_default();

    let mut chosen = String::new();
    let mut draft = String::new();
    let mut rejection = String::new();
    let analysis_summary = analysis.summary.as_deref().unwrap_or("");
    for raw in [generated.as_str(), analysis_summary] {
        let candidate = strip_type_prefix(raw, None, None).trim().to_owned();
        if candidate.is_empty() {
            continue;
        }
        match accept_summary(&candidate, &analysis.commit_type, stat) {
            Ok(summary) => {
                chosen = summary;
                break;
            }
            Err(reason) => {
                if draft.is_empty() {
                    draft = candidate;
                    rejection = reason;
                }
            }
        }
    }
    if chosen.is_empty() && !draft.is_empty() {
        chosen = rewrite_summary_for_compliance(
            inference,
            &draft,
            &rejection,
            &analysis.commit_type,
            chars,
            stat,
        );
    }
    if chosen.is_empty() {
        chosen = fallback_summary_for_commit(
            stat,
            &analysis.details,
            &analysis.commit_type,
            config.summary_hard_limit,
        );
    }
    take_chars(&chosen, config.summary_hard_limit)
        .trim_end_matches([' ', '.'])
        .to_owned()
}

/// Accept a candidate as-is, or after past-tense repair. `Err` carries the rejection reason.
fn accept_summary(candidate: &str, commit_type: &str, stat: &str) -> Result<String, String> {
    let report = validate_summary_quality(candidate, commit_type, stat);
    if report.ok() {
        return Ok(candidate.to_owned());
    }
    let repaired = repair_summary_tense(candidate);
    if validate_summary_quality(&repaired, commit_type, stat).ok() {
        return Ok(repaired);
    }
    Err(report.error_messages())
}

/// Ask the model to rewrite a rejected draft. Empty when the rewrite fails or is still rejected.
fn rewrite_summary_for_compliance(
    inference: &Inference<'_>,
    draft: &str,
    rejection: &str,
    commit_type: &str,
    chars: usize,
    stat: &str,
) -> String {
    let prompt = render_conventional_prompt(
        PromptFamily::SummaryRewrite,
        &PromptContext::default()
            .with_text("commit_type", commit_type)
            .with_text("chars", chars.to_string())
            .with_text("draft", draft)
            .with_text("rejection", rejection),
    );
    inference
        .complete(Tier::Primary, &prompt.system, &prompt.user, |reply| {
            parse_summary_markdown(&reply.text)
                .map(|summary| strip_type_prefix(&summary, None, None).trim().to_owned())
        })
        .ok()
        .and_then(|summary| accept_summary(&summary, commit_type, stat).ok())
        .unwrap_or_default()
}

/// Deterministic past-tense summary built from the details when the model's summary is unusable.
fn fallback_summary_for_commit(
    stat: &str,
    details: &[String],
    commit_type: &str,
    limit: usize,
) -> String {
    let candidate = fallback_summary(stat, details, "", Some(limit), commit_type);
    if validate_summary_quality(&candidate, commit_type, stat).ok() {
        return candidate;
    }
    let first_detail = details
        .first()
        .map(|detail| detail.trim().trim_end_matches('.').to_owned())
        .unwrap_or_default();
    let mut cleaned = if first_detail.is_empty() {
        strip_type_prefix(&candidate, None, None).trim().to_owned()
    } else {
        first_detail
    };
    for variant in [
        commit_type.to_owned(),
        format!("{commit_type}ed"),
        format!("{commit_type}d"),
    ] {
        if cleaned
            .to_lowercase()
            .starts_with(&format!("{} ", variant.to_lowercase()))
        {
            cleaned = cleaned.get(variant.len()..).unwrap_or("").trim().to_owned();
            break;
        }
    }
    let verb = match commit_type {
        "feat" => "added",
        "fix" => "fixed",
        "refactor" => "restructured",
        "docs" => "documented",
        "test" => "tested",
        "perf" => "optimized",
        "build" | "ci" | "chore" => "updated",
        "style" => "formatted",
        "revert" => "reverted",
        _ => "changed",
    };
    let word = first_word(&cleaned);
    let prefixed = if !word.is_empty() && is_past_tense_first_word(word) {
        cleaned.clone()
    } else {
        format!(
            "{verb} {}",
            if cleaned.is_empty() {
                "files"
            } else {
                cleaned.as_str()
            }
        )
    };
    if cp_len(&prefixed) <= limit {
        return prefixed;
    }
    fallback_summary("", details, "", Some(limit), commit_type)
}

/// Validate the message, repairing the summary and scope up to `VALIDATION_ATTEMPTS` times.
fn validate_and_process(
    message: CommitMessage,
    detail_points: &[String],
    inference: &Inference<'_>,
    config: &GenerationConfig,
    input: &GenerationInput<'_>,
) -> Result<GenerationResult, String> {
    let user_context = input.context.user_context.as_deref();
    let project_names = input.context.project_names.as_slice();
    let mut current = message;
    let mut validation_error: Option<String> = None;
    for attempt in 0..VALIDATION_ATTEMPTS {
        current = post_process_commit_message(current, config)?;
        if attempt == 0 && first_line_len(&current) > config.summary_soft_limit {
            let analysis = ConventionalAnalysis {
                commit_type: current.commit_type.clone(),
                scope: current.scope.clone(),
                summary: None,
                details: detail_points.to_vec(),
                issue_refs: Vec::new(),
            };
            let summary = generate_summary_from_analysis(
                inference,
                config,
                &analysis,
                input.stat,
                user_context,
            );
            current = post_process_commit_message(CommitMessage { summary, ..current }, config)?;
            continue;
        }
        let mut report = validate_commit_message(&current, config, input.stat, project_names);
        if report.ok() {
            return Ok(GenerationResult {
                commit: current,
                validation_error: None,
            });
        }
        if current.scope.is_some() && report.has_error("project_name_scope") {
            current = post_process_commit_message(
                CommitMessage {
                    scope: None,
                    ..current
                },
                config,
            )?;
            report = validate_commit_message(&current, config, input.stat, project_names);
            if report.ok() {
                return Ok(GenerationResult {
                    commit: current,
                    validation_error: None,
                });
            }
        }
        validation_error = Some(report.error_messages());
        if attempt + 1 < VALIDATION_ATTEMPTS {
            let summary = fallback_summary(
                input.stat,
                detail_points,
                "",
                Some(config.summary_guideline),
                &current.commit_type,
            );
            current = post_process_commit_message(CommitMessage { summary, ..current }, config)?;
        }
    }
    Ok(GenerationResult {
        commit: current,
        validation_error,
    })
}

/// Changed-line count when the change is small enough for the fast path; `None` otherwise.
fn count_auto_fast_lines(numstat: &str, config: &GenerationConfig) -> Option<usize> {
    if config.auto_fast_threshold_lines == 0 {
        return None;
    }
    let changed = count_changed_lines(numstat);
    (changed != 0 && changed <= config.auto_fast_threshold_lines).then_some(changed)
}

fn first_line_len(message: &CommitMessage) -> usize {
    format_conventional_commit(message)
        .split('\n')
        .next()
        .map_or(0, cp_len)
}

fn style_commit(summary: String) -> CommitMessage {
    CommitMessage {
        commit_type: "style".to_owned(),
        scope: None,
        summary,
        body: Vec::new(),
        footers: Vec::new(),
    }
}

fn base_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}
