//! Inline word completion engine: a learned word model trained on submitted prompts, and a project
//! vocabulary built from identifiers and file-name stems. Both are bounded, case-insensitive for
//! matching, and return only the suffix to show as ghost text.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::storage::redact::redact_text_counted;

/// Largest number of distinct learned words kept. Past this, the least useful words are evicted.
pub const MAX_LEARNED_WORDS: usize = 20_000;
const EVICT_TO: usize = 18_000;
/// A learned word's weight halves after this many submitted prompts without it.
const HALF_LIFE_SUBMISSIONS: f64 = 64.0;
/// Typed prefixes shorter than this never complete.
const MIN_PREFIX: usize = 2;
const MIN_WORD: usize = 3;
const MAX_WORD: usize = 40;
const MAX_VOCABULARY_WORDS: usize = 50_000;
const VOCABULARY_BYTE_BUDGET: u64 = 32 * 1024 * 1024;
const VOCABULARY_FILE_LIMIT: u64 = 256 * 1024;
const BINARY_PROBE_BYTES: usize = 8 * 1024;
const FORMAT_VERSION: u32 = 1;

/// One word, keyed by its ASCII-lowercase form. `display` keeps the most recent casing.
#[derive(Clone, Debug, PartialEq)]
pub struct Stat {
    pub display: String,
    pub count: u32,
    /// Submission tick of the most recent occurrence (always 0 for project vocabulary).
    pub last: u64,
}

/// Sorted, case-insensitive word index.
#[derive(Clone, Debug, Default)]
pub struct WordIndex {
    words: BTreeMap<String, Stat>,
}

impl WordIndex {
    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Adds `count` occurrences of `word` at `now`, inserting it only while fewer than `cap` words
    /// are stored.
    fn bump(&mut self, word: &str, count: u32, now: u64, cap: usize) {
        let key = word.to_ascii_lowercase();
        if let Some(stat) = self.words.get_mut(&key) {
            stat.count = stat.count.saturating_add(count);
            stat.last = stat.last.max(now);
            if stat.display != word {
                stat.display = word.to_owned();
            }
            return;
        }
        if self.words.len() >= cap {
            return;
        }
        self.words.insert(
            key,
            Stat {
                display: word.to_owned(),
                count,
                last: now,
            },
        );
    }

    /// Drops the lowest-scoring words until at most `limit` remain.
    fn evict_to(&mut self, limit: usize, score: impl Fn(&Stat) -> f64) {
        if self.words.len() <= limit {
            return;
        }
        let mut ranked: Vec<(f64, &String)> = self
            .words
            .iter()
            .map(|(key, stat)| (score(stat), key))
            .collect();
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
        let excess = self.words.len() - limit;
        let doomed: Vec<String> = ranked
            .into_iter()
            .take(excess)
            .map(|(_, key)| key.clone())
            .collect();
        for key in doomed {
            self.words.remove(&key);
        }
    }

    /// Returns the suffix of the best-scoring word that extends `prefix` (matched ignoring ASCII
    /// case). Ties go to the alphabetically first word, so results are deterministic.
    pub fn complete(&self, prefix: &str, score: impl Fn(&Stat) -> f64) -> Option<&str> {
        if prefix.len() < MIN_PREFIX || !prefix.is_ascii() {
            return None;
        }
        let folded = prefix.to_ascii_lowercase();
        let mut best: Option<(f64, &Stat)> = None;
        for (key, stat) in self.words.range::<str, _>((
            std::ops::Bound::Included(folded.as_str()),
            std::ops::Bound::Unbounded,
        )) {
            if !key.starts_with(folded.as_str()) {
                break;
            }
            if stat.display.len() <= prefix.len() {
                continue;
            }
            let value = score(stat);
            if best.is_none_or(|(top, _)| value > top) {
                best = Some((value, stat));
            }
        }
        best.and_then(|(_, stat)| stat.display.get(prefix.len()..))
    }
}

/// Weight of a learned word now: its count, halved for every half-life of submissions since its
/// last use. Recent and frequent words win; stale words fade without being deleted at once.
fn learned_score(stat: &Stat, now: u64) -> f64 {
    let age = now.saturating_sub(stat.last) as f64;
    f64::from(stat.count) * 0.5f64.powf(age / HALF_LIFE_SUBMISSIONS)
}

/// Whether `word` is a plausible word to learn or suggest. Rejects secret-looking tokens (long
/// mixed letter/digit runs, long hex) and anything that is not a plain identifier-like word.
pub fn is_learnable_word(word: &str) -> bool {
    let len = word.len();
    if !(MIN_WORD..=MAX_WORD).contains(&len) {
        return false;
    }
    if !word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return false;
    }
    if word.bytes().all(|b| b.is_ascii_digit()) || word.as_bytes()[0].is_ascii_digit() {
        return false;
    }
    if len >= 16 && word.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let has_digit = word.bytes().any(|b| b.is_ascii_digit());
    let has_letter = word.bytes().any(|b| b.is_ascii_alphabetic());
    !(len >= 12 && has_digit && has_letter)
}

