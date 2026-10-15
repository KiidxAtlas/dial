//! Bounded detectors for runaway repeated reasoning and output.
//!
//! Three signals:
//! - exact suffix cycle (every model): a byte-identical unit repeated back-to-back at the end of the
//!   stream. Units are at least two characters; short units (≤ 60) need four repeats covering 180
//!   characters; long units need three repeats covering 1024 characters; the unit must contain a letter
//!   or a pictograph, so separators, indentation, table borders and digit runs never count.
//! - near-duplicate and progress-stall paragraphs (semantic; gemini, deepseek and xai only).
//! - reasoning-summary header runaway (gemini only), see [`GeminiHeaderRunDetector`].
//!
//! Memory is bounded: the exact tail is capped and the semantic windows hold fixed counts.
use std::collections::{HashSet, VecDeque};
use std::sync::LazyLock;

use regex::Regex;

const EXACT_TAIL_WINDOW: usize = 4096;
const EXACT_MAX_UNIT: usize = 1024;
const EXACT_CHECK_STRIDE: usize = 128;
const EXACT_SHORT_MAX_UNIT: usize = 60;
const EXACT_SHORT_MIN_REPEATED_CHARS: usize = 180;
const EXACT_LONG_MIN_REPEATED_CHARS: usize = 1024;
const SEGMENT_CHAR_CAP: usize = 700;
const SEGMENT_MIN_NORM_CHARS: usize = 60;
const SEGMENT_WINDOW: usize = 16;
const SEGMENT_SIMILARITY: f64 = 0.8;
const SEGMENT_MIN_COUNT: usize = 8;
const SEGMENT_MIN_CLUSTER: usize = 4;
const LEX_NOVELTY_WINDOW: usize = 8;
const LEX_STALL_NOVELTY_FLOOR: f64 = 0.2;
const LEX_STALL_MIN_RUN: usize = 8;

/// Reasoning-summary headers in one generation before the gemini header runaway trips.
pub(crate) const GEMINI_HEADER_RUNAWAY_THRESHOLD: usize = 36;

/// Model families whose reasoning is prone to loops, and which the semantic and header
/// heuristics target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelClass {
    Gemini,
    DeepSeek,
    Xai,
    Other,
}

pub(crate) fn model_class(model: &str) -> ModelClass {
    let id = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    if id.starts_with("gemini") {
        ModelClass::Gemini
    } else if id.starts_with("deepseek") {
        ModelClass::DeepSeek
    } else if id.starts_with("grok") || id.starts_with("xai") {
        ModelClass::Xai
    } else {
        ModelClass::Other
    }
}

/// Which detectors run for a model. The exact cycle detector runs for every model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoopPolicy {
    pub(crate) semantic: bool,
    pub(crate) header: bool,
}

/// `DIAL_NO_THINKING_LOOP_GUARD=1` disables every stream loop detector.
pub(crate) fn disabled_by_env() -> bool {
    std::env::var("DIAL_NO_THINKING_LOOP_GUARD").is_ok_and(|value| value == "1")
}

pub(crate) fn policy_for(model: &str) -> Option<LoopPolicy> {
    policy_with(model, disabled_by_env())
}

fn policy_with(model: &str, disabled: bool) -> Option<LoopPolicy> {
    if disabled {
        return None;
    }
    let class = model_class(model);
    Some(LoopPolicy {
        semantic: matches!(
            class,
            ModelClass::Gemini | ModelClass::DeepSeek | ModelClass::Xai
        ),
        header: class == ModelClass::Gemini,
    })
}

#[derive(Debug)]
pub(crate) struct ThinkingLoopGuard {
    tail: Vec<char>,
    since_check: usize,
    z: Vec<usize>,
    semantic: Option<SemanticDetector>,
}

/// A letter, or a character in the emoji/pictograph blocks (`\p{L}` or `\p{Extended_Pictographic}`).
fn meaningful(ch: char) -> bool {
    ch.is_alphabetic() || matches!(ch as u32, 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0x1F000..=0x1FAFF)
}

impl ThinkingLoopGuard {
    pub(crate) fn new(semantic: bool) -> Self {
        Self {
            tail: Vec::new(),
            since_check: 0,
            z: Vec::new(),
            semantic: semantic.then(SemanticDetector::default),
        }
    }

