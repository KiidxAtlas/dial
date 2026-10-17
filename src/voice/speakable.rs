//! Strip presentation markup from assistant answers before passing them to speech engines.
pub fn speakable(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let cleaned = strip_inline(strip_block_markers(trimmed));
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(cleaned);
        if out.chars().count() >= max_chars {
            break;
        }
    }
    truncate_at_sentence(out, max_chars)
}

/// Cut to at most `max_chars` characters, preferring the last sentence end in the second half.
fn truncate_at_sentence(mut out: String, max_chars: usize) -> String {
    if let Some((end, _)) = out.char_indices().nth(max_chars) {
        let cut = out[..end]
            .rfind(['.', '!', '?'])
            .filter(|&p| p * 2 >= end)
            .map_or(end, |p| p + 1);
        out.truncate(cut);
    }
    out
}

/// Drop leading heading (`#`) and quote (`>`) markers. Only line starts count, so `C#` and `a -> b` survive.
fn strip_block_markers(mut line: &str) -> &str {
    loop {
        let after_hashes = line.trim_start_matches('#');
        if after_hashes.len() < line.len()
            && (after_hashes.is_empty() || after_hashes.starts_with(' '))
        {
            line = after_hashes.trim_start();
        } else if let Some(rest) = line.strip_prefix('>') {
            line = rest.trim_start();
        } else {
            return line;
        }
    }
}

/// Remove inline markdown from one line. `*` and `_` are emphasis only at word edges, so identifiers
/// such as `snake_case` and arithmetic such as `2*3` survive. An unclosed `[` or `![` is kept as
/// plain text instead of swallowing the rest of the line.
fn strip_inline(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut cleaned = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '`' => {}
            '*' | '_' => {
                let end = chars[i..]
                    .iter()
                    .position(|&n| n != c)
                    .map_or(chars.len(), |p| i + p);
                let inside_word = i > 0
                    && chars[i - 1].is_alphanumeric()
                    && chars.get(end).is_some_and(|n| n.is_alphanumeric());
                if inside_word {
                    cleaned.extend(&chars[i..end]);
                }
                i = end;
                continue;
            }
            '[' => {
                if let Some(close) = find_from(&chars, i + 1, ']') {
                    let next = skip_target(&chars, close + 1);
                    if next > close + 1 {
                        cleaned.extend(&chars[i + 1..close]);
                        i = next;
                        continue;
                    }
                }
                cleaned.push(c);
            }
            '!' if chars.get(i + 1) == Some(&'[') => {
                if let Some(close) = find_from(&chars, i + 2, ']') {
                    let next = skip_target(&chars, close + 1);
                    if next > close + 1 {
                        i = next;
                        continue;
                    }
                }
                cleaned.push(c);
            }
            _ => cleaned.push(c),
        }
        i += 1;
    }
    cleaned
}

/// Index of the first `target` at or after `from`.
fn find_from(chars: &[char], from: usize, target: char) -> Option<usize> {
    chars
        .get(from..)?
        .iter()
        .position(|&c| c == target)
        .map(|p| from + p)
}

/// Skip a `(target)` link destination starting at `at`, returning the index after it. Without a
/// closing `)` nothing is skipped, so the text stays visible.
fn skip_target(chars: &[char], at: usize) -> usize {
    if chars.get(at) == Some(&'(')
        && let Some(close) = find_from(chars, at + 1, ')')
    {
        return close + 1;
    }
    at
}

#[cfg(test)]
mod tests;
