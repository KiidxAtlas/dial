//! Session-local plan artifacts addressable through `local://<slug>.md`.
use std::{fs, path::Path};

#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
fn slug(value: &str) -> Option<&str> {
    let value = value.strip_prefix("local://").unwrap_or(value);
    let value = value.strip_suffix(".md").unwrap_or(value);
    (!value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(value)
}

#[cfg(test)]
pub fn resolve_local(state_dir: &Path, session_id: &str, uri: &str) -> Option<PathBuf> {
    let slug = slug(uri)?;
    let path = state_dir
        .join("plans")
        .join(session_id)
        .join(format!("{slug}.md"));
    path.is_file().then_some(path)
}

pub fn save(state_dir: &Path, session_id: &str, plan: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !session_id.is_empty()
            && session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid session id"
    );
    let title = plan
        .lines()
        .find_map(|line| line.trim().strip_prefix('#'))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("plan");
    let mut slug = String::new();
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("plan");
    }
    slug.truncate(80);
    let dir = state_dir.join("plans").join(session_id);
    fs::create_dir_all(&dir)?;
    crate::security::store::write_private_atomic(&dir.join(format!("{slug}.md")), plan.as_bytes())?;
    Ok(format!("local://{slug}.md"))
}

#[cfg(test)]
mod tests;
