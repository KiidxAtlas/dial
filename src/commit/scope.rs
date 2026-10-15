//! Conventional-commit scope inference from `git diff --numstat` output.

use std::collections::HashSet;
use std::path::Path;

use super::config::{EXCLUDED_FILES, GenerationConfig};

const PLACEHOLDER_DIRS: &[&str] = &[
    "src", "lib", "bin", "crates", "benches", "examples", "internal", "pkg", "include", "tests",
    "test", "docs", "packages", "modules",
];

const SKIP_DIRS: &[&str] = &[
    ".test",
    "tests",
    "benches",
    "examples",
    "target",
    "build",
    "node_modules",
    ".github",
];

/// Weighted path candidate considered for a conventional commit scope.
struct ScopeCandidate {
    path: String,
    percentage: f64,
    confidence: f64,
}

/// Scope hint rendered into analysis prompts, plus its wide-change classification.
pub(crate) struct ScopeCandidatesResult {
    pub scope_candidates: String,
    pub is_wide: bool,
}

/// Accumulates changed-line totals per meaningful path component.
/// Components keep first-seen order, so ties sort by first appearance.
#[derive(Default)]
struct ScopeAnalyzer {
    component_lines: Vec<(String, usize)>,
    total_lines: usize,
}

impl ScopeAnalyzer {
    fn from_numstat(numstat: &str) -> Self {
        let mut analyzer = Self::default();
        for line in numstat.lines() {
            analyzer.process_numstat_line(line);
        }
        analyzer
    }

    /// Process one `added\tdeleted\tpath` numstat row.
    fn process_numstat_line(&mut self, line: &str) {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            return;
        }
        let changed = parse_count(parts[0]) + parse_count(parts[1]);
        if changed == 0 {
            return;
        }
        let file = numstat_path(&parts[2..].join("\t"));
        if EXCLUDED_FILES
            .iter()
            .any(|pattern| file.ends_with(*pattern))
        {
            return;
        }
        self.total_lines += changed;
        for component in extract_components_from_path(&file) {
            if component.split('/').any(|segment| segment.contains('.')) {
                continue;
            }
            match self
                .component_lines
                .iter()
                .position(|(path, _)| *path == component)
            {
                Some(index) => self.component_lines[index].1 += changed,
                None => self.component_lines.push((component, changed)),
            }
        }
    }

    /// Candidates sorted by confidence descending (stable).
    fn build_scope_candidates(&self) -> Vec<ScopeCandidate> {
        if self.total_lines == 0 {
            return Vec::new();
        }
        let mut candidates: Vec<ScopeCandidate> = self
            .component_lines
            .iter()
            .filter(|(path, _)| path.contains('/') || !PLACEHOLDER_DIRS.contains(&path.as_str()))
            .map(|(path, lines)| {
                let percentage = *lines as f64 / self.total_lines as f64 * 100.0;
                let confidence = if path.contains('/') {
                    if percentage > 60.0 {
                        percentage * 1.2
                    } else {
                        percentage * 0.8
                    }
                } else {
                    percentage
                };
                ScopeCandidate {
                    path: path.clone(),
                    percentage,
                    confidence,
                }
            })
            .collect();
        candidates.sort_by(|left, right| right.confidence.total_cmp(&left.confidence));
        candidates
    }
}

/// Render the scope hint and wide-change flag for a numstat listing.
pub(crate) fn extract_scope_candidates(
    numstat: &str,
    config: &GenerationConfig,
) -> ScopeCandidatesResult {
    let analyzer = ScopeAnalyzer::from_numstat(numstat);
    let candidates = analyzer.build_scope_candidates();
    if analyzer.total_lines == 0 {
        return ScopeCandidatesResult {
            scope_candidates: "(none - no meaningful scopes)".to_owned(),
            is_wide: false,
        };
    }
    let first_segments: HashSet<&str> = candidates
        .iter()
        .map(|candidate| candidate.path.split('/').next().unwrap_or(""))
        .collect();
    let dominant_is_small = candidates
        .first()
        .is_some_and(|candidate| candidate.percentage / 100.0 < config.wide_change_threshold);
    if dominant_is_small || first_segments.len() >= 3 {
        let pattern = if config.wide_change_abstract {
            analyze_wide_change(numstat)
        } else {
            None
        };
        let scope_candidates = match pattern {
            Some(pattern) => format!("(cross-cutting: {pattern})"),
            None => "(none - multi-component change)".to_owned(),
        };
        return ScopeCandidatesResult {
            scope_candidates,
            is_wide: true,
        };
    }
    let top = &candidates[..candidates.len().min(5)];
    let parts: Vec<String> = top
        .iter()
        .filter(|candidate| candidate.percentage >= 10.0)
        .map(|candidate| {
            let confidence = if candidate.path.contains('/') && candidate.percentage > 60.0 {
                "high confidence"
            } else {
                "moderate confidence"
            };
            format!(
                "{} ({:.0}%, {confidence})",
                candidate.path,
                candidate.percentage.round()
            )
        })
        .collect();
    if parts.is_empty() {
        return ScopeCandidatesResult {
            scope_candidates: "(none - unclear component)".to_owned(),
            is_wide: false,
        };
    }
    let mut scope_candidates = parts.join(", ");
    if top
        .iter()
        .any(|candidate| candidate.path.contains('/') && candidate.percentage > 60.0)
    {
        scope_candidates.push_str("\nPrefer 2-segment scopes marked 'high confidence'.");
    }
    ScopeCandidatesResult {
        scope_candidates,
        is_wide: false,
    }
}

