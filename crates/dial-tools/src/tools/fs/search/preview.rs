//! Opt-in declaration reads for finalized grep pages; never alter match ordering or coverage.
use super::{GrepFile, MAX_GREP_OUTPUT, Read, read_file};
use crate::tools::{Workspace, code::navigation};
use ast_grep_core::Language as _;

pub(super) const UNAVAILABLE: &str = "[grep preview: unavailable (requires local lines output and a supported enclosing declaration)]\n";
const NOTICE: &str = "[grep preview: bounded to 3 files among the first 16 matching files, 40 lines/file and 6 KiB; unavailable, screened, changed or budget-limited declarations may be omitted; search coverage above is unchanged]\n";
pub(super) const RESERVE: usize = NOTICE.len();

pub(super) fn render(
    ws: &Workspace,
    files: &[GrepFile],
    page: &str,
    returned_body: usize,
) -> String {
    let mut targets = Vec::new();
    for file in files.iter().take(16) {
        let Some(source) = &file.source else { continue };
        let rel = ws.rel(&file.path);
        // Only actual matching rows retained by final pagination qualify, not context or
        // candidates discarded by the output cap. Multiline summary rows use the same prefix.
        let Some(number) = page[..returned_body].lines().find_map(|line| {
            let rest = line.strip_prefix(&format!("{rel}:"))?;
            rest.split_once(':')?.0.parse::<usize>().ok()
        }) else {
            continue;
        };
        if ast_grep_language::SupportLang::from_path(&file.path).is_none() {
            continue;
        }
        // A declaration location from an old search snapshot must not select a new revision.
        // The ordinary navigation reader independently validates freshness again before
        // publishing complete numbered rows and their edit witnesses.
        let Ok(path) = ws.resolve_read(&rel) else {
            continue;
        };
        let mut current = Vec::new();
        if !matches!(read_file(&path, &mut current), Read::Text(_)) || current != source.as_bytes()
        {
            continue;
        }
        let declaration = navigation::declarations(&file.path, source)
            .into_iter()
            .filter(|(start, end, _)| *start <= number && number <= *end)
            .min_by_key(|(start, end, _)| end - start);
        if let Some((start, end, _)) = declaration {
            targets.push((rel, start, end));
        }
    }
    let hits: Vec<_> = targets
        .iter()
        .map(|(path, start, end)| (path.as_str(), *start, *end, ""))
        .collect();
    let mut out = NOTICE.to_owned();
    // Reserve before the ordinary reader records anything: never clip a witnessed excerpt.
    let budget = MAX_GREP_OUTPUT
        .saturating_sub(page.len() + out.len())
        .min(6 * 1024 - out.len());
    out.push_str(&navigation::source_section(ws, &hits, budget));
    out
}

/// Only restore preview paths that execution could match in the original search scope and glob.
pub(super) fn source_targets(
    ws: &Workspace,
    args: &serde_json::Value,
    glob: Option<&super::Glob>,
    text: &str,
) -> Vec<String> {
    let Ok(scopes) = super::resolve_scopes(ws, args, glob) else {
        return Vec::new();
    };
    navigation::source_paths(ws, text)
        .into_iter()
        .filter(|path| {
            ws.resolve_read(path).is_ok_and(|resolved| {
                // The walker emits canonical spellings, never a symlink. A retained header
                // must not grant witnesses to a replacement target reached through an alias.
                if ws.rel(&resolved) != *path {
                    return false;
                }
                scopes.roots.iter().any(|scope| {
                    resolved.starts_with(scope)
                        && glob.is_none_or(|g| g.matches_path(&resolved, g.pattern_root(ws, scope)))
                })
            })
        })
        .collect()
}
