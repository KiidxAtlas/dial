//! `--models` scoped model list: pattern parsing, matching, and scope resolution.
//!
//! The scope is process-only. It replaces the `/model next` cycle set for this run and picks the
//! starting model when no explicit `--provider` selects one. It is never written to
//! `model_favorites` or any other setting.
//!
//! Matching order per non-glob pattern:
//! 1. exact `provider/id`, then exact bare id (`match_model` with `exact_only`);
//! 2. a valid `:<level>` suffix (`:max` and `:auto` allowed): a literal id ending in that suffix
//!    wins, otherwise the base is parsed recursively and the level is recorded;
//! 3. `match_model` on the whole pattern: provider-scoped fuzzy (`fuzzy_match`), then substring
//!    over id or label, aliases before dated ids and the newest dated id first;
//! 4. an invalid `:<suffix>` is warned about, then the base is matched without it.
//!
//! Glob patterns (`*`, `?`, `[`) match `id` or `provider/id` in list order. A concrete `:<level>`
//! on a glob is committed to the glob base even when nothing matches; `:max` and `:auto` try the
//! literal glob first.
//!
//! An unmatched pattern is a warning, not an error; it is logged and matching continues.
//!
//! Not implemented (no Dial data or settings support for them):
//! - variant alias collapse, provider wire-route thinking inference, Bedrock inference profiles,
//!   OpenRouter fallback model ids, provider-locked cross matches against the bundled catalog;
//! - the exact-match revision spelling key;
//! - `@upstream` routing (Dial `ModelSelection` cannot carry aggregator routing);
//! - role aliases such as `@smol`, which depend on the settings role system;
//! - usage ranks and provider priority (no Dial data; ties fall back to list order);
//! - model-kind filtering for non-chat runners (warned as unmatched instead).

use super::fuzzy_match::fuzzy_match;
use super::model_flags::{CliThinking, ModelFlags, apply_thinking};
use crate::config::types::Config;
use crate::llm::catalog::{self, ModelEntry};
use crate::llm::model_config::ModelSelection;
use crate::storage::paths::Layout;
use anyhow::Result;
use parking_lot::RwLock;

/// One resolved scope entry in pattern order, duplicates removed.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeEntry {
    pub selection: ModelSelection,
    /// Explicit `:<level>` from the pattern; `None` when the pattern named no level (or `:auto`).
    pub thinking: Option<CliThinking>,
}

/// Resolver output. `warnings` report patterns that matched nothing or carried an invalid suffix.
#[derive(Debug, Default, PartialEq)]
pub struct Resolved {
    pub entries: Vec<ScopeEntry>,
    pub warnings: Vec<String>,
}

/// Process-wide scope installed by [`apply`]. `None` when `--models` was not given.
static ACTIVE: RwLock<Option<Vec<ScopeEntry>>> = RwLock::new(None);

/// Returns the scope's model selections for the `/model next` cycle, or `None` when no scope is
/// active and the saved favorites apply.
pub fn cycle_set() -> Option<Vec<ModelSelection>> {
    ACTIVE.read().as_ref().map(|entries| {
        entries
            .iter()
            .map(|entry| entry.selection.clone())
            .collect()
    })
}

/// True when `--models` installed a scope for this process.
pub fn is_active() -> bool {
    ACTIVE.read().is_some()
}

/// Resolves `--models` against the available chat models, installs the process scope, and applies
/// the starting model and thinking level to `config` in memory. Call before `model_flags::apply`.
///
/// Without `--provider`, the configured main model stays when it is inside the scope; otherwise the
/// first scope entry becomes the main model. A scope entry's explicit `:<level>` applies only when
/// `--thinking` is absent.
pub fn apply(layout: &Layout, config: Config, flags: &ModelFlags) -> Result<Config> {
    let Some(patterns) = flags.models.as_ref() else {
        return Ok(config);
    };
    let mut selections = vec![config.selection()];
    selections.extend(config.model_roles.values().cloned());
    let discovery = catalog::discover(layout, &config.providers, &selections);
    let available: Vec<ModelEntry> = discovery
        .models
        .into_iter()
        .filter(|entry| entry.available && entry.supports_chat())
        .collect();
    let resolved = resolve(&patterns.0, &available);
    for warning in &resolved.warnings {
        eprintln!("warning: {warning}");
    }
    let first = resolved.entries.first().cloned();
    *ACTIVE.write() = Some(resolved.entries);
    if flags.provider.is_some() {
        return Ok(config);
    }
    let current = config.selection();
    let Some(start) = ACTIVE
        .read()
        .as_ref()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| same_model(&entry.selection, &current))
                .cloned()
        })
        .or(first)
    else {
        return Ok(config);
    };
    let mut config = config;
    if !same_model(&start.selection, &current) {
        config = config.with_selection(&start.selection);
    }
    match (flags.thinking, start.thinking) {
        (None, Some(level)) => apply_thinking(config, level),
        _ => Ok(config),
    }
}

