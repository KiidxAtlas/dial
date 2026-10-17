//! Structural triggers inspect submitted source, never read workspace files or execute code.
use crate::runtime::structural::{Query, lang_of_path, parse_lang};
use ast_grep_language::SupportLang;
use serde_json::Value;
use std::path::Path;

pub(super) struct Conditions(Vec<(SupportLang, Query)>, Option<SupportLang>);
impl std::fmt::Debug for Conditions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Conditions").field(&self.0.len()).finish()
    }
}
impl Conditions {
    pub fn compile(patterns: &[String], language: Option<&str>) -> Result<Self, String> {
        if patterns.len() > 16 || patterns.iter().any(|p| p.is_empty() || p.len() > 4096) {
            return Err(
                "astCondition needs at most 16 nonempty patterns, each at most 4096 bytes".into(),
            );
        }
        let languages = match language {
            Some(name) => vec![parse_lang(name)?],
            None => SupportLang::all_langs().to_vec(),
        };
        if languages.len().saturating_mul(patterns.len()) > 256 {
            return Err(
                "AST parser compilation budget exceeded; select astLanguage or reduce patterns"
                    .into(),
            );
        }
        let mut queries = Vec::new();
        for pattern in patterns {
            let before = queries.len();
            for &lang in &languages {
                if let Ok(query) = Query::compile(lang, pattern, None) {
                    queries.push((lang, query));
                }
            }
            if queries.len() == before {
                return Err(format!(
                    "astCondition pattern is invalid in the selected languages: {pattern:?}"
                ));
            }
        }
        Ok(Self(queries, language.map(parse_lang).transpose()?))
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn matches(&self, args: &Value) -> Result<bool, String> {
        let mut bytes = 0;
        let mut fragments = Vec::new();
        collect(args, None, 0, &mut bytes, &mut fragments)?;
        let mut owned = Vec::new();
        if let Some(input) = args.get("input").and_then(Value::as_str) {
            if input.len() > 128 * 1024 {
                return Err("AST rule patch coverage budget exceeded".into());
            }
            if input.trim_start().starts_with('[') {
                owned = crate::tools::edit::hashline::proposed_sources(input)?;
            } else if input.trim_start().starts_with("*** Edit File:") {
                owned = crate::tools::edit::editing::proposed_sloppy_sources(input)?;
            } else if input.trim_start().starts_with("*** Begin Patch") {
                let mut path = None;
                let mut source = String::new();
                for line in input.lines() {
                    if let Some(destination) = line.strip_prefix("*** Move to: ") {
                        path = Some(destination.to_owned());
                        continue;
                    }
                    if line.starts_with("*** ") {
                        if let Some(path) = path.take() {
                            owned.push((path, std::mem::take(&mut source)));
                        }
                        path = line
                            .strip_prefix("*** Add File: ")
                            .or_else(|| line.strip_prefix("*** Update File: "))
                            .map(str::to_owned);
                    } else if let Some(added) = line.strip_prefix('+') {
                        source.push_str(added);
                        source.push('\n');
                    }
                }
                if let Some(path) = path {
                    owned.push((path, source));
                }
            }
        }
        collect_diffs(args, None, 0, &mut owned)?;
        for (path, source) in &owned {
            bytes = bytes
                .checked_add(source.len())
                .ok_or("AST source budget overflow")?;
            if bytes > 128 * 1024 || fragments.len() >= 128 {
                return Err("AST source coverage budget exceeded".into());
            }
            fragments.push((Some(path.as_str()), source));
        }
        for (path, source) in fragments {
            let Some(lang) = self
                .1
                .or_else(|| path.and_then(|path| lang_of_path(Path::new(path))))
            else {
                continue;
            };
            for (language, query) in &self.0 {
                if *language == lang && !query.find(source, 1)?.is_empty() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

// Structured patch proposals retain the nearest file identity through edits arrays.
fn collect_diffs(
    value: &Value,
    path: Option<&str>,
    depth: usize,
    out: &mut Vec<(String, String)>,
) -> Result<(), String> {
    if depth > 8 {
        return Err("AST patch nesting budget exceeded".into());
    }
    match value {
        Value::Object(map) => {
            let path = map.get("path").and_then(Value::as_str).or(path);
            if let Some(diff) = map.get("diff").and_then(Value::as_str)
                && let Some(path) = path
            {
                if diff.len() > 128 * 1024 || out.len() >= 128 {
                    return Err("AST patch source budget exceeded".into());
                }
                let text = if map.get("op").and_then(Value::as_str) == Some("create")
                    && !diff.lines().any(|line| line.starts_with('+'))
                {
                    diff.to_owned()
                } else {
                    diff.lines()
                        .filter_map(|line| line.strip_prefix('+'))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                out.push((path.into(), text));
            }
            for value in map
                .values()
                .filter(|value| value.is_array() || value.is_object())
            {
                collect_diffs(value, path, depth + 1, out)?;
            }
        }
        Value::Array(values) => {
            if values.len() > 128 {
                return Err("AST patch operation count budget exceeded".into());
            }
            for value in values {
                collect_diffs(value, path, depth + 1, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// Only source-bearing fields participate. Strings such as shell commands, URLs and query patterns
// are not code edits. Each fragment keeps its nearest explicit file identity.
fn collect<'a>(
    value: &'a Value,
    path: Option<&'a str>,
    depth: usize,
    bytes: &mut usize,
    out: &mut Vec<(Option<&'a str>, &'a str)>,
) -> Result<(), String> {
    if depth > 8 {
        return Err("AST rule argument nesting budget exceeded".into());
    }
    match value {
        Value::Object(map) => {
            let path = ["path", "file_path", "file", "uri"]
                .into_iter()
                .find_map(|key| map.get(key).and_then(Value::as_str))
                .or(path);
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "content"
                        | "contents"
                        | "new_string"
                        | "newText"
                        | "replacement"
                        | "rewrite"
                        | "code"
                ) {
                    if let Some(source) = value.as_str() {
                        *bytes = bytes
                            .checked_add(source.len())
                            .ok_or("AST rule source budget overflow")?;
                        if *bytes > 128 * 1024 || out.len() >= 128 {
                            return Err(
                                "AST rule source coverage budget exceeded; split this attempt"
                                    .into(),
                            );
                        }
                        out.push((path, source));
                    }
                } else if value.is_object() || value.is_array() {
                    collect(value, path, depth + 1, bytes, out)?;
                }
            }
        }
        Value::Array(items) => {
            if items.len() > 128 {
                return Err("AST rule argument count budget exceeded".into());
            }
            for value in items {
                collect(value, path, depth + 1, bytes, out)?
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests;
