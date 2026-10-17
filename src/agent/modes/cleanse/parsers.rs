//! Checker output parsers. Each parser turns one tool's stdout/stderr into
//! [`Diagnostic`]s. Parsers emit drafts; [`finish`] sanitizes messages, normalizes file paths into the
//! project, and drops anything that cannot be located or described.

use std::path::Path;

use regex::Regex;
use serde_json::Value;

use super::{Diagnostic, Severity, dedupe_keep_first, lexical_normalize, rel_string};

/// Parser identifier from a checker spec, or the built-in parser a discovered checker uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Rust,
    RustTest,
    Go,
    GoTest,
    Staticcheck,
    Golangci,
    Ruff,
    Pyright,
    Pylint,
    Flake8,
    Ty,
    Eslint,
    Biome,
    Oxlint,
    DenoLint,
    Stylelint,
    Shellcheck,
    Generic,
}

impl Kind {
    /// Parser id as written in discovery output. `None` for ids this port does not implement.
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "rust" => Self::Rust,
            "rust-test" => Self::RustTest,
            "go" => Self::Go,
            "go-test" => Self::GoTest,
            "staticcheck" => Self::Staticcheck,
            "golangci" => Self::Golangci,
            "ruff" => Self::Ruff,
            "pyright" => Self::Pyright,
            "pylint" => Self::Pylint,
            "flake8" => Self::Flake8,
            "ty" => Self::Ty,
            "eslint" => Self::Eslint,
            "biome" => Self::Biome,
            "oxlint" => Self::Oxlint,
            "deno-lint" => Self::DenoLint,
            "stylelint" => Self::Stylelint,
            "shellcheck" => Self::Shellcheck,
            "generic" | "mypy" => Self::Generic,
            _ => return None,
        })
    }
}

/// Where a checker ran. `cwd` and `root` are absolute.
pub struct Ctx<'a> {
    pub checker: &'a str,
    pub cwd: &'a Path,
    pub root: &'a Path,
}

/// Parsed but unvalidated finding.
struct Draft {
    file: Option<String>,
    line: Option<u64>,
    column: Option<u64>,
    end_line: Option<u64>,
    end_column: Option<u64>,
    code: Option<String>,
    severity: Severity,
    message: String,
    suggestion: Option<String>,
}

impl Draft {
    fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self {
            file: None,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            code: None,
            severity,
            message: message.into(),
            suggestion: None,
        }
    }

    fn at(mut self, file: Option<String>, line: Option<u64>, column: Option<u64>) -> Self {
        self.file = file;
        self.line = line;
        self.column = column;
        self
    }

    fn end(mut self, line: Option<u64>, column: Option<u64>) -> Self {
        self.end_line = line;
        self.end_column = column;
        self
    }

    fn code(mut self, code: Option<String>) -> Self {
        self.code = code;
        self
    }

    fn suggest(mut self, suggestion: Option<String>) -> Self {
        self.suggestion = suggestion;
        self
    }
}

/// Parses one checker's output into normalized, deduplicated diagnostics.
pub fn parse(kind: Kind, ctx: &Ctx, stdout: &str, stderr: &str) -> Vec<Diagnostic> {
    let mut drafts = match kind {
        Kind::Rust => rust(stdout),
        Kind::RustTest => rust_test(stdout, stderr),
        Kind::Go => go(stdout, stderr),
        Kind::GoTest => go_test(stdout),
        Kind::Staticcheck => staticcheck(stdout, stderr),
        Kind::Golangci => golangci(stdout, stderr),
        Kind::Ruff => ruff(stdout),
        Kind::Pyright => pyright(stdout),
        Kind::Pylint => pylint(stdout),
        Kind::Flake8 => flake8(stdout, stderr),
        Kind::Ty => ty(stdout, stderr),
        Kind::Eslint => eslint(stdout),
        Kind::Biome => biome(stdout, stderr),
        Kind::Oxlint => oxlint(stdout, stderr),
        Kind::DenoLint => deno_lint(stdout, stderr),
        Kind::Stylelint => stylelint(stdout),
        Kind::Shellcheck => shellcheck(stdout, stderr),
        Kind::Generic => generic(stdout, stderr),
    };
    if drafts.is_empty() && matches!(kind, Kind::Go | Kind::GoTest) {
        drafts = generic(stdout, stderr);
    }
    dedupe_keep_first(finish(ctx, drafts))
}