/// Resolves `--models` patterns against `models` (already filtered to available chat
/// models). Entries keep pattern order; a model already in the scope is skipped. A pattern that
/// matches nothing becomes a warning and resolution continues.
pub fn resolve(patterns: &[String], models: &[ModelEntry]) -> Resolved {
    let mut resolved = Resolved::default();
    for raw in patterns {
        let pattern = raw.trim();
        if is_glob(pattern) {
            resolve_glob(pattern, models, &mut resolved);
            continue;
        }
        match parse(pattern, models) {
            Some(parsed) => {
                if let Some(warning) = parsed.warning {
                    resolved.warnings.push(warning);
                }
                push_entry(&mut resolved, models, parsed.index, parsed.thinking);
            }
            None => resolved.warnings.push(unmatched(pattern)),
        }
    }
    resolved
}

fn unmatched(pattern: &str) -> String {
    format!("No models match pattern \"{pattern}\"")
}

/// Resolves a glob scope pattern.
fn resolve_glob(pattern: &str, models: &[ModelEntry], resolved: &mut Resolved) {
    // A concrete level (`off`..`xhigh`) is committed to the glob base, even when that matches nothing.
    if let Some((base, _, Suffix::Level(level))) = split_suffix(pattern, false) {
        let hits = glob_hits(base, models);
        push_hits(resolved, models, pattern, &hits, Some(level));
        return;
    }
    // `:max` and `:auto`: the literal glob (suffix included) wins when it matches anything.
    if let Some((base, _, suffix)) = split_suffix(pattern, true) {
        let literal = glob_hits(pattern, models);
        if !literal.is_empty() {
            push_hits(resolved, models, pattern, &literal, None);
            return;
        }
        let thinking = match suffix {
            Suffix::Level(level) => Some(level),
            Suffix::Auto => None,
        };
        let hits = glob_hits(base, models);
        push_hits(resolved, models, pattern, &hits, thinking);
        return;
    }
    let hits = glob_hits(pattern, models);
    push_hits(resolved, models, pattern, &hits, None);
}

fn push_hits(
    resolved: &mut Resolved,
    models: &[ModelEntry],
    pattern: &str,
    hits: &[usize],
    thinking: Option<CliThinking>,
) {
    if hits.is_empty() {
        resolved.warnings.push(unmatched(pattern));
        return;
    }
    for &index in hits {
        push_entry(resolved, models, index, thinking);
    }
}

fn push_entry(
    resolved: &mut Resolved,
    models: &[ModelEntry],
    index: usize,
    thinking: Option<CliThinking>,
) {
    let selection = &models[index].selection;
    if resolved
        .entries
        .iter()
        .any(|entry| same_model(&entry.selection, selection))
    {
        return;
    }
    resolved.entries.push(ScopeEntry {
        selection: selection.clone(),
        thinking,
    });
}

/// A thinking suffix after `:`. `Auto` carries no level.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Suffix {
    Level(CliThinking),
    Auto,
}

/// Splits a trailing `:<suffix>` off `pattern`: `(base, suffix text, suffix)`. `max` and `auto` are
/// recognised only when `allow_max_auto` is set.
fn split_suffix(pattern: &str, allow_max_auto: bool) -> Option<(&str, &str, Suffix)> {
    let (base, name) = pattern.rsplit_once(':')?;
    let suffix = match name.to_lowercase().as_str() {
        "off" => Suffix::Level(CliThinking::Off),
        "minimal" => Suffix::Level(CliThinking::Minimal),
        "low" => Suffix::Level(CliThinking::Low),
        "medium" => Suffix::Level(CliThinking::Medium),
        "high" => Suffix::Level(CliThinking::High),
        "xhigh" => Suffix::Level(CliThinking::XHigh),
        "max" if allow_max_auto => Suffix::Level(CliThinking::Max),
        "auto" if allow_max_auto => Suffix::Auto,
        _ => return None,
    };
    Some((base, name, suffix))
}

