//! Skill registry index: a JSON list of named skill packages, each with a Git source. Protocol: docs/extensibility.md.

use super::source::{self, valid_package_name};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::time::Duration;

pub const INDEX_VERSION: u32 = 1;
const MAX_INDEX_BYTES: u64 = 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// One skill a registry offers. `url` uses the install syntax: `repo[@ref][#path]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u32,
    skills: Vec<Entry>,
}

/// Parses and validates an index: known version, valid unique names, parseable source URLs.
pub fn parse_index(bytes: &[u8]) -> Result<Vec<Entry>> {
    ensure!(
        bytes.len() as u64 <= MAX_INDEX_BYTES,
        "registry index is larger than 1 MiB"
    );
    let index: Index = serde_json::from_slice(bytes).context(
        "registry index is not valid JSON in the documented format: {\"version\": 1, \"skills\": [...]}",
    )?;
    ensure!(
        index.version == INDEX_VERSION,
        "unsupported registry index version {}; expected {INDEX_VERSION}",
        index.version
    );
    let mut seen = BTreeSet::new();
    for entry in &index.skills {
        ensure!(
            valid_package_name(&entry.name),
            "registry skill name {:?} must use letters, digits, '-', '_' or '.'",
            entry.name
        );
        ensure!(
            seen.insert(entry.name.as_str()),
            "registry lists skill {:?} more than once",
            entry.name
        );
        source::parse(&entry.url)
            .with_context(|| format!("registry skill {:?} has an invalid url", entry.name))?;
    }
    Ok(index.skills)
}

/// Loads an index from `file://` or `http(s)://`. Redirects are not followed and responses are size-capped.
pub fn fetch_index(location: &str) -> Result<Vec<Entry>> {
    if let Some(path) = location.strip_prefix("file://") {
        let meta = fs::metadata(path).with_context(|| format!("reading registry index {path}"))?;
        ensure!(
            meta.len() <= MAX_INDEX_BYTES,
            "registry index is larger than 1 MiB"
        );
        let bytes = fs::read(path).with_context(|| format!("reading registry index {path}"))?;
        return parse_index(&bytes);
    }
    ensure!(
        location.starts_with("https://") || location.starts_with("http://"),
        "registry URL {location:?} must start with https://, http:// or file://"
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent
        .get(location)
        .call()
        .with_context(|| format!("fetching registry {location}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_INDEX_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading registry {location}"))?;
    parse_index(&bytes)
}

/// Entries whose name or description contains `query` (case-insensitive). An empty query matches all.
pub fn search<'a>(entries: &'a [Entry], query: &str) -> Vec<&'a Entry> {
    let query = query.trim().to_lowercase();
    entries
        .iter()
        .filter(|entry| {
            query.is_empty()
                || entry.name.to_lowercase().contains(&query)
                || entry.description.to_lowercase().contains(&query)
        })
        .collect()
}

pub fn find<'a>(entries: &'a [Entry], name: &str) -> Option<&'a Entry> {
    entries.iter().find(|entry| entry.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{"version":1,"skills":[
        {"name":"pdf","description":"Read and fill PDF forms","url":"https://example.com/org/skills.git#pdf"},
        {"name":"docx","url":"git@example.com:org/skills.git@v2#docx"}
    ]}"#;

    #[test]
    fn parses_a_valid_index() {
        let entries = parse_index(GOOD.as_bytes()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].description, "");
        assert_eq!(
            find(&entries, "docx").unwrap().url,
            "git@example.com:org/skills.git@v2#docx"
        );
        assert!(find(&entries, "missing").is_none());
    }

    #[test]
    fn rejects_bad_indexes() {
        let cases = [
            r#"{"version":2,"skills":[]}"#,
            r#"{"version":1,"skills":[],"extra":1}"#,
            r#"{"version":1,"skills":[{"name":"a b","url":"https://x/r"}]}"#,
            r#"{"version":1,"skills":[{"name":"a","url":"https://x/r"},{"name":"a","url":"https://x/r"}]}"#,
            r#"{"version":1,"skills":[{"name":"a","url":"https://x/r#../up"}]}"#,
            r#"{"version":1,"skills":[{"name":"a","url":""}]}"#,
            "not json",
        ];
        for case in cases {
            assert!(parse_index(case.as_bytes()).is_err(), "accepted {case}");
        }
    }

    #[test]
    fn search_matches_names_and_descriptions_case_insensitively() {
        let entries = parse_index(GOOD.as_bytes()).unwrap();
        assert_eq!(search(&entries, "PDF").len(), 1);
        assert_eq!(search(&entries, "forms")[0].name, "pdf");
        assert_eq!(search(&entries, "  ").len(), 2);
        assert!(search(&entries, "nothing-like-this").is_empty());
    }

    #[test]
    fn loads_an_index_from_a_file_url_and_rejects_other_schemes() {
        let dir = std::env::temp_dir().join(format!("dial-registry-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.json");
        fs::write(&path, GOOD).unwrap();
        let entries = fetch_index(&format!("file://{}", path.display())).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(fetch_index("ftp://example.com/index.json").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