/// Validates drafts: sanitized non-empty message, positive locations, and a project-relative file.
/// A draft that names a file outside the project is dropped rather than reported at project scope.
fn finish(ctx: &Ctx, drafts: Vec<Draft>) -> Vec<Diagnostic> {
    drafts
        .into_iter()
        .filter_map(|draft| {
            let message = collapse(&sanitize(&draft.message));
            if message.is_empty() {
                return None;
            }
            let file = match draft.file.as_deref() {
                Some(raw) => Some(normalize_file(raw, ctx.cwd, ctx.root)?),
                None => None,
            };
            Some(Diagnostic {
                checker: ctx.checker.to_owned(),
                file,
                line: positive(draft.line),
                column: positive(draft.column),
                end_line: positive(draft.end_line),
                end_column: positive(draft.end_column),
                code: non_empty(draft.code),
                severity: draft.severity,
                message,
                suggestion: non_empty(draft.suggestion).map(|text| collapse(&sanitize(&text))),
            })
        })
        .collect()
}

fn positive(value: Option<u64>) -> Option<u64> {
    value.filter(|n| *n > 0)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// Maps a raw file reference to a project-relative `/` path. `None` when it is outside the project,
/// names the project root, or is a placeholder like `<stdin>`.
fn normalize_file(raw: &str, cwd: &Path, root: &Path) -> Option<String> {
    let mut value = raw.trim();
    let quoted = value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')));
    if quoted {
        value = value[1..value.len() - 1].trim();
    }
    if value.is_empty() || (value.starts_with('<') && value.ends_with('>')) {
        return None;
    }
    let value = value.strip_prefix("file://").unwrap_or(value);
    let decoded = percent_decode(value);
    let path = Path::new(&decoded);
    let candidates = if path.is_absolute() {
        vec![path.to_path_buf()]
    } else {
        vec![cwd.join(path), root.join(path)]
    };
    candidates.into_iter().find_map(|candidate| {
        let normalized = lexical_normalize(&candidate);
        let rel = normalized.strip_prefix(root).ok()?;
        let text = rel_string(rel);
        (!text.is_empty()).then_some(text)
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Removes ANSI CSI escapes and control characters other than newline and tab.
pub(super) fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Severity from a numeric level (2 error, 1 warning) or a named one. Missing defaults to warning.
fn severity_of_value(value: Option<&Value>) -> Severity {
    match value {
        None | Some(Value::Null) => Severity::Warning,
        Some(Value::Number(number)) => {
            let level = number.as_f64().unwrap_or(0.0);
            if level >= 2.0 {
                Severity::Error
            } else if level >= 1.0 {
                Severity::Warning
            } else {
                Severity::Info
            }
        }
        Some(Value::String(name)) => severity_of_str(name),
        Some(_) => Severity::Warning,
    }
}

fn severity_of_str(name: &str) -> Severity {
    let name = name.to_ascii_lowercase();
    if name.contains("error") || name == "fatal" {
        Severity::Error
    } else if name.contains("warn") || name == "convention" || name == "refactor" {
        Severity::Warning
    } else {
        Severity::Info
    }
}

/// Walks object keys. `None` when any key is missing or not a string.
fn get<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
}

fn text(value: &Value, path: &[&str]) -> Option<String> {
    get(value, path)?.as_str().map(str::to_owned)
}

fn num(value: &Value, path: &[&str]) -> Option<u64> {
    get(value, path)?.as_u64()
}

/// One-based value from a zero-based JSON field.
fn one_based(value: &Value, path: &[&str]) -> Option<u64> {
    num(value, path).map(|n| n + 1)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Every top-level JSON value in `text`. Whole-text JSON first, then balanced `{}`/`[]` slices.
fn json_values(text: &str) -> Vec<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return vec![value];
    }
    let bytes = trimmed.as_bytes();
    let mut values = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' && bytes[i] != b'[' {
            i += 1;
            continue;
        }
        let Some(end) = balanced_end(bytes, i) else {
            i += 1;
            continue;
        };
        match serde_json::from_slice::<Value>(&bytes[i..=end]) {
            Ok(value) => {
                values.push(value);
                i = end + 1;
            }
            Err(_) => i += 1,
        }
    }
    values
}

/// Index of the bracket closing the one at `start`, skipping string contents.
fn balanced_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn all_json(stdout: &str, stderr: &str) -> Vec<Value> {
    let mut values = json_values(stdout);
    values.extend(json_values(stderr));
    values
}

fn combined(stdout: &str, stderr: &str) -> String {
    format!("{stdout}\n{stderr}")
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static cleanse parser regex")
}

fn group(captures: &regex::Captures, index: usize) -> Option<String> {
    captures.get(index).map(|m| m.as_str().to_owned())
}

fn group_u64(captures: &regex::Captures, index: usize) -> Option<u64> {
    captures.get(index).and_then(|m| m.as_str().parse().ok())
}

fn rust(stdout: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in json_values(stdout) {
        if value.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        let severity = match message.get("level").and_then(Value::as_str) {
            Some("error") => Severity::Error,
            Some("warning") => Severity::Warning,
            _ => continue,
        };
        let spans = array(message, "spans");
        let primary = spans
            .iter()
            .find(|span| flag(span, "is_primary"))
            .or_else(|| spans.first());
        let mut draft = Draft::new(severity, text(message, &["message"]).unwrap_or_default())
            .code(text(message, &["code", "code"]));
        if let Some(span) = primary {
            draft = draft
                .at(
                    text(span, &["file_name"]),
                    num(span, &["line_start"]),
                    num(span, &["column_start"]),
                )
                .end(num(span, &["line_end"]), num(span, &["column_end"]))
                .suggest(text(span, &["suggested_replacement"]));
        }
        drafts.push(draft);
    }
    drafts
}

fn rust_test(stdout: &str, stderr: &str) -> Vec<Draft> {
    let mut drafts = rust(stdout);
    let panic = regex(r"thread '([^']+)' panicked at ([^\s:]+):(\d+):(\d+)");
    for captures in panic.captures_iter(&combined(stdout, stderr)) {
        let name = group(&captures, 1).unwrap_or_default();
        drafts.push(
            Draft::new(Severity::Error, format!("test {name} panicked"))
                .at(
                    group(&captures, 2),
                    group_u64(&captures, 3),
                    group_u64(&captures, 4),
                )
                .code(Some("test-failure".to_owned())),
        );
    }
    drafts
}

fn go(stdout: &str, stderr: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in all_json(stdout, stderr) {
        go_visit(&value, None, &mut drafts);
    }
    drafts
}

/// `go vet -json` nests `{package: {analyzer: [{posn, message}]}}`. Records with `posn` and `message` are
/// findings; any other object is walked with its key as the analyzer name.
fn go_visit(value: &Value, analyzer: Option<&str>, drafts: &mut Vec<Draft>) {
    match value {
        Value::Array(items) => {
            for item in items {
                go_visit(item, analyzer, drafts);
            }
        }
        Value::Object(map) => {
            let position = map
                .get("posn")
                .or_else(|| map.get("position"))
                .and_then(Value::as_str);
            let message = map.get("message").and_then(Value::as_str);
            if let (Some(position), Some(message)) = (position, message) {
                let location = regex(r"^(.+?):(\d+)(?::(\d+))?$");
                if let Some(captures) = location.captures(position.trim()) {
                    let code = map
                        .get("category")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| analyzer.map(str::to_owned));
                    drafts.push(
                        Draft::new(Severity::Warning, message)
                            .at(
                                group(&captures, 1),
                                group_u64(&captures, 2),
                                group_u64(&captures, 3),
                            )
                            .code(code),
                    );
                }
                return;
            }
            for (key, child) in map {
                go_visit(child, Some(key), drafts);
            }
        }
        _ => {}
    }
}

