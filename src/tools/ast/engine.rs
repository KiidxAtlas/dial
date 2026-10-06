//! Matching and rewriting of a single source text with ast-grep. No file system access.

use ast_grep_core::meta_var::MetaVariable;
use ast_grep_core::replacer::TemplateFix;
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::{AstGrep, Language, NodeMatch, Pattern};
use ast_grep_language::SupportLang;

type Doc = StrDoc<SupportLang>;

const MAX_CAPTURE_CHARS: usize = 120;
const MAX_LINE_CHARS: usize = 300;

/// A pattern (and optional rewrite template) compiled for one language.
pub(super) struct Query {
    lang: SupportLang,
    pattern: Pattern,
    template: Option<TemplateFix>,
}

/// One structural match.
pub(super) struct Hit {
    /// 1-based line and character column of the match start.
    pub line: usize,
    pub column: usize,
    /// The source line the match starts on, bounded.
    pub text: String,
    /// `($NAME, text)` pairs in name order; multi captures are named `$$$NAME`.
    pub captures: Vec<(String, String)>,
}

/// One replaced byte range of the original text.
pub(super) struct Splice {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

pub(super) struct Rewritten {
    pub text: String,
    pub splices: Vec<Splice>,
}

pub(super) fn parse_lang(name: &str) -> Result<SupportLang, String> {
    name.trim()
        .parse()
        .map_err(|_| format!("unsupported language {name:?}; supported: {}", lang_names()))
}

pub(super) fn lang_names() -> String {
    let names: Vec<String> = SupportLang::all_langs()
        .iter()
        .map(|l| l.to_string().to_lowercase())
        .collect();
    names.join(", ")
}

pub(super) fn lang_of_path(path: &std::path::Path) -> Option<SupportLang> {
    SupportLang::from_path(path)
}

fn parse(src: &str, lang: SupportLang) -> Result<AstGrep<Doc>, String> {
    AstGrep::try_new(src, lang).map_err(|e| format!("cannot parse as {lang}: {e}"))
}

fn squash(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let mut cut: String = flat.chars().take(limit).collect();
    cut.push('…');
    cut
}

fn error_count(root: &AstGrep<Doc>) -> usize {
    root.root()
        .dfs()
        .filter(|n| n.is_error() || n.is_missing())
        .count()
}

/// 1-based `line:column` of the first syntax error, for refusal messages.
fn first_error(root: &AstGrep<Doc>) -> String {
    match root.root().dfs().find(|n| n.is_error() || n.is_missing()) {
        Some(node) => {
            let pos = node.start_pos();
            format!("{}:{}", pos.line() + 1, pos.column(&node) + 1)
        }
        None => "?".to_owned(),
    }
}

impl Query {
    /// Compile `pattern` (and `rewrite`) for `lang`. A rewrite may only use variables the pattern defines.
    pub fn compile(
        lang: SupportLang,
        pattern: &str,
        rewrite: Option<&str>,
    ) -> Result<Query, String> {
        let compiled = Pattern::try_new(pattern, lang).map_err(|e| e.to_string())?;
        if compiled.has_error() {
            return Err(format!("pattern is not valid {lang} syntax"));
        }
        let template = match rewrite {
            Some(text) => {
                let Ok(template) = TemplateFix::try_new(text, &lang);
                let defined = compiled.defined_vars();
                let mut unknown: Vec<&str> = template
                    .used_vars()
                    .into_iter()
                    .filter(|v| !defined.contains(v))
                    .collect();
                unknown.sort_unstable();
                if !unknown.is_empty() {
                    return Err(format!(
                        "rewrite uses variables the pattern does not capture: ${}",
                        unknown.join(", $")
                    ));
                }
                Some(template)
            }
            None => None,
        };
        Ok(Query {
            lang,
            pattern: compiled,
            template,
        })
    }

