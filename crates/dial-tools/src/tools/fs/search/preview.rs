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

/// Rehydration only admits preview paths inside the original local search scope and glob.
pub(super) fn source_targets(ws: &Workspace, args: &serde_json::Value, text: &str) -> Vec<String> {
    if args["preview"] != true || super::OutputMode::parse(args) != Ok(super::OutputMode::Lines) {
        return Vec::new();
    }
    let Ok(roots) = super::scopes(ws, args) else {
        return Vec::new();
    };
    let Ok(glob) = crate::tools::opt_str(args, "glob")
        .and_then(|pattern| pattern.map(super::Glob::new).transpose())
    else {
        return Vec::new();
    };
    navigation::source_paths(ws, text)
        .into_iter()
        .filter(|path| {
            ws.resolve_read(path).is_ok_and(|resolved| {
                roots.iter().any(|root| resolved.starts_with(root))
                    && glob.as_ref().is_none_or(|glob| {
                        let root = ws.root_containing(&resolved).unwrap_or(ws.root());
                        glob.matches(
                            &resolved
                                .strip_prefix(root)
                                .unwrap_or(&resolved)
                                .to_string_lossy(),
                        )
                    })
            })
        })
        .collect()
}
