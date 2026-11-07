//! Incremental summarization for portable compaction methods: the previous summary is updated
//! rather than re-summarized, a split turn keeps its own prefix summary, read/modified file lists
//! carry across compactions, and summarizer input is planned in message-boundary windows that fit
//! the compaction model (halving and re-planning when the provider rejects a window as too long).
use super::execution::RequestCompletion;
use crate::agent::Ui;
use crate::agent::compact::compaction::{
    CompactionContext, CompactionControlDenied, MAX_SUMMARY_TOKENS, RemoteCompactionObserver,
    SummaryWorker, estimate, interrupted, public_transcript, summary_instructions, summary_output,
    with_ui,
};
use crate::agent::prompts::compaction::{
    BRANCH_PREAMBLE, BRANCH_PROMPT, FILE_TARGETS_NOTE, HANDOFF_UPDATE, SYSTEM_PROMPT,
    TURN_PREFIX_PROMPT, UPDATE_PROMPT,
};
use crate::config::compaction::{CompactionConfig, CompactionMethod};
use crate::storage::transcript::is_history;
use crate::{
    llm::{Chat, provider::Backend},
    storage::session::Session,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::{BTreeSet, VecDeque},
    ops::Range,
    sync::{Arc, atomic::AtomicBool},
};

/// Fallback when the compaction model's window is unknown.
const DEFAULT_SUMMARY_INPUT_WINDOW: u64 = 200_000;
/// Floor for one summarization window, scaled down for small models (never below 1k).
const MIN_SUMMARY_INPUT_TOKENS: u64 = 16_384;
/// `Agent::compaction_evidence_notes` appends harness notes after this marker; they are regenerated
/// on every commit, so an update summary must not fold them back in.
const EVIDENCE_NOTES_MARKER: &str = "\n\nOriginal user goal: ";
const FILE_LIST_LIMIT: usize = 16;
const FILE_LIST_BYTES: usize = 4_096;
/// Hard bounds on one fold: model attempts (retries included) and oversized-text fragments. Exceeding
/// either fails before anything is committed; the summary never pretends to cover what it did not send.
const MAX_SUMMARY_CALLS: usize = 64;
const MAX_SUMMARY_FRAGMENTS: usize = 256;
/// Bytes reserved for a fragment label, so a labelled fragment still fits its token budget.
const FRAGMENT_LABEL_BYTES: usize = 96;
// Keep legacy tag names for stored-summary and branch consumers; the lists are not result evidence.
const READ_FILES: &str = "read-files";
const LEGACY_FILE_TARGETS_NOTE: &str = "File-operation lists below are attempted targets observed in tool-call arguments, not evidence of successful reads or modifications. Calls may have failed, been denied, or never executed; consult retained results for outcomes.";
const MODIFIED_FILES: &str = "modified-files";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Prompt {
    Main,
    TurnPrefix,
    Branch,
}

/// The model-written part of a committed compaction summary message: harness notes and file lists removed.
fn previous_summary(message: &Value) -> Option<(&str, FileOps)> {
    let text = Session::compaction_summary_text(message)?;
    let text = text
        .split_once(EVIDENCE_NOTES_MARKER)
        .map_or(text, |(summary, _)| summary);
    // Current notes no longer duplicate goals/state/todos from the exact machine carry.
    // Keep the old marker above for historical summaries.
    let text = text
        .split_once("\n\nRetained evidence (read and mutation results): ")
        .map_or(text, |(summary, _)| summary);
    Some(FileOps::take_from(text))
}

/// A human-authored user message that starts a turn (not a harness reminder, summary or rollover).
fn is_turn_start(message: &Value) -> bool {
    message["role"] == "user"
        && is_history(message)
        && [
            "dial_reminder",
            "dial_exploration_reminder",
            "dial_compaction_critical",
            "dial_generated_user",
            "dial_compaction_summary",
            "dial_compaction_notes",
            "dial_compaction_files",
            "dial_branch_summary",
            "dial_context_rollover",
            "dial_continuation",
        ]
        .iter()
        .all(|key| {
            message
                .get(*key)
                .is_none_or(|value| value.is_null() || *value == false)
        })
        && message["dial_snapcompact"].is_null()
        && Session::compaction_summary_text(message).is_none()
}

