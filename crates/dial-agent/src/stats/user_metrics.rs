//! Deterministic user-message behaviour signals (`/api/stats/frustration`). These are keyword and
//! shape heuristics only; a model-judged frustration score is not implemented.
//!
//! Message text is never stored: [`Db::frustration`] reads the in-tree transcripts on demand and
//! returns aggregate counts only.

use super::{Db, for_each_record, injected, session_file_in_tree};
use anyhow::Result;
use fancy_regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::{path::Path, sync::LazyLock};

/// Shouting needs at least this many letters in a sentence.
const YELLING_MIN_LETTERS: usize = 4;
/// Fraction of letters that must be uppercase for a sentence to count as shouted.
const YELLING_THRESHOLD: f64 = 0.5;
/// Messages with this many or more non-empty prose lines are treated as pasted content.
const MAX_PROSE_LINES: usize = 3;

const PROFANITY: &[&str] = &[
    "arse",
    "arsehole",
    "ass",
    "asshole",
    "bastard",
    "bitch",
    "bollocks",
    "bullshit",
    "crap",
    "damn",
    "dammit",
    "dick",
    "dumbass",
    "fuck",
    "fucked",
    "fucker",
    "fucking",
    "goddamn",
    "motherfucker",
    "piss",
    "shit",
    "shitty",
    "slut",
    "twat",
    "wanker",
    "whore",
];

/// Counts of heuristic signals across user messages, plus the text volume they came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct UserMessageMetrics {
    pub chars: u64,
    pub words: u64,
    pub yelling: u64,
    pub profanity: u64,
    pub anguish: u64,
    pub negation: u64,
    pub repetition: u64,
    pub blame: u64,
}

impl UserMessageMetrics {
    /// True when any behaviour signal fired (volume alone does not flag a message).
    pub fn flagged(&self) -> bool {
        self.yelling + self.profanity + self.anguish + self.negation + self.repetition + self.blame
            > 0
    }

    fn add(&mut self, other: &Self) {
        self.chars += other.chars;
        self.words += other.words;
        self.yelling += other.yelling;
        self.profanity += other.profanity;
        self.anguish += other.anguish;
        self.negation += other.negation;
        self.repetition += other.repetition;
        self.blame += other.blame;
    }
}

/// Aggregate frustration heuristics over the user messages of the sessions in a range.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FrustrationReport {
    pub since_ms: Option<i64>,
    pub sessions_scanned: u64,
    pub sessions_unreadable: u64,
    pub user_messages: u64,
    pub flagged_messages: u64,
    /// Share of user messages with at least one signal, in percent.
    pub flagged_rate: Option<f64>,
    pub signals: UserMessageMetrics,
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("static stats pattern compiles")
}

static DRAMA: LazyLock<Regex> = LazyLock::new(|| pattern(r"[!?][!?1]{2,}"));
static ANGUISH: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)\b(?:no{3,}|a+h{2,}|u+r?g+h+|a+r+g+h+|g+r{2,}|st+o{3,}p+|w+h+y{3,}|f+u{3,}c*k*|wtf{3,}|o+m+g{2,}|ye+s{3,}|g+o+d{3,}|br+u+h{2,})\b",
    )
});
static DUDE: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)\bdude\b"));
static SAD_EMOTICON: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?:^|[\s.!?])[:;]-?\(+"));
static PROFANITY_RE: LazyLock<Regex> =
    LazyLock::new(|| pattern(&format!(r"(?i)\b(?:{})\b", PROFANITY.join("|"))));
static NEGATION_LEAD: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)^[ \t]*(?:(?:nope|nah|nvm|wrong|incorrect)\b|no(?=\s*(?:[,.!?;:–—]|-(?!\w)|$|(?:i|im|u|you|ur|we|it|its|that|thats|this|the|they|theyre|he|she|man|dude|bro|wait|dont|not|stop|just|again|please|plz|but|actually|literally|seriously|sorry|no|never|nothing|wtf|why|what|wrong)\b)))",
    )
});
static NEGATION_PHRASE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)\b(?:that['’]?s\s+not\s+(?:what|right|it)|not\s+what\s+i\s+(?:meant|asked|said|wanted)|makes\s+(?:no|zero)\s+sense)\b",
    )
});
static REPETITION_RECALL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)\b(?:(?:like|as)\s+i\s+(?:said|told\s+you|asked)|i\s+(?:meant|said|told\s+you|asked\s+you|already\s+(?:said|told|did|asked|wrote)))\b",
    )
});
static REPETITION_STILL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)\bstill\s+(?:doesn['’]?t|doesnt|isn['’]?t|isnt|not|broken|wrong|fails|failing|the\s+same|same)\b",
    )
});
static BLAME_YOU: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?i)\byou\s+(?:didn['’]?t|did\s+not|broke|missed|forgot|keep|always|never|still|ignored)\b",
    )
});
static BLAME_WHY: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i)\bwhy\s+(?:would|did)\s+(?:you|u)\b"));
static BLAME_STOP: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?im)(?:^|(?<=[.!?\n]))\s*stop\s+\w+ing\b"));

