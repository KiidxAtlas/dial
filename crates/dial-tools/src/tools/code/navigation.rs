//! Deterministic repository retrieval: lexical evidence and actual syntax declarations, not embeddings.
mod cache;
mod related;
mod semantic;
use crate::config::types::Config;
use crate::tools::fs::walk::Walk;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace, opt_str, opt_u64, req_str, tool_schema};
use ast_grep_core::{AstGrep, Language};
use ast_grep_language::SupportLang;
pub use semantic::{SemanticRequest, semantic_request};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, BinaryHeap},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_TOTAL: u64 = 64 * 1024 * 1024;
const MAX_OUTPUT: usize = 20 * 1024;

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Find {
        lsp: Arc::new(super::lsp::LspTool::new(&config.lsp)),
    })]
}
struct Find {
    lsp: Arc<dyn Tool>,
}
impl Tool for Find {
    fn fork_session(&self) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            lsp: self.lsp.fork_session()?,
        }))
    }
    fn name(&self) -> &str {
        "find"
    }
    fn schema(&self) -> Value {
        tool_schema(
            "find",
            "Find implementation evidence from a behavior description. Behavior mode defaults to deterministic local retrieval over lexical/syntax candidates; every row states its attribution (via=lexical, via=exact-text or via=model-rerank). By default (related=true in behavior mode) the top hits also get a bounded related-evidence bundle: the enclosing owner and signature type definitions (syntax spans), plus call-sites, mentions and config references found by whole-word lexical match (not a resolved call graph or symbol resolution). Related rows are navigation aids, never edit witnesses: read the lines before editing. Set related=false for hit rows only. Set ranking=semantic to have the configured model rerank bounded candidates (one extra model request), which can surface declarations with no matching query terms; evidence stays grounded to indexed paths and spans. Exact mode matches literal text case-sensitively (including multiline) deterministically and omits the bundle unless related=true. Optional lsp_path requests actual language-server metadata, the only resolved-symbol source.",
            json!({
                "query": {"type":"string", "description":"Behavior description, quoted mandatory phrases, or exact text"},
                "path": {"type":"string", "description":"File/directory scope, default workspace"},
                "mode": {"type":"string", "enum":["behavior", "exact"]},
                "ranking": {"type":"string", "enum":["lexical", "semantic"], "description":"Behavior mode defaults to lexical; semantic adds one configured-model reranking request. Exact mode is always deterministic."},
                "grep_keywords": {"type":"array", "items":{"type":"string"}, "description":"Optional known literal terms to boost in behavior mode"},
                "lsp_path": {"type":"string", "description":"Optional source file selecting a real language server for supplementary workspace-symbol metadata; startup needs approval"},
                "lsp_query": {"type":"string", "description":"Optional exact symbol query for language-server metadata, defaults to query"},
                "related": {"type":"boolean", "description":"Append bounded related owner/type/call-site/config evidence for the top hits (lexical provenance, not resolved). Defaults to true in behavior mode and false in exact mode."},
                "limit": {"type":"integer", "minimum":1, "maximum":100}
            }),
            &["query"],
        )
    }
    fn needs_approval(&self, args: &Value) -> bool {
        lsp_args(args)
            .ok()
            .flatten()
            .is_some_and(|args| self.lsp.needs_approval(&args))
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        match semantic::enabled(args) {
            Ok(true) => {
                return ToolOutput::err(
                    "semantic find requires the agent's configured-model reranking integration; omit ranking (lexical) for deterministic local retrieval",
                );
            }
            Ok(false) => {}
            Err(e) => return ToolOutput::err(e),
        }
        let supplemental = match lsp_args(args) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        };
        let mut text = match find(ctx.workspace, args) {
            Ok(text) => text,
            Err(e) => return ToolOutput::err(e),
        };
        if let Some(args) = supplemental {
            let metadata = self.lsp.call(ctx, &args);
            text.push_str(
                "\n[Supplementary real language-server metadata; not part of lexical ranking]\n",
            );
            // Metadata is auxiliary: a failing server must not turn the successful lexical rows into
            // an error, so the failure is stated inline and the find result stays successful.
            if metadata.is_error {
                text.push_str(
                    "[language-server metadata failed; the lexical results above are unaffected]\n",
                );
            }
            let available = MAX_OUTPUT.saturating_sub(text.len());
            let mut end = metadata.text.len().min(available);
            while !metadata.text.is_char_boundary(end) {
                end -= 1;
            }
            text.push_str(&metadata.text[..end]);
            if end < metadata.text.len() {
                text.push_str("\n[language-server metadata clipped]\n");
                if let Some(store) = ctx.workspace.artifacts() {
                    match store.retain(&metadata.text) {
                        Ok(uri) => {
                            text.push_str(&format!("[complete language-server metadata: {uri}]\n"))
                        }
                        Err(e) => {
                            return ToolOutput::err(format!(
                                "retaining language-server metadata: {e:#}"
                            ));
                        }
                    }
                }
            }
        }
        ToolOutput {
            execution: Default::default(),
            data: None,
            text,
            is_error: false,
        }
    }
}
pub fn lsp_args(args: &Value) -> Result<Option<Value>, String> {
    let Some(path) = opt_str(args, "lsp_path")? else {
        if opt_str(args, "lsp_query")?.is_some() {
            return Err("lsp_query requires lsp_path".into());
        }
        return Ok(None);
    };
    let query = opt_str(args, "lsp_query")?.unwrap_or(req_str(args, "query")?);
    if query.trim().is_empty() || query.len() > 4096 {
        return Err("language-server query must be 1..=4096 bytes and nonblank".into());
    }
    Ok(Some(
        json!({"action":"workspace_symbols", "path":path, "query":query}),
    ))
}