    /// Feed newly streamed reasoning or text. Returns a reason on the first loop verdict.
    pub(crate) fn push(&mut self, delta: &str) -> Option<String> {
        if delta.is_empty() {
            return None;
        }
        let added = delta.chars().count();
        self.tail.extend(delta.chars());
        if self.tail.len() > 2 * EXACT_TAIL_WINDOW {
            let excess = self.tail.len() - EXACT_TAIL_WINDOW;
            self.tail.drain(..excess);
        }
        self.since_check += added;
        if self.since_check >= EXACT_CHECK_STRIDE || added >= EXACT_CHECK_STRIDE {
            self.since_check = 0;
            if let Some(hit) = self.exact_hit() {
                return Some(hit);
            }
        }
        self.semantic.as_mut()?.push(delta)
    }

    /// Final check when a thinking block or text stream ends: the exact cycle always, then any
    /// buffered trailing paragraph, which may complete a duplicate cluster.
    pub(crate) fn flush(&mut self) -> Option<String> {
        if let Some(hit) = self.exact_hit() {
            return Some(hit);
        }
        self.semantic.as_mut()?.flush()
    }

    fn exact_hit(&mut self) -> Option<String> {
        let window = &self.tail[self.tail.len().saturating_sub(EXACT_TAIL_WINDOW)..];
        exact_cycle(window, &mut self.z).map(|(len, times)| {
            format!("repeated an exact {len}-character cycle {times}× back-to-back")
        })
    }
}

/// Z-function over the reversed window. Returns `(unit_len, count)`
/// for the first qualifying suffix cycle, shortest unit first.
fn exact_cycle(text: &[char], z: &mut Vec<usize>) -> Option<(usize, usize)> {
    let n = text.len();
    if n < EXACT_SHORT_MIN_REPEATED_CHARS {
        return None;
    }
    let max_unit = EXACT_MAX_UNIT.min(n / 3);
    z.clear();
    z.resize(max_unit + 1, 0);
    let reversed = |k: usize| text[n - 1 - k];
    let (mut left, mut right) = (0usize, 0usize);
    for i in 1..=max_unit {
        let mut zi = if i <= right {
            (right - i + 1).min(z[i - left])
        } else {
            0
        };
        while i + zi < n && reversed(zi) == reversed(i + zi) {
            zi += 1;
        }
        z[i] = zi;
        if i + zi > right + 1 {
            left = i;
            right = i + zi - 1;
        }
    }
    for len in 2..=max_unit {
        let count = 1 + z[len] / len;
        let (min_count, min_chars) = if len <= EXACT_SHORT_MAX_UNIT {
            (4, EXACT_SHORT_MIN_REPEATED_CHARS)
        } else {
            (3, EXACT_LONG_MIN_REPEATED_CHARS)
        };
        if count < min_count || len * count < min_chars {
            continue;
        }
        if text[n - len..].iter().copied().any(meaningful) {
            return Some((len, count));
        }
    }
    None
}

static BOUNDARY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n").expect("static regex"));

/// Concrete-anchor pattern. `\w` and `\b` are ASCII here, as in JavaScript without the `u` flag.
static ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"`[^`]+`",
        r"|(?-u:\b)[A-Za-z0-9_]{2,}\.[A-Za-z][A-Za-z0-9_]{0,4}(?-u:\b)",
        r"|[A-Za-z0-9_-]+(?:/[A-Za-z0-9_-]+){2,}",
        r"|(?-u:\b)[A-Za-z0-9_]+_[A-Za-z0-9_]+(?-u:\b)",
        r"|(?-u:\b)[a-z]+[A-Z][A-Za-z0-9_]*(?-u:\b)",
        r"|(?-u:\b)[A-Z][a-z]+[A-Z][A-Za-z0-9_]*(?-u:\b)",
    ))
    .expect("static regex")
});

/// Near-duplicate paragraph and progress-stall detection over blank-line-separated segments.
#[derive(Debug, Default)]
struct SemanticDetector {
    pending: String,
    fingerprints: VecDeque<HashSet<String>>,
    vocab: VecDeque<HashSet<String>>,
    anchors: VecDeque<HashSet<String>>,
    lex_stall_run: usize,
    count: usize,
}

impl SemanticDetector {
    fn push(&mut self, delta: &str) -> Option<String> {
        self.pending.push_str(delta);
        loop {
            let boundary = BOUNDARY.find(&self.pending).map(|m| (m.start(), m.end()));
            let raw = if let Some((start, end)) = boundary {
                let raw = self.pending[..start].to_string();
                self.pending.drain(..end);
                raw
            } else if self.pending.chars().count() > SEGMENT_CHAR_CAP {
                // A runaway segment without a boundary is forced out in capped pieces.
                let (head, rest) = split_at_chars(&self.pending, SEGMENT_CHAR_CAP);
                let raw = head.to_string();
                self.pending = rest.to_string();
                raw
            } else {
                return None;
            };
            if let Some(hit) = self.consume_capped(&raw) {
                return Some(hit);
            }
        }
    }

