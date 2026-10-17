//! `ast_search` and `ast_edit`: structural code search and rewrite with ast-grep patterns.
//!
//! Patterns are code with metavariables: `$VAR` matches one node, `$$$ARGS` any run of nodes.
//! A rewrite is a template over the same variables. Every file must be rewritable before
//! anything is written, then changes are committed atomically. Syntax diagnostics are advisory.

mod commit;
mod diff;
mod engine;
mod scan;
#[cfg(test)]
mod tests;

use super::ext::{Concurrency, Tool, ToolContext};
use super::{ToolOutput, Workspace, opt_str, opt_u64, req_str, tool_schema};
use crate::config::types::Config;
use commit::{Change, commit};
use scan::{Coverage, Queries, Target, scan};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::sync::Arc;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_PATTERN: usize = 4096;
/// Total size of original plus rewritten text held while validating an edit.
const MAX_EDIT_BYTES: usize = 64 * 1024 * 1024;

pub fn tools(_config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(AstSearch), Arc::new(AstEdit)]
}

struct AstSearch;
struct AstEdit;

fn lang_help() -> String {
    format!(
        "Language is inferred from each file's extension when `lang` is omitted; with `lang`, only that language's files are searched. Languages: {}.",
        engine::lang_names()
    )
}

fn pattern_arg(args: &Value) -> Result<&str, String> {
    let pattern = req_str(args, "pattern")?;
    if pattern.trim().is_empty() || pattern.len() > MAX_PATTERN {
        return Err(format!("pattern must be 1..={MAX_PATTERN} bytes of code"));
    }
    Ok(pattern)
}

impl Tool for AstSearch {
    fn name(&self) -> &str {
        "ast_search"
    }

    fn schema(&self) -> Value {
        tool_schema(
            "ast_search",
            &format!(
                "Structural code search with an ast-grep pattern: a code fragment where `$VAR` matches one syntax node and `$$$ARGS` \
                 any run of nodes (e.g. `foo($A, $$$REST)`, `fn $NAME($$$) -> Result<$T, $E>`). Unlike grep it ignores formatting and \
                 comments and understands syntax. Prints `path:line:col: line text` and the captured variables. {}",
                lang_help()
            ),
            json!({
                "pattern": {"type": "string", "description": "ast-grep pattern: valid code in the target language with metavariables"},
                "lang": {"type": "string", "description": "Language name; omit to infer from file extensions"},
                "path": {"type": "string", "description": "File or directory to search (default: the workspace)"},
                "limit": {"type": "integer", "description": format!("Maximum matches to show (default {DEFAULT_LIMIT}, max {MAX_LIMIT})")},
            }),
            &["pattern"],
        )
    }

    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }

    fn concurrency(&self, _args: &Value) -> Concurrency {
        // Pure search: no writes, no state shared with sibling calls.
        Concurrency::Shared
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match search(ctx.workspace, args) {
            Ok((text, observations)) => {
                let mut output = ToolOutput::ok(text);
                if !observations.is_empty() {
                    output.data = Some(json!({"source_observations":observations}));
                }
                output
            }
            Err(message) => ToolOutput::err(message),
        }
    }
}

impl Tool for AstEdit {
    fn name(&self) -> &str {
        "ast_edit"
    }

    fn schema(&self) -> Value {
        tool_schema(
            "ast_edit",
            &format!(
                "Structural code rewrite: every match of the ast-grep `pattern` is replaced by `rewrite`, a template that may use the \
                 pattern's variables (`$A`, `$$$ARGS`). Use it for renames and API migrations across files. Defaults to a dry run that \
                 shows a diff; set `dry_run` to false to apply. Applying is all-or-nothing if any file cannot be rewritten. \
                 Syntax diagnostics are advisory and never veto changes. After applying, `read` files again before using `edit`. {}",
                lang_help()
            ),
            json!({
                "pattern": {"type": "string", "description": "ast-grep pattern: valid code in the target language with metavariables"},
                "rewrite": {"type": "string", "description": "Replacement template; may use the pattern's variables. Empty deletes the match"},
                "lang": {"type": "string", "description": "Language name; omit to infer from file extensions"},
                "path": {"type": "string", "description": "File or directory to rewrite (default: the workspace)"},
                "dry_run": {"type": "boolean", "description": "Preview only (default true); false writes the files"},
            }),
            &["pattern", "rewrite"],
        )
    }

    fn needs_approval(&self, args: &Value) -> bool {
        !matches!(
            args.get("dry_run"),
            None | Some(Value::Null) | Some(Value::Bool(true))
        )
    }

    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match edit(ctx, args) {
            Ok(text) => ToolOutput::ok(text),
            Err(message) => ToolOutput::err(message),
        }
    }
}

fn search(
    ws: &Workspace,
    args: &Value,
) -> Result<(String, Vec<crate::runtime::fingerprint::Observation>), String> {
    let pattern = pattern_arg(args)?;
    let limit = opt_u64(args, "limit")?.map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, MAX_LIMIT));
    let target = Target::resolve(ws, opt_str(args, "path")?, opt_str(args, "lang")?)?;
    let mut queries = Queries::new(pattern, None);
    let mut out = String::new();
    let mut observations = Vec::new();
    let (mut shown, mut files_hit, mut full) = (0usize, 0usize, false);
    let coverage = scan(ws, &target, &mut queries, &mut |file, query| {
        if target.is_file() {
            observations.push(crate::runtime::fingerprint::Observation::text(
                &file.abs, &file.text,
            ));
        }
        let remaining = limit - shown;
        let Ok(hits) = query.find(&file.text, remaining) else {
            return true;
        };
        if !hits.is_empty() {
            files_hit += 1;
        }
        for hit in hits {
            let mut entry = format!("{}:{}:{}: {}\n", file.rel, hit.line, hit.column, hit.text);
            for (name, text) in &hit.captures {
                let _ = writeln!(entry, "    {name} = {text}");
            }
            if out.len() + entry.len() > MAX_OUTPUT {
                full = true;
                return false;
            }
            out.push_str(&entry);
            shown += 1;
        }
        if shown >= limit {
            full = true;
        }
        !full
    })?;
    if shown == 0 {
        out.push_str("no matches\n");
    } else {
        let _ = writeln!(out, "[{shown} matches in {files_hit} files]");
    }
    if full {
        out.push_str(
            "[stopped at the match limit or 64 KiB; narrow the search with path or raise limit]\n",
        );
    }
    out.push_str(&coverage.footer());
    Ok((out, observations))
}