static FENCED_CODE: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?s)```.*?```"));
static XML_TAG_PAIR: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"<([A-Za-z][\w-]*)\b[^>]*>[\s\S]*?</\1>"));
static XML_TAG_BARE: LazyLock<Regex> = LazyLock::new(|| pattern(r"</?[A-Za-z][\w-]*\b[^>]*/?>"));
static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| pattern(r"`[^`\n]*`"));
static URL: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i)\bhttps?://\S+"));
static FILE_MENTION: LazyLock<Regex> = LazyLock::new(|| pattern(r"(^|\s)@[\w./-]+"));
static DOTTED_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"(^|[\s("'\[])[\w-]+(?:\.[\w-]+)+(?=$|[\s)"'\],:;!?]|\.(?!\w))"#));
static QUOTE_LINE: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?m)^[ \t]*>.*$"));
static IMAGE_MARKER: LazyLock<Regex> = LazyLock::new(|| pattern(r"\[Image #\d+\]"));
static ANSI_ESCAPE: LazyLock<Regex> = LazyLock::new(|| pattern(r"\x1b\[[0-9;]*[A-Za-z]"));

static SENTENCE: LazyLock<Regex> = LazyLock::new(|| pattern(r"[^.!?\n]+"));
static LETTER: LazyLock<Regex> = LazyLock::new(|| pattern(r"\p{L}"));
static UPPER_LETTER: LazyLock<Regex> = LazyLock::new(|| pattern(r"\p{Lu}"));
static UPPER_RUN: LazyLock<Regex> = LazyLock::new(|| pattern(r"\p{Lu}{2,}"));
static TRIPLED_LETTER: LazyLock<Regex> = LazyLock::new(|| pattern(r"(\p{Lu})\1\1"));
static WORD: LazyLock<Regex> = LazyLock::new(|| pattern(r"\S+"));

/// Number of matches of `re` in `text`. A match that fails to run (backtrack limit) is not counted.
fn count(re: &Regex, text: &str) -> u64 {
    re.find_iter(text).filter_map(Result::ok).count() as u64
}

fn replace(re: &Regex, text: &str, with: &str) -> String {
    re.replace_all(text, with).into_owned()
}

/// Removes code, markup, URLs, file mentions, dotted identifiers, quotes and escape codes so the
/// signals only see what the person wrote in prose.
fn strip_structured(text: &str) -> String {
    let text = replace(&FENCED_CODE, text, "\n");
    let text = replace(&XML_TAG_PAIR, &text, "\n");
    let text = replace(&XML_TAG_BARE, &text, " ");
    let text = replace(&INLINE_CODE, &text, " ");
    let text = replace(&URL, &text, " ");
    let text = replace(&FILE_MENTION, &text, "${1} ");
    let text = replace(&DOTTED_TOKEN, &text, "${1} ");
    let text = replace(&QUOTE_LINE, &text, "");
    let text = replace(&IMAGE_MARKER, &text, " ");
    replace(&ANSI_ESCAPE, &text, "")
}