    fn flush(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.pending);
        self.consume_capped(&rest)
    }

    fn consume_capped(&mut self, raw: &str) -> Option<String> {
        let mut rest = raw;
        while !rest.is_empty() {
            let (chunk, tail) = split_at_chars(rest, SEGMENT_CHAR_CAP);
            if let Some(hit) = self.consume(chunk) {
                return Some(hit);
            }
            rest = tail;
        }
        None
    }

    fn consume(&mut self, raw: &str) -> Option<String> {
        let segment = strip_code_and_titles(raw);
        let normalized = normalize_segment(&segment);
        if normalized.len() < SEGMENT_MIN_NORM_CHARS {
            return None;
        }

        // (a) Near-duplicate trigram cluster: the same paragraph with cosmetic wording drift.
        let fingerprint = trigram_shingles(&normalized);
        let cluster = 1 + self
            .fingerprints
            .iter()
            .filter(|prev| jaccard(&fingerprint, prev) >= SEGMENT_SIMILARITY)
            .count();

        // (b) Progress-lexicon stall: low novelty and no new concrete anchor.
        let words: HashSet<String> = normalized
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        let novelty = {
            let prior: HashSet<&str> = self.vocab.iter().flatten().map(String::as_str).collect();
            if prior.is_empty() {
                1.0
            } else {
                words.iter().filter(|w| !prior.contains(w.as_str())).count() as f64
                    / words.len() as f64
            }
        };
        // Canonicalize so `Foo`, Foo and FOO are one anchor and cannot pose as new.
        let anchors: HashSet<String> = ANCHOR
            .find_iter(&segment)
            .map(|m| m.as_str().replace('`', "").to_lowercase())
            .collect();
        let new_anchor = anchors
            .iter()
            .any(|a| self.anchors.iter().all(|seen| !seen.contains(a)));

        if novelty <= LEX_STALL_NOVELTY_FLOOR && !new_anchor {
            self.lex_stall_run += 1;
        } else {
            self.lex_stall_run = 0;
        }

        push_bounded(&mut self.fingerprints, fingerprint, SEGMENT_WINDOW);
        push_bounded(&mut self.vocab, words, LEX_NOVELTY_WINDOW);
        push_bounded(&mut self.anchors, anchors, LEX_NOVELTY_WINDOW);
        self.count += 1;

        if self.count >= SEGMENT_MIN_COUNT {
            if cluster >= SEGMENT_MIN_CLUSTER {
                return Some(format!(
                    "{cluster} near-identical segments within the last {SEGMENT_WINDOW}"
                ));
            }
            if self.lex_stall_run >= LEX_STALL_MIN_RUN {
                return Some(format!(
                    "{} low-information segments recycling recent wording",
                    self.lex_stall_run
                ));
            }
        }
        None
    }
}

fn push_bounded<T>(window: &mut VecDeque<T>, item: T, cap: usize) {
    window.push_back(item);
    if window.len() > cap {
        window.pop_front();
    }
}

/// Splits after `n` characters (not bytes).
fn split_at_chars(text: &str, n: usize) -> (&str, &str) {
    let index = text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    text.split_at(index)
}

