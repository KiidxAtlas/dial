//! Post-processing of model output: Unicode folding, verb tense, length and detail budgets.

use unicode_normalization::UnicodeNormalization;

use super::{
    commit_types::CommitMessage,
    config::GenerationConfig,
    text::lowercase_first_token,
    validation::{is_past_tense_verb, present_to_past, verb_stem},
};

/// Replacements applied before NFKD: fractions, super- and subscripts.
fn pre_nfkd_replacement(ch: char) -> Option<&'static str> {
    Some(match ch {
        '\u{2260}' => "!=",
        '\u{BD}' => "1/2",
        '\u{BC}' => "1/4",
        '\u{BE}' => "3/4",
        '\u{2153}' => "1/3",
        '\u{2154}' => "2/3",
        '\u{2155}' => "1/5",
        '\u{2156}' => "2/5",
        '\u{2157}' => "3/5",
        '\u{2158}' => "4/5",
        '\u{2159}' => "1/6",
        '\u{215A}' => "5/6",
        '\u{215B}' => "1/8",
        '\u{215C}' => "3/8",
        '\u{215D}' => "5/8",
        '\u{215E}' => "7/8",
        '\u{2070}' => "^0",
        '\u{B9}' => "^1",
        '\u{B2}' => "^2",
        '\u{B3}' => "^3",
        '\u{2074}' => "^4",
        '\u{2075}' => "^5",
        '\u{2076}' => "^6",
        '\u{2077}' => "^7",
        '\u{2078}' => "^8",
        '\u{2079}' => "^9",
        '\u{2080}' => "_0",
        '\u{2081}' => "_1",
        '\u{2082}' => "_2",
        '\u{2083}' => "_3",
        '\u{2084}' => "_4",
        '\u{2085}' => "_5",
        '\u{2086}' => "_6",
        '\u{2087}' => "_7",
        '\u{2088}' => "_8",
        '\u{2089}' => "_9",
        _ => return None,
    })
}

/// Replacements applied after NFKD: quotes, dashes, arrows, bullets, marks, Greek letters, spaces.
fn post_nfkd_replacement(ch: char) -> Option<&'static str> {
    Some(match ch {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2039}' | '\u{203A}' => "'",
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{AB}' | '\u{BB}' => "\"",
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2212}' => "-",
        '\u{2013}' | '\u{2014}' | '\u{2015}' => "--",
        '\u{2192}' => "->",
        '\u{2190}' => "<-",
        '\u{2194}' => "<->",
        '\u{21D2}' => "=>",
        '\u{21D0}' => "<=",
        '\u{21D4}' => "<=>",
        '\u{2191}' => "^",
        '\u{2193}' => "v",
        '\u{2264}' => "<=",
        '\u{2265}' => ">=",
        '\u{2248}' => "~=",
        '\u{2261}' => "==",
        '\u{D7}' => "x",
        '\u{F7}' => "/",
        '\u{2026}' | '\u{22EF}' | '\u{22EE}' => "...",
        '\u{2022}' | '\u{25E6}' | '\u{25AA}' | '\u{25AB}' | '\u{25C6}' | '\u{25C7}' => "-",
        '\u{2713}' | '\u{2714}' => "v",
        '\u{2717}' | '\u{2718}' => "x",
        '\u{3BB}' => "lambda",
        '\u{3B1}' => "alpha",
        '\u{3B2}' => "beta",
        '\u{3B3}' => "gamma",
        '\u{3B4}' => "delta",
        '\u{3B5}' => "epsilon",
        '\u{3B8}' => "theta",
        '\u{3BC}' => "mu",
        '\u{3C0}' => "pi",
        '\u{3C3}' => "sigma",
        '\u{3A3}' => "Sigma",
        '\u{394}' => "Delta",
        '\u{3A0}' => "Pi",
        '\u{A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => " ",
        '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => "",
        _ => return None,
    })
}