    /// Up to `limit` matches in source order.
    pub fn find(&self, src: &str, limit: usize) -> Result<Vec<Hit>, String> {
        let root = parse(src, self.lang)?;
        let mut lines: Option<Vec<&str>> = None;
        let mut hits = Vec::new();
        for found in root.root().find_all(&self.pattern).take(limit) {
            let start = found.start_pos();
            let lines = lines.get_or_insert_with(|| src.lines().collect());
            let text = lines.get(start.line()).copied().unwrap_or_default();
            hits.push(Hit {
                line: start.line() + 1,
                column: start.column(&*found) + 1,
                text: squash_line(text),
                captures: captures(&found),
            });
        }
        Ok(hits)
    }

    /// Rewrite every match. `Ok(None)` when nothing matches. Refuses a result that has
    /// more syntax errors than the input, so a bad template never produces broken code.
    pub fn rewrite(&self, src: &str) -> Result<Option<Rewritten>, String> {
        let template = self
            .template
            .as_ref()
            .ok_or("internal error: query has no rewrite")?;
        let root = parse(src, self.lang)?;
        let edits = root.root().replace_all(&self.pattern, template);
        if edits.is_empty() {
            return Ok(None);
        }
        let crlf = src.contains("\r\n") && !has_bare_lf(src);
        let mut splices = Vec::with_capacity(edits.len());
        let mut cursor = 0;
        for edit in edits {
            let end = edit.position + edit.deleted_length;
            if edit.position < cursor || end > src.len() {
                return Err("internal error: overlapping rewrite ranges".to_owned());
            }
            let mut text = String::from_utf8(edit.inserted_text)
                .map_err(|_| "rewrite produced invalid UTF-8".to_owned())?;
            if crlf {
                text = text.replace("\r\n", "\n").replace('\n', "\r\n");
            }
            cursor = end;
            splices.push(Splice {
                start: edit.position,
                end,
                text,
            });
        }
        let text = apply(src, &splices);
        let before = error_count(&root);
        let updated = parse(&text, self.lang)?;
        if error_count(&updated) > before {
            return Err(format!(
                "the rewritten code no longer parses as {} (syntax error at {}); nothing was changed. Check that the rewrite is valid {} in the context of the pattern",
                self.lang,
                first_error(&updated),
                self.lang
            ));
        }
        Ok(Some(Rewritten { text, splices }))
    }
}

fn has_bare_lf(src: &str) -> bool {
    src.matches('\n').count() != src.matches("\r\n").count()
}

fn squash_line(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(MAX_LINE_CHARS).collect();
    cut.push_str(" …[truncated]");
    cut
}

fn captures(found: &NodeMatch<'_, Doc>) -> Vec<(String, String)> {
    let env = found.get_env();
    let mut out: Vec<(String, String)> = env
        .get_matched_variables()
        .filter_map(|var| match var {
            MetaVariable::Capture(name, _) => {
                let node = env.get_match(&name)?;
                Some((format!("${name}"), squash(&node.text(), MAX_CAPTURE_CHARS)))
            }
            MetaVariable::MultiCapture(name) => {
                let nodes = env.get_multiple_matches(&name);
                let texts: Vec<String> = nodes
                    .iter()
                    .filter(|n| n.is_named())
                    .map(|n| n.text().into_owned())
                    .collect();
                Some((
                    format!("$$${name}"),
                    squash(&texts.join(", "), MAX_CAPTURE_CHARS),
                ))
            }
            _ => None,
        })
        .collect();
    out.sort();
    out
}

/// `src` with the (sorted, non-overlapping) splices applied.
pub(super) fn apply(src: &str, splices: &[Splice]) -> String {
    let mut out = String::with_capacity(src.len());
    let mut cursor = 0;
    for splice in splices {
        out.push_str(&src[cursor..splice.start]);
        out.push_str(&splice.text);
        cursor = splice.end;
    }
    out.push_str(&src[cursor..]);
    out
}
