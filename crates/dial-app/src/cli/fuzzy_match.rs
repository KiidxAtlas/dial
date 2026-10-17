//! Fuzzy subsequence matching with word-boundary,
//! compact-phrase, acronym, and alphanumeric-swap scoring. Lower scores rank higher.
//!
//! Positions are counted in `char`s, not in JavaScript-style UTF-16 units. The two only differ for
//! characters outside the Basic Multilingual Plane, which model ids do not contain.

use std::collections::HashSet;

const ALPHANUMERIC_SWAP_PENALTY: f64 = 5.0;
const COMPACT_PHRASE_BONUS: f64 = 1200.0;
const PHRASE_BONUS: f64 = 1000.0;
/// Shortest needle worth scanning past its leading occurrence.
const MIN_SHADOW_RESCAN_LENGTH: usize = 3;

/// Returns the match score when `query` fuzzy-matches `text`, `None` otherwise.
pub fn fuzzy_match(query: &str, text: &str) -> Option<f64> {
    let Some(prepared) = prepare_query(query) else {
        return Some(0.0);
    };
    fuzzy_match_core(&prepared, &build_index(text))
}

struct PreparedQuery {
    normalized: Vec<char>,
    tokens: Vec<Vec<char>>,
    token_swaps: Vec<Vec<Vec<char>>>,
    compact: Vec<char>,
}

struct SearchWord {
    text: Vec<char>,
    index: usize,
    ordinal: usize,
}

struct SearchIndex {
    normalized: Vec<char>,
    compact: Vec<char>,
    compact_word_starts: HashSet<usize>,
    words: Vec<SearchWord>,
}

/// Combining-mark ranges standing in for Unicode general category `Mark`.
fn is_mark(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
    )
}