/// Total changed lines (additions + deletions) in a numstat listing.
pub(crate) fn count_changed_lines(numstat: &str) -> usize {
    ScopeAnalyzer::from_numstat(numstat).total_lines
}

/// Classify cross-cutting changes by the kinds of files touched.
fn analyze_wide_change(numstat: &str) -> Option<&'static str> {
    let paths: Vec<String> = numstat.lines().filter_map(path_from_numstat_line).collect();
    if paths.is_empty() {
        return None;
    }
    let mut markdown = 0usize;
    let mut tests = 0usize;
    let mut configs = 0usize;
    let mut has_cargo_toml = false;
    let mut has_package_json = false;
    let mut errors = 0usize;
    let mut types = 0usize;
    for file in &paths {
        let lower = file.to_lowercase();
        let suffix = Path::new(file)
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        if suffix == ".md" {
            markdown += 1;
        }
        if file.contains("/test")
            || file.contains("test_")
            || file.contains("_test.")
            || file.contains(".test.")
        {
            tests += 1;
        }
        if matches!(suffix.as_str(), ".toml" | ".yaml" | ".yml" | ".json") {
            configs += 1;
        }
        if file.contains("Cargo.toml") {
            has_cargo_toml = true;
        }
        if file.contains("package.json") {
            has_package_json = true;
        }
        if lower.contains("error") || lower.contains("exception") || lower.contains("fail") {
            errors += 1;
        }
        if lower.contains("type") || lower.contains("struct") || lower.contains("enum") {
            types += 1;
        }
    }
    let total = paths.len() as f64;
    let share = |count: usize| count as f64 * 100.0 / total;
    if has_cargo_toml || has_package_json {
        Some("deps")
    } else if share(markdown) > 70.0 {
        Some("docs")
    } else if share(tests) > 60.0 {
        Some("tests")
    } else if share(errors) > 40.0 {
        Some("error-handling")
    } else if share(types) > 40.0 {
        Some("type-refactor")
    } else if share(configs) > 50.0 {
        Some("config")
    } else {
        None
    }
}

fn path_from_numstat_line(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() >= 3 {
        Some(numstat_path(&parts[2..].join("\t")))
    } else {
        None
    }
}

/// Parse a numstat count the way `Number.parseInt(raw, 10)` does; non-numeric (binary) counts are zero.
fn parse_count(raw: &str) -> usize {
    let digits: String = raw
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(0)
}

/// Extract the destination path from git's brace (`a/{old => new}/b`) or arrow (`old => new`) rename syntax.
pub(crate) fn extract_path_from_rename(path_part: &str) -> String {
    let value = path_part.trim();
    if let Some(brace_start) = value.find('{') {
        if let Some(arrow) = value[brace_start..]
            .find(" => ")
            .map(|offset| offset + brace_start)
        {
            if let Some(brace_end) = value[arrow..].find('}').map(|offset| offset + arrow) {
                return format!(
                    "{}{}{}",
                    &value[..brace_start],
                    value[arrow + 4..brace_end].trim(),
                    &value[brace_end + 1..]
                )
                .trim()
                .to_owned();
            }
        }
        return value.to_owned();
    }
    match value.find(" => ") {
        Some(arrow) => value[arrow + 4..].trim().to_owned(),
        None => value.to_owned(),
    }
}

/// Decode a numstat path field. Without `-z`, git C-quotes names with special bytes
/// (`"src/caf\303\251.rs"`); unquoted names still go through rename handling.
fn numstat_path(field: &str) -> String {
    match field
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        Some(inner) => unquote_c_bytes(inner),
        None => extract_path_from_rename(field),
    }
}

/// Decode git's C-style escapes (`\a \b \t \n \v \f \r \" \\` and three-digit octal bytes).
fn unquote_c_bytes(inner: &str) -> String {
    let bytes = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' || index + 1 >= bytes.len() {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        let octal = bytes
            .get(index + 1..index + 4)
            .filter(|digits| digits.iter().all(|d| (b'0'..=b'7').contains(d)));
        if let Some(digits) = octal {
            let value = digits
                .iter()
                .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
            out.push(u8::try_from(value).unwrap_or(b'?'));
            index += 4;
            continue;
        }
        out.push(match bytes[index + 1] {
            b'a' => 7,
            b'b' => 8,
            b't' => 9,
            b'n' => 10,
            b'v' => 11,
            b'f' => 12,
            b'r' => 13,
            other => other,
        });
        index += 2;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Single- and two-segment meaningful components of a path.
fn extract_components_from_path(file: &str) -> Vec<String> {
    let normalized = file.replace('\\', "/");
    let segments: Vec<&str> = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let mut meaningful: Vec<&str> = Vec::new();
    for (index, &segment) in segments.iter().enumerate() {
        if PLACEHOLDER_DIRS.contains(&segment) {
            if segments.len() > index + 1 {
                continue;
            }
            break;
        }
        if segment.contains('.') && !segment.starts_with('.') {
            continue;
        }
        if SKIP_DIRS.contains(&segment) {
            continue;
        }
        let stripped = match segment.rfind('.') {
            Some(dot) => &segment[..dot],
            None => segment,
        };
        if !stripped.is_empty() && !stripped.starts_with('.') {
            meaningful.push(stripped);
        }
    }
    match meaningful.as_slice() {
        [] => Vec::new(),
        [first] => vec![(*first).to_owned()],
        [first, second, ..] => vec![(*first).to_owned(), format!("{first}/{second}")],
    }
}