fn go_test(stdout: &str) -> Vec<Draft> {
    let line_regex = regex(r"^\s*(?:\./)?(.+?\.go):(\d+)(?::(\d+))?:\s*(.+?)\s*$");
    let mut drafts = Vec::new();
    for value in json_values(stdout) {
        if let Some(output) = value.get("Output").and_then(Value::as_str) {
            for line in output.lines() {
                let Some(captures) = line_regex.captures(line) else {
                    continue;
                };
                drafts.push(
                    Draft::new(Severity::Error, group(&captures, 4).unwrap_or_default())
                        .at(
                            group(&captures, 1),
                            group_u64(&captures, 2),
                            group_u64(&captures, 3),
                        )
                        .code(Some("test-failure".to_owned())),
                );
            }
        }
        if value.get("Action").and_then(Value::as_str) == Some("fail")
            && let Some(test) = value.get("Test").and_then(Value::as_str)
        {
            drafts.push(
                Draft::new(Severity::Error, format!("test {test} failed"))
                    .code(Some("test-failure".to_owned())),
            );
        }
    }
    drafts
}

fn staticcheck(stdout: &str, stderr: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in all_json(stdout, stderr) {
        if value.get("location").is_none() {
            continue;
        }
        drafts.push(
            Draft::new(
                severity_of_value(value.get("severity")),
                text(&value, &["message"]).unwrap_or_default(),
            )
            .at(
                text(&value, &["location", "file"]),
                num(&value, &["location", "line"]),
                num(&value, &["location", "column"]),
            )
            .end(
                num(&value, &["end", "line"]),
                num(&value, &["end", "column"]),
            )
            .code(text(&value, &["code"])),
        );
    }
    drafts
}