fn replace_characters(text: &str, replacement: fn(char) -> Option<&'static str>) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match replacement(ch) {
            Some(replacement) => out.push_str(replacement),
            None => out.push(ch),
        }
    }
    out
}

/// Fold Unicode punctuation, symbols, fractions, arrows, and spaces to ASCII; recompose letters with NFC.
pub(crate) fn normalize_commit_unicode(text: &str) -> String {
    let pre = replace_characters(text, pre_nfkd_replacement);
    let decomposed: String = pre.nfkd().collect();
    replace_characters(&decomposed, post_nfkd_replacement)
        .nfc()
        .collect()
}

/// Estimate tokens with the four-UTF-8-bytes rule.
pub(crate) fn estimate_commit_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Keep the highest-priority details inside `max_tokens`, preserving their original order.
pub(crate) fn cap_commit_details(details: &mut Vec<String>, max_tokens: usize) {
    if details.is_empty() {
        return;
    }
    let total: usize = details
        .iter()
        .map(|detail| estimate_commit_tokens(detail))
        .sum();
    if total <= max_tokens {
        return;
    }
    struct Scored {
        index: usize,
        score: usize,
        tokens: usize,
    }
    let mut scored: Vec<Scored> = details
        .iter()
        .enumerate()
        .map(|(index, detail)| {
            let lower = detail.to_lowercase();
            let mut score = 0;
            if lower.contains("security")
                || lower.contains("vulnerability")
                || lower.contains("exploit")
                || lower.contains("critical")
                || (lower.contains("fix") && lower.contains("crash"))
            {
                score += 100;
            }
            if lower.contains("breaking") || lower.contains("incompatible") {
                score += 90;
            }
            if lower.contains("performance")
                || lower.contains("faster")
                || lower.contains("optimization")
            {
                score += 80;
            }
            if lower.contains("fix") || lower.contains("bug") {
                score += 70;
            }
            if lower.contains("api") || lower.contains("interface") || lower.contains("public") {
                score += 50;
            }
            if lower.contains("user") || lower.contains("client") {
                score += 40;
            }
            if lower.contains("deprecated") || lower.contains("removed") {
                score += 35;
            }
            score += (detail.len() / 20).min(10);
            Scored {
                index,
                score,
                tokens: estimate_commit_tokens(detail),
            }
        })
        .collect();
    scored.sort_by_key(|item| std::cmp::Reverse(item.score));
    let mut budget = max_tokens;
    let mut keep = Vec::new();
    for item in &scored {
        if item.tokens > budget {
            continue;
        }
        keep.push(item.index);
        budget -= item.tokens;
    }
    keep.sort_unstable();
    let retained: Vec<String> = keep.iter().map(|&index| details[index].clone()).collect();
    *details = retained;
}

fn join_first_rest(first: &str, rest: &str) -> String {
    if rest.is_empty() {
        first.to_owned()
    } else {
        format!("{first} {rest}")
    }
}

/// Split the leading ASCII letters from the rest of a token.
fn split_verb_token(token: &str) -> Option<(&str, &str)> {
    let end = token
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_alphabetic())
        .map_or(token.len(), |(index, _)| index);
    if end == 0 {
        None
    } else {
        Some((&token[..end], &token[end..]))
    }
}

/// Past form of a present-tense verb, also accepting plural `-s`, `-es`, and `-ies` forms.
fn past_for_presentish(stem: &str) -> Option<String> {
    if let Some(direct) = present_to_past(stem) {
        return Some(direct);
    }
    if let Some(singular) = stem.strip_suffix('s').and_then(present_to_past) {
        return Some(singular);
    }
    if let Some(singular) = stem.strip_suffix("es").and_then(present_to_past) {
        return Some(singular);
    }
    if let Some(root) = stem.strip_suffix("ies") {
        return present_to_past(&format!("{root}y"));
    }
    None
}

