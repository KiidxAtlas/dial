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
pub struct Query {
    lang: SupportLang,
    pattern: Pattern,
    /// Search-only body-completed form of a declaration written without its body, such as
    /// `fn $NAME($$$) -> $T`, so signature-style queries also match definitions. Never used
    /// for rewrites: replacing such a match would silently drop the unwritten body.
    completed: Option<Pattern>,
    template: Option<TemplateFix>,
}

/// One structural match.
pub struct Hit {
    /// 1-based line and character column of the match start.
    pub line: usize,
    pub column: usize,
    /// The source line the match starts on, bounded.
    pub text: String,
    /// `($NAME, text)` pairs in name order; multi captures are named `$$$NAME`.
    pub captures: Vec<(String, String)>,
}

/// One replaced byte range of the original text.
pub struct Splice {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

pub struct Rewritten {
    pub text: String,
    pub splices: Vec<Splice>,
}

pub fn parse_lang(name: &str) -> Result<SupportLang, String> {
    name.trim()
        .parse()
        .map_err(|_| format!("unsupported language {name:?}; supported: {}", lang_names()))
}

pub fn lang_names() -> String {
    let names: Vec<String> = SupportLang::all_langs()
        .iter()
        .map(|l| l.to_string().to_lowercase())
        .collect();
    names.join(", ")
}

pub fn lang_of_path(path: &std::path::Path) -> Option<SupportLang> {
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

impl Query {
    /// Compile `pattern` (and `rewrite`) for `lang`. A rewrite may only use variables the pattern defines.
    pub fn compile(
        lang: SupportLang,
        pattern: &str,
        rewrite: Option<&str>,
    ) -> Result<Query, String> {
        let primary = Pattern::try_new(pattern, lang)
            .map_err(|e| e.to_string())
            .and_then(|compiled| {
                if compiled.has_error() {
                    Err(format!("pattern is not valid {lang} syntax"))
                } else {
                    Ok(compiled)
                }
            });
        let completed = if rewrite.is_none() {
            completed_declaration(pattern, lang)
        } else {
            None
        };
        // A body-less declaration that is only valid with its body (a Java method, for
        // example) is searched in its completed form alone.
        let (compiled, completed) = match (primary, completed) {
            (Ok(compiled), completed) => (compiled, completed),
            (Err(_), Some(completed)) => (completed, None),
            (Err(error), None) => return Err(error),
        };
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
            completed,
            template,
        })
    }

    /// Up to `limit` matches in source order.
    pub fn find(&self, src: &str, limit: usize) -> Result<Vec<Hit>, String> {
        let root = parse(src, self.lang)?;
        let node = root.root();
        let mut found: Vec<NodeMatch<'_, Doc>> = node.find_all(&self.pattern).take(limit).collect();
        if let Some(completed) = &self.completed {
            // Both streams are in source pre-order, so the first `limit` merged matches come
            // from the first `limit` of each. A node matched by both forms is reported once.
            found.extend(node.find_all(completed).take(limit));
            found.sort_by_key(|m| (m.range().start, std::cmp::Reverse(m.range().end)));
            found.dedup_by_key(|m| m.range());
            found.truncate(limit);
        }
        let lines: Vec<&str> = if found.is_empty() {
            Vec::new()
        } else {
            src.lines().collect()
        };
        Ok(found
            .iter()
            .map(|found| {
                let start = found.start_pos();
                let text = lines.get(start.line()).copied().unwrap_or_default();
                Hit {
                    line: start.line() + 1,
                    column: start.column(found) + 1,
                    text: squash_line(text),
                    captures: captures(src, found),
                }
            })
            .collect())
    }

    /// Rewrite every match. `Ok(None)` when nothing matches.
    /// Source syntax diagnostics are advisory at the tool layer, not a rewrite veto.
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

/// Node kinds that name a declaration whose body a signature-style pattern may omit.
const DECLARATION_KINDS: &[&str] = &["function", "method", "constructor", "declaration"];

/// Body-completed form of a declaration pattern written without a body: the pattern with
/// ` { $$$ }` appended, kept only when that parses as a function/method declaration. Patterns
/// that already end in a block or statement terminator are left alone.
fn completed_declaration(pattern: &str, lang: SupportLang) -> Option<Pattern> {
    let trimmed = pattern.trim_end();
    if trimmed.is_empty() || trimmed.ends_with(['}', ';']) {
        return None;
    }
    let text = format!("{trimmed} {{ $$$ }}");
    let completed = Pattern::try_new(&text, lang).ok()?;
    if completed.has_error() {
        return None;
    }
    let tree = AstGrep::new(&text, lang);
    let root = tree.root();
    let mut top = root.children().filter(|n| n.is_named());
    let declaration = top.next().filter(|_| top.next().is_none())?;
    let kind = declaration.kind();
    DECLARATION_KINDS
        .iter()
        .any(|word| kind.contains(word))
        .then_some(completed)
}

fn captures(src: &str, found: &NodeMatch<'_, Doc>) -> Vec<(String, String)> {
    let env = found.get_env();
    let mut out: Vec<(String, String)> = env
        .get_matched_variables()
        .filter_map(|var| match var {
            MetaVariable::Capture(name, _) => {
                let node = env.get_match(&name)?;
                Some((format!("${name}"), squash(&node.text(), MAX_CAPTURE_CHARS)))
            }
            MetaVariable::MultiCapture(name) => {
                // The original source span from the first to the last named node, so token
                // trees (macro arguments) keep their punctuation instead of being split into
                // fragments. Separators at either edge are outside the span.
                let nodes = env.get_multiple_matches(&name);
                let mut named = nodes.iter().filter(|n| n.is_named());
                let text = match (named.next(), named.next_back()) {
                    (Some(first), last) => {
                        let end = last.unwrap_or(first).range().end;
                        src.get(first.range().start..end).unwrap_or_default()
                    }
                    (None, _) => "",
                };
                Some((format!("$$${name}"), squash(text, MAX_CAPTURE_CHARS)))
            }
            _ => None,
        })
        .collect();
    out.sort();
    out
}

/// `src` with the (sorted, non-overlapping) splices applied.
pub fn apply(src: &str, splices: &[Splice]) -> String {
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

#[cfg(test)]
mod tests;