fn golangci(stdout: &str, stderr: &str) -> Vec<Draft> {
    let line_regex = regex(r"^(.+?):(\d+):(\d+):\s+(.*?)\s+\(([A-Za-z0-9_-]+)\)$");
    combined(stdout, stderr)
        .lines()
        .filter_map(|line| line_regex.captures(line.trim()))
        .map(|captures| {
            Draft::new(Severity::Warning, group(&captures, 4).unwrap_or_default())
                .at(
                    group(&captures, 1),
                    group_u64(&captures, 2),
                    group_u64(&captures, 3),
                )
                .code(group(&captures, 5))
        })
        .collect()
}

fn ruff(stdout: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in json_values(stdout) {
        let Some(records) = value.as_array() else {
            continue;
        };
        for record in records {
            drafts.push(
                Draft::new(
                    Severity::Warning,
                    text(record, &["message"]).unwrap_or_default(),
                )
                .at(
                    text(record, &["filename"]),
                    num(record, &["location", "row"]),
                    num(record, &["location", "column"]),
                )
                .end(
                    num(record, &["end_location", "row"]),
                    num(record, &["end_location", "column"]),
                )
                .code(text(record, &["code"]))
                .suggest(text(record, &["fix", "message"])),
            );
        }
    }
    drafts
}

fn pyright(stdout: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in json_values(stdout) {
        for record in array(&value, "generalDiagnostics") {
            drafts.push(
                Draft::new(
                    severity_of_value(record.get("severity")),
                    text(record, &["message"]).unwrap_or_default(),
                )
                .at(
                    text(record, &["file"]),
                    one_based(record, &["range", "start", "line"]),
                    one_based(record, &["range", "start", "character"]),
                )
                .end(
                    one_based(record, &["range", "end", "line"]),
                    one_based(record, &["range", "end", "character"]),
                )
                .code(text(record, &["rule"])),
            );
        }
    }
    drafts
}