/// Files the current turn attempted to modify, newest first: write/edit targets in tool calls
/// since the latest human turn start (or since the history's start when an earlier compaction
/// already folded that turn start). Attempted targets, never confirmed outcomes.
pub(in crate::agent) fn edited_this_turn(messages: &[Value]) -> Vec<String> {
    let start = messages.iter().rposition(is_turn_start).unwrap_or(0);
    let mut ops = FileOps::default();
    for message in &messages[start..] {
        ops.observe(message);
    }
    ops.recent
        .iter()
        .rev()
        .filter(|path| ops.modified.contains(*path))
        .cloned()
        .collect()
}

/// Index into `source` where the turn containing the cut starts, when the cut lands mid-turn.
fn turn_split(messages: &[Value], source: &[usize], keep: usize) -> Option<usize> {
    let kept = messages.get(keep)?;
    if !is_history(kept) || is_turn_start(kept) {
        return None;
    }
    source
        .iter()
        .rposition(|&index| is_turn_start(&messages[index]))
}

fn transcript_part(message: &Value) -> Result<String> {
    if let Some(text) = message["dial_snapcompact"]["source_text"].as_str() {
        return Ok(text.to_owned());
    }
    if let Some(history) = message["dial_anthropic_compaction"]["history"].as_array() {
        let mut out = String::new();
        for item in history {
            out.push_str(&public_transcript(item)?);
            out.push('\n');
        }
        return Ok(out);
    }
    public_transcript(message)
}

/// Conservative local estimate for summarizer text: code-dense JSON transcripts tokenize near 3 bytes/token.
fn text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(3)
}

fn min_input_tokens(window: u64) -> u64 {
    MIN_SUMMARY_INPUT_TOKENS.min((window / 8).max(1_024))
}

/// Usable conversation input for one summarization call: the window is discounted to 80% because the
/// local estimate disagrees with provider tokenizers, then the emitted and carried summaries come off.
fn input_budget(window: u64, output: u32) -> u64 {
    (window.saturating_mul(8) / 10)
        .saturating_sub(u64::from(output) + u64::from(MAX_SUMMARY_TOKENS))
        .max(min_input_tokens(window))
}

/// Partition `range` into windows of at most `budget` tokens, splitting only on message boundaries.
fn plan(sizes: &[u64], range: Range<usize>, budget: u64) -> Vec<Range<usize>> {
    let mut windows = Vec::new();
    let mut start = range.start;
    let mut current = 0u64;
    for index in range.clone() {
        if current > 0 && current.saturating_add(sizes[index]) > budget {
            windows.push(start..index);
            start = index;
            current = 0;
        }
        current = current.saturating_add(sizes[index]);
    }
    if start < range.end {
        windows.push(start..range.end);
    }
    windows
}

/// One summarizer window: whole message texts, or one labelled fragment of an oversized text.
struct Window {
    units: Vec<String>,
    budget: u64,
}

/// Split one oversized text into ordered, labelled fragments that each fit `budget` tokens. A cut
/// lands on a line break in the window's second half when one exists, otherwise on a UTF-8 boundary,
/// so every byte reaches exactly one fragment.
fn fragments(text: &str, budget: u64) -> Result<Vec<String>> {
    let room = usize::try_from(budget.saturating_mul(3))
        .unwrap_or(usize::MAX)
        .saturating_sub(FRAGMENT_LABEL_BYTES)
        .max(4);
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        ensure!(
            pieces.len() < MAX_SUMMARY_FRAGMENTS,
            "transcript needs more than {MAX_SUMMARY_FRAGMENTS} summary fragments; no summary was committed"
        );
        if rest.len() <= room {
            pieces.push(rest);
            break;
        }
        let end = rest.floor_char_boundary(room);
        let cut = rest[..end]
            .rfind('\n')
            .filter(|&at| at >= end / 2)
            .map_or(end, |at| at + 1);
        pieces.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    let count = pieces.len();
    Ok(pieces
        .into_iter()
        .enumerate()
        .map(|(index, piece)| format!("[fragment {} of {count}]\n{piece}", index + 1))
        .collect())
}

