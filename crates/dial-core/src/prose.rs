//! Word view of a user's message for deterministic keyword hints. Pasted tracebacks, logs and code
//! are usually fenced or indented, and their identifiers (`_build_toolbar`, `<Button-1>`) would
//! otherwise read as request words, so only unindented lines outside code fences count.

/// Lowercased words from the prose lines of `text`. A word is a run of alphanumerics and `_`, so an
/// identifier such as `_build_toolbar` stays one word instead of yielding `build`.
pub fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    let mut fenced = false;
    text.lines()
        .filter(move |line| {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                return false;
            }
            !fenced && !line.starts_with([' ', '\t'])
        })
        .flat_map(|line| line.split(|c: char| !(c.is_alphanumeric() || c == '_')))
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

#[cfg(test)]
mod tests;