/// Normalizes text for search: camel-case splits, lowercase, non-word runs to single spaces.
fn normalize(text: &str) -> Vec<char> {
    let chars: Vec<char> = text.chars().collect();

    // `/([A-Z]+)([A-Z][a-z])/g` -> "$1 $2": split before the last capital of an uppercase run that
    // is followed by a lowercase letter.
    let mut split = Vec::with_capacity(chars.len() + 4);
    let mut i = 0;
    while i < chars.len() {
        let mut j = i;
        while j < chars.len() && chars[j].is_ascii_uppercase() {
            j += 1;
        }
        if j - i >= 2 && j < chars.len() && chars[j].is_ascii_lowercase() {
            split.extend_from_slice(&chars[i..j - 1]);
            split.push(' ');
            split.push(chars[j - 1]);
            i = j;
        } else if j > i {
            split.extend_from_slice(&chars[i..j]);
            i = j;
        } else {
            split.push(chars[i]);
            i += 1;
        }
    }

    // `/([a-z0-9])([A-Z])/g` -> "$1 $2"
    let mut camel = Vec::with_capacity(split.len() + 4);
    for (k, &c) in split.iter().enumerate() {
        if c.is_ascii_uppercase()
            && k > 0
            && (split[k - 1].is_ascii_lowercase() || split[k - 1].is_ascii_digit())
        {
            camel.push(' ');
        }
        camel.push(c);
    }

    let replaced: String = camel
        .iter()
        .flat_map(|c| c.to_lowercase())
        .map(|c| {
            if c.is_alphanumeric() || is_mark(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    replaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect()
}

fn build_index(text: &str) -> SearchIndex {
    let normalized = normalize(text);
    let mut words = Vec::new();
    let mut compact = Vec::new();
    let mut compact_word_starts = HashSet::new();
    if !normalized.is_empty() {
        let mut index = 0;
        for (ordinal, word) in normalized.split(|c| *c == ' ').enumerate() {
            words.push(SearchWord {
                text: word.to_vec(),
                index,
                ordinal,
            });
            compact_word_starts.insert(compact.len());
            compact.extend_from_slice(word);
            index += word.len() + 1;
        }
    }
    SearchIndex {
        normalized,
        compact,
        compact_word_starts,
        words,
    }
}

fn prepare_query(query: &str) -> Option<PreparedQuery> {
    let normalized = normalize(query);
    if normalized.is_empty() {
        return None;
    }
    let tokens: Vec<Vec<char>> = normalized
        .split(|c| *c == ' ')
        .map(<[char]>::to_vec)
        .collect();
    let token_swaps = tokens.iter().map(|token| swap_queries(token)).collect();
    let compact = normalized.iter().copied().filter(|c| *c != ' ').collect();
    Some(PreparedQuery {
        normalized,
        tokens,
        token_swaps,
        compact,
    })
}

/// Each adjacent letter/digit pair swapped, deduplicated in order.
fn swap_queries(query: &[char]) -> Vec<Vec<char>> {
    let mut variants: Vec<Vec<char>> = Vec::new();
    for i in 0..query.len().saturating_sub(1) {
        let (current, next) = (query[i], query[i + 1]);
        let swap = (current.is_ascii_lowercase() && next.is_ascii_digit())
            || (current.is_ascii_digit() && next.is_ascii_lowercase());
        if !swap {
            continue;
        }
        let mut variant = query[..i].to_vec();
        variant.push(next);
        variant.push(current);
        variant.extend_from_slice(&query[i + 2..]);
        if !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    variants
}

fn with_position(score: f64, index: usize) -> f64 {
    score + index as f64 * 0.01
}

/// First occurrence of `needle` in `hay` at or after `from`.
fn find_from(hay: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
}

fn is_word_boundary_phrase(normalized: &[char], index: usize, length: usize) -> bool {
    let before = index == 0 || normalized[index - 1] == ' ';
    let after_index = index + length;
    let after = after_index == normalized.len() || normalized[after_index] == ' ';
    before && after
}

/// Offset of the first whole-word occurrence of `phrase`, or `None`.
fn find_word_boundary_phrase(normalized: &[char], phrase: &[char]) -> Option<usize> {
    if phrase.is_empty() {
        return None;
    }
    let first = find_from(normalized, phrase, 0)?;
    if is_word_boundary_phrase(normalized, first, phrase.len()) {
        return Some(first);
    }
    if first == 0 || normalized[first - 1] == ' ' || phrase.len() < MIN_SHADOW_RESCAN_LENGTH {
        return None;
    }
    let mut at = find_from(normalized, phrase, first + 1);
    while let Some(position) = at {
        if is_word_boundary_phrase(normalized, position, phrase.len()) {
            return Some(position);
        }
        at = find_from(normalized, phrase, position + 1);
    }
    None
}

/// Offset in the space-free text where `needle` starts a word.
fn find_compact_word_start(index: &SearchIndex, needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let first = find_from(&index.compact, needle, 0)?;
    if index.compact_word_starts.contains(&first) {
        return Some(first);
    }
    if needle.len() < MIN_SHADOW_RESCAN_LENGTH {
        return None;
    }
    let mut at = find_from(&index.compact, needle, first + 1);
    while let Some(position) = at {
        if index.compact_word_starts.contains(&position) {
            return Some(position);
        }
        at = find_from(&index.compact, needle, position + 1);
    }
    None
}

struct CharacterMatch {
    score: f64,
    span: i64,
}

/// In-order subsequence scoring; `None` when `query` is not a subsequence.
fn score_characters(query: &[char], text: &[char]) -> Option<CharacterMatch> {
    if query.is_empty() {
        return Some(CharacterMatch {
            score: 0.0,
            span: 0,
        });
    }
    if query.len() > text.len() {
        return None;
    }
    let mut query_index = 0;
    let mut score = 0.0;
    let mut first_match: i64 = -1;
    let mut last_match: i64 = -1;
    let mut consecutive: i64 = 0;
    for (i, &c) in text.iter().enumerate() {
        if query_index >= query.len() {
            break;
        }
        if c != query[query_index] {
            continue;
        }
        let i = i as i64;
        if first_match < 0 {
            first_match = i;
        }
        if last_match == i - 1 {
            consecutive += 1;
            score -= consecutive as f64 * 5.0;
        } else {
            consecutive = 0;
            if last_match >= 0 {
                score += ((i - last_match - 1) * 2) as f64;
            }
        }
        score += i as f64 * 0.1;
        last_match = i;
        query_index += 1;
    }
    if query_index < query.len() {
        return None;
    }
    Some(CharacterMatch {
        score,
        span: last_match - first_match + 1,
    })
}

fn score_token_against_word(token: &[char], word: &SearchWord) -> Option<f64> {
    let (token_len, word_len) = (token.len() as i64, word.text.len() as i64);
    if word.text == token {
        return Some(with_position(-200.0, word.index));
    }
    if word.text.starts_with(token) {
        return Some(with_position(
            -170.0 + (word_len - token_len) as f64 * 0.5,
            word.index,
        ));
    }
    if token.starts_with(&word.text) && token_len - word_len <= 2 {
        return Some(with_position(
            -150.0 + (token_len - word_len) as f64,
            word.index,
        ));
    }
    if let Some(offset) = find_from(&word.text, token, 0) {
        return Some(with_position(-20.0 + offset as f64, word.index));
    }
    let characters = score_characters(token, &word.text)?;
    let max_span = (token_len + 2).max((token_len as f64 * 1.8).ceil() as i64);
    if characters.span > max_span {
        return None;
    }
    Some(with_position(-40.0 + characters.score, word.index))
}

/// Acronym score: a 2–4 character token whose characters start consecutive-ish words.
fn score_acronym(token: &[char], index: &SearchIndex) -> Option<f64> {
    if token.len() < 2 || token.len() > 4 || index.words.is_empty() {
        return None;
    }
    let mut query_index = 0;
    let mut first_ordinal: Option<usize> = None;
    let mut last_ordinal = 0;
    let mut first_text_index = 0;
    for word in &index.words {
        if word.text[0] != token[query_index] {
            continue;
        }
        if first_ordinal.is_none() {
            first_ordinal = Some(word.ordinal);
            first_text_index = word.index;
        }
        last_ordinal = word.ordinal;
        query_index += 1;
        if query_index == token.len() {
            break;
        }
    }
    let first_ordinal = first_ordinal?;
    if query_index < token.len() {
        return None;
    }
    let word_span = (last_ordinal - first_ordinal + 1) as i64;
    if word_span > token.len() as i64 + 2 {
        return None;
    }
    Some(with_position(
        -30.0 + word_span as f64 * 4.0 - token.len() as f64 * 2.0,
        first_text_index,
    ))
}

/// Keeps the lower of `best` and `candidate`, as the `score < best.score` tests do.
fn keep_lower(best: &mut Option<f64>, candidate: Option<f64>) {
    if let Some(score) = candidate {
        match *best {
            Some(current) if current <= score => {}
            _ => *best = Some(score),
        }
    }
}

/// Best score among compact, per-word, and acronym matches.
fn score_token_direct(token: &[char], index: &SearchIndex) -> Option<f64> {
    if token.is_empty() {
        return Some(0.0);
    }
    let mut best: Option<f64> = None;
    keep_lower(
        &mut best,
        find_compact_word_start(index, token).map(|at| with_position(-140.0, at)),
    );
    for word in &index.words {
        keep_lower(&mut best, score_token_against_word(token, word));
    }
    keep_lower(&mut best, score_acronym(token, index));
    best
}

/// Direct score, falling back to alphanumeric-swap variants with a penalty.
fn score_token(token: &[char], swaps: &[Vec<char>], index: &SearchIndex) -> Option<f64> {
    if let Some(score) = score_token_direct(token, index) {
        return Some(score);
    }
    let mut best: Option<f64> = None;
    for variant in swaps {
        keep_lower(
            &mut best,
            score_token_direct(variant, index).map(|direct| direct + ALPHANUMERIC_SWAP_PENALTY),
        );
    }
    best
}

fn fuzzy_match_core(query: &PreparedQuery, index: &SearchIndex) -> Option<f64> {
    if index.words.is_empty() {
        return None;
    }
    let mut total = 0.0;
    if let Some(at) = find_word_boundary_phrase(&index.normalized, &query.normalized) {
        total -= PHRASE_BONUS;
        total += at as f64 * 0.01;
    }
    if let Some(at) = find_compact_word_start(index, &query.compact) {
        total -= COMPACT_PHRASE_BONUS;
        total += at as f64 * 0.01;
    }
    for (token, swaps) in query.tokens.iter().zip(&query.token_swaps) {
        total += score_token(token, swaps, index)?;
    }
    Some(total)
}