/// Queue `units` at `budget`, in order. A lone text that cannot fit alone is split into labelled
/// fragments instead of being truncated; fragments then travel through the same fold as any window.
fn replan(
    pending: &mut VecDeque<Window>,
    units: Vec<String>,
    budget: u64,
    made: &mut usize,
) -> Result<()> {
    let mut next = Vec::new();
    if let [only] = units.as_slice()
        && text_tokens(only) > budget
    {
        let pieces = fragments(only, budget)?;
        *made = made.saturating_add(pieces.len());
        ensure!(
            *made <= MAX_SUMMARY_FRAGMENTS,
            "transcript needs more than {MAX_SUMMARY_FRAGMENTS} summary fragments; no summary was committed"
        );
        next.extend(pieces.into_iter().map(|piece| Window {
            units: vec![piece],
            budget,
        }));
    } else {
        let sizes: Vec<u64> = units
            .iter()
            .map(|unit| text_tokens(unit).saturating_add(1))
            .collect();
        for range in plan(&sizes, 0..units.len(), budget) {
            next.push(Window {
                units: units[range].to_vec(),
                budget,
            });
        }
    }
    for window in next.into_iter().rev() {
        pending.push_front(window);
    }
    Ok(())
}

fn escape_boundaries(text: &str) -> Cow<'_, str> {
    if !text.contains("</conversation>") && !text.contains("</previous-summary>") {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.replace("</conversation>", "<\\/conversation>")
            .replace("</previous-summary>", "<\\/previous-summary>"),
    )
}

fn prompt_text(prompt: Prompt, method: CompactionMethod, focus: &str, updating: bool) -> String {
    let instructions = match prompt {
        Prompt::TurnPrefix => TURN_PREFIX_PROMPT.to_owned(),
        Prompt::Branch if focus.is_empty() => BRANCH_PROMPT.to_owned(),
        Prompt::Branch => {
            format!("{BRANCH_PROMPT}\n\nAdditional summary focus from the operator:\n{focus}")
        }
        Prompt::Main if !updating => summary_instructions(method, focus),
        Prompt::Main if method == CompactionMethod::Handoff => format!(
            "{}\n\n{HANDOFF_UPDATE}",
            summary_instructions(method, focus)
        ),
        Prompt::Main if focus.is_empty() => UPDATE_PROMPT.to_owned(),
        Prompt::Main => {
            format!("{UPDATE_PROMPT}\n\nAdditional summary focus from the operator:\n{focus}")
        }
    };
    if updating && prompt != Prompt::Main {
        format!("{instructions}\n\n{HANDOFF_UPDATE}")
    } else {
        instructions
    }
}

fn summary_request(
    conversation: &str,
    previous: Option<&str>,
    prompt: &str,
    output: u32,
    context: &CompactionContext<'_>,
) -> Chat {
    let mut text = format!(
        "<conversation>\n{}\n</conversation>\n\n",
        escape_boundaries(conversation)
    );
    if let Some(previous) = previous {
        text.push_str(&format!(
            "<previous-summary>\n{}\n</previous-summary>\n\n",
            escape_boundaries(previous)
        ));
    }
    text.push_str(prompt);
    let mut request = Chat {
        messages: vec![
            json!({"role":"system","content":SYSTEM_PROMPT}),
            json!({"role":"user","content":text}),
        ],
        tools: Vec::new(),
        temperature: 0.2,
        max_tokens: output,
        seed: None,
    };
    if let Some(note) = context.supplemental_message() {
        request.messages.insert(1, note);
    }
    request
}

fn request_estimate(request: &Chat) -> u64 {
    request
        .messages
        .iter()
        .map(|message| text_tokens(message["content"].as_str().unwrap_or("")))
        .sum::<u64>()
        + 32
}

/// Attempted read/modification targets from call arguments, not confirmed tool outcomes.
#[derive(Default, Debug, PartialEq, Eq)]
struct FileOps {
    read: BTreeSet<String>,
    modified: BTreeSet<String>,
    /// Oldest to newest, so bounded inventories favor recent targets rather than lexical order.
    recent: Vec<String>,
}

/// `read` `paths` entries may carry a line selector (`src/a.rs:1-80`); observe the file itself.
fn strip_selector(target: &str) -> &str {
    match target.rsplit_once(':') {
        Some((path, selector))
            if !selector.is_empty()
                && selector
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b',')) =>
        {
            path
        }
        _ => target,
    }
}