/// Convert a known leading present-tense verb of the summary to past tense.
pub(crate) fn normalize_summary_verb(summary: &str, commit_type: &str) -> String {
    let stripped = summary.trim();
    if stripped.is_empty() {
        return stripped.to_owned();
    }
    let mut parts = stripped.split_whitespace();
    let first_word = parts.next().unwrap_or("");
    let rest = parts.collect::<Vec<_>>().join(" ");
    let first_lower = first_word.to_lowercase();
    if is_past_tense_verb(&first_lower) {
        return if commit_type == "refactor" && first_lower == "refactored" {
            join_first_rest("restructured", &rest)
        } else {
            stripped.to_owned()
        };
    }
    let Some((raw_stem, suffix)) = split_verb_token(first_word) else {
        return stripped.to_owned();
    };
    if verb_stem(first_word).is_none() {
        return stripped.to_owned();
    }
    let stem = raw_stem.to_lowercase();
    if !suffix.is_empty() && !suffix.starts_with('-') && !suffix.starts_with('/') {
        return stripped.to_owned();
    }
    if stem == "re" && suffix.starts_with('-') {
        let inner_source = &suffix[1..];
        let inner_len = inner_source
            .char_indices()
            .find(|(_, ch)| !ch.is_ascii_alphabetic())
            .map_or(inner_source.len(), |(index, _)| index);
        if inner_len == 0 {
            return stripped.to_owned();
        }
        let inner = inner_source[..inner_len].to_lowercase();
        let tail = &inner_source[inner_len..];
        let Some(mut inner_past) = past_for_presentish(&inner) else {
            return stripped.to_owned();
        };
        if commit_type == "refactor" && inner_past == "refactored" {
            inner_past = "restructured".to_owned();
        }
        return join_first_rest(&format!("re-{inner_past}{tail}"), &rest);
    }
    let Some(mut past) = past_for_presentish(&stem) else {
        return stripped.to_owned();
    };
    if commit_type == "refactor" && past == "refactored" {
        past = "restructured".to_owned();
    }
    join_first_rest(&format!("{past}{suffix}"), &rest)
}

/// Normalize summary, body, and footers before final validation. Errors when the summary exceeds the hard byte limit.
pub(crate) fn post_process_commit_message(
    message: CommitMessage,
    config: &GenerationConfig,
) -> Result<CommitMessage, String> {
    let mut summary = normalize_commit_unicode(&message.summary);
    summary = summary
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    summary = summary
        .trim()
        .trim_end_matches(['.', ';', ':'])
        .trim()
        .to_owned();
    summary = lowercase_first_token(&summary);
    summary = normalize_summary_verb(&summary, &message.commit_type);
    summary = lowercase_first_token(summary.trim())
        .trim_end_matches('.')
        .trim()
        .to_owned();
    if summary.len() > config.summary_hard_limit {
        return Err(format!(
            "Summary exceeds {} bytes",
            config.summary_hard_limit
        ));
    }

    let mut body: Vec<String> = Vec::new();
    for raw in &message.body {
        let normalized = normalize_commit_unicode(raw).replace(['\r', '\n'], " ");
        let mut detail = normalized
            .trim()
            .trim_start_matches(['\u{2022}', '-', '*', '+'])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        detail = detail.trim_end_matches(['.', ';', ',']).trim().to_owned();
        if detail.is_empty() {
            continue;
        }
        if let Some(first) = detail.chars().next()
            && first.is_lowercase()
        {
            detail = format!("{}{}", first.to_uppercase(), &detail[first.len_utf8()..]);
        }
        if !detail.ends_with('.') {
            detail.push('.');
        }
        body.push(detail);
    }
    cap_commit_details(&mut body, config.max_detail_tokens);

    Ok(CommitMessage {
        commit_type: message.commit_type,
        scope: message.scope,
        summary,
        body,
        footers: message
            .footers
            .iter()
            .map(|footer| normalize_commit_unicode(footer))
            .collect(),
    })
}