fn pylint(stdout: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in json_values(stdout) {
        let Some(records) = value.as_array() else {
            continue;
        };
        for record in records {
            let code = text(record, &["symbol"]).or_else(|| text(record, &["message-id"]));
            drafts.push(
                Draft::new(
                    severity_of_value(record.get("type")),
                    text(record, &["message"]).unwrap_or_default(),
                )
                .at(
                    text(record, &["path"]),
                    num(record, &["line"]),
                    one_based(record, &["column"]),
                )
                .end(num(record, &["endLine"]), one_based(record, &["endColumn"]))
                .code(code),
            );
        }
    }
    drafts
}

fn flake8(stdout: &str, stderr: &str) -> Vec<Draft> {
    let line_regex = regex(r"^(.+?):(\d+):(\d+):\s+([A-Z]+\d+)\s+(.*)$");
    combined(stdout, stderr)
        .lines()
        .filter_map(|line| line_regex.captures(line.trim()))
        .map(|captures| {
            let code = group(&captures, 4).unwrap_or_default();
            let severity = if code.starts_with('F') || code.starts_with("E9") {
                Severity::Error
            } else {
                Severity::Warning
            };
            Draft::new(severity, group(&captures, 5).unwrap_or_default())
                .at(
                    group(&captures, 1),
                    group_u64(&captures, 2),
                    group_u64(&captures, 3),
                )
                .code(Some(code))
        })
        .collect()
}

fn ty(stdout: &str, stderr: &str) -> Vec<Draft> {
    let line_regex = regex(r"(?i)^(.+?):(\d+):(\d+):\s+(error|warning|info)\[([^\]]+)\]\s+(.*)$");
    combined(stdout, stderr)
        .lines()
        .filter_map(|line| line_regex.captures(line.trim()))
        .map(|captures| {
            Draft::new(
                severity_of_str(&group(&captures, 4).unwrap_or_default()),
                group(&captures, 6).unwrap_or_default(),
            )
            .at(
                group(&captures, 1),
                group_u64(&captures, 2),
                group_u64(&captures, 3),
            )
            .code(group(&captures, 5))
        })
        .collect()
}

fn eslint(stdout: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in json_values(stdout) {
        let Some(files) = value.as_array() else {
            continue;
        };
        for file in files {
            let path = text(file, &["filePath"]);
            for message in array(file, "messages") {
                drafts.push(
                    Draft::new(
                        severity_of_value(message.get("severity")),
                        text(message, &["message"]).unwrap_or_default(),
                    )
                    .at(
                        path.clone(),
                        num(message, &["line"]),
                        num(message, &["column"]),
                    )
                    .end(num(message, &["endLine"]), num(message, &["endColumn"]))
                    .code(text(message, &["ruleId"]))
                    .suggest(
                        message
                            .get("fix")
                            .map(|_| "automatic fix available".to_owned()),
                    ),
                );
            }
        }
    }
    drafts
}

fn biome(stdout: &str, stderr: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in all_json(stdout, stderr) {
        for record in array(&value, "diagnostics") {
            let file = text(record, &["location", "path", "file"])
                .or_else(|| text(record, &["location", "path"]));
            let message = text(record, &["description"])
                .or_else(|| text(record, &["message"]))
                .or_else(|| text(record, &["title"]))
                .unwrap_or_default();
            drafts.push(
                Draft::new(severity_of_value(record.get("severity")), message)
                    .at(file, None, None)
                    .code(text(record, &["category"])),
            );
        }
    }
    drafts
}

fn oxlint(stdout: &str, stderr: &str) -> Vec<Draft> {
    let line_regex = regex(r"(?i)^(.+?):(\d+):(\d+):\s+(.*?)\s+\[(Error|Warning)/([^\]]+)\]$");
    combined(stdout, stderr)
        .lines()
        .filter_map(|line| line_regex.captures(line.trim()))
        .map(|captures| {
            Draft::new(
                severity_of_str(&group(&captures, 5).unwrap_or_default()),
                group(&captures, 4).unwrap_or_default(),
            )
            .at(
                group(&captures, 1),
                group_u64(&captures, 2),
                group_u64(&captures, 3),
            )
            .code(group(&captures, 6))
        })
        .collect()
}