/// Drops code lines, markdown headings and bold titles: per-thought formatting, not chain of
/// The code-line lookahead is applied per line here, as the regex crate lacks lookahead.
fn strip_code_and_titles(segment: &str) -> String {
    segment
        .split('\n')
        .filter(|line| !is_stripped_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_stripped_line(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t']);
    if trimmed.starts_with("```") {
        return true;
    }
    // Indented (two or more spaces, or a tab) and not a list item.
    let indented = line.starts_with("  ") || line.starts_with('\t');
    if indented && !is_list_marker(trimmed) {
        return true;
    }
    // Any line ending in a code punctuation character.
    if line
        .trim_end_matches([' ', '\t'])
        .ends_with(['{', '}', '[', ']', '(', ';', ',', '>'])
    {
        return true;
    }
    // Markdown heading: one to six hashes, then a space or tab.
    let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with([' ', '\t']) {
        return true;
    }
    // Bold title: `**...**` or `***...***`, at least one character inside.
    let bold = trimmed.trim_end_matches([' ', '\t']);
    bold.starts_with("**") && bold.ends_with("**") && bold.chars().count() >= 5
}

fn is_list_marker(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some('-' | '*' | '+') => chars.next().is_some_and(|c| c == ' ' || c == '\t'),
        Some(c) if c.is_ascii_digit() => {
            let rest = text.trim_start_matches(|c: char| c.is_ascii_digit());
            let mut after = rest.chars();
            matches!(after.next(), Some('.' | ')'))
                && after.next().is_some_and(|c| c == ' ' || c == '\t')
        }
        _ => false,
    }
}

/// Lowercase, non-alphanumerics become separators, and tokens without a letter are dropped.
fn normalize_segment(segment: &str) -> String {
    let mapped: String = segment
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                ' '
            }
        })
        .collect();
    mapped
        .split_whitespace()
        .filter(|token| token.bytes().any(|b| b.is_ascii_lowercase()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Word-trigram shingles; fewer than three words yields the whole segment as one shingle.
fn trigram_shingles(normalized: &str) -> HashSet<String> {
    let words: Vec<&str> = normalized.split(' ').filter(|w| !w.is_empty()).collect();
    match words.len() {
        0 => HashSet::new(),
        1 | 2 => HashSet::from([words.join(" ")]),
        _ => words.windows(3).map(|w| w.join(" ")).collect(),
    }
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (small, large) = if a.len() < b.len() { (a, b) } else { (b, a) };
    let intersection = small.iter().filter(|x| large.contains(*x)).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Counts reasoning-summary header lines (`## Title`, `**Title**`) in a generation. Gemini can emit
/// dozens of these with no tool call, so a long run ends the attempt.
#[derive(Debug, Default)]
pub(crate) struct GeminiHeaderRunDetector {
    pending: String,
    count: usize,
    fired: bool,
}

impl GeminiHeaderRunDetector {
    /// Returns the header count the first time the threshold is reached, then `None` for good.
    pub(crate) fn push(&mut self, delta: &str) -> Option<usize> {
        if self.fired {
            return None;
        }
        self.pending.push_str(delta);
        while let Some(newline) = self.pending.find('\n') {
            let header = is_reasoning_summary_header(self.pending[..newline].trim());
            self.pending.drain(..=newline);
            if header {
                self.count += 1;
                if self.count >= GEMINI_HEADER_RUNAWAY_THRESHOLD {
                    self.fired = true;
                    return Some(self.count);
                }
            }
        }
        None
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Whether a trimmed line is a reasoning-summary header: `/^#{1,6}[ \t]+\S/` or `/^\*{2,3}.+\*{2,3}$/`.
fn is_reasoning_summary_header(line: &str) -> bool {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) {
        let rest = &line[hashes..];
        let after = rest.trim_start_matches([' ', '\t']);
        if after.len() < rest.len() && after.chars().next().is_some_and(|c| !c.is_whitespace()) {
            return true;
        }
    }
    line.starts_with("**") && line.ends_with("**") && line.chars().count() >= 5
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(text: &str) -> Option<String> {
        let mut guard = ThinkingLoopGuard::new(true);
        let chars: Vec<char> = text.chars().collect();
        chars
            .chunks(17)
            .find_map(|chunk| guard.push(&chunk.iter().collect::<String>()))
            .or_else(|| guard.flush())
    }

    /// Deterministic pseudo-random lowercase words, so no cycle or shared vocabulary exists.
    fn random_words(seed: u32, count: usize) -> Vec<String> {
        let mut state = seed;
        (0..count)
            .map(|_| {
                (0..7)
                    .map(|_| {
                        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                        char::from(b'a' + ((state >> 16) % 26) as u8)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn verbatim_back_to_back_repetition_trips() {
        assert!(feed(&"🌊 ".repeat(120)).is_some());
        assert!(feed(&"I will now check the file again. ".repeat(8)).is_some());
    }

    #[test]
    fn formatting_runs_and_brief_repeats_are_not_loops() {
        for text in [
            "-".repeat(500),
            " ".repeat(500),
            "| --- ".repeat(100),
            "00 ".repeat(200),
            "🌊 ".repeat(26),
            format!("intro\n{}\n", "=".repeat(400)),
        ] {
            assert_eq!(feed(&text), None, "{text:?}");
        }
    }

    #[test]
    fn long_cycles_need_three_repeats_and_1024_characters() {
        // A pseudo-random unit, so no shorter cycle exists inside it.
        let mut state: u32 = 12345;
        let unit: String = (0..300)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                char::from(b'a' + ((state >> 16) % 26) as u8)
            })
            .collect::<String>()
            + ".";
        assert_eq!(
            feed(&unit.repeat(3)),
            None,
            "903 repeated characters is below the long-cycle floor"
        );
        assert!(feed(&unit.repeat(4)).is_some());
    }

    #[test]
    fn bounded_tail_handles_large_stream() {
        let mut guard = ThinkingLoopGuard::new(true);
        assert!(guard.push(&"x".repeat(20_000)).is_some());
        assert!(guard.tail.len() <= 2 * EXACT_TAIL_WINDOW);
    }

    #[test]
    fn distinct_paragraphs_do_not_trip() {
        let mut guard = ThinkingLoopGuard::new(true);
        for (index, words) in random_words(7, 40).chunks(6).enumerate() {
            let paragraph = format!(
                "Step {index}: {}. Next I weigh the {} option carefully.\n\n",
                words.join(" "),
                words[0]
            );
            assert_eq!(guard.push(&paragraph), None, "paragraph {index}");
        }
        assert_eq!(guard.flush(), None);
    }

    #[test]
    fn near_duplicate_paragraphs_trip_on_cluster() {
        let mut guard = ThinkingLoopGuard::new(true);
        let mut hit = None;
        for index in 0..10 {
            let paragraph = format!(
                "We should inspect the parser module next and then compare the output with fixture number {index} before deciding anything else.\n\n"
            );
            hit = hit.or_else(|| guard.push(&paragraph));
        }
        let reason = hit.expect("near-duplicate paragraphs must trip");
        assert!(reason.contains("near-identical segments"), "{reason}");
    }

    #[test]
    fn recycled_vocabulary_without_new_anchors_trips_progress_stall() {
        let vocabulary = random_words(99, 12);
        let mut guard = ThinkingLoopGuard::new(true);
        let mut hit = None;
        for round in 0..12 {
            let mut words = vocabulary.clone();
            words.rotate_left((round * 5) % 12);
            words.reverse();
            hit = hit.or_else(|| guard.push(&format!("{}.\n\n", words.join(" "))));
        }
        assert!(
            hit.is_some(),
            "recycled wording with no new anchors must trip"
        );
    }

    #[test]
    fn flush_catches_a_trailing_duplicate_paragraph() {
        let mut guard = ThinkingLoopGuard::new(true);
        let paragraph = |index: usize| {
            format!(
                "We should inspect the parser module next and then compare the output with fixture number {index} before deciding anything else."
            )
        };
        for index in 0..7 {
            assert_eq!(guard.push(&format!("{}\n\n", paragraph(index))), None);
        }
        assert!(guard.push(&paragraph(7)).is_none());
        assert!(guard.flush().is_some());
    }

    #[test]
    fn code_titles_and_headings_do_not_count_as_prose() {
        let mut guard = ThinkingLoopGuard::new(true);
        for index in 0..12 {
            assert_eq!(guard.push(&format!("## Plan {index}\n\n**Maintaining Momentum {index}**\n\n    let x{index} = y;\n\n")), None);
        }
    }

    #[test]
    fn gemini_header_run_trips_at_thirty_six() {
        let mut thirty_five = GeminiHeaderRunDetector::default();
        let text: String = (0..35).map(|i| format!("## Plan step {i}\n")).collect();
        assert_eq!(thirty_five.push(&text), None);

        let mut thirty_six = GeminiHeaderRunDetector::default();
        let text: String = (0..36).map(|i| format!("## Plan step {i}\n")).collect();
        assert_eq!(
            thirty_six.push(&text),
            Some(GEMINI_HEADER_RUNAWAY_THRESHOLD)
        );
        assert_eq!(thirty_six.push("## again\n"), None, "latches after firing");
    }

    #[test]
    fn policy_scopes_semantic_and_header_detectors_by_model_class() {
        let gemini = policy_with("google/gemini-2.5-pro", false).unwrap();
        assert!(gemini.semantic && gemini.header);
        let deepseek = policy_with("deepseek-reasoner", false).unwrap();
        assert!(deepseek.semantic && !deepseek.header);
        let other = policy_with("anthropic/claude-sonnet", false).unwrap();
        assert!(!other.semantic && !other.header);
        assert_eq!(
            policy_with("gemini-2.5-pro", true),
            None,
            "kill switch disables every detector"
        );
    }
}