impl FileOps {
    fn observe(&mut self, message: &Value) {
        if message["role"] != "assistant" {
            return;
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            let arguments = match &call["function"]["arguments"] {
                Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
                other => other.clone(),
            };
            match call["function"]["name"].as_str().unwrap_or("") {
                "read" => match arguments["paths"].as_array() {
                    Some(paths) => {
                        for path in paths {
                            self.add(false, path.as_str().map(strip_selector));
                        }
                    }
                    None => self.add(false, arguments["path"].as_str()),
                },
                "write" => self.add(true, arguments["path"].as_str()),
                "edit" => match crate::tools::declared_edit_targets(&arguments) {
                    Ok(paths) if !paths.is_empty() => {
                        for path in &paths {
                            self.add(true, Some(path.as_str()));
                        }
                    }
                    _ => self.add(true, arguments["path"].as_str()),
                },
                // Non-preview ast_edit calls attempt to modify the supplied file or directory;
                // neither execution success nor directory contents are inferred here.
                "ast_edit" if arguments["dry_run"] == false => {
                    self.add(true, arguments["path"].as_str());
                }
                // rename_file attempts a move unless apply:false previews it. Server edits may
                // target other files not named in the arguments; those are not enumerated here.
                "lsp" if arguments["action"] == "rename_file" => {
                    if arguments["apply"] != false {
                        self.add(true, arguments["path"].as_str());
                        self.add(true, arguments["new_name"].as_str());
                    } else {
                        self.add(false, arguments["path"].as_str());
                    }
                }
                // Other LSP actions read their file. Rename and code-action edits can touch files the
                // arguments do not name, so they are not enumerated as modified.
                "lsp" => self.add(false, arguments["path"].as_str()),
                _ => {}
            }
        }
    }

    fn add(&mut self, modified: bool, path: Option<&str>) {
        let Some(path) = path.map(str::trim).filter(|path| {
            !path.is_empty()
                && !path.contains("://")
                && !path.contains('\n')
                && !path.starts_with('[')
        }) else {
            return;
        };
        if modified {
            self.modified.insert(path.to_owned());
        } else {
            self.read.insert(path.to_owned());
        }
        self.recent.retain(|old| old != path);
        self.recent.push(path.to_owned());
    }

    fn merge(&mut self, other: FileOps) {
        for path in other.recent {
            self.add(other.modified.contains(&path), Some(&path));
        }
    }

    /// Split a summary into its text and the file lists this module appended to it.
    fn take_from(summary: &str) -> (&str, FileOps) {
        let start = [
            READ_FILES,
            MODIFIED_FILES,
            FILE_TARGETS_NOTE,
            LEGACY_FILE_TARGETS_NOTE,
        ]
        .iter()
        .filter_map(|tag| {
            if matches!(*tag, FILE_TARGETS_NOTE | LEGACY_FILE_TARGETS_NOTE) {
                summary.find(&format!("\n\n{tag}"))
            } else {
                summary.find(&format!("\n<{tag}>\n"))
            }
        })
        .min();
        let Some(start) = start else {
            return (summary.trim(), FileOps::default());
        };
        let mut ops = FileOps::default();
        let tail = &summary[start..];
        for (tag, modified) in [(READ_FILES, false), (MODIFIED_FILES, true)] {
            let open = format!("<{tag}>\n");
            let close = format!("\n</{tag}>");
            let Some(body) = tail
                .find(&open)
                .map(|at| &tail[at + open.len()..])
                .and_then(|body| body.find(&close).map(|end| &body[..end]))
            else {
                continue;
            };
            for line in body.lines().rev() {
                ops.add(modified, Some(line));
            }
        }
        (summary[..start].trim(), ops)
    }

    fn upsert(&self, summary: &str) -> String {
        let mut out = FileOps::take_from(summary).0.to_owned();
        let read: Vec<&String> = self
            .recent
            .iter()
            .rev()
            .filter(|path| self.read.contains(*path) && !self.modified.contains(*path))
            .collect();
        let modified: Vec<&String> = self
            .recent
            .iter()
            .rev()
            .filter(|path| self.modified.contains(*path))
            .collect();
        if !read.is_empty() || !modified.is_empty() {
            out.push_str(&format!("\n\n{FILE_TARGETS_NOTE}"));
        }
        let mut remaining = FILE_LIST_BYTES;
        for (tag, list) in [(MODIFIED_FILES, modified), (READ_FILES, read)] {
            if list.is_empty() {
                continue;
            }
            out.push_str(&format!("\n\n<{tag}>\n"));
            let mut shown = 0;
            for path in &list {
                if shown == FILE_LIST_LIMIT || path.len() + 1 > remaining {
                    continue;
                }
                remaining -= path.len() + 1;
                shown += 1;
                out.push_str(path);
                out.push('\n');
            }
            if list.len() > shown {
                out.push_str(&format!(
                    "[... {} older or oversized targets omitted; full calls remain in session history]\n",
                    list.len() - shown
                ));
            }
            out.push_str(&format!("</{tag}>"));
        }
        out
    }
}