#[derive(Debug)]
struct Evidence {
    path: String,
    start: usize,
    end: usize,
    score: usize,
    kind: String,
    snippet: String,
    source_sha256: String,
    context: String,
    revision: Arc<Revision>,
}
#[derive(Debug)]
struct Revision {
    path: PathBuf,
    identity: cache::Identity,
    sha256: [u8; 32],
}
impl Evidence {
    fn is_current(&self) -> bool {
        fs::symlink_metadata(&self.revision.path).is_ok_and(|meta| {
            meta.is_file()
                && !meta.file_type().is_symlink()
                && cache::Identity::of(&meta) == self.revision.identity
        }) && self
            .revision
            .path
            .canonicalize()
            .is_ok_and(|path| path == self.revision.path)
    }
    fn verify_current(&self) -> bool {
        // Semantic rendering happens after a charged model step. Check identity
        // first, then re-read/hash once per selected file, never once per row.
        if !self.is_current() {
            return false;
        }
        let Ok(mut file) = fs::File::open(&self.revision.path) else {
            return false;
        };
        if !file
            .metadata()
            .is_ok_and(|meta| cache::Identity::of(&meta) == self.revision.identity)
        {
            return false;
        }
        let mut data = Vec::new();
        if (&mut file)
            .take(MAX_FILE + 1)
            .read_to_end(&mut data)
            .is_err()
            || data.len() as u64 > MAX_FILE
            || !file
                .metadata()
                .is_ok_and(|meta| cache::Identity::of(&meta) == self.revision.identity)
            || !self.is_current()
        {
            return false;
        }
        use sha2::{Digest, Sha256};
        let revision: [u8; 32] = Sha256::digest(&data).into();
        revision == self.revision.sha256
    }
}
struct Retrieval {
    results: Vec<Evidence>,
    limit: usize,
    exact: bool,
    coverage: String,
}
pub(crate) fn tokens(text: &str) -> BTreeSet<String> {
    // Split snake_case and camelCase before lowercasing; no language-model interpretation.
    let mut separated = String::with_capacity(text.len());
    let mut lower = false;
    for c in text.chars() {
        if lower && c.is_uppercase() {
            separated.push(' ');
        }
        separated.push(c);
        lower = c.is_lowercase();
    }
    separated
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() > 1)
        .map(|s| s.to_lowercase())
        .filter(|s| {
            !matches!(
                s.as_str(),
                "the"
                    | "and"
                    | "for"
                    | "with"
                    | "from"
                    | "that"
                    | "this"
                    | "where"
                    | "how"
                    | "does"
                    | "which"
                    | "into"
                    | "are"
                    | "when"
                    | "implement"
                    | "implementation"
            )
        })
        .collect()
}
fn quoted(query: &str) -> Result<Vec<String>, String> {
    let parts: Vec<_> = query.split('"').collect();
    if parts.len() % 2 == 0 {
        return Err("unclosed quoted phrase in behavior query".into());
    }
    Ok(parts
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|p| !p.is_empty())
        .map(|p| p.to_lowercase())
        .collect())
}
pub(crate) fn declarations(path: &Path, text: &str) -> Vec<(usize, usize, String)> {
    let Some(lang) = SupportLang::from_path(path) else {
        return vec![];
    };
    let Ok(tree) = AstGrep::try_new(text, lang) else {
        return vec![];
    };
    tree.root()
        .dfs()
        .filter_map(|node| {
            let kind = node.kind();
            if !matches!(
                kind.as_ref(),
                "function_item"
                    | "struct_item"
                    | "enum_item"
                    | "trait_item"
                    | "impl_item"
                    | "function_definition"
                    | "class_definition"
                    | "function_declaration"
                    | "method_declaration"
                    | "class_declaration"
                    | "interface_declaration"
                    | "type_declaration"
                    | "method_definition"
                    | "arrow_function"
            ) {
                return None;
            }
            let start = node.start_pos().line() + 1;
            let end = node.end_pos().line() + 1;
            Some((start, end, format!("{lang}:{}", kind)))
        })
        .take(4000)
        .collect()
}
/// Workspace mutation observers call this before changing a path. Cache hits
/// additionally verify a fresh read, so external writes need no observer.
pub(crate) fn invalidate(path: &Path) {
    cache::invalidate(path);
}