/// A non-glob pattern hit: the matched model index, its explicit level, and a warning if the
/// pattern carried an invalid suffix.
struct Parsed {
    index: usize,
    thinking: Option<CliThinking>,
    warning: Option<String>,
}

fn parse(pattern: &str, models: &[ModelEntry]) -> Option<Parsed> {
    if let Some(index) = match_model(pattern, models, true) {
        return Some(Parsed {
            index,
            thinking: None,
            warning: None,
        });
    }
    if let Some((base, name, suffix)) = split_suffix(pattern, true) {
        // A literal id ending in `:<suffix>` keeps winning over the suffix split.
        if let Some(index) = match_model(pattern, models, false) {
            let id = models[index].selection.model.to_lowercase();
            if id.ends_with(&format!(":{}", name.to_lowercase())) {
                return Some(Parsed {
                    index,
                    thinking: None,
                    warning: None,
                });
            }
        }
        // The suffix is never absorbed into a longer sibling id: strip it and parse only the base.
        let inner = parse(base, models)?;
        let thinking = match suffix {
            Suffix::Level(level) => Some(level),
            Suffix::Auto => None,
        };
        return Some(Parsed {
            index: inner.index,
            thinking,
            warning: inner.warning,
        });
    }
    if let Some(index) = match_model(pattern, models, false) {
        return Some(Parsed {
            index,
            thinking: None,
            warning: None,
        });
    }
    let (prefix, suffix) = pattern.rsplit_once(':')?;
    let inner = parse(prefix, models)?;
    Some(Parsed {
        index: inner.index,
        thinking: inner.thinking,
        warning: Some(format!(
            "Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead."
        )),
    })
}

/// Model matching: exact `provider/id`, exact bare id, then (unless `exact_only`) the
/// provider-scoped fuzzy match, then the substring fallback.
fn match_model(pattern: &str, models: &[ModelEntry], exact_only: bool) -> Option<usize> {
    let qualified = pattern
        .split_once('/')
        .map(|(provider, id)| (provider.trim(), id.trim()))
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty());
    if let Some((provider, id)) = qualified
        && let Some(index) = models.iter().position(|entry| {
            eq_ci(&entry.selection.provider, provider) && eq_ci(&entry.selection.model, id)
        })
    {
        return Some(index);
    }
    if let Some(index) = models
        .iter()
        .position(|entry| eq_ci(&entry.selection.model, pattern))
    {
        return Some(index);
    }
    if exact_only {
        return None;
    }
    if let Some((provider, id)) = qualified
        && let Some(index) = provider_fuzzy(provider, id, models)
    {
        return Some(index);
    }
    substring_match(pattern, models)
}

/// Provider-scoped fuzzy match: the best fuzzy score among the provider's ids. Ties keep list
/// order.
fn provider_fuzzy(provider: &str, id: &str, models: &[ModelEntry]) -> Option<usize> {
    let mut scored: Vec<(f64, usize)> = models
        .iter()
        .enumerate()
        .filter(|(_, entry)| eq_ci(&entry.selection.provider, provider))
        .filter_map(|(index, entry)| {
            fuzzy_match(id, &entry.selection.model).map(|score| (score, index))
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.first().map(|&(_, index)| index)
}

/// Case-insensitive substring over id or label. Aliases beat dated ids; among dated ids the newest
/// id wins, then list order.
fn substring_match(pattern: &str, models: &[ModelEntry]) -> Option<usize> {
    let needle = pattern.to_lowercase();
    let matches: Vec<usize> = (0..models.len())
        .filter(|&index| {
            let entry = &models[index];
            entry.selection.model.to_lowercase().contains(&needle)
                || entry.label.to_lowercase().contains(&needle)
        })
        .collect();
    if let Some(&alias) = matches
        .iter()
        .find(|&&index| !is_dated(&models[index].selection.model))
    {
        return Some(alias);
    }
    let newest = matches
        .iter()
        .map(|&index| models[index].selection.model.as_str())
        .max()?;
    matches
        .into_iter()
        .find(|&index| models[index].selection.model == newest)
}

fn is_glob(pattern: &str) -> bool {
    pattern.chars().any(|c| matches!(c, '*' | '?' | '['))
}

/// Models whose lowered `id` or `provider/id` matches the lowered glob, in list order.
fn glob_hits(pattern: &str, models: &[ModelEntry]) -> Vec<usize> {
    let expansions = brace_expand(&pattern.to_lowercase());
    (0..models.len())
        .filter(|&index| {
            let selection = &models[index].selection;
            let id: Vec<char> = selection.model.to_lowercase().chars().collect();
            let qualified: Vec<char> = format!(
                "{}/{}",
                selection.provider.to_lowercase(),
                selection.model.to_lowercase()
            )
            .chars()
            .collect();
            expansions.iter().any(|glob| {
                let glob: Vec<char> = glob.chars().collect();
                glob_match(&glob, &id) || glob_match(&glob, &qualified)
            })
        })
        .collect()
}

/// Expands the first `{a,b}` group, recursively (Bun.Glob brace syntax). An unclosed `{` stays
/// literal.
fn brace_expand(pattern: &str) -> Vec<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let Some(open) = chars.iter().position(|&c| c == '{') else {
        return vec![pattern.to_owned()];
    };
    let mut depth = 0i32;
    let mut commas = Vec::new();
    let mut close = None;
    for (i, &c) in chars.iter().enumerate().skip(open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(i),
            _ => {}
        }
    }
    let Some(close) = close else {
        return vec![pattern.to_owned()];
    };
    let prefix: String = chars[..open].iter().collect();
    let suffix: String = chars[close + 1..].iter().collect();
    let mut bounds = vec![open];
    bounds.extend(commas);
    bounds.push(close);
    let mut out = Vec::new();
    for window in bounds.windows(2) {
        let alternative: String = chars[window[0] + 1..window[1]].iter().collect();
        out.extend(brace_expand(&format!("{prefix}{alternative}{suffix}")));
    }
    out
}