/// Summarize `messages[..keep]` for a portable replacement prefix. A leading committed summary is
/// updated (never re-summarized from its own JSON), and a cut inside a turn gets a separate
/// turn-prefix summary so the turn's original request survives verbatim intent.
#[allow(clippy::too_many_arguments)]
pub(super) fn summarize_prefix(
    backend: &mut Backend,
    messages: &[Value],
    keep: usize,
    policy: &CompactionConfig,
    context: &CompactionContext<'_>,
    method: CompactionMethod,
    focus: &str,
    interrupt: &Arc<AtomicBool>,
    ui: &mut dyn Ui,
    complete: &mut dyn FnMut(&mut Backend, &Chat, &mut dyn Ui) -> Result<String>,
    remote_observer: &mut Option<&mut RemoteCompactionObserver<'_>>,
) -> Result<String> {
    summarize_prefix_with_worker(
        backend,
        messages,
        keep,
        policy,
        context,
        method,
        focus,
        interrupt,
        ui,
        &mut |backend, request, ui, _| complete(backend, request, ui),
        remote_observer,
        &mut None,
    )
}

/// Carry the previous summary's file lists forward. Historical read inventories are navigation noise,
/// not current edit witnesses: keep mutation targets, and let new reads repopulate the recent list.
fn carry_forward(ops: &mut FileOps, carried: FileOps) {
    ops.merge(carried);
    ops.read.clear();
    ops.recent.retain(|path| ops.modified.contains(path));
}

/// Finish a live Handoff summary over the full projected request. File targets are attempted
/// operations across every message in the request, including any prior committed summary.
pub(super) fn finish_handoff(messages: &[Value], summary: &str) -> String {
    let mut ops = FileOps::default();
    let mut seen_history = false;
    for message in messages {
        let history = is_history(message);
        if history
            && !seen_history
            && let Some((_, carried)) = previous_summary(message)
        {
            seen_history = true;
            carry_forward(&mut ops, carried);
            continue;
        }
        seen_history |= history;
        ops.observe(message);
    }
    ops.upsert(summary)
}