struct Ranked {
    evidence: Evidence,
    sequence: usize,
}
impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        rank(&self.evidence, &other.evidence).then(self.sequence.cmp(&other.sequence))
    }
}
fn retain(
    heap: &mut BinaryHeap<Ranked>,
    sequence: &mut usize,
    path: &str,
    start: usize,
    end: usize,
    score: usize,
    kind: &str,
    source_sha256: &str,
    revision: &Arc<Revision>,
    materialize: impl FnOnce() -> (String, String),
) {
    let order = *sequence;
    *sequence += 1;
    if heap.len() == 1000
        && heap.peek().is_some_and(|worst| {
            // Match the stable full-source sort, including original declaration/window order.
            worst.evidence.score > score
                || (worst.evidence.score == score
                    && (
                        worst.evidence.path.as_str(),
                        worst.evidence.start,
                        worst.evidence.end,
                        worst.sequence,
                    ) <= (path, start, end, order))
        })
    {
        return;
    }
    let (snippet, context) = materialize();
    if heap.len() == 1000 {
        heap.pop();
    }
    heap.push(Ranked {
        evidence: Evidence {
            path: path.to_owned(),
            start,
            end,
            score,
            kind: kind.to_owned(),
            snippet,
            source_sha256: source_sha256.to_owned(),
            context,
            revision: Arc::clone(revision),
        },
        sequence: order,
    });
}
fn intersects(lines: &[usize], start: usize, end: usize) -> bool {
    lines
        .get(lines.partition_point(|line| *line < start))
        .is_some_and(|line| *line <= end)
}
fn snippet(lines: &[&str], start: usize, end: usize) -> String {
    lines[start.saturating_sub(1)..end.min(lines.len())]
        .iter()
        .take(3)
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}
fn retrieve(ws: &Workspace, args: &Value, semantic: bool) -> Result<Retrieval, String> {
    let query = req_str(args, "query")?;
    if query.trim().is_empty() || query.len() > 4096 {
        return Err("query must be 1..=4096 bytes and nonblank".into());
    }
    let exact = match opt_str(args, "mode")?.unwrap_or("behavior") {
        "exact" => true,
        "behavior" => false,
        _ => return Err("mode must be behavior or exact".into()),
    };
    let limit = opt_u64(args, "limit")?.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err("limit must be 1..=100".into());
    }
    let terms = tokens(query);
    let phrases = if exact { vec![] } else { quoted(query)? };
    let keywords: Vec<String> = match args.get("grep_keywords") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(items)) if items.len() <= 32 => items
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 256)
                    .map(str::to_owned)
                    .ok_or(
                        "grep_keywords must contain nonempty strings up to 256 bytes".to_string(),
                    )
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("grep_keywords must be an array of at most 32 strings".into()),
    };
    if !semantic && !exact && terms.is_empty() && phrases.is_empty() && keywords.is_empty() {
        return Err("behavior query contains no searchable terms".into());
    }
    let start = ws.resolve(opt_str(args, "path")?.unwrap_or("."))?;
    if !start.exists() {
        return Err("search scope does not exist".into());
    }
    cache::prune(ws.root());
    let mut walk = Walk::new(ws);
    let (mut bytes, mut skipped, mut scanned) = (0u64, 0usize, 0usize);
    let mut byte_capped = false;
    let mut heap = BinaryHeap::new();
    let mut sequence = 0usize;
    walk.run(&start, &mut |path| {
        // Cache entries never enumerate files or bypass the current scope/ignore policy.
        let Ok(meta) = fs::symlink_metadata(path) else {
            invalidate(path);
            skipped += 1;
            return true;
        };
        if !meta.is_file()
            || meta.file_type().is_symlink()
            || !ws
                .resolve(&ws.rel(path))
                .is_ok_and(|resolved| resolved == path)
            || meta.len() > MAX_FILE
        {
            invalidate(path);
            skipped += 1;
            return true;
        }
        if bytes + meta.len() > MAX_TOTAL {
            byte_capped = true;
            return false;
        }
        let identity = cache::Identity::of(&meta);
        let Ok(mut file) = fs::File::open(path) else {
            invalidate(path);
            skipped += 1;
            return true;
        };
        let mut data = Vec::with_capacity(meta.len() as usize);
        // Validate both the open descriptor and its path: replacement, same-size
        // overwrite, deletion and symlink races cannot reuse old evidence.
        let stable = file
            .metadata()
            .is_ok_and(|meta| cache::Identity::of(&meta) == identity)
            && (&mut file)
                .take(MAX_FILE + 1)
                .read_to_end(&mut data)
                .is_ok()
            && data.len() as u64 <= MAX_FILE
            && data.len() as u64 == meta.len()
            && file
                .metadata()
                .is_ok_and(|meta| cache::Identity::of(&meta) == identity)
            && fs::symlink_metadata(path).is_ok_and(|meta| {
                !meta.file_type().is_symlink() && cache::Identity::of(&meta) == identity
            })
            && ws
                .resolve(&ws.rel(path))
                .is_ok_and(|resolved| resolved == path);
        if !stable {
            invalidate(path);
            skipped += 1;
            return true;
        }
        if bytes + data.len() as u64 > MAX_TOTAL {
            byte_capped = true;
            return false;
        }
        bytes += data.len() as u64;
        let Ok(text) = std::str::from_utf8(&data) else {
            invalidate(path);
            skipped += 1;
            return true;
        };
        if text.contains('\0') {
            invalidate(path);
            skipped += 1;
            return true;
        }
        scanned += 1;
        let rel = ws.rel(path);
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(&data);
        let source_sha256 = format!("{digest:x}");
        let revision = Arc::new(Revision {
            path: path.to_path_buf(),
            identity: identity.clone(),
            sha256: digest.into(),
        });
        if exact {
            let lines: Vec<_> = text.lines().collect();
            let (mut previous_offset, mut line) = (0, 1);
            for (offset, matched) in text.match_indices(query).take(100) {
                line += text[previous_offset..offset]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count();
                previous_offset = offset;
                let end = line + matched.bytes().filter(|b| *b == b'\n').count();
                retain(
                    &mut heap,
                    &mut sequence,
                    &rel,
                    line,
                    end,
                    1,
                    "exact-text",
                    &source_sha256,
                    &revision,
                    || (snippet(&lines, line, end), String::new()),
                );
            }
        } else {
            // Lexical retrieval retains its original global top 1000. Semantic
            // retrieval first ranks a bounded file-local pool, then contributes
            // at most four nonoverlapping spans to the bounded global pool.
            let mut file_heap = BinaryHeap::new();
            let target = if semantic { &mut file_heap } else { &mut heap };
            let source = cache::evidence(ws.root(), path, &rel, &identity, digest.into(), text);
            // Query tokens are compiled once; resolve their per-file postings once.
            // A range query is equivalent to tokens(joined full-source lines), without
            // copying, lowercasing or tokenizing each overlapping declaration/window.
            let postings: Vec<_> = terms
                .iter()
                .filter_map(|term| source.postings.get(term))
                .collect();
            let path_score = terms.intersection(&source.path_terms).count() * 3;
            let windows = (1..=source.lines())
                .step_by(8)
                .map(|line| (line, (line + 15).min(source.lines()), "lexical-window"));
            let spans = source
                .declarations
                .iter()
                .map(|(start, end, kind)| (*start, *end, kind.as_str()))
                .chain(windows);
            for (start, end, kind) in spans {
                if start == 0 || start > source.lines() {
                    continue;
                }
                let body = source.body(start, end.min(source.lines()));
                if !phrases.iter().all(|phrase| {
                    source
                        .folded(start, end.min(source.lines()))
                        .contains(phrase)
                }) {
                    continue;
                }
                let hits = postings
                    .iter()
                    .filter(|lines| intersects(lines, start, end))
                    .count();
                let literal_hits = keywords
                    .iter()
                    .filter(|keyword| body.contains(keyword.as_str()))
                    .count();
                if hits == 0 && literal_hits == 0 && phrases.is_empty() && !semantic {
                    continue;
                }
                let title_hits = postings
                    .iter()
                    .filter(|lines| intersects(lines, start, start))
                    .count();
                let score = hits * 10
                    + title_hits * 8
                    + path_score
                    + literal_hits * 15
                    + phrases.len() * 20
                    + usize::from(kind != "lexical-window") * 2;
                retain(
                    target,
                    &mut sequence,
                    &rel,
                    start,
                    end,
                    score,
                    kind,
                    &source_sha256,
                    &revision,
                    || {
                        let snippet = body
                            .split('\n')
                            .take(3)
                            .map(str::trim)
                            .collect::<Vec<_>>()
                            .join(" ")
                            .chars()
                            .take(300)
                            .collect();
                        let context = if semantic {
                            body.chars().take(2400).collect()
                        } else {
                            String::new()
                        };
                        (snippet, context)
                    },
                );
            }
            if semantic {
                let mut selected: Vec<Ranked> = Vec::new();
                for hit in file_heap.into_sorted_vec() {
                    if semantic::can_add_candidate(
                        selected.iter().map(|hit| &hit.evidence),
                        &hit.evidence,
                    ) {
                        selected.push(hit);
                    }
                    if selected.len() == semantic::MAX_PER_PATH {
                        break;
                    }
                }
                for hit in selected {
                    heap.push(hit);
                    if heap.len() > 1000 {
                        heap.pop();
                    }
                }
            }
        }
        true
    });
    let mut current = BTreeMap::new();
    let mut stale = 0usize;
    let results = heap
        .into_sorted_vec()
        .into_iter()
        .map(|hit| hit.evidence)
        .filter(|hit| {
            let valid = if let Some(valid) = current.get(&hit.revision.path) {
                *valid
            } else {
                let valid = hit.is_current();
                current.insert(hit.revision.path.clone(), valid);
                valid
            };
            if !valid {
                invalidate(&hit.revision.path);
                stale += 1;
            }
            valid
        })
        .collect();
    let coverage = format!(
        "[{scanned} UTF-8 files scanned; {skipped} binary/oversize/unreadable/changing files skipped; {bytes} bytes; walk truncated={}; corpus byte cap={byte_capped}; corpus limit=64 MiB (fresh read bytes, including cache reuse); file limit=2 MiB; at most 100 exact hits/file and 1000 candidates retained; {stale} stale candidates omitted at return; immutable evidence cache=32 MiB decoded/128 files, source SHA256 verified at every read boundary]\n",
        walk.truncated
    );
    Ok(Retrieval {
        results,
        limit: limit as usize,
        exact,
        coverage,
    })
}
fn find(ws: &Workspace, args: &Value) -> Result<String, String> {
    let retrieved = retrieve(ws, args, false)?;
    let mut out = String::from(
        "Retrieval: deterministic lexical + syntax declarations; scores are relevance points (no embeddings).\n",
    );
    let mut chosen: Vec<(&str, usize, usize, &str)> = vec![];
    for hit in &retrieved.results {
        if chosen.len() >= retrieved.limit {
            break;
        }
        if !retrieved.exact
            && chosen
                .iter()
                .any(|(p, s, e, _)| *p == hit.path && *s <= hit.end && hit.start <= *e)
        {
            continue;
        }
        let row = format!(
            "{}:{}-{} score={} via={} [{}] source_sha256={} {}\n",
            hit.path,
            hit.start,
            hit.end,
            hit.score,
            if retrieved.exact {
                "exact-text"
            } else {
                "lexical"
            },
            hit.kind,
            hit.source_sha256,
            hit.snippet
        );
        if out.len() + row.len() > MAX_OUTPUT {
            break;
        }
        out.push_str(&row);
        chosen.push((&hit.path, hit.start, hit.end, &hit.source_sha256));
    }
    if chosen.is_empty() {
        out.push_str("No matching evidence.\n");
    }
    let targets: Vec<(&str, usize, usize, &str)> = chosen.clone();
    out.push_str(&format!(
        "[{} results]\n{}",
        chosen.len(),
        retrieved.coverage
    ));
    out.push_str(&related_section(
        ws,
        args,
        retrieved.exact,
        &targets,
        MAX_OUTPUT.saturating_sub(out.len()),
    )?);
    Ok(out)
}
/// `related` defaults to true in behavior mode and false in exact mode; an explicit value wins.
fn related_enabled(args: &Value, exact: bool) -> Result<bool, String> {
    match args.get("related") {
        None | Some(Value::Null) => Ok(!exact),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err("argument \"related\" must be a boolean".into()),
    }
}
/// Bounded related-evidence bundle for the top hits; empty when disabled or no room remains.
fn related_section(
    ws: &Workspace,
    args: &Value,
    exact: bool,
    hits: &[(&str, usize, usize, &str)],
    budget: usize,
) -> Result<String, String> {
    if !related_enabled(args, exact)? || hits.is_empty() {
        return Ok(String::new());
    }
    let scope = ws.resolve(opt_str(args, "path")?.unwrap_or("."))?;
    Ok(related_for(ws, &scope, hits, budget))
}
fn related_for(
    ws: &Workspace,
    scope: &Path,
    hits: &[(&str, usize, usize, &str)],
    budget: usize,
) -> String {
    let targets: Vec<related::Target> = hits
        .iter()
        .take(related::MAX_TARGETS)
        .map(|(path, start, end, sha)| related::Target {
            path: (*path).to_owned(),
            start: *start,
            end: *end,
            sha256: (*sha).to_owned(),
        })
        .collect();
    related::render(ws, scope, &targets, budget.min(6 * 1024))
}
fn rank(a: &Evidence, b: &Evidence) -> std::cmp::Ordering {
    b.score
        .cmp(&a.score)
        .then(a.path.cmp(&b.path))
        .then(a.start.cmp(&b.start))
        .then(a.end.cmp(&b.end))
}

#[cfg(test)]
mod tests;
