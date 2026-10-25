//! OSC 8 hyperlinks for scrollback rows. Only text that already *is* the target becomes a link:
//! an `http(s)` URL, or a path (optionally `:line[:col]`) that exists on disk, resolved against
//! the project directory. A link can therefore never point somewhere its visible text does not
//! name, whatever the model writes.
use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
};

/// Distinct tokens remembered before the cache starts over.
const CACHE_LIMIT: usize = 4096;
/// Longest token considered (a path or URL longer than this stays plain text).
const TOKEN_LIMIT: usize = 2048;

/// Finds link targets in row text. One per terminal; the path cache lives as long as it does.
#[derive(Debug, Default)]
pub struct Linker {
    root: Option<PathBuf>,
    home: Option<PathBuf>,
    /// iTerm2: `#line` routes `file://` links through Semantic History (the configured editor).
    line_fragments: bool,
    /// Token without its line suffix → absolute existing path (`None`: not a path on disk).
    paths: HashMap<String, Option<(PathBuf, bool)>>,
}

impl Linker {
    pub fn new(line_fragments: bool) -> Self {
        Self {
            line_fragments,
            home: std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from),
            ..Self::default()
        }
    }

    /// Directory relative paths resolve against; a change forgets cached lookups.
    pub fn set_root(&mut self, root: Option<&Path>) {
        if self.root.as_deref() != root {
            self.root = root.map(Path::to_path_buf);
            self.paths.clear();
        }
    }

    /// Byte ranges of `text` to link and their URLs. `full` means the row filled its width, so a
    /// token touching the end may continue on the next row and is left plain.
    pub fn links(&mut self, text: &str, full: bool) -> Vec<(Range<usize>, String)> {
        let mut found = Vec::new();
        for (start, raw) in tokens(text) {
            let (lead, token) = trim_token(raw);
            if token.is_empty() || token.len() > TOKEN_LIMIT {
                continue;
            }
            let begin = start + lead;
            let range = begin..begin + token.len();
            if full && start + raw.len() == text.len() {
                continue;
            }
            if let Some(url) = self.target(token) {
                found.push((range, url));
            }
        }
        found
    }

    fn target(&mut self, token: &str) -> Option<String> {
        if token.starts_with("https://") || token.starts_with("http://") {
            let url = url::Url::parse(token).ok()?;
            return url.host_str().is_some().then(|| url.as_str().to_owned());
        }
        if !(token.contains('/') || token.contains('.'))
            || !token.chars().any(char::is_alphanumeric)
            || token.contains("://")
        {
            return None;
        }
        let (path, line) = split_line(token);
        let (absolute, is_dir) = self.resolve(path)?;
        let fragment = (self.line_fragments && !is_dir).then(|| match line {
            Some((line, Some(col))) => format!("{line}:{col}"),
            Some((line, None)) => line.to_string(),
            None => "1".to_owned(),
        });
        file_url(&absolute, "", fragment.as_deref())
    }

    fn resolve(&mut self, path: &str) -> Option<(PathBuf, bool)> {
        if let Some(cached) = self.paths.get(path) {
            return cached.clone();
        }
        let candidate = if let Some(rest) = path.strip_prefix("~/") {
            self.home.as_ref().map(|home| home.join(rest))
        } else if Path::new(path).is_absolute() {
            Some(PathBuf::from(path))
        } else {
            self.root.as_ref().map(|root| root.join(path))
        };
        let found = candidate.and_then(|candidate| {
            let meta = std::fs::metadata(&candidate).ok()?;
            Some((candidate, meta.is_dir()))
        });
        if self.paths.len() >= CACHE_LIMIT {
            self.paths.clear();
        }
        self.paths.insert(path.to_owned(), found.clone());
        found
    }
}

/// `file://host/percent-encoded/path[#fragment]`; `None` for a relative path.
pub fn file_url(path: &Path, host: &str, fragment: Option<&str>) -> Option<String> {
    let mut url = url::Url::from_file_path(path).ok()?;
    if !host.is_empty() {
        // An invalid host name keeps the local form rather than failing.
        let _ = url.set_host(Some(host));
    }
    url.set_fragment(fragment);
    Some(url.into())
}

/// Whitespace-separated runs with their byte offsets.
fn tokens(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.split_whitespace()
        .map(move |token| (token.as_ptr() as usize - text.as_ptr() as usize, token))
}

/// Strip quoting and sentence punctuation around a token; returns the bytes cut from the front.
/// Digits are never trimmed, so `file.rs:12` keeps its line.
fn trim_token(raw: &str) -> (usize, &str) {
    let trimmed = raw.trim_start_matches(['(', '[', '{', '<', '"', '\'', '`', '@']);
    let lead = raw.len() - trimmed.len();
    let token = trimmed.trim_end_matches([
        ')', ']', '}', '>', '"', '\'', '`', ',', ';', '!', '?', '.', ':',
    ]);
    (lead, token)
}

/// `path:12` or `path:12:5` → the path and its line/column.
fn split_line(token: &str) -> (&str, Option<(u32, Option<u32>)>) {
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse::<u32>().ok())
            .flatten()
    };
    if let Some((head, last)) = token.rsplit_once(':')
        && let Some(last) = number(last)
    {
        if let Some((path, line)) = head.rsplit_once(':')
            && let Some(line) = number(line)
        {
            return (path, Some((line, Some(last))));
        }
        return (head, Some((last, None)));
    }
    (token, None)
}

#[cfg(test)]
mod tests;
