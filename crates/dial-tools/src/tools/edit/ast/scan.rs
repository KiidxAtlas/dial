//! Finding and loading the source files an AST query runs over.

use super::engine::{Query, lang_of_path, parse_lang};
use crate::tools::{Workspace, fs::walk::Walk, io_message};
use ast_grep_language::SupportLang;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_FILE: u64 = 2 * 1024 * 1024;

/// Where a query runs: a single file or a directory tree, in one language or inferred per file.
pub(super) struct Target {
    start: PathBuf,
    is_file: bool,
    lang: Option<SupportLang>,
}

impl Target {
    pub fn is_file(&self) -> bool {
        self.is_file
    }
    pub fn resolve(
        ws: &Workspace,
        path: Option<&str>,
        lang: Option<&str>,
    ) -> Result<Target, String> {
        let start = match path {
            Some(input) => ws.resolve(input)?,
            None => ws.root().to_owned(),
        };
        let meta =
            fs::metadata(&start).map_err(|e| io_message("searching", &ws.rel(&start), &e))?;
        let lang = lang.map(parse_lang).transpose()?;
        let is_file = meta.is_file();
        if is_file && lang.is_none() && lang_of_path(&start).is_none() {
            return Err(format!(
                "cannot infer the language of {:?}; pass `lang`",
                ws.rel(&start)
            ));
        }
        Ok(Target {
            start,
            is_file,
            lang,
        })
    }

    /// Language to use for `path`, or `None` to skip the file.
    fn language(&self, path: &Path) -> Option<SupportLang> {
        match (self.lang, self.is_file) {
            (Some(lang), true) => Some(lang),
            (Some(lang), false) => (lang_of_path(path) == Some(lang)).then_some(lang),
            (None, _) => lang_of_path(path),
        }
    }
}

/// A file ready to be queried.
pub(super) struct SourceFile {
    pub abs: PathBuf,
    pub rel: String,
    pub text: String,
    pub perms: fs::Permissions,
}

/// What a scan could not cover, for the tool's footer.
#[derive(Default)]
pub(super) struct Coverage {
    /// Binary, oversized or non-UTF-8 files.
    pub skipped: usize,
    /// Languages the pattern does not compile for (auto-detect mode only), with the reason.
    pub rejected: Vec<(SupportLang, String)>,
    pub truncated: bool,
    pub files: usize,
}

impl Coverage {
    pub fn footer(&self) -> String {
        let mut out = String::new();
        if self.skipped > 0 {
            out.push_str(&format!(
                "[{} binary, oversized or non-UTF-8 files skipped]\n",
                self.skipped
            ));
        }
        for (lang, why) in &self.rejected {
            out.push_str(&format!("[{lang} files skipped: {why}]\n"));
        }
        if self.truncated {
            out.push_str(
                "[workspace walk truncated at its entry limit; results may be incomplete]\n",
            );
        }
        out
    }
}

/// Compiled queries by language, compiled on first use.
pub(super) struct Queries<'a> {
    pattern: &'a str,
    rewrite: Option<&'a str>,
    compiled: HashMap<SupportLang, Result<Query, String>>,
}

impl<'a> Queries<'a> {
    pub fn new(pattern: &'a str, rewrite: Option<&'a str>) -> Self {
        Queries {
            pattern,
            rewrite,
            compiled: HashMap::new(),
        }
    }

    pub fn get(&mut self, lang: SupportLang) -> Result<&Query, &str> {
        let (pattern, rewrite) = (self.pattern, self.rewrite);
        self.compiled
            .entry(lang)
            .or_insert_with(|| Query::compile(lang, pattern, rewrite))
            .as_ref()
            .map_err(String::as_str)
    }
}

fn load(path: &Path, rel: String) -> Result<SourceFile, ()> {
    let file = crate::tools::fs::walk::open_search_file(path).map_err(|_| ())?;
    let meta = file.metadata().map_err(|_| ())?;
    if meta.len() > MAX_FILE {
        return Err(());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(());
    }
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err(());
    }
    let text = String::from_utf8(bytes).map_err(|_| ())?;
    Ok(SourceFile {
        abs: path.to_owned(),
        rel,
        text,
        perms: meta.permissions(),
    })
}

/// Run `visit` on every loadable file of `target` whose language has a query, in sorted order.
/// `visit` returns false to stop early. An error means the pattern is unusable for the
/// requested language(s), or for every language met.
pub(super) fn scan(
    ws: &Workspace,
    target: &Target,
    queries: &mut Queries,
    visit: &mut dyn FnMut(SourceFile, &Query) -> bool,
) -> Result<Coverage, String> {
    let mut coverage = Coverage::default();
    if let Some(lang) = target.lang {
        queries.get(lang).map_err(str::to_owned)?;
    }
    let mut compiled_any = false;
    let mut each = |path: &Path| -> bool {
        let Some(lang) = target.language(path) else {
            return true;
        };
        let query = match queries.get(lang) {
            Ok(query) => query,
            Err(why) => {
                if !coverage.rejected.iter().any(|(l, _)| *l == lang) {
                    coverage.rejected.push((lang, why.to_owned()));
                }
                return true;
            }
        };
        compiled_any = true;
        match load(path, ws.rel(path)) {
            Ok(file) => {
                coverage.files += 1;
                visit(file, query)
            }
            Err(()) => {
                coverage.skipped += 1;
                true
            }
        }
    };
    if target.is_file {
        each(&target.start);
    } else {
        let mut walk = Walk::new(ws);
        walk.run(&target.start, &mut each);
        coverage.truncated = walk.truncated;
    }
    if !compiled_any && let Some((lang, why)) = coverage.rejected.first() {
        return Err(format!("pattern does not compile for {lang}: {why}"));
    }
    Ok(coverage)
}