/// Summarize with an optional independently owned cloud route for the independent turn-prefix fold.
/// Incremental folds stay sequential, and both results settle before the prepared summary is used.
#[allow(clippy::too_many_arguments)]
pub(super) fn summarize_prefix_with_worker(
    backend: &mut Backend,
    messages: &[Value],
    keep: usize,
    policy: &CompactionConfig,
    context: &CompactionContext<'_>,
    method: CompactionMethod,
    focus: &str,
    interrupt: &Arc<AtomicBool>,
    ui: &mut dyn Ui,
    complete: &mut RequestCompletion<'_>,
    remote_observer: &mut Option<&mut RemoteCompactionObserver<'_>>,
    worker: &mut Option<SummaryWorker>,
) -> Result<String> {
    let keep = keep.min(messages.len());
    let mut ops = FileOps::default();
    let mut previous: Option<&str> = None;
    let mut seen_history = false;
    let mut source = Vec::new();
    for (index, message) in messages[..keep].iter().enumerate() {
        let history = is_history(message);
        if !history
            && !(method == CompactionMethod::Handoff
                && matches!(message["role"].as_str(), Some("system" | "developer")))
        {
            continue;
        }
        ensure!(
            message["dial_compaction_model"].is_null(),
            "portable compaction cannot decode prior encrypted native state; reconstruct the immutable source archive before switching methods"
        );
        if history
            && !seen_history
            && let Some((text, carried)) = previous_summary(message)
        {
            seen_history = true;
            carry_forward(&mut ops, carried);
            previous = Some(text).filter(|text| !text.is_empty());
            continue;
        }
        seen_history |= history;
        ops.observe(message);
        source.push(index);
    }
    ensure!(
        !source.is_empty(),
        "nothing new to summarize after the previous summary"
    );
    let parts = |indices: &[usize]| {
        indices
            .iter()
            .map(|&index| transcript_part(&messages[index]))
            .collect::<Result<Vec<_>>>()
    };
    let summary = if let Some(at) = turn_split(messages, &source, keep) {
        let (history, prefix) = source.split_at(at);
        let history = parts(history)?;
        let prefix = parts(prefix)?;
        let fork = if !history.is_empty()
            && !backend.local_inference()
            && method != CompactionMethod::Remote
            && remote_observer.is_none()
            && worker
                .as_ref()
                .is_some_and(|worker| !worker.backend.local_inference())
        {
            worker.take()
        } else {
            None
        };
        let (earlier, turn) = if let Some(worker) = fork {
            // The deadline helper links user stop to a request-local flag without ever setting
            // the user's flag when one fold fails. Preserve the original failure after settling.
            let mut outcome = None;
            let linked = crate::agent::model::roles::with_request_deadline(
                interrupt.clone(),
                std::time::Duration::from_secs(
                    crate::agent::compact::compaction_runtime::maintenance_timeout(
                        context.timeout_secs,
                    ),
                ),
                |cancel| {
                    let failure = std::sync::Mutex::new(None);
                    let settle = |result: Result<String>| match result {
                        Ok(text) => Some(text),
                        Err(error) => {
                            let mut first = failure.lock().unwrap_or_else(|e| e.into_inner());
                            if first.is_none() {
                                *first = Some(error);
                            }
                            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                            None
                        }
                    };
                    outcome = Some(std::thread::scope(|scope| -> Result<(String, String)> {
                        let (mut worker_ui, replay) =
                            crate::agent::compact::compaction_runtime::buffer_progress();
                        let child_cancel = cancel.clone();
                        let settle_child = &settle;
                        let child = std::thread::Builder::new()
                            .name("dial-summary-turn-prefix".into())
                            .spawn_scoped(scope, move || {
                                let SummaryWorker {
                                    mut backend,
                                    mut complete,
                                } = worker;
                                let result = std::panic::catch_unwind(
                                    std::panic::AssertUnwindSafe(|| {
                                        fold(
                                            &mut backend,
                                            &prefix,
                                            None,
                                            Prompt::TurnPrefix,
                                            policy,
                                            context,
                                            method,
                                            focus,
                                            &child_cancel,
                                            &mut worker_ui,
                                            &mut |backend, request, ui| {
                                                complete(backend, request, ui, child_cancel.clone())
                                            },
                                            &mut None,
                                        )
                                    }),
                                )
                                .unwrap_or_else(|_| {
                                    Err(anyhow::anyhow!(
                                        "split-summary worker panicked; no summary was committed"
                                    ))
                                });
                                (settle_child(result), replay)
                            })
                            .context("start independent split-summary worker")?;
                        let earlier =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                fold(
                                    backend,
                                    &history,
                                    previous,
                                    Prompt::Main,
                                    policy,
                                    context,
                                    method,
                                    focus,
                                    &cancel,
                                    ui,
                                    &mut |backend, request, ui| {
                                        complete(backend, request, ui, cancel.clone())
                                    },
                                    remote_observer,
                                )
                            }))
                            .unwrap_or_else(|_| {
                                Err(anyhow::anyhow!(
                                    "split-summary history fold panicked; no summary was committed"
                                ))
                            });
                        let earlier = settle(earlier);
                        // Every path joins: neither failure nor user interruption detaches inference.
                        let (turn, replay) = child
                            .join()
                            .map_err(|_| anyhow::anyhow!("split-summary worker panicked"))?;
                        replay(ui);
                        if let Some(error) =
                            failure.lock().unwrap_or_else(|e| e.into_inner()).take()
                        {
                            return Err(error);
                        }
                        Ok((
                            earlier.context("history summary missing")?,
                            turn.context("turn summary missing")?,
                        ))
                    }));
                    Ok(())
                },
            );
            interrupted(interrupt)?;
            match outcome {
                Some(Err(error)) => return Err(error),
                Some(Ok(summary)) => {
                    linked?;
                    summary
                }
                None => {
                    linked?;
                    anyhow::bail!("split summary did not run")
                }
            }
        } else {
            let earlier = if history.is_empty() {
                previous.map_or_else(|| "No prior history.".to_owned(), str::to_owned)
            } else {
                fold(
                    backend,
                    &history,
                    previous,
                    Prompt::Main,
                    policy,
                    context,
                    method,
                    focus,
                    interrupt,
                    ui,
                    &mut |backend, request, ui| complete(backend, request, ui, interrupt.clone()),
                    remote_observer,
                )?
            };
            let turn = fold(
                backend,
                &prefix,
                None,
                Prompt::TurnPrefix,
                policy,
                context,
                method,
                focus,
                interrupt,
                ui,
                &mut |backend, request, ui| complete(backend, request, ui, interrupt.clone()),
                remote_observer,
            )?;
            (earlier, turn)
        };
        format!("{earlier}\n\n---\n\n**Turn Context (split turn):**\n\n{turn}")
    } else {
        fold(
            backend,
            &parts(&source)?,
            previous,
            Prompt::Main,
            policy,
            context,
            method,
            focus,
            interrupt,
            ui,
            &mut |backend, request, ui| complete(backend, request, ui, interrupt.clone()),
            remote_observer,
        )?
    };
    Ok(ops.upsert(&summary))
}