/// A sentence is shouted when its uppercase letters dominate and it has either two or more
/// uppercase runs, or one long run containing a tripled capital (e.g. `WTTT`).
fn is_shouted_sentence(sentence: &str) -> bool {
    let runs: Vec<&str> = UPPER_RUN
        .find_iter(sentence)
        .filter_map(Result::ok)
        .map(|m| m.as_str())
        .collect();
    if runs.len() >= 2 {
        return true;
    }
    match runs.first() {
        Some(run) => {
            run.chars().count() >= YELLING_MIN_LETTERS
                && TRIPLED_LETTER.is_match(run).unwrap_or(false)
        }
        None => false,
    }
}

fn count_yelling_sentences(prose: &str) -> u64 {
    SENTENCE
        .find_iter(prose)
        .filter_map(Result::ok)
        .filter(|m| {
            let sentence = m.as_str();
            let letters = count(&LETTER, sentence) as usize;
            if letters < YELLING_MIN_LETTERS {
                return false;
            }
            let uppercase = count(&UPPER_LETTER, sentence) as f64;
            uppercase / letters as f64 > YELLING_THRESHOLD && is_shouted_sentence(sentence)
        })
        .count() as u64
}

/// Computes the signals for one user message. Volume (`chars`, `words`) is always reported; the
/// behaviour signals are only computed for short prose.
pub fn compute(text: &str) -> UserMessageMetrics {
    let trimmed = text.trim();
    let mut metrics = UserMessageMetrics {
        chars: trimmed.chars().count() as u64,
        words: count(&WORD, trimmed),
        ..UserMessageMetrics::default()
    };
    let stripped = strip_structured(trimmed);
    let prose = stripped.trim();
    let prose_lines = prose
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .count();
    if prose.is_empty() || prose_lines >= MAX_PROSE_LINES {
        return metrics;
    }
    metrics.anguish = count(&DRAMA, prose)
        + count(&ANGUISH, prose)
        + count(&DUDE, prose)
        + count(&SAD_EMOTICON, prose);
    metrics.negation = count(&NEGATION_LEAD, prose) + count(&NEGATION_PHRASE, prose);
    metrics.repetition = count(&REPETITION_RECALL, prose) + count(&REPETITION_STILL, prose);
    metrics.blame = count(&BLAME_YOU, prose) + count(&BLAME_WHY, prose) + count(&BLAME_STOP, prose);
    metrics.yelling = count_yelling_sentences(prose);
    metrics.profanity = count(&PROFANITY_RE, prose);
    metrics
}

/// Text of a transcript message: a plain string, or the `text` parts of a content array.
fn message_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Tallies the user messages of one transcript. Errors (unreadable file) discard the tally.
fn scan_session(file: &Path) -> Result<FrustrationReport> {
    let mut tally = FrustrationReport::default();
    for_each_record(file, |value| {
        if value["type"] != "message" {
            return;
        }
        let message = &value["message"];
        if message["role"] != "user" || injected(message) {
            return;
        }
        let metrics = compute(&message_text(&message["content"]));
        tally.user_messages += 1;
        if metrics.flagged() {
            tally.flagged_messages += 1;
        }
        tally.signals.add(&metrics);
    })?;
    Ok(tally)
}

impl Db {
    /// Frustration heuristics over the user messages of sessions active since `since_ms` (None =
    /// all time). Transcripts are read on demand; nothing about their text is stored.
    pub fn frustration(&self, user_dir: &Path, since_ms: Option<i64>) -> Result<FrustrationReport> {
        let since = since_ms.unwrap_or(i64::MIN);
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM sessions WHERE last >= ?1 ORDER BY path")?;
        let paths: Vec<String> = stmt
            .query_map([since], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut report = FrustrationReport {
            since_ms,
            ..FrustrationReport::default()
        };
        for stored in paths {
            let Some((_, file)) = session_file_in_tree(user_dir, Path::new(&stored)) else {
                report.sessions_unreadable += 1;
                continue;
            };
            match scan_session(&file) {
                Ok(tally) => {
                    report.sessions_scanned += 1;
                    report.user_messages += tally.user_messages;
                    report.flagged_messages += tally.flagged_messages;
                    report.signals.add(&tally.signals);
                }
                Err(_) => report.sessions_unreadable += 1,
            }
        }
        let user_messages = report.user_messages;
        report.flagged_rate = (user_messages > 0)
            .then(|| report.flagged_messages as f64 * 100.0 / user_messages as f64);
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
