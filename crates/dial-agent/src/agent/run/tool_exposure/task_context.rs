//! Advisory package names from complete, successful manifest reads, never instructions or authority.
use crate::llm::ToolCall;
use crate::runtime::{execution::ToolStatus, tool_output::ToolOutput};
use std::collections::BTreeSet;

const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_SIGNALS: usize = 32;
const MAX_NAME_BYTES: usize = 128;

/// This deliberately handles only ordinary single-target package.json reads. Raw reads, selectors
/// and batches have different completeness/error framing; declining them is safer than guessing.
/// No filesystem access, execution, persistent state or permission changes occur here.
pub(super) fn signals(call: &ToolCall, output: &ToolOutput) -> Vec<String> {
    if call.name != "read"
        || call.malformed_arguments
        || output.is_error
        || ToolStatus::from_output(output) != ToolStatus::Ok
        || output.text.len() > MAX_INPUT_BYTES
        || ["paths", "offset", "limit"]
            .iter()
            .any(|key| call.arguments.get(key).is_some())
    {
        return Vec::new();
    }
    let Some(path) = call.arguments.get("path").and_then(|value| value.as_str()) else {
        return Vec::new();
    };
    if path.len() > 4096
        || path.contains([':', '\\', '\n', '\r'])
        || path.rsplit('/').next() != Some("package.json")
    {
        return Vec::new();
    }
    let Some(source) = complete_source(&output.text) else {
        return Vec::new();
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&source) else {
        return Vec::new();
    };
    let mut names = BTreeSet::new();
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(dependencies) = manifest.get(section).and_then(|value| value.as_object()) {
            for (name, version) in dependencies {
                // Names only: scripts, descriptions, URLs, aliases and version values never become
                // query text. Non-string versions are not package.json dependency declarations.
                if version.is_string() && package_name(name) {
                    names.insert(name.clone());
                    if names.len() > MAX_SIGNALS {
                        names.pop_last();
                    }
                }
            }
        }
    }
    names.into_iter().collect()
}

fn package_name(name: &str) -> bool {
    fn part(value: &str) -> bool {
        !value.is_empty()
            && value.as_bytes()[0].is_ascii_alphanumeric()
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            })
    }
    if name.len() > MAX_NAME_BYTES {
        return false;
    }
    if let Some(scoped) = name.strip_prefix('@') {
        scoped
            .split_once('/')
            .is_some_and(|(scope, package)| part(scope) && part(package))
    } else {
        part(name)
    }
}

/// Both read modes emit consecutive numbered rows. Hashline adds a tag and raw-revision header.
/// A default complete read has no footer; any footer, skipped row or clipped row fails closed.
fn complete_source(text: &str) -> Option<String> {
    let mut rows = text.lines().peekable();
    let hashline = rows.peek()?.starts_with('[');
    if hashline {
        let header = rows.next()?;
        let (path, tag) = header
            .strip_prefix('[')?
            .strip_suffix(']')?
            .rsplit_once('#')?;
        if path.rsplit('/').next() != Some("package.json")
            || tag.len() != 4
            || !tag.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        let revision = rows
            .next()?
            .strip_prefix("[Source raw SHA256:")?
            .strip_suffix(']')?;
        if revision.len() != 64 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
    }
    let mut source = String::new();
    for (index, row) in rows.enumerate() {
        let (number, content) = row.split_once(if hashline { ':' } else { '\t' })?;
        if number.trim().parse::<usize>().ok()? != index + 1
            || content.contains("…[line truncated]")
        {
            return None;
        }
        source.push_str(content);
        source.push('\n');
    }
    Some(source)
}

#[cfg(test)]
mod tests;