/// A summary of an abandoned branch with attempted file-operation targets (not success evidence).
#[derive(Debug, PartialEq, Eq)]
pub struct BranchSummary {
    pub summary: String,
    /// Attempted read targets observed in call arguments, excluding modification targets.
    pub read_files: Vec<String>,
    /// Attempted modification targets observed in call arguments, regardless of outcome.
    pub modified_files: Vec<String>,
}

/// Newest-first budgeting of an abandoned branch. Returns the kept indices in chronological order
/// and the file ops of every message visited. An earlier summary may still fit just past the
/// budget, since it carries the history that no longer fits verbatim.
fn budgeted_tail(messages: &[Value], budget: u64) -> (Vec<usize>, FileOps) {
    let mut ops = FileOps::default();
    let mut kept = Vec::new();
    let mut total = 0u64;
    for (index, message) in messages.iter().enumerate().rev() {
        let carried = Session::compaction_summary_text(message);
        if let Some(text) = carried {
            ops.merge(FileOps::take_from(text).1);
        }
        ops.observe(message);
        let tokens = estimate(message);
        if total.saturating_add(tokens) > budget {
            if carried.is_some() && total.saturating_mul(10) < budget.saturating_mul(9) {
                kept.push(index);
            }
            break;
        }
        total += tokens;
        kept.push(index);
    }
    kept.reverse();
    (kept, ops)
}

/// Summarize an abandoned branch (chronological `messages`) so the branch the user returns to can
/// carry what happened there. The summary runs locally: `method` only selects the remote path in
/// `fold`, which this never takes.
#[allow(clippy::too_many_arguments)]
pub fn summarize_branch(
    backend: &mut Backend,
    messages: &[Value],
    policy: &CompactionConfig,
    context: &CompactionContext<'_>,
    focus: &str,
    interrupt: &Arc<AtomicBool>,
    ui: &mut dyn Ui,
    complete: &mut dyn FnMut(&mut Backend, &Chat, &mut dyn Ui) -> Result<String>,
) -> Result<BranchSummary> {
    let window = if context.summary_window > 0 {
        u64::from(context.summary_window)
    } else {
        DEFAULT_SUMMARY_INPUT_WINDOW
    };
    let output = summary_output(policy, context);
    let (kept, ops) = budgeted_tail(messages, input_budget(window, output));
    if kept.is_empty() {
        return Ok(BranchSummary {
            summary: "No content to summarize".to_owned(),
            read_files: Vec::new(),
            modified_files: Vec::new(),
        });
    }
    let parts = kept
        .iter()
        .map(|&index| transcript_part(&messages[index]))
        .collect::<Result<Vec<_>>>()?;
    let summary = fold(
        backend,
        &parts,
        None,
        Prompt::Branch,
        policy,
        context,
        CompactionMethod::Soft,
        focus,
        interrupt,
        ui,
        complete,
        &mut None,
    )?;
    let text = ops.upsert(&format!("{BRANCH_PREAMBLE}\n\n{summary}"));
    Ok(BranchSummary {
        summary: text,
        read_files: ops
            .read
            .iter()
            .filter(|path| !ops.modified.contains(*path))
            .cloned()
            .collect(),
        modified_files: ops.modified.iter().cloned().collect(),
    })
}