fn deno_lint(stdout: &str, stderr: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in all_json(stdout, stderr) {
        for record in array(&value, "diagnostics") {
            drafts.push(
                Draft::new(
                    Severity::Warning,
                    text(record, &["message"]).unwrap_or_default(),
                )
                .at(
                    text(record, &["filename"]),
                    one_based(record, &["range", "start", "line"]),
                    one_based(record, &["range", "start", "col"]),
                )
                .end(
                    one_based(record, &["range", "end", "line"]),
                    one_based(record, &["range", "end", "col"]),
                )
                .code(text(record, &["code"]))
                .suggest(text(record, &["hint"])),
            );
        }
        for record in array(&value, "errors") {
            drafts.push(
                Draft::new(
                    Severity::Error,
                    text(record, &["message"]).unwrap_or_default(),
                )
                .at(text(record, &["file_path"]), None, None),
            );
        }
    }
    drafts
}

fn stylelint(stdout: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in json_values(stdout) {
        let Some(sources) = value.as_array() else {
            continue;
        };
        for source in sources {
            let file = text(source, &["source"]);
            for warning in array(source, "warnings") {
                drafts.push(
                    Draft::new(
                        severity_of_value(warning.get("severity")),
                        text(warning, &["text"]).unwrap_or_default(),
                    )
                    .at(
                        file.clone(),
                        num(warning, &["line"]),
                        num(warning, &["column"]),
                    )
                    .end(num(warning, &["endLine"]), num(warning, &["endColumn"]))
                    .code(text(warning, &["rule"])),
                );
            }
        }
    }
    drafts
}

fn shellcheck(stdout: &str, stderr: &str) -> Vec<Draft> {
    let mut drafts = Vec::new();
    for value in all_json(stdout, stderr) {
        let comments: &[Value] = match value.as_array() {
            Some(items) => items,
            None => array(&value, "comments"),
        };
        for comment in comments {
            drafts.push(
                Draft::new(
                    severity_of_value(comment.get("level")),
                    text(comment, &["message"]).unwrap_or_default(),
                )
                .at(
                    text(comment, &["file"]),
                    num(comment, &["line"]),
                    num(comment, &["column"]),
                )
                .end(num(comment, &["endLine"]), num(comment, &["endColumn"]))
                .code(num(comment, &["code"]).map(|code| format!("SC{code}")))
                .suggest(
                    comment
                        .get("fix")
                        .map(|_| "automatic fix available".to_owned()),
                ),
            );
        }
    }
    drafts
}

/// GCC/Clang-style and MSVC-style `file:line:col: severity: message` lines.
fn generic(stdout: &str, stderr: &str) -> Vec<Draft> {
    let msvc = regex(
        r"(?i)^(.*?)\((\d+),(\d+)\):\s*(error|warning|info)(?:\s+([A-Za-z]+\d+))?:\s*(.*?)(?:\s+\[[^\]]+\])?$",
    );
    let gcc = regex(
        r"(?i)^(.*?):(\d+)(?::(\d+))?:\s*(error|warning|info|note)(?:\s*\[([^\]]+)\])?:\s*(.*)$",
    );
    let mut drafts = Vec::new();
    for line in combined(stdout, stderr).lines().map(str::trim) {
        if let Some(captures) = msvc.captures(line) {
            drafts.push(
                Draft::new(
                    severity_of_str(&group(&captures, 4).unwrap_or_default()),
                    group(&captures, 6).unwrap_or_default(),
                )
                .at(
                    group(&captures, 1),
                    group_u64(&captures, 2),
                    group_u64(&captures, 3),
                )
                .code(group(&captures, 5)),
            );
            continue;
        }
        if let Some(captures) = gcc.captures(line) {
            drafts.push(
                Draft::new(
                    severity_of_str(&group(&captures, 4).unwrap_or_default()),
                    group(&captures, 6).unwrap_or_default(),
                )
                .at(
                    group(&captures, 1),
                    group_u64(&captures, 2),
                    group_u64(&captures, 3),
                )
                .code(group(&captures, 5)),
            );
        }
    }
    drafts
}

#[cfg(test)]
mod tests;