/// Words from a submitted prompt that may be learned. Whitespace chunks that look like paths,
/// mentions, URLs or Windows paths are skipped whole, since their pieces are not prose.
pub fn learnable_words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    for chunk in text.split_whitespace() {
        if chunk.contains(['@', '/', '\\']) {
            continue;
        }
        for word in chunk.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if is_learnable_word(word) {
                words.push(word);
            }
        }
    }
    words
}

/// The word model learned from submitted prompts, persisted across sessions.
#[derive(Debug, Default)]
pub struct WordModel {
    index: WordIndex,
    tick: u64,
}

impl WordModel {
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Learns the words of one submitted prompt. Prompts containing a secret are skipped entirely.
    /// Returns whether anything was learned.
    pub fn learn_prompt(&mut self, text: &str) -> bool {
        if redact_text_counted(text).1 > 0 {
            return false;
        }
        let words = learnable_words(text);
        if words.is_empty() {
            return false;
        }
        self.tick += 1;
        for word in words {
            self.index.bump(word, 1, self.tick, usize::MAX);
        }
        if self.index.len() > MAX_LEARNED_WORDS {
            let now = self.tick;
            self.index
                .evict_to(EVICT_TO, |stat| learned_score(stat, now));
        }
        true
    }

    pub fn complete(&self, prefix: &str) -> Option<&str> {
        let now = self.tick;
        self.index.complete(prefix, |stat| learned_score(stat, now))
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let stored = StoredModel {
            version: FORMAT_VERSION,
            tick: self.tick,
            words: self
                .index
                .words
                .values()
                .map(|stat| StoredWord {
                    word: stat.display.clone(),
                    count: stat.count,
                    last: stat.last,
                })
                .collect(),
        };
        serde_json::to_vec(&stored).context("could not encode learned words")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<WordModel> {
        let stored: StoredModel =
            serde_json::from_slice(bytes).context("learned words file is not valid JSON")?;
        ensure!(
            stored.version == FORMAT_VERSION,
            "learned words file has unsupported version {}",
            stored.version
        );
        let tick = stored.tick;
        let mut model = WordModel {
            index: WordIndex::default(),
            tick,
        };
        for word in stored.words {
            if !is_learnable_word(&word.word) {
                continue;
            }
            let key = word.word.to_ascii_lowercase();
            model.index.words.insert(
                key,
                Stat {
                    display: word.word,
                    count: word.count.max(1),
                    last: word.last.min(tick),
                },
            );
        }
        if model.index.len() > MAX_LEARNED_WORDS {
            model
                .index
                .evict_to(EVICT_TO, |stat| learned_score(stat, tick));
        }
        Ok(model)
    }
}

#[derive(Serialize, Deserialize)]
struct StoredModel {
    version: u32,
    tick: u64,
    words: Vec<StoredWord>,
}

#[derive(Serialize, Deserialize)]
struct StoredWord {
    word: String,
    count: u32,
    last: u64,
}

/// Completion candidates from the project: identifiers in readable text files and words from
/// file-name stems. Frequency decides; the counts are occurrences across the project.
pub fn completion_from_project<'a>(index: &'a WordIndex, prefix: &str) -> Option<&'a str> {
    index.complete(prefix, |stat| f64::from(stat.count))
}

/// Builds the project vocabulary for `files` (paths relative to `root`). Stops early once
/// `current` no longer equals `generation`, so a superseded scan costs nothing more.
pub fn build_vocabulary(
    root: &Path,
    files: &[String],
    current: &AtomicU64,
    generation: u64,
) -> WordIndex {
    let mut index = WordIndex::default();
    let mut budget = VOCABULARY_BYTE_BUDGET;
    for rel in files {
        if current.load(Ordering::Relaxed) != generation {
            return WordIndex::default();
        }
        let stem = Path::new(rel)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        for word in stem.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if is_learnable_word(word) {
                index.bump(word, 1, 0, MAX_VOCABULARY_WORDS);
            }
        }
        let path = root.join(rel);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > VOCABULARY_FILE_LIMIT || meta.len() > budget {
            continue;
        }
        budget -= meta.len();
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes[..bytes.len().min(BINARY_PROBE_BYTES)].contains(&0) {
            continue;
        }
        scan_identifiers(&bytes, &mut index);
    }
    index
}

fn scan_identifiers(bytes: &[u8], index: &mut WordIndex) {
    let mut start: Option<usize> = None;
    for position in 0..=bytes.len() {
        let byte = bytes.get(position).copied().unwrap_or(b' ');
        let word_byte = byte.is_ascii_alphanumeric() || byte == b'_';
        match (start, word_byte) {
            (None, true) => start = Some(position),
            (Some(from), false) => {
                start = None;
                if let Ok(word) = std::str::from_utf8(&bytes[from..position])
                    && is_learnable_word(word)
                {
                    index.bump(word, 1, 0, MAX_VOCABULARY_WORDS);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests;
