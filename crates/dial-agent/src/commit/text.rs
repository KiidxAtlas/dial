//! Text helpers for the commit pipeline. Prompt budgets count code points and limits count bytes.

/// Length in Unicode code points.
pub(crate) fn cp_len(text: &str) -> usize {
    text.chars().count()
}

/// Longest prefix of `text` that fits in `max_bytes`, cut on a char boundary.
pub(crate) fn truncate_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// First `count` code points of `text`.
pub(crate) fn take_chars(text: &str, count: usize) -> &str {
    match text.char_indices().nth(count) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Collapse every whitespace run to one space and trim.
pub(crate) fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// First whitespace-delimited word.
pub(crate) fn first_word(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or("")
}

/// Trim, then append a period unless the text is empty or already ends a sentence.
pub(crate) fn ensure_sentence(text: &str) -> String {
    let cleaned = text.trim();
    if cleaned.is_empty() || cleaned.ends_with(['.', '!', '?']) {
        cleaned.to_owned()
    } else {
        format!("{cleaned}.")
    }
}

/// Strip one pair of matching wrapping quotes (ASCII or curly), then trim.
pub(crate) fn strip_wrapping_quotes(text: &str) -> &str {
    let stripped = text.trim();
    let mut chars = stripped.chars();
    let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else {
        return stripped;
    };
    let closing = match first {
        '"' => '"',
        '\'' => '\'',
        '`' => '`',
        '\u{201C}' => '\u{201D}',
        '\u{2018}' => '\u{2019}',
        _ => return stripped,
    };
    if last != closing {
        return stripped;
    }
    stripped[first.len_utf8()..stripped.len() - last.len_utf8()].trim()
}

/// Remove duplicates ignoring case and surrounding whitespace, keeping first occurrences.
pub(crate) fn dedupe_ci(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let trimmed = item.trim().to_owned();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_lowercase()) {
            out.push(trimmed);
        }
    }
    out
}

/// Lowercase the first character when the leading token is not an all-caps acronym.
pub(crate) fn lowercase_first_token(text: &str) -> String {
    let token = text.split_whitespace().next().unwrap_or("");
    let mut has_letters = false;
    let mut all_uppercase = true;
    for ch in token.chars().filter(|ch| ch.is_alphabetic()) {
        has_letters = true;
        if !ch.is_uppercase() {
            all_uppercase = false;
        }
    }
    if has_letters && all_uppercase {
        return text.to_owned();
    }
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_uppercase() => {
            format!("{}{}", first.to_lowercase(), chars.as_str())
        }
        _ => text.to_owned(),
    }
}
