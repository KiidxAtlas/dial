//! Corpus loading and seeded mutation-case generation.
//!
//! A case is a set of line [`Hunk`]s against one corpus file. Every grammar's call is rendered from
//! the same hunks (see `calls.rs`), so grammars are compared on identical intended changes.
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{fs, path::Path};

const MAX_CORPUS_FILES: usize = 64;
const MAX_CORPUS_FILE_BYTES: u64 = 256 * 1024;

const EMBEDDED: [(&str, &str); 3] = [
    (
        "pricing.rs",
        include_str!("../../../../../../examples/benchmarks/edit-grammar-corpus/pricing.rs"),
    ),
    (
        "pages.ts",
        include_str!("../../../../../../examples/benchmarks/edit-grammar-corpus/pages.ts"),
    ),
    (
        "billing.py",
        include_str!("../../../../../../examples/benchmarks/edit-grammar-corpus/billing.py"),
    ),
];

/// One corpus file: a workspace-relative name and LF text that ends with a newline.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub name: String,
    pub text: String,
}

impl SourceFile {
    pub fn new(name: &str, text: &str) -> Self {
        let mut text = text.replace("\r\n", "\n");
        if !text.ends_with('\n') {
            text.push('\n');
        }
        Self {
            name: name.to_owned(),
            text,
        }
    }

    fn lines(&self) -> Vec<String> {
        self.text.lines().map(str::to_owned).collect()
    }
}

/// The committed corpus under `examples/benchmarks/edit-grammar-corpus`.
pub fn embedded_corpus() -> Vec<SourceFile> {
    EMBEDDED
        .iter()
        .map(|(name, text)| SourceFile::new(name, text))
        .collect()
}

/// Every regular UTF-8 file directly inside `dir`, sorted by name.
pub fn load_corpus(dir: &Path) -> Result<Vec<SourceFile>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading corpus {}", dir.display()))? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if !meta.is_file() {
            continue;
        }
        ensure!(
            meta.len() <= MAX_CORPUS_FILE_BYTES,
            "corpus file {} exceeds {MAX_CORPUS_FILE_BYTES} bytes",
            entry.path().display()
        );
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            bail!("corpus file name {:?} is not UTF-8", entry.file_name());
        };
        names.push(name);
    }
    names.sort();
    ensure!(
        !names.is_empty(),
        "corpus {} has no regular files",
        dir.display()
    );
    ensure!(
        names.len() <= MAX_CORPUS_FILES,
        "corpus {} has more than {MAX_CORPUS_FILES} files",
        dir.display()
    );
    names
        .iter()
        .map(|name| {
            let text = fs::read_to_string(dir.join(name))
                .with_context(|| format!("corpus file {name} is not UTF-8 text"))?;
            Ok(SourceFile::new(name, &text))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    RenameIdentifier,
    ReplaceStatement,
    InsertAfter,
    DeleteBlock,
    ChangeBody,
    DuplicateLine,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::RenameIdentifier,
        Kind::ReplaceStatement,
        Kind::InsertAfter,
        Kind::DeleteBlock,
        Kind::ChangeBody,
        Kind::DuplicateLine,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::RenameIdentifier => "rename_identifier",
            Kind::ReplaceStatement => "replace_statement",
            Kind::InsertAfter => "insert_after",
            Kind::DeleteBlock => "delete_block",
            Kind::ChangeBody => "change_body",
            Kind::DuplicateLine => "duplicate_line",
        }
    }
}

/// Replace `remove` lines starting at 0-based `start` with `insert` (pure insertion when `remove`
/// is zero). Hunks of one case never overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub start: usize,
    pub remove: usize,
    pub insert: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Case {
    pub id: String,
    pub file: String,
    pub kind: Kind,
    /// Lines inserted at the top of the file after the model's read and before its edit.
    pub shift: usize,
    pub lines: Vec<String>,
    pub hunks: Vec<Hunk>,
    /// Whole-word rename `(old, new)`: lets replace-all and structural grammars express the case.
    pub rename: Option<(String, String)>,
}

impl Case {
    /// Scenario label used for grouping: the mutation kind, or `stale_read` when the file shifts.
    pub fn scenario(&self) -> String {
        if self.shift > 0 {
            "stale_read".to_owned()
        } else {
            self.kind.name().to_owned()
        }
    }

    fn shift_prefix(&self) -> String {
        let comment = comment_prefix(&self.file);
        (1..=self.shift)
            .map(|n| format!("{comment} shifted above {n}\n"))
            .collect()
    }