/// Bun.Glob-style matching: `*` (no `/`), `**` (any run), `?` (one non-`/`), `[...]` classes with
/// `!`/`^` negation and ranges, and `\` escapes. Inputs are already lowered.
fn glob_match(pattern: &[char], text: &[char]) -> bool {
    let Some((&first, rest)) = pattern.split_first() else {
        return text.is_empty();
    };
    match first {
        '*' if rest.first() == Some(&'*') => {
            let rest = &rest[1..];
            (0..=text.len()).any(|skip| glob_match(rest, &text[skip..]))
        }
        '*' => {
            for skip in 0..=text.len() {
                if glob_match(rest, &text[skip..]) {
                    return true;
                }
                if text.get(skip) == Some(&'/') {
                    return false;
                }
            }
            false
        }
        '?' => text.first().is_some_and(|&c| c != '/') && glob_match(rest, &text[1..]),
        '[' => match parse_class(pattern) {
            None => text.first() == Some(&'[') && glob_match(rest, &text[1..]),
            Some((negate, members, used)) => match text.first() {
                Some(&c) => {
                    let hit = members.iter().any(|&(low, high)| c >= low && c <= high);
                    hit != negate && glob_match(&pattern[used..], &text[1..])
                }
                None => false,
            },
        },
        '\\' if !rest.is_empty() => {
            text.first() == Some(&rest[0]) && glob_match(&rest[1..], &text[1..])
        }
        c => text.first() == Some(&c) && glob_match(rest, &text[1..]),
    }
}

/// A parsed `[...]` class: `(negated, ranges, chars consumed)`.
type ParsedClass = (bool, Vec<(char, char)>, usize);

/// Parses a `[...]` class at the start of `pattern`: `(negated, ranges, chars consumed)`. `None`
/// when the class is unclosed, so the `[` is literal.
fn parse_class(pattern: &[char]) -> Option<ParsedClass> {
    let mut i = 1;
    let negate = matches!(pattern.get(i), Some('!') | Some('^'));
    if negate {
        i += 1;
    }
    let start = i;
    let mut members = Vec::new();
    loop {
        let &c = pattern.get(i)?;
        if c == ']' && i > start {
            return Some((negate, members, i + 1));
        }
        if pattern.get(i + 1) == Some(&'-') && pattern.get(i + 2).is_some_and(|&n| n != ']') {
            members.push((c, pattern[i + 2]));
            i += 3;
        } else {
            members.push((c, c));
            i += 1;
        }
    }
}

/// Whether the id has a `-YYYYMMDD` date suffix.
fn is_dated(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() >= 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit)
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Same provider and model id.
fn same_model(a: &ModelSelection, b: &ModelSelection) -> bool {
    a.provider == b.provider && a.model == b.model
}

#[cfg(test)]
mod tests;