/// Summarize `parts` as a fold: each window updates the summary carried out of the previous one.
/// Oversized texts are fragmented, never truncated; a provider rejection re-plans at a learned
/// ceiling; the call and fragment counts are bounded so an oversized transcript fails explicitly.
#[allow(clippy::too_many_arguments)]
fn fold(
    backend: &mut Backend,
    parts: &[String],
    previous: Option<&str>,
    prompt: Prompt,
    policy: &CompactionConfig,
    context: &CompactionContext<'_>,
    method: CompactionMethod,
    focus: &str,
    interrupt: &Arc<AtomicBool>,
    ui: &mut dyn Ui,
    complete: &mut dyn FnMut(&mut Backend, &Chat, &mut dyn Ui) -> Result<String>,
    remote_observer: &mut Option<&mut RemoteCompactionObserver<'_>>,
) -> Result<String> {
    ensure!(!parts.is_empty(), "nothing to summarize");
    let output = summary_output(policy, context);
    let window = if context.summary_window > 0 {
        u64::from(context.summary_window)
    } else {
        DEFAULT_SUMMARY_INPUT_WINDOW
    };
    let floor = min_input_tokens(window);
    let budget = input_budget(window, output);
    let sizes: Vec<u64> = parts
        .iter()
        .map(|part| text_tokens(part).saturating_add(1))
        .collect();
    let mut made = 0usize;
    let mut pending: VecDeque<Window> = VecDeque::new();
    if sizes.iter().sum::<u64>() <= budget {
        pending.push_back(Window {
            units: parts.to_vec(),
            budget,
        });
    } else {
        for range in plan(&sizes, 0..parts.len(), budget) {
            pending.push_back(Window {
                units: parts[range].to_vec(),
                budget,
            });
        }
    }
    let mut carried = previous.map(str::to_owned);
    // The largest window the provider is still believed to accept. A rejection lowers it for every
    // remaining sibling window, so one compaction never relearns the same limit window by window.
    let mut ceiling = budget;
    let mut calls = 0usize;
    while let Some(Window {
        units,
        budget: window_budget,
    }) = pending.pop_front()
    {
        if window_budget > ceiling {
            replan(&mut pending, units, ceiling, &mut made)?;
            continue;
        }
        let joined = units.join("\n");
        let tokens = text_tokens(&joined);
        if tokens > window_budget {
            replan(&mut pending, units, window_budget, &mut made)?;
            continue;
        }
        interrupted(interrupt)?;
        let halved = window_budget.min(tokens) / 2;
        let instructions = prompt_text(prompt, method, focus, carried.is_some());
        let request = summary_request(&joined, carried.as_deref(), &instructions, output, context);
        if request_estimate(&request).saturating_add(u64::from(output)) > window {
            ensure!(
                halved >= floor && halved < window_budget,
                "compaction role window ({window} tokens) cannot fit one summary window plus the carried summary and {output} output tokens"
            );
            replan(&mut pending, units, halved, &mut made)?;
            continue;
        }
        ensure!(
            calls < MAX_SUMMARY_CALLS,
            "compaction summary needs more than {MAX_SUMMARY_CALLS} model calls to cover the transcript; no summary was committed"
        );
        calls += 1;
        let result = if method == CompactionMethod::Remote {
            (|| -> Result<String> {
                let endpoint = policy
                    .remote_endpoint
                    .as_deref()
                    .context("remote summary endpoint is not configured")?;
                if let Some(observer) = remote_observer.as_deref_mut() {
                    observer(backend, None).map_err(CompactionControlDenied)?;
                }
                let reply = with_ui(ui, interrupt.clone(), || {
                    backend.compact_remote_summary(
                        endpoint,
                        context.user_dir,
                        &request,
                        context.timeout_secs,
                        interrupt.clone(),
                    )
                })?;
                if let Some(observer) = remote_observer.as_deref_mut() {
                    observer(backend, Some(&reply)).map_err(CompactionControlDenied)?;
                }
                Ok(reply.text)
            })()
        } else {
            complete(backend, &request, ui)
        };
        match result {
            Ok(summary) => {
                interrupted(interrupt)?;
                ensure!(
                    !summary.trim().is_empty(),
                    "compaction model produced an empty summary"
                );
                carried = Some(summary.trim().to_owned());
            }
            Err(error) => {
                interrupted(interrupt)?;
                // The catalog window can overstate what the provider accepts: halve what was sent and re-plan.
                if error.downcast_ref::<CompactionControlDenied>().is_some()
                    || !crate::llm::errors::is_context_overflow(&error)
                    || halved < floor
                    || halved >= window_budget
                {
                    return Err(error);
                }
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, format!("Summary window of about {tokens} tokens exceeded the compaction model's accepted input; re-planning at {halved} tokens.")));
                ceiling = ceiling.min(halved);
                replan(&mut pending, units, halved, &mut made)?;
            }
        }
    }
    carried.context("nothing to summarize")
}

#[cfg(test)]
mod tests;