/// A file whose rewrite succeeded, with its preview.
struct Planned {
    change: Change,
    replacements: usize,
    preview: String,
}

/// What `ast_edit` found: files to change and files it refused.
#[derive(Default)]
struct Plan {
    files: Vec<Planned>,
    refused: Vec<String>,
    bytes: usize,
}

fn plan(
    ws: &Workspace,
    pattern: &str,
    rewrite: &str,
    target: &Target,
) -> Result<(Plan, Coverage), String> {
    let mut queries = Queries::new(pattern, Some(rewrite));
    let mut plan = Plan::default();
    let mut over_budget = false;
    let coverage = scan(ws, target, &mut queries, &mut |file, query| {
        match query.rewrite(&file.text) {
            Ok(None) => {}
            Ok(Some(done)) => {
                plan.bytes += file.text.len() + done.text.len();
                if plan.bytes > MAX_EDIT_BYTES {
                    over_budget = true;
                    return false;
                }
                let preview = diff::preview(&file.rel, &file.text, &done.splices);
                plan.files.push(Planned {
                    replacements: done.splices.len(),
                    preview,
                    change: Change {
                        abs: file.abs,
                        rel: file.rel,
                        original: file.text,
                        updated: done.text,
                        perms: file.perms,
                    },
                });
            }
            Err(why) => plan.refused.push(format!("{}: {why}", file.rel)),
        }
        true
    })?;
    if over_budget {
        return Err(format!(
            "the rewrite touches more than {} MiB of source; narrow `path`",
            MAX_EDIT_BYTES >> 20
        ));
    }
    Ok((plan, coverage))
}

fn edit(ctx: &ToolContext, args: &Value) -> Result<String, String> {
    let ws = ctx.workspace;
    let pattern = pattern_arg(args)?;
    let rewrite = req_str(args, "rewrite")?;
    let dry_run = match args.get("dry_run") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err("argument \"dry_run\" must be a boolean".to_owned()),
    };
    let target = Target::resolve(ws, opt_str(args, "path")?, opt_str(args, "lang")?)?;
    let (plan, coverage) = plan(ws, pattern, rewrite, &target)?;
    let total: usize = plan.files.iter().map(|p| p.replacements).sum();
    if plan.files.is_empty() && plan.refused.is_empty() {
        return Ok(format!("no matches\n{}", coverage.footer()));
    }
    let refused = refusals(&plan.refused);
    if dry_run {
        let mut out = format!(
            "dry run: {total} replacements in {} files; nothing written (set dry_run to false to apply)\n",
            plan.files.len()
        );
        out.push_str(&refused);
        append_previews(&mut out, &plan.files);
        out.push_str(&coverage.footer());
        return Ok(out);
    }
    if !plan.refused.is_empty() {
        return Err(format!(
            "nothing was written: {} file(s) cannot be rewritten\n{refused}",
            plan.refused.len()
        ));
    }
    let targets: Vec<std::path::PathBuf> = plan
        .files
        .iter()
        .map(|planned| planned.change.abs.clone())
        .collect();
    ctx.authorize_mutation("ast_edit", &targets, false)?;
    let mut out = format!(
        "applied {total} replacements in {} files:\n",
        plan.files.len()
    );
    for planned in &plan.files {
        let _ = writeln!(out, "  {} ({})", planned.change.rel, planned.replacements);
    }
    append_previews(&mut out, &plan.files);
    let changes: Vec<Change> = plan.files.into_iter().map(|p| p.change).collect();
    for note in commit(ws, &changes)? {
        let _ = writeln!(out, "{note}");
    }
    for change in &changes {
        if let Some(advisory) = ws.syntax_advisory(&change.abs, &change.original, &change.updated) {
            let _ = writeln!(out, "{advisory}");
        }
    }
    out.push_str(
        "Earlier [path#TAG] headers for these files are stale; read them again before editing.\n",
    );
    out.push_str(&coverage.footer());
    Ok(out)
}

fn refusals(refused: &[String]) -> String {
    refused.iter().map(|r| format!("REFUSED {r}\n")).collect()
}

fn append_previews(out: &mut String, files: &[Planned]) {
    for (i, planned) in files.iter().enumerate() {
        if out.len() + planned.preview.len() > MAX_OUTPUT {
            let room = MAX_OUTPUT.saturating_sub(out.len());
            let cut = planned
                .preview
                .char_indices()
                .map(|(at, _)| at)
                .take_while(|&at| at <= room)
                .last()
                .unwrap_or(0);
            out.push_str(&planned.preview[..cut]);
            let _ = writeln!(
                out,
                "\n[preview truncated at 64 KiB: {} files not (fully) shown; narrow `path` to see them]",
                files.len() - i
            );
            return;
        }
        out.push_str(&planned.preview);
    }
}

#[cfg(test)]
mod registration;
