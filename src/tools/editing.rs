//! Mode-specific editing contracts, bounded preparation and controller-facing preview/recovery.
use super::{
    EditMode, Workspace,
    edit_transaction::{self, Change},
    hashline, opt_bool, present, req_str, tool_schema,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone)]
pub struct EditRepairRequest {
    pub path: String,
    pub before: String,
    pub proposed: String,
    pub diagnostics: Value,
}
pub trait EditRepair: Send + Sync {
    fn repair(&self, request: &EditRepairRequest) -> Result<String, String>;
}
#[derive(Clone, Default)]
pub(crate) struct RepairSlot(pub Option<Arc<dyn EditRepair>>);
impl std::fmt::Debug for RepairSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Some(EditRepair)"
        } else {
            "None"
        })
    }
}

pub fn instructions(mode: EditMode) -> &'static str {
    match mode {
        EditMode::Hashline => super::HASHLINE_EDIT_DESC,
        EditMode::Replace => notebook_edit_description!(
            "Replace old_string with new_string in literal path; old_string must be nonempty and unique unless replace_all=true. Read every touched line first. Exact matching is preferred; enabled fuzzy matching uses normalized Unicode/whitespace then bounded high-confidence source matching and indentation adjustment. Ambiguous matches reject. Recovery diagnostics identify actual matched source. Generated-file and seen-line guards precede one rollback transaction."
        ),
        EditMode::Patch => notebook_edit_description!(
            "Input {path,edits:[{op?:'create'|'update'|'delete',rename?:string,diff?:string}]}. Entries target literal path and evaluate sequentially, committing once. Update diff uses unified @@ line hints or symbolic/hierarchical context headers and space/+/- rows; *** End of File pins EOF. Placement uses exact, normalized whitespace/Unicode, indentation-aware and bounded high-confidence fallback matching; equivalent echoed/repeated context can be repaired, with diagnostics. Ambiguity or conflicting overlap rejects. Create diff supplies complete source (uniform + prefixes are accepted); delete has no diff. Rename destinations must not exist. Read all used anchors/touched lines first. Literal : and ? are not read selectors."
        ),
        EditMode::ApplyPatch => notebook_edit_description!(
            "input is *** Begin Patch, *** Add File: path (+prefixed complete content), *** Update File: path (optional *** Move to: destination, then @@ context/+/- hunks), *** Delete File: path, and *** End Patch. A shell-style EOF wrapper is accepted. Uses the same native normalized, hierarchical, indentation-aware and bounded high-confidence hunk placement as patch. Every section validates before one rollback transaction; ambiguity, conflicting overlap, stale witnesses and existing destinations reject. Read all existing target anchors/touched lines first."
        ),
        EditMode::Sloppy => notebook_edit_description!(
            "input is *** Edit File: path (JSON-quote ambiguous paths; append all for all matches), then *** Find existing text and *** Replace complete final text (empty deletes), *** Insert Before or *** Insert After new lines. Bare *** Edit File: continues the prior path. Prefer raw exact source, no diff prefixes/envelopes/fences. Find … captures omitted text: mid-line gaps stay on that line, line-end gaps span lines; Replace … re-emits captures in order. Native sloppy parsing also recognizes selection markers «…», desired-state fragments and bounded copy/prefix/header recovery; matching uses normalized Unicode/whitespace, captures and bounded unique fuzzy/context reconciliation with explicit recovery diagnostics. Replacement/insert text remains authored text, not a guessed operator or delimiter repair. Operations address original source positions; ambiguity/conflicting overlap rejects, all files commit in one rollback transaction. Read every matched/captured source region first."
        ),
    }
}
pub fn schema(mode: EditMode) -> Value {
    match mode {
        EditMode::Replace => tool_schema(
            "edit",
            instructions(mode),
            json!({"path":{"type":"string"},"old_string":{"type":"string","minLength":1},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}}),
            &["path", "old_string", "new_string"],
        ),
        EditMode::Patch => tool_schema(
            "edit",
            instructions(mode),
            json!({"path":{"type":"string"},"edits":{"type":"array","minItems":1,"maxItems":128,"items":{"type":"object","properties":{"op":{"type":"string","enum":["create","update","delete"]},"rename":{"type":"string"},"diff":{"type":"string"}},"additionalProperties":false}}}),
            &["path", "edits"],
        ),
        _ => tool_schema(
            "edit",
            instructions(mode),
            json!({"input":{"type":"string","description":match mode {
                EditMode::Hashline => "Hashline patch text; the tool description is the complete contract.",
                EditMode::ApplyPatch => "Apply-patch text using *** Begin Patch and *** End Patch; follow the tool description.",
                EditMode::Sloppy => "Sloppy patch text; the tool description is the complete contract.",
                _ => unreachable!(),
            }}}),
            &["input"],
        ),
    }
}
fn path_text(input: &str) -> Result<String, String> {
    let text = input.trim();
    let text = if text.starts_with('"') {
        serde_json::from_str::<String>(text)
            .map_err(|e| format!("invalid JSON-quoted edit path: {e}"))?
    } else {
        text.to_owned()
    };
    if text.is_empty() || text.len() > 4096 || text.contains(['\0', '\n', '\r']) {
        return Err("edit path must be a nonempty literal path no longer than 4096 bytes".into());
    }
    Ok(text)
}
fn literal_path(input: &str) -> Result<String, String> {
    if input.is_empty() || input.len() > 4096 || input.contains(['\0', '\n', '\r']) {
        return Err("invalid literal edit path".into());
    }
    Ok(input.to_owned())
}
pub(crate) fn validate_shape(mode: EditMode, args: &Value) -> Result<(), String> {
    let object = args.as_object().ok_or("edit arguments must be an object")?;
    let allowed: &[&str] = match mode {
        EditMode::Replace => &["path", "old_string", "new_string", "replace_all"],
        EditMode::Patch => &["path", "edits"],
        _ => &["input"],
    };
    if object
        .iter()
        .any(|(key, value)| !value.is_null() && !allowed.contains(&key.as_str()))
    {
        return Err("edit arguments contain fields from another mode or unknown fields".into());
    }
    Ok(())
}
#[derive(Clone)]
struct Entry {
    path: String,
    op: Op,
    rename: Option<String>,
    diff: Option<String>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Create,
    Update,
    Delete,
}
fn patch_entries(args: &Value) -> Result<Vec<Entry>, String> {
    let path = literal_path(req_str(args, "path")?)?;
    let edits = args["edits"].as_array().ok_or("edits must be an array")?;
    if edits.is_empty() || edits.len() > 128 {
        return Err("edits requires 1..128 entries".into());
    }
    edits
        .iter()
        .map(|edit| {
            let obj = edit.as_object().ok_or("patch entry must be an object")?;
            if obj.iter().any(|(key, value)| {
                !value.is_null() && !matches!(key.as_str(), "op" | "rename" | "diff")
            }) {
                return Err("unknown patch entry field".into());
            }
            let op = match super::opt_str(edit, "op")? {
                None | Some("update") => Op::Update,
                Some("create") => Op::Create,
                Some("delete") => Op::Delete,
                _ => return Err("patch op must be create, update or delete".into()),
            };
            let rename = super::opt_str(edit, "rename")?
                .map(literal_path)
                .transpose()?;
            let diff = super::opt_str(edit, "diff")?.map(str::to_owned);
            validate_entry(op, rename.as_deref(), diff.as_deref())?;
            Ok(Entry {
                path: path.clone(),
                op,
                rename,
                diff,
            })
        })
        .collect()
}
fn validate_entry(op: Op, rename: Option<&str>, diff: Option<&str>) -> Result<(), String> {
    match op {
        Op::Create if rename.is_some() || diff.is_none() => {
            Err("create requires raw diff content and cannot rename".into())
        }
        Op::Delete if rename.is_some() || diff.is_some() => {
            Err("delete cannot have diff or rename".into())
        }
        Op::Update if diff.is_none() => Err("update requires diff hunks".into()),
        Op::Update => {
            let hunks = super::edit_native::diff_string::parse_diff_hunks(diff.unwrap())
                .map_err(|e| e.to_string())?;
            if hunks.len() > 256 {
                return Err("patch exceeds 256 hunks".into());
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
fn apply_entries(input: &str, streaming: bool) -> Result<Vec<Entry>, String> {
    use super::edit_native::apply_patch::{
        Operation, parse_apply_patch, parse_apply_patch_streaming,
    };
    if input.len() > 8 * 1024 * 1024 {
        return Err("patch input exceeds 8 MiB".into());
    }
    let native = if streaming {
        parse_apply_patch_streaming(input)
    } else {
        parse_apply_patch(input)
    }
    .map_err(|e| e.to_string())?;
    if native.len() > 128 || (!streaming && native.is_empty()) {
        return Err("patch requires 1..128 file sections".into());
    }
    native
        .into_iter()
        .map(|entry| {
            let op = match entry.op {
                Operation::Create => Op::Create,
                Operation::Delete => Op::Delete,
                Operation::Update => Op::Update,
            };
            let path = path_text(&entry.path)?;
            let rename = entry.rename.as_deref().map(path_text).transpose()?;
            validate_entry(op, rename.as_deref(), entry.diff.as_deref())?;
            Ok(Entry {
                path,
                op,
                rename,
                diff: entry.diff,
            })
        })
        .collect()
}
fn sloppy_sections(
    input: &str,
) -> Result<Vec<super::edit_native::sloppy::types::SloppySection>, String> {
    if input.len() > 8 * 1024 * 1024 {
        return Err("sloppy input exceeds 8 MiB".into());
    }
    let sections = super::edit_native::sloppy::parse::split_sloppy_sections(input)
        .map_err(|e| e.to_string())?;
    if sections.is_empty() || sections.len() > 128 {
        return Err("sloppy payload requires 1..128 file sections".into());
    }
    if sections
        .iter()
        .map(|s| s.body.lines().filter(|line| line.starts_with('«')).count())
        .sum::<usize>()
        > 256
    {
        return Err("sloppy payload exceeds 256 operations".into());
    }
    Ok(sections)
}
/// Literal proposals from the native sloppy grammar, without opening any source file.
pub(crate) fn proposed_sloppy_sources(input: &str) -> Result<Vec<(String, String)>, String> {
    use super::edit_native::sloppy::types::OperationRewrite;
    let mut sources = Vec::new();
    for section in sloppy_sections(input)? {
        for operation in
            super::edit_native::sloppy::parse::parse_operations(&section.body, "", &section.path)
                .map_err(|error| error.to_string())?
        {
            match operation.rewrite {
                OperationRewrite::Explicit { text } | OperationRewrite::Insert { text, .. } => {
                    sources.push((section.path.clone(), text))
                }
                OperationRewrite::Inline { replacements } => sources.extend(
                    replacements
                        .into_iter()
                        .map(|text| (section.path.clone(), text)),
                ),
            }
        }
    }
    Ok(sources)
}

/// Permission extraction fully validates mode grammar; payload bodies never grant targets.
pub fn targets(mode: EditMode, args: &Value) -> Result<Vec<String>, String> {
    validate_shape(mode, args)?;
    let mut out = match mode {
        EditMode::Hashline => hashline::targets(req_str(args, "input")?)?,
        EditMode::Replace => {
            req_str(args, "old_string")?;
            req_str(args, "new_string")?;
            opt_bool(args, "replace_all")?;
            vec![literal_path(req_str(args, "path")?)?]
        }
        EditMode::Patch => patch_entries(args)?
            .into_iter()
            .flat_map(|entry| std::iter::once(entry.path).chain(entry.rename))
            .collect(),
        EditMode::ApplyPatch => apply_entries(req_str(args, "input")?, false)?
            .into_iter()
            .flat_map(|entry| std::iter::once(entry.path).chain(entry.rename))
            .collect(),
        EditMode::Sloppy => sloppy_sections(req_str(args, "input")?)?
            .into_iter()
            .map(|s| s.path)
            .collect(),
    };
    out.sort();
    out.dedup();
    Ok(out)
}
pub fn declared_targets(args: &Value) -> Result<Vec<String>, String> {
    let mode = if let Some(input) = args["input"].as_str() {
        // A sloppy payload that fails to compile stays sloppy so its parse error is reported.
        if !matches!(super::edit_native::sloppy::parse::split_sloppy_sections(input), Ok(sections) if sections.is_empty())
        {
            EditMode::Sloppy
        } else if input
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && *line != "*** Begin Patch")
            .is_some_and(|line| line.starts_with('['))
        {
            EditMode::Hashline
        } else if super::edit_native::apply_patch::parse_apply_patch(input).is_ok() {
            EditMode::ApplyPatch
        } else {
            return Err("unrecognized edit input grammar".into());
        }
    } else if present(args, "edits") {
        EditMode::Patch
    } else {
        EditMode::Replace
    };
    targets(mode, args)
}
/// Only an entire unfenced final text payload is recoverable; examples/prose never execute.
pub fn inline_payload(text: &str) -> Option<(std::ops::Range<usize>, Value)> {
    let trimmed = text.trim();
    if !trimmed.starts_with("*** Edit File:") {
        return None;
    }
    if trimmed
        .lines()
        .any(|line| line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~"))
    {
        return None;
    }
    let regions = super::edit_native::sloppy::parse::extract_inline_sloppy_regions(trimmed);
    if regions.len() != 1 || regions[0].payload.trim() != trimmed {
        return None;
    }
    sloppy_sections(trimmed).ok()?;
    let mut pending = false;
    let mut pairs = 0;
    for line in trimmed.lines() {
        match line {
            "*** Find" => {
                if pending {
                    return None;
                }
                pending = true;
            }
            "*** Replace" | "*** Insert Before" | "*** Insert After" => {
                if !pending {
                    return None;
                }
                pending = false;
                pairs += 1;
            }
            _ if line.starts_with("*** Edit File:") && pending => return None,
            _ => {}
        }
    }
    if pending || pairs == 0 {
        return None;
    }
    let start = text.len() - text.trim_start().len();
    Some((start..start + trimmed.len(), json!({"input":trimmed})))
}

pub(crate) fn generated_guard(
    ws: &Workspace,
    path: &Path,
    before: Option<&str>,
) -> Result<(), String> {
    if !ws.edit_settings.block_auto_generated {
        return Ok(());
    }
    let name = path.file_name().and_then(|x| x.to_str()).unwrap_or("");
    static NAME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^zz_generated\.|\.pb\.(go|cc|h|c|js|ts)$|_pb2(?:_grpc)?\.py$|\.gen\.(go|ts|js|py)$|\.swagger\.json$|\.mocks?\.(go|ts|js)$").unwrap()
    });
    static MARK: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)@generated\b|\bcode\s+generated\s+by\s+[a-z0-9_.-]+|\bcode\s+generated\b.*\bdo\s+not\s+edit\b|\bthis\s+file\s+was\s+automatically\s+generated\b|\bgenerated\s+by\s+(protoc(?:-gen-[\w-]+)?|sqlc|buf|swagger(?:-codegen)?|openapi(?:-generator)?|grpc-gateway|mockery|stringer|easyjson|deepcopy-gen|defaulter-gen|conversion-gen|client-gen|lister-gen|informer-gen|kysely-codegen|napi-rs)\b").unwrap()
    });
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let slash = [
        "c", "cc", "cpp", "cs", "dart", "go", "h", "hpp", "java", "js", "jsx", "kt", "kts", "mjs",
        "cjs", "php", "rs", "scala", "swift", "ts", "tsx",
    ]
    .contains(&ext.as_str());
    let hash = [
        "py", "rb", "sh", "bash", "zsh", "yml", "yaml", "toml", "ini", "cfg", "conf", "env", "pl",
        "r",
    ]
    .contains(&ext.as_str())
        || ["dockerfile", "makefile", "justfile"].contains(&name.to_ascii_lowercase().as_str());
    let mut comments = String::new();
    let mut block = false;
    let source = before.unwrap_or("");
    let mut end = source.len().min(1024);
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    let html = ["html", "htm", "xml", "svg", "xhtml"].contains(&ext.as_str());
    let close = if html { "-->" } else { "*/" };
    for (i, line) in source[..end]
        .trim_start_matches('\u{feff}')
        .lines()
        .take(40)
        .enumerate()
    {
        let line = line.trim();
        if line.is_empty() || i == 0 && line.starts_with("#!") {
            continue;
        }
        let start = (slash && (line.starts_with("//") || line.starts_with("/*")))
            || (hash && line.starts_with('#'))
            || (ext == "sql" && line.starts_with("--"))
            || (html && line.starts_with("<!--"));
        if !start && !block {
            break;
        }
        block = (block || slash && line.starts_with("/*") || html && line.starts_with("<!--"))
            && !line.contains(close);
        comments.push_str(line);
        comments.push('\n');
    }
    if NAME.is_match(name) || MARK.is_match(&comments) {
        return Err(format!(
            "cannot modify auto-generated file {}; change its generator/source instead (edit.block_auto_generated controls this guard)",
            super::edit_source::display_path(ws.rel(path))
        ));
    }
    Ok(())
}
pub(crate) fn seen_guard(
    ws: &Workspace,
    path: &Path,
    before: &str,
    ranges: &[(usize, usize)],
) -> Result<(), String> {
    if !ws.edit_settings.enforce_seen_lines {
        return Ok(());
    }
    ws.store()
        .check_ranges(path, &hashline::normalize(before), ranges)
}
fn line_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    // Offsets come from matchers over normalized text; clamp defensively so a
    // mid-character offset can never panic the slice below.
    let floor = |mut at: usize| {
        at = at.min(text.len());
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let start = floor(start);
    let end = floor(end).max(start);
    let first = text[..start].bytes().filter(|&b| b == b'\n').count() + 1;
    let last = if start == end {
        first
    } else {
        text[..end].bytes().filter(|&b| b == b'\n').count()
            + usize::from(!text[..end].ends_with('\n'))
    };
    let total = hashline::split_lines(text).len().max(1);
    (first.min(total), last.min(total).max(first.min(total)))
}
fn replace_plan(
    ws: &Workspace,
    args: &Value,
    preview_no_change: Option<&std::cell::Cell<bool>>,
) -> Result<Vec<Change>, String> {
    use super::edit_native::{
        error::EditError,
        fuzzy::{self, ReplaceOutcome},
    };
    let path = ws.resolve(req_str(args, "path")?)?;
    let mut change = Change::load(ws, path, None)?;
    let text = change.require_text()?;
    let old = req_str(args, "old_string")?;
    let new = req_str(args, "new_string")?;
    let all = opt_bool(args, "replace_all")?;
    if old.is_empty() {
        return Err("old_string must not be empty".into());
    }
    if old == new {
        if let Some(flag) = preview_no_change {
            flag.set(true);
        }
        return Err("old_string and new_string are identical; no change".into());
    }
    let source = super::edit_source::Source::load(&change.path, Some(text))?;
    let lf = &source.text;
    let witness = |start, end| {
        source
            .check_ranges(ws, &change.path, &[line_range(lf, start, end)])
            .map_err(EditError::apply)
    };
    let result = fuzzy::replace_text(
        lf,
        old,
        new,
        ws.edit_settings.fuzzy_match,
        all,
        Some(ws.edit_settings.fuzzy_threshold),
        &witness,
    )
    .map_err(|e| e.to_string())?;
    let replaced = match result {
        ReplaceOutcome::Replaced(result) => result,
        ReplaceOutcome::NoChange => {
            if let Some(flag) = preview_no_change {
                flag.set(true);
            }
            return Err(
                "matched source already equals the requested replacement; no change".into(),
            );
        }
        ReplaceOutcome::Missed(outcome) => {
            let count = outcome
                .occurrences
                .or(outcome.fuzzy_matches)
                .filter(|n| *n > 1);
            let detail = if let Some(count) = count {
                let lines = outcome
                    .occurrence_lines
                    .as_ref()
                    .map(|lines| {
                        lines
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|| "multiple candidates".into());
                format!(
                    "old_string matches {count} places (lines {lines}); add unique context.\n{}",
                    if outcome.occurrences.is_some() {
                        fuzzy::format_occurrence_error(
                            &super::edit_source::display_path(ws.rel(&change.path)),
                            &outcome,
                        )
                    } else {
                        fuzzy::format_match_error(
                            &super::edit_source::display_path(ws.rel(&change.path)),
                            old,
                            outcome.closest.as_ref(),
                            ws.edit_settings.fuzzy_match,
                            ws.edit_settings.fuzzy_threshold,
                            outcome.fuzzy_matches,
                        )
                    }
                )
            } else {
                format!(
                    "old_string not found with a unique eligible match.\n{}",
                    fuzzy::format_match_error(
                        &super::edit_source::display_path(ws.rel(&change.path)),
                        old,
                        outcome.closest.as_ref(),
                        ws.edit_settings.fuzzy_match,
                        ws.edit_settings.fuzzy_threshold,
                        outcome.fuzzy_matches
                    )
                )
            };
            return Err(format!("{detail}.{}", diagnose_miss(lf, old)));
        }
    };
    let flags = hashline::transition_flags(lf, &replaced.content, &source.flags(ws, &change.path));
    let persisted = source.persist_checked(ws, replaced.content, &change.path, &flags)?;
    change.projection = persisted.projection;
    change.seen_after = Some(flags);
    change.report = Some(format!(
        "replaced {} occurrence{} in {}{}",
        replaced.count,
        if replaced.count == 1 { "" } else { "s" },
        super::edit_source::display_path(ws.rel(&change.path)),
        if replaced.notes.is_empty() {
            String::new()
        } else {
            format!("\n{}", replaced.notes.join("\n"))
        }
    ));
    change.after = Some(persisted.raw);
    Ok(vec![change])
}
fn diagnose_miss(text: &str, old: &str) -> String {
    let content: Vec<_> = old.lines().filter(|line| !line.trim().is_empty()).collect();
    if !content.is_empty()
        && content.iter().all(|line| {
            let line = line.trim_start();
            let n = line.bytes().take_while(u8::is_ascii_digit).count();
            n > 0 && line[n..].starts_with(['\t', ':'])
        })
    {
        return " old_string copied a read line number; copy only source after the tab/colon."
            .into();
    }
    let file: Vec<_> = text.lines().collect();
    let pattern: Vec<_> = old.lines().map(str::trim).collect();
    if !pattern.is_empty()
        && pattern.len() <= file.len()
        && let Some(at) = file.windows(pattern.len()).position(|window| {
            window
                .iter()
                .map(|line| line.trim())
                .eq(pattern.iter().copied())
        })
    {
        let rows = file[at..at + pattern.len()]
            .iter()
            .take(14)
            .enumerate()
            .map(|(n, line)| format!("{}:{line}\n", at + n + 1))
            .collect::<String>();
        return format!(
            " The text exists at lines {}-{} with different whitespace. Actual text:\n{rows}",
            at + 1,
            at + pattern.len()
        );
    }
    let Some(anchor) = content
        .first()
        .map(|line| line.trim())
        .filter(|line| line.len() >= 4)
    else {
        return " Read the affected source and copy its exact text.".into();
    };
    let nearby = file
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(anchor))
        .take(3)
        .map(|(n, line)| format!("{}:{line}", n + 1))
        .collect::<Vec<_>>();
    if nearby.is_empty() {
        format!(
            " Not even its first line ({}) occurs; read again.",
            crate::runtime::presentation::truncate(anchor, 80)
        )
    } else {
        format!(
            " Its first line occurs in the file but the following lines differ:\n{}",
            nearby.join("\n")
        )
    }
}
fn apply_hunks(
    ws: &Workspace,
    path: &Path,
    text: &str,
    diff: &str,
    witnesses: &[bool],
) -> Result<(String, Vec<String>), String> {
    use super::edit_native::{diff_string::parse_diff_hunks, error::EditError, patch};
    let hunks = parse_diff_hunks(diff).map_err(|e| e.to_string())?;
    if hunks.len() > 256 {
        return Err("diff exceeds 256 hunks".into());
    }
    let lf = hashline::normalize_body(text);
    let witness = |start: usize, end: usize| {
        if ws.edit_settings.enforce_seen_lines
            && (start..end).any(|n| !witnesses.get(n).copied().unwrap_or(false))
        {
            Err(EditError::apply(format!(
                "{} hunk touches source lines not displayed in full",
                super::edit_source::display_path(ws.rel(path))
            )))
        } else {
            Ok(())
        }
    };
    let (after, notes) = patch::apply_hunks(
        &lf,
        &super::edit_source::display_path(ws.rel(path)),
        &hunks,
        ws.edit_settings.fuzzy_threshold,
        ws.edit_settings.fuzzy_match,
        &witness,
    )
    .map_err(|e| e.to_string())?;
    Ok((after, notes))
}
fn entries_plan(
    ws: &Workspace,
    entries: Vec<Entry>,
    sequential: bool,
) -> Result<Vec<Change>, String> {
    let mut grouped: BTreeMap<PathBuf, Vec<Entry>> = BTreeMap::new();
    for entry in entries {
        let path = ws.resolve(&entry.path)?;
        if !sequential && grouped.contains_key(&path) {
            return Err("multiple sections target one file; combine their hunks".into());
        }
        grouped.entry(path).or_default().push(entry);
    }
    let sources: BTreeSet<_> = grouped.keys().cloned().collect();
    let mut changes = BTreeMap::new();
    for (path, entries) in grouped {
        let mut change = Change::load(ws, path.clone(), None)?;
        let source = super::edit_source::Source::load(
            &path,
            change.before.as_ref().map(|b| b.text.as_str()),
        )?;
        let mut flags = source.flags(ws, &path);
        let mut current = change.before.as_ref().map(|_| source.text.to_string());
        let mut rename = None;
        let count = entries.len();
        for (index, entry) in entries.into_iter().enumerate() {
            match entry.op {
                Op::Create => {
                    if current.is_some() {
                        return Err("create never overwrites an existing file".into());
                    }
                    current = entry.diff.map(|diff| {
                        super::edit_native::diff_string::normalize_create_content(&diff)
                    });
                    flags = vec![
                        true;
                        hashline::split_lines(&hashline::normalize_body(
                            current.as_deref().unwrap()
                        ))
                        .len()
                    ];
                }
                Op::Delete => {
                    current.as_ref().ok_or("delete target does not exist")?;
                    if ws.edit_settings.enforce_seen_lines && flags.iter().any(|seen| !*seen) {
                        return Err(
                            "delete requires every source line to have been displayed".into()
                        );
                    }
                    current = None;
                    flags.clear();
                }
                Op::Update => {
                    let text = current.as_ref().ok_or("update target does not exist")?;
                    let (after, notes) =
                        apply_hunks(ws, &path, text, entry.diff.as_deref().unwrap(), &flags)?;
                    if !notes.is_empty() {
                        let report = change.report.get_or_insert_with(|| {
                            format!(
                                "updated {}",
                                super::edit_source::display_path(ws.rel(&path))
                            )
                        });
                        report.push('\n');
                        report.push_str(&notes.join("\n"));
                    }
                    flags = hashline::transition_flags(text, &after, &flags);
                    current = Some(after);
                }
            }
            if entry.rename.is_some() {
                if index + 1 != count {
                    return Err("rename must be the last patch entry".into());
                }
                rename = entry.rename;
            }
        }
        change.seen_after = Some(flags);
        if let Some(current) = current {
            let persisted = source.persist_checked(
                ws,
                current,
                &path,
                change.seen_after.as_deref().unwrap_or(&[]),
            )?;
            change.after = Some(persisted.raw);
            change.projection = persisted.projection;
        } else {
            if source.raw_revision.is_some() {
                change.projection = Some(super::edit_source::Projection {
                    before: Some(source.text.into_owned()),
                    after: String::new(),
                    kind_changed: false,
                });
            }
            change.after = None;
        }
        if change.before.is_none() && change.after.is_none() {
            return Err("patch produces no file or source change".into());
        }
        if let Some(rename) = rename {
            let dest = ws.resolve(&rename)?;
            if sources.contains(&dest) || changes.contains_key(&dest) {
                return Err("rename destination is another patch source/target".into());
            }
            let mut destination = Change::load(ws, dest.clone(), change.after.take())?;
            if destination.before.is_some() {
                return Err("rename destination exists; no clobber permitted".into());
            }
            if destination.after.is_none() {
                return Err("cannot rename a deleted file".into());
            }
            destination.seen_after = change.seen_after.take();
            let kind_changed = super::edit_source::notebook_path(&path)
                != super::edit_source::notebook_path(&dest);
            if let Some(projection) = change.projection.take() {
                change.projection = Some(super::edit_source::Projection {
                    before: projection.before,
                    after: String::new(),
                    kind_changed: false,
                });
                destination.projection = Some(super::edit_source::Projection {
                    before: None,
                    after: projection.after,
                    kind_changed,
                });
            } else if kind_changed {
                destination.projection = Some(super::edit_source::Projection {
                    before: None,
                    after: String::new(),
                    kind_changed: true,
                });
            }
            if let Some(original) = &change.before {
                edit_transaction::inherit_permissions(&mut destination, original);
            }
            changes.insert(dest, destination);
        }
        changes.insert(path, change);
    }
    Ok(changes.into_values().collect())
}
fn sloppy_plan(
    ws: &Workspace,
    sections: Vec<super::edit_native::sloppy::types::SloppySection>,
    preview_no_change: Option<&std::cell::Cell<bool>>,
) -> Result<Vec<Change>, String> {
    use super::edit_native::{
        error::EditError,
        sloppy::apply::{ApplyContext, apply_sloppy},
    };
    let mut grouped: BTreeMap<PathBuf, String> = BTreeMap::new();
    for section in sections {
        let path = ws.resolve(&section.path)?;
        let body = grouped.entry(path).or_default();
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&section.body);
    }
    let mut changes = Vec::new();
    for (path, body) in grouped {
        let mut change = Change::load(ws, path, None)?;
        let text = change.require_text()?;
        let source = super::edit_source::Source::load(&change.path, Some(text))?;
        let lf = &source.text;
        let mut notes = Vec::new();
        let witness = |start, end| {
            source
                .check_ranges(ws, &change.path, &[line_range(lf, start, end)])
                .map_err(EditError::apply)
        };
        let after = apply_sloppy(
            lf,
            &body,
            ApplyContext {
                path: &super::edit_source::display_path(ws.rel(&change.path)),
                notes: &mut notes,
                store: &ws.native_edits,
                canonical: &change.path,
                streaming: false,
                witness: &witness,
            },
        )
        .map_err(|e| {
            if matches!(&e, EditError::NoChange(_))
                && let Some(flag) = preview_no_change
            {
                flag.set(true);
            }
            e.to_string()
        })?;
        if after.len() > edit_transaction::MAX_FILE {
            return Err("edited source exceeds 4 MiB".into());
        }
        let flags = hashline::transition_flags(lf, &after, &source.flags(ws, &change.path));
        let persisted = source.persist_checked(ws, after, &change.path, &flags)?;
        change.projection = persisted.projection;
        change.seen_after = Some(flags);
        change.after = Some(persisted.raw);
        change.report = Some(format!(
            "updated {}{}",
            super::edit_source::display_path(ws.rel(&change.path)),
            if notes.is_empty() {
                String::new()
            } else {
                format!("\n{}", notes.join("\n"))
            }
        ));
        changes.push(change);
    }
    Ok(changes)
}
pub(crate) fn prepare(ws: &Workspace, args: &Value) -> Result<Vec<Change>, String> {
    prepare_with_preview(ws, args, None)
}
pub(crate) fn prepare_with_preview(
    ws: &Workspace,
    args: &Value,
    preview_no_change: Option<&std::cell::Cell<bool>>,
) -> Result<Vec<Change>, String> {
    ws.edit_settings.validate()?;
    validate_shape(ws.edit_mode, args)?;
    match ws.edit_mode {
        EditMode::Replace => replace_plan(ws, args, preview_no_change),
        EditMode::Patch => entries_plan(ws, patch_entries(args)?, true),
        EditMode::ApplyPatch => {
            entries_plan(ws, apply_entries(req_str(args, "input")?, false)?, false)
        }
        EditMode::Sloppy => sloppy_plan(
            ws,
            sloppy_sections(req_str(args, "input")?)?,
            preview_no_change,
        ),
        EditMode::Hashline => Err("hashline preparation uses its snapshot engine".into()),
    }
}
/// Preview of a partially streamed edit: an incomplete apply_patch envelope is
/// accepted instead of failing the preview.
pub(crate) fn prepare_streaming_preview(
    ws: &Workspace,
    args: &Value,
    preview_no_change: Option<&std::cell::Cell<bool>>,
) -> Result<Vec<Change>, String> {
    if ws.edit_mode != EditMode::ApplyPatch {
        return prepare_with_preview(ws, args, preview_no_change);
    }
    ws.edit_settings.validate()?;
    validate_shape(ws.edit_mode, args)?;
    entries_plan(ws, apply_entries(req_str(args, "input")?, true)?, false)
}
/// Authorizes the paths an edit is about to touch.
type PathAuthorizer<'a> = &'a dyn Fn(&[PathBuf]) -> Result<(), String>;
#[derive(Default)]
pub(crate) struct Execution<'a> {
    pub repair: Option<super::syntax::RepairCallback<'a>>,
    pub authorize: Option<PathAuthorizer<'a>>,
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    pub preview_no_change: Option<&'a std::cell::Cell<bool>>,
}
impl Execution<'_> {
    pub fn mark_no_change(&self) {
        if let Some(flag) = self.preview_no_change {
            flag.set(true);
        }
    }
    pub fn authorize(&self, paths: &[PathBuf]) -> Result<(), String> {
        self.check_cancel()?;
        if let Some(authorize) = self.authorize {
            authorize(paths)?;
        }
        self.check_cancel()
    }
    pub fn check_cancel(&self) -> Result<(), String> {
        if self
            .cancel
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
        {
            Err("edit cancelled before transaction commit".into())
        } else {
            Ok(())
        }
    }
}
pub(crate) fn finish(ws: &Workspace, changes: Vec<Change>, commit: bool) -> Result<String, String> {
    finish_with(ws, changes, commit, &Execution::default())
}
pub(crate) fn finish_with(
    ws: &Workspace,
    mut changes: Vec<Change>,
    commit: bool,
    execution: &Execution<'_>,
) -> Result<String, String> {
    let _lease = if commit {
        Some(super::document_transaction::Lease::acquire(
            changes.iter().map(|change| change.path.clone()),
        )?)
    } else {
        None
    };
    let mut warnings = if commit {
        super::document_transaction::rebase(ws, &mut changes)?
    } else {
        Vec::new()
    };
    let mut report = Vec::new();
    let mut records = Vec::new();
    for change in &mut changes {
        execution.check_cancel()?;
        let before = change.before.as_ref().map(|x| x.text.as_str());
        generated_guard(ws, &change.path, before)?;
        if let Some(projected_before) = change
            .projection
            .as_ref()
            .and_then(|projection| projection.before.as_deref())
        {
            generated_guard(ws, &change.path, Some(projected_before))?;
        }
        if let Some(after) = &change.after {
            if before == Some(after.as_str()) && !change.authored_whole {
                execution.mark_no_change();
                return Err(format!(
                    "{} would not change; nothing was written",
                    ws.rel(&change.path)
                ));
            }
            if commit && change.projection.is_none() {
                let carrier = if change.authored_whole {
                    super::edit_source::Source::plain(before.unwrap_or(""))
                } else {
                    super::edit_source::Source::load(&change.path, before)?
                };
                let proposed = super::edit_source::Source::plain(after);
                let source = super::syntax::prepare_edit(
                    ws,
                    &change.path,
                    &carrier.text,
                    &proposed.text,
                    change.seen_after.as_deref(),
                    execution.repair,
                )?;
                if let Some(flags) = &change.seen_after {
                    change.seen_after = Some(hashline::transition_flags(
                        &proposed.text,
                        &hashline::normalize_body(&source.text),
                        flags,
                    ));
                }
                if source.text.as_str() != proposed.text.as_ref() {
                    let repaired = if source.text.contains("\r\n") {
                        source.text.replace("\r\n", "\n")
                    } else {
                        source.text
                    };
                    change.after = Some(proposed.persist(repaired, &change.path)?.raw);
                }
                warnings.extend(source.warnings);
                if let Some(record) = source.blackbox {
                    records.push(record);
                }
                if before == change.after.as_deref() && !change.authored_whole {
                    execution.mark_no_change();
                    return Err(format!(
                        "{} has no change after parse repair; nothing was written\n{}",
                        ws.rel(&change.path),
                        warnings.join("\n")
                    ));
                }
            }
        }
        let summary = change.report.clone().unwrap_or_else(|| {
            format!(
                "{} {}",
                if change.after.is_none() {
                    "deleted"
                } else if change.before.is_none() {
                    "created"
                } else {
                    "updated"
                },
                super::edit_source::display_path(ws.rel(&change.path))
            )
        });
        let logical_before = change
            .projection
            .as_ref()
            .and_then(|projection| projection.before.as_deref())
            .unwrap_or(before.unwrap_or(""));
        let logical_after = change
            .projection
            .as_ref()
            .map(|projection| projection.after.as_str())
            .unwrap_or(change.after.as_deref().unwrap_or(""));
        let kind_changed = change
            .projection
            .as_ref()
            .is_some_and(|projection| projection.kind_changed);
        let header = change
            .after
            .as_ref()
            .filter(|_| !kind_changed)
            .map_or(String::new(), |raw| {
                format!(
                    "[{}#{}]\n[Source raw SHA256:{}]\n{}",
                    super::edit_source::display_path(ws.rel(&change.path)),
                    hashline::tag_of(&hashline::normalize(logical_after)),
                    super::edit_source::revision_hex(raw),
                    if change.projection.is_some() {
                        "[Notebook cell-source projection]\n"
                    } else {
                        ""
                    }
                )
            });
        report.push(format!("{header}{summary}{}",if change.authored_whole{String::new()}else if kind_changed{"\nCarrier extension changed; raw bytes were retained. Read the destination before further edits.".into()}else{super::diff::preview(logical_before,logical_after)}));
    }
    if commit {
        execution.authorize(
            &changes
                .iter()
                .map(|change| change.path.clone())
                .collect::<Vec<_>>(),
        )?;
        warnings.extend(edit_transaction::commit(ws, &changes)?);
        let mut store = ws.store();
        for change in &changes {
            if let Some(after) = &change.after {
                if let Some(projection) = &change.projection {
                    if projection.kind_changed {
                        store.forget(&change.path);
                        continue;
                    }
                    let flags = change
                        .seen_after
                        .as_ref()
                        .filter(|flags| {
                            flags.len() == hashline::split_lines(&projection.after).len()
                        })
                        .map_or_else(
                            || vec![false; hashline::split_lines(&projection.after).len()],
                            Clone::clone,
                        );
                    store.record_version(
                        &change.path,
                        &projection.after,
                        Some(super::edit_source::revision(after)),
                        &hashline::ranges_from_flags(&flags),
                    );
                } else if change.authored_whole {
                    store.record_all(&change.path, &hashline::normalize(after));
                } else if let Some(flags) = &change.seen_after {
                    store.record_flags(&change.path, after, flags);
                } else {
                    store.record_transition(
                        &change.path,
                        change
                            .before
                            .as_ref()
                            .map(|b| b.text.as_str())
                            .unwrap_or(""),
                        after,
                    );
                }
            } else {
                store.forget(&change.path);
            }
        }
        drop(store);
        for record in records {
            if let Err(error) = super::syntax::record_blackbox(ws, &record) {
                warnings.push(format!(
                    "Edit committed, but parse-regression recording failed: {error}"
                ));
            }
        }
    }
    let mut output = report.join("\n");
    if !commit {
        output.insert_str(0, "Preview only; no files modified.\n");
    }
    for note in warnings {
        output.push('\n');
        output.push_str(&note);
    }
    Ok(output)
}
pub(crate) fn edit(ws: &Workspace, args: &Value) -> Result<String, String> {
    finish(ws, prepare(ws, args)?, true)
}