    /// The file as it exists when the edit is attempted.
    pub fn before(&self) -> String {
        let mut text = self.shift_prefix();
        for line in &self.lines {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    /// The only acceptable result of a correct edit.
    pub fn expected(&self) -> String {
        let mut text = self.shift_prefix();
        text.push_str(&apply(&self.lines, &self.hunks));
        text
    }

    pub fn original(&self) -> String {
        apply(&self.lines, &[])
    }
}

/// Apply non-overlapping hunks to `lines`; the result ends with a newline per line.
pub fn apply(lines: &[String], hunks: &[Hunk]) -> String {
    let mut sorted: Vec<&Hunk> = hunks.iter().collect();
    sorted.sort_by_key(|hunk| hunk.start);
    let mut out = String::new();
    let mut at = 0;
    for hunk in sorted {
        for line in &lines[at..hunk.start] {
            out.push_str(line);
            out.push('\n');
        }
        for line in &hunk.insert {
            out.push_str(line);
            out.push('\n');
        }
        at = hunk.start + hunk.remove;
    }
    for line in &lines[at..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// SplitMix64, salted per stream so cases do not shift when another kind changes.
struct Rng(u64);

impl Rng {
    fn new(seed: u64, salt: &str) -> Self {
        let hash = salt.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
        Rng(seed ^ hash)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        (!items.is_empty()).then(|| &items[self.below(items.len())])
    }
}

fn comment_prefix(file: &str) -> &'static str {
    if file.ends_with(".py") { "#" } else { "//" }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn indent_text(line: &str) -> &str {
    &line[..indent_of(line)]
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn opens_block(line: &str) -> bool {
    let trimmed = line.trim_end();
    trimmed.ends_with('{') || trimmed.ends_with(':')
}

/// End (exclusive) of the indented body that follows the block header at `header`, without
/// trailing blank lines.
fn body_end(lines: &[String], header: usize) -> usize {
    let indent = indent_of(&lines[header]);
    let mut end = header + 1;
    while end < lines.len() && (blank(&lines[end]) || indent_of(&lines[end]) > indent) {
        end += 1;
    }
    while end > header + 1 && blank(&lines[end - 1]) {
        end -= 1;
    }
    end
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_count(line: &str, word: &str) -> usize {
    line.match_indices(word)
        .filter(|(at, _)| {
            let before = line[..*at].chars().next_back();
            let after = line[at + word.len()..].chars().next();
            !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
        })
        .count()
}

fn replace_word(line: &str, word: &str, new: &str) -> String {
    let mut out = String::new();
    let mut last = 0;
    for (at, _) in line.match_indices(word) {
        let before = line[..at].chars().next_back();
        let after = line[at + word.len()..].chars().next();
        if at >= last && !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char) {
            out.push_str(&line[last..at]);
            out.push_str(new);
            last = at + word.len();
        }
    }
    out.push_str(&line[last..]);
    out
}

/// Free functions declared at column zero whose name is a plain identifier everywhere it appears,
/// so a whole-word rename and a substring replace-all agree.
fn renameable(lines: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    for line in lines.iter().filter(|line| indent_of(line) == 0) {
        let mut rest = line.as_str();
        for prefix in ["pub ", "export ", "async "] {
            rest = rest.strip_prefix(prefix).unwrap_or(rest);
        }
        let Some(rest) = ["fn ", "function ", "def "]
            .iter()
            .find_map(|keyword| rest.strip_prefix(keyword))
        else {
            continue;
        };
        let name: String = rest.chars().take_while(|c| is_word_char(*c)).collect();
        let whole: usize = lines.iter().map(|line| word_count(line, &name)).sum();
        let substring: usize = lines.iter().map(|line| line.matches(&name).count()).sum();
        if name.len() >= 4 && whole > 0 && whole == substring && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

type Plan = (Vec<Hunk>, Option<(String, String)>);

fn plan(kind: Kind, file: &str, lines: &[String], rng: &mut Rng) -> Option<Plan> {
    let comment = comment_prefix(file);
    match kind {
        Kind::RenameIdentifier => {
            let name = rng.pick(&renameable(lines))?.clone();
            let new = format!("{name}_renamed");
            let hunks = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| word_count(line, &name) > 0)
                .map(|(start, line)| Hunk {
                    start,
                    remove: 1,
                    insert: vec![replace_word(line, &name, &new)],
                })
                .collect();
            Some((hunks, Some((name, new))))
        }
        Kind::ReplaceStatement => {
            let candidates: Vec<usize> = (0..lines.len())
                .filter(|&i| {
                    let trimmed = lines[i].trim();
                    indent_of(&lines[i]) > 0
                        && !trimmed.is_empty()
                        && !opens_block(trimmed)
                        && !trimmed.starts_with(['}', ')', ']'])
                        && !trimmed.starts_with(comment)
                })
                .collect();
            let start = *rng.pick(&candidates)?;
            let line = format!(
                "{}{comment} replaced: {}",
                indent_text(&lines[start]),
                lines[start].trim()
            );
            Some((single(start, 1, vec![line]), None))
        }
        Kind::InsertAfter => {
            let candidates: Vec<usize> = (0..lines.len()).filter(|&i| !blank(&lines[i])).collect();
            let anchor = *rng.pick(&candidates)?;
            let indent = indent_text(&lines[anchor]);
            let count = 1 + rng.below(2);
            let insert = (1..=count)
                .map(|n| format!("{indent}{comment} inserted {n}"))
                .collect();
            Some((single(anchor + 1, 0, insert), None))
        }
        Kind::DeleteBlock => {
            let headers: Vec<usize> = (0..lines.len().saturating_sub(1))
                .filter(|&i| {
                    opens_block(&lines[i]) && indent_of(&lines[i + 1]) > indent_of(&lines[i])
                })
                .collect();
            let header = *rng.pick(&headers)?;
            let mut end = body_end(lines, header);
            if end < lines.len()
                && indent_of(&lines[end]) == indent_of(&lines[header])
                && lines[end].trim_start().starts_with('}')
            {
                end += 1;
            }
            // A block at either end of the file takes its blank separator with it, so the result
            // neither starts nor ends in blank lines that some grammars normalize away.
            let mut start = header;
            if end == lines.len() {
                while start > 0 && blank(&lines[start - 1]) {
                    start -= 1;
                }
            }
            if start == 0 {
                while end < lines.len() && blank(&lines[end]) {
                    end += 1;
                }
            }
            Some((single(start, end - start, Vec::new()), None))
        }
        Kind::ChangeBody => {
            let headers: Vec<usize> = (0..lines.len().saturating_sub(1))
                .filter(|&i| {
                    indent_of(&lines[i]) == 0
                        && opens_block(&lines[i])
                        && indent_of(&lines[i + 1]) > 0
                })
                .collect();
            let header = *rng.pick(&headers)?;
            let end = body_end(lines, header);
            let indent = indent_text(&lines[header + 1]);
            let insert = (1..=2)
                .map(|n| format!("{indent}{comment} body rewritten {n}"))
                .collect();
            Some((single(header + 1, end - header - 1, insert), None))
        }
        Kind::DuplicateLine => {
            let repeated = |i: usize| lines.iter().filter(|line| **line == lines[i]).count() > 1;
            let mut candidates: Vec<usize> = (0..lines.len())
                .filter(|&i| !blank(&lines[i]) && repeated(i))
                .collect();
            let substantial: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&i| lines[i].trim().len() >= 4)
                .collect();
            if !substantial.is_empty() {
                candidates = substantial;
            }
            let start = *rng.pick(&candidates)?;
            let line = format!("{}{comment} duplicate edited", indent_text(&lines[start]));
            Some((single(start, 1, vec![line]), None))
        }
    }
}

fn single(start: usize, remove: usize, insert: Vec<String>) -> Vec<Hunk> {
    vec![Hunk {
        start,
        remove,
        insert,
    }]
}

/// Cases for one corpus file and seed: each applicable mutation kind once, plus one case whose
/// file gains one to three lines above the edit between the model's read and its edit.
pub fn generate(file: &SourceFile, seed: u64) -> Vec<Case> {
    let lines = file.lines();
    let case = |kind: Kind, shift: usize, (hunks, rename): Plan| Case {
        id: format!(
            "{}/seed{seed}/{}{}",
            file.name,
            if shift > 0 { "stale_read+" } else { "" },
            kind.name()
        ),
        file: file.name.clone(),
        kind,
        shift,
        lines: lines.clone(),
        hunks,
        rename,
    };
    let mut cases = Vec::new();
    for kind in Kind::ALL {
        let mut rng = Rng::new(seed, &format!("{}:{}", file.name, kind.name()));
        if let Some(planned) = plan(kind, &file.name, &lines, &mut rng) {
            cases.push(case(kind, 0, planned));
        }
    }
    let mut rng = Rng::new(seed, &format!("{}:stale_read", file.name));
    let first = rng.below(Kind::ALL.len());
    let shift = 1 + rng.below(3);
    let stale = (0..Kind::ALL.len()).find_map(|offset| {
        let kind = Kind::ALL[(first + offset) % Kind::ALL.len()];
        plan(kind, &file.name, &lines, &mut rng).map(|planned| (kind, planned))
    });
    if let Some((kind, planned)) = stale {
        cases.push(case(kind, shift, planned));
    }
    cases
}
