//! Conservative verbatim checks, not a semantic-equivalence judge. Human draft review remains
//! necessary for scope, negation, inferred requirements and other natural-language meaning.
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

// Preserve whole path/reference/numeric tokens, not substrings (2 must not match 20).
static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[@~\p{L}\p{N}_./\\:+%\-]+").expect("built-in turn-draft token regex is valid")
});

fn tokens(text: &str) -> impl Iterator<Item = &str> {
    TOKENS
        .find_iter(text)
        .map(|m| m.as_str().trim_end_matches(['.', ':']))
}

fn protected(token: &str) -> bool {
    // Common prose abbreviations are not filenames. Real dotted identifiers remain conservative.
    if token.eq_ignore_ascii_case("e.g") || token.eq_ignore_ascii_case("i.e") {
        return false;
    }
    let numeric = token.trim_start_matches(['-', '+']);
    token.starts_with('@')
        || token.contains('/')
        || token.contains('\\')
        || token.split_once('.').is_some_and(|(stem, extension)| {
            !stem.is_empty() && extension.chars().next().is_some_and(char::is_alphabetic)
        })
        || numeric.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn contains_span(draft: &str, span: &str) -> bool {
    let joins_token = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '/' | '\\');
    draft.match_indices(span).any(|(at, _)| {
        let before = draft[..at].chars().next_back();
        let after = draft[at + span.len()..].chars().next();
        !(span.chars().next().is_some_and(joins_token) && before.is_some_and(joins_token)
            || span.chars().next_back().is_some_and(joins_token) && after.is_some_and(joins_token))
    })
}

/// Reject missing explicit material without echoing it into an error/log. Delimited code and
/// quotations must survive byte-for-byte internally; the surrounding delimiters may change.
/// Unclosed delimiters are treated as prose rather than inventing a quote span.
pub(super) fn preserves(original: &str, draft: &str) -> bool {
    let drafted: HashSet<_> = tokens(draft).collect();
    if tokens(original).any(|token| protected(token) && !drafted.contains(token)) {
        return false;
    }
    let mut rest = original;
    let mut missing_closers = HashSet::new();
    while let Some((offset, delimiter)) = rest.char_indices().find(|(offset, c)| {
        *c == '`'
            || (matches!(c, '\'' | '"' | '“' | '‘')
                && (*offset == 0
                    || rest[..*offset]
                        .chars()
                        .next_back()
                        .is_some_and(|p| !p.is_alphanumeric())))
    }) {
        rest = &rest[offset..];
        let width = if delimiter == '`' {
            rest.bytes().take_while(|b| *b == b'`').count()
        } else {
            delimiter.len_utf8()
        };
        let closing = match delimiter {
            '“' => "”",
            '‘' => "’",
            _ => &rest[..width],
        };
        let inner = &rest[width..];
        if missing_closers.contains(closing) {
            rest = inner;
            continue;
        }
        // Escaped quotes and apostrophes inside contractions are content, not delimiters.
        let end = inner.match_indices(closing).find(|(at, _)| {
            let escaped = inner[..*at]
                .bytes()
                .rev()
                .take_while(|b| *b == b'\\')
                .count()
                % 2
                != 0;
            let contraction = delimiter == '\''
                && inner[..*at]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
                && inner[*at + closing.len()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric);
            !escaped && !contraction
        });
        if let Some((end, _)) = end {
            if !contains_span(draft, &inner[..end]) {
                return false;
            }
            rest = &inner[end + closing.len()..];
        } else {
            missing_closers.insert(closing);
            rest = inner;
        }
    }
    true
}
