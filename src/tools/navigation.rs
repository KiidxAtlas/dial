//! Deterministic repository retrieval: lexical evidence and actual syntax declarations, not embeddings.
mod semantic;
use super::ext::{Tool, ToolContext};
use super::walk::Walk;
use super::{ToolOutput, Workspace, opt_str, opt_u64, req_str, tool_schema};
use crate::config::types::Config;
use ast_grep_core::{AstGrep, Language};
use ast_grep_language::SupportLang;
pub use semantic::{SemanticRequest, semantic_request};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, io::Read, path::Path, sync::Arc};

const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_TOTAL: u64 = 64 * 1024 * 1024;
const MAX_OUTPUT: usize = 20 * 1024;

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Find {
        lsp: super::lsp::LspTool::new(&config.lsp),
    })]
}
struct Find {
    lsp: super::lsp::LspTool,
}
impl Tool for Find {
    fn name(&self) -> &str {
        "find"
    }
    fn schema(&self) -> Value {
        tool_schema(
            "find",
            "Find implementation evidence from a behavior description. Behavior mode defaults to real configured-model semantic reranking of bounded lexical/syntax candidates, including declarations with no matching query terms; source evidence stays grounded to indexed paths and spans. Set ranking=lexical for deterministic local retrieval. No embeddings or semantic-probability claims. Exact mode matches literal text case-sensitively (including multiline) deterministically. Optional lsp_path requests actual language-server metadata.",
            json!({
                "query": {"type":"string", "description":"Behavior description, quoted mandatory phrases, or exact text"},
                "path": {"type":"string", "description":"File/directory scope, default workspace"},
                "mode": {"type":"string", "enum":["behavior", "exact"]},
                "ranking": {"type":"string", "enum":["semantic", "lexical"], "description":"Behavior mode defaults to configured-model semantic reranking; lexical explicitly selects deterministic local retrieval. Exact mode is always deterministic."},
                "grep_keywords": {"type":"array", "items":{"type":"string"}, "description":"Optional known literal terms to boost in behavior mode"},
                "lsp_path": {"type":"string", "description":"Optional source file selecting a real language server for supplementary workspace-symbol metadata; startup needs approval"},
                "lsp_query": {"type":"string", "description":"Optional exact symbol query for language-server metadata, defaults to query"},
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
                    "semantic find requires the agent's configured-model reranking integration; use ranking=lexical only when deterministic lexical retrieval is intended",
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
        let mut error = false;
        if let Some(args) = supplemental {
            let metadata = self.lsp.call(ctx, &args);
            error = metadata.is_error;
            text.push_str(
                "\n[Supplementary real language-server metadata; not part of lexical ranking]\n",
            );
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
            data: None,
            text,
            is_error: error,
        }
    }
}
pub(crate) fn lsp_args(args: &Value) -> Result<Option<Value>, String> {
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
    context: String,
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
    let mut walk = Walk::new(ws);
    let (mut bytes, mut skipped, mut scanned) = (0u64, 0usize, 0usize);
    let mut byte_capped = false;
    let mut results: Vec<Evidence> = Vec::new();
    walk.run(&start, &mut |path| {
        let Ok(meta) = fs::metadata(path) else {
            skipped += 1;
            return true;
        };
        if meta.len() > MAX_FILE {
            skipped += 1;
            return true;
        }
        if bytes + meta.len() > MAX_TOTAL {
            byte_capped = true;
            return false;
        }
        let Ok(file) = fs::File::open(path) else {
            skipped += 1;
            return true;
        };
        let mut data = Vec::new();
        if file.take(MAX_FILE + 1).read_to_end(&mut data).is_err() || data.len() as u64 > MAX_FILE {
            skipped += 1;
            return true;
        }
        bytes += data.len() as u64;
        let Ok(text) = std::str::from_utf8(&data) else {
            skipped += 1;
            return true;
        };
        if text.contains('\0') {
            skipped += 1;
            return true;
        }
        scanned += 1;
        let rel = ws.rel(path);
        let lines: Vec<_> = text.lines().collect();
        if exact {
            let (mut previous_offset, mut line) = (0, 1);
            for (offset, matched) in text.match_indices(query).take(100) {
                line += text[previous_offset..offset]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count();
                previous_offset = offset;
                let end = line + matched.bytes().filter(|b| *b == b'\n').count();
                results.push(Evidence {
                    path: rel.clone(),
                    start: line,
                    end,
                    score: 1,
                    kind: "exact-text".into(),
                    snippet: snippet(&lines, line, end),
                    context: String::new(),
                });
            }
        } else {
            let path_terms = tokens(&rel);
            let mut spans = declarations(path, text);
            // Small overlapping windows retain comments/docs and unsupported languages as evidence.
            for line in (1..=lines.len()).step_by(8) {
                spans.push((line, (line + 15).min(lines.len()), "lexical-window".into()));
            }
            for (start, end, kind) in spans {
                let body = lines[start - 1..end.min(lines.len())].join("\n");
                let folded = body.to_lowercase();
                if !phrases.iter().all(|p| folded.contains(p)) {
                    continue;
                }
                let body_terms = tokens(&body);
                let hits = terms.intersection(&body_terms).count();
                let literal_hits = keywords
                    .iter()
                    .filter(|k| body.contains(k.as_str()))
                    .count();
                if hits == 0 && literal_hits == 0 && phrases.is_empty() && !semantic {
                    continue;
                }
                let title_terms = tokens(lines[start - 1]);
                let score = hits * 10
                    + terms.intersection(&title_terms).count() * 8
                    + terms.intersection(&path_terms).count() * 3
                    + literal_hits * 15
                    + phrases.len() * 20
                    + usize::from(kind != "lexical-window") * 2;
                let context = if semantic {
                    body.chars().take(2400).collect()
                } else {
                    String::new()
                };
                results.push(Evidence {
                    path: rel.clone(),
                    start,
                    end,
                    score,
                    kind,
                    snippet: snippet(&lines, start, end),
                    context,
                });
                if results.len() > 2000 {
                    results.sort_by(rank);
                    results.truncate(1000);
                }
            }
        }
        if results.len() > 1000 {
            results.sort_by(rank);
            results.truncate(1000);
        }
        true
    });
    results.sort_by(rank);
    let coverage = format!(
        "[{scanned} UTF-8 files scanned; {skipped} binary/oversize/unreadable files skipped; {bytes} bytes; walk truncated={}; corpus byte cap={byte_capped}; at most 100 exact hits/file and 1000 candidates retained]\n",
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
    let mut chosen: Vec<(&str, usize, usize)> = vec![];
    for hit in &retrieved.results {
        if chosen.len() >= retrieved.limit {
            break;
        }
        if !retrieved.exact
            && chosen
                .iter()
                .any(|(p, s, e)| *p == hit.path && *s <= hit.end && hit.start <= *e)
        {
            continue;
        }
        let row = format!(
            "{}:{}-{} score={} [{}] {}\n",
            hit.path, hit.start, hit.end, hit.score, hit.kind, hit.snippet
        );
        if out.len() + row.len() > MAX_OUTPUT {
            break;
        }
        out.push_str(&row);
        chosen.push((&hit.path, hit.start, hit.end));
    }
    if chosen.is_empty() {
        out.push_str("No matching evidence.\n");
    }
    out.push_str(&format!(
        "[{} results]\n{}",
        chosen.len(),
        retrieved.coverage
    ));
    Ok(out)
}
fn rank(a: &Evidence, b: &Evidence) -> std::cmp::Ordering {
    b.score
        .cmp(&a.score)
        .then(a.path.cmp(&b.path))
        .then(a.start.cmp(&b.start))
        .then(a.end.cmp(&b.end))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_declarations_and_literal_multiline_deterministically() {
        let (dir, ws) = crate::tools::testkit::workspace();
        fs::write(
            dir.path().join("retry.rs"),
            "// reconnect network after a timeout\npub fn reconnect_network() {\n    retry();\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("other.py"),
            "def render_page():\n    pass\n",
        )
        .unwrap();
        let args = json!({"query":"network reconnect timeout"});
        let first = find(&ws, &args).unwrap();
        assert_eq!(first, find(&ws, &args).unwrap());
        assert!(first.contains("retry.rs:1-4"));
        let exact = find(&ws, &json!({"query":"retry();\n}","mode":"exact"})).unwrap();
        assert!(exact.contains("retry.rs:3-4"));
        assert!(!exact.contains("other.py:"));
    }
    #[test]
    fn language_declarations_and_optional_server_contract() {
        let (dir, ws) = crate::tools::testkit::workspace();
        fs::write(
            dir.path().join("network.rs"),
            "pub fn reconnect_network() {\n    retry();\n}\n",
        )
        .unwrap();
        let evidence = find(&ws, &json!({"query":"reconnect network"})).unwrap();
        assert!(evidence.contains("network.rs:1-3"));
        assert!(evidence.contains("function_item"));
        assert!(lsp_args(&json!({"query":"timeout", "lsp_query":"timeout"})).is_err());
        let args = lsp_args(&json!({"query":"network behavior", "lsp_path":"network.rs", "lsp_query":"reconnect_network"})).unwrap().unwrap();
        assert_eq!(args["action"], "workspace_symbols");
        assert_eq!(args["query"], "reconnect_network");
        let tool = tools(&Config::default()).remove(0);
        assert!(!tool.needs_approval(&json!({"query":"network"})));
        assert_eq!(tool.schema()["function"]["name"], "find");
    }
    #[test]
    fn confines_scope_and_honors_quoted_phrases() {
        let (dir, ws) = crate::tools::testkit::workspace();
        fs::write(dir.path().join("a.txt"), "network timeout\n").unwrap();
        assert!(find(&ws, &json!({"query":"timeout", "path":"../"})).is_err());
        assert!(
            find(&ws, &json!({"query":"\"timeout network\""}))
                .unwrap()
                .contains("No matching")
        );
        assert!(find(&ws, &json!({"query":"\"unclosed"})).is_err());
    }
}
