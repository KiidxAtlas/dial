//! Executable module-boundary checks (see ARCHITECTURE.md). Each rule lists a module and the
//! sibling modules it must not depend on.
//!
//! Test-gated modules (out-of-line module files and their submodules, and files marked
//! `#![cfg(test)]`) are exempt; production code after a test module is still scanned. Comments
//! and string/char literal contents are ignored.
//!
//! Tests live in their own files: `tests_live_in_their_own_files` rejects inline test-gated
//! module bodies and `#[test]` functions outside a test module file.

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("unreadable module directory {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("unreadable entry in {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Source roots that hold the crate's modules: the root package's `src/` and every workspace
/// crate's `crates/<name>/src/`. Each module lives under exactly one of them.
fn source_roots(manifest: &Path) -> Result<Vec<PathBuf>, String> {
    let mut roots = vec![manifest.join("src")];
    let crates = manifest.join("crates");
    let entries = fs::read_dir(&crates)
        .map_err(|e| format!("unreadable crates directory {}: {e}", crates.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("unreadable entry in {}: {e}", crates.display()))?;
        let src = entry.path().join("src");
        if src.is_dir() {
            roots.push(src);
        }
    }
    roots.sort();
    Ok(roots)
}

/// Every Rust file of a top-level module: `<root>/<m>.rs` plus everything under `<root>/<m>/`,
/// for the one source root that holds it. A module in no root is reported as missing rather
/// than treated as empty.
fn module_files(roots: &[PathBuf], module: &str) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut found = false;
    for src in roots {
        let flat = src.join(format!("{module}.rs"));
        let dir = src.join(module);
        if !flat.is_file() && !dir.is_dir() {
            continue;
        }
        found = true;
        if flat.is_file() {
            files.push(flat);
        }
        if dir.exists() {
            rust_files(&dir, &mut files)?;
        }
    }
    if !found {
        return Err(format!(
            "module root `{module}` not found under {} (expected {module}.rs or {module}/)",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    files.sort();
    Ok(files)
}

/// Module definitions made twice under one directory: `foo.rs` beside `foo/mod.rs`. A module is
/// either `foo.rs` (children, if any, in `foo/`) or `foo/mod.rs`, never both. Every directory
/// under `base` is checked, so crate source roots are covered like the root package's `src/`.
fn ambiguous_modules(dir: &Path, base: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("unreadable module directory {}: {e}", dir.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("unreadable entry in {}: {e}", dir.display()))?;
        paths.push(entry.path());
    }
    paths.sort();
    for path in &paths {
        if path.is_dir() {
            ambiguous_modules(path, base, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem != "mod" && dir.join(stem).join("mod.rs").is_file() {
                out.push(format!(
                    "{}: `{stem}.rs` and `{stem}/mod.rs` both define module `{stem}`; keep \
                     `{stem}.rs` (children in `{stem}/`) or `{stem}/mod.rs`",
                    path.strip_prefix(base).unwrap_or(path).display()
                ));
            }
        }
    }
    Ok(())
}

/// Layout violations across the root package's `src/` and every crate's `src/`.
fn layout_violations(manifest: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for src in source_roots(manifest)? {
        ambiguous_modules(&src, &src, &mut out)?;
    }
    Ok(out)
}

/// Blank comments and the contents of string/char literals (keeping byte offsets and newlines),
/// so braces and paths inside them are neither counted nor matched.
fn strip(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let blank = |out: &mut Vec<u8>, range: Range<usize>| {
        for byte in &mut out[range] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let end = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |n| i + n);
                blank(&mut out, i..end);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let (mut depth, mut j) = (1, i + 2);
                while j < b.len() && depth > 0 {
                    if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                        depth += 1;
                        j += 2;
                    } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                        depth -= 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                blank(&mut out, i..j);
                i = j;
            }
            b'r' | b'b' | b'c' if raw_string_start(b, i).is_some() => {
                let (open_end, hashes) = raw_string_start(b, i).unwrap();
                let mut j = open_end;
                let close = loop {
                    if j >= b.len() {
                        break b.len();
                    }
                    if b[j] == b'"'
                        && b[j + 1..]
                            .iter()
                            .take(hashes)
                            .filter(|&&c| c == b'#')
                            .count()
                            == hashes
                    {
                        break j;
                    }
                    j += 1;
                };
                blank(&mut out, open_end..close);
                i = (close + 1 + hashes).min(b.len());
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                blank(&mut out, i + 1..j.min(b.len()));
                i = j + 1;
            }
            b'\'' => {
                // Char literal ('x', '\n', '\u{..}', multi-byte) versus lifetime/label ('a).
                let rest = &text[i + 1..];
                let len = if rest.starts_with('\\') {
                    rest.get(2..)
                        .and_then(|tail| tail.find('\''))
                        .map(|n| n + 2)
                } else {
                    rest.chars()
                        .next()
                        .filter(|c| rest[c.len_utf8()..].starts_with('\''))
                        .map(|c| c.len_utf8())
                };
                match len {
                    Some(len) => {
                        blank(&mut out, i + 1..i + 1 + len);
                        i += len + 2;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).expect("blanking keeps UTF-8 boundaries of untouched bytes")
}

/// `r"`, `r#"`, `br"`, `cr#"` … at `i` (not inside an identifier): returns (content start, #s).
fn raw_string_start(b: &[u8], i: usize) -> Option<(usize, usize)> {
    if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return None;
    }
    let mut j = i;
    if b[j] == b'b' || b[j] == b'c' {
        j += 1;
    }
    if b.get(j) != Some(&b'r') {
        return None;
    }
    j += 1;
    let hashes = b[j..].iter().take_while(|&&c| c == b'#').count();
    j += hashes;
    (b.get(j) == Some(&b'"')).then_some((j + 1, hashes))
}

fn skip_ws(s: &str, mut i: usize) -> usize {
    while i < s.len() && s.as_bytes()[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// End (exclusive) of the bracket group opening at `open`.
fn matching(s: &str, open: usize, (l, r): (u8, u8)) -> usize {
    let mut depth = 0usize;
    for (n, &c) in s.as_bytes()[open..].iter().enumerate() {
        if c == l {
            depth += 1;
        } else if c == r {
            depth -= 1;
            if depth == 0 {
                return open + n + 1;
            }
        }
    }
    s.len()
}

fn ident(s: &str, i: usize) -> &str {
    let end = s[i..]
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map_or(s.len(), |n| i + n);
    &s[i..end]
}

struct TestModules {
    /// Byte ranges of inline test-gated `mod x { … }` bodies.
    inline: Vec<Range<usize>>,
    /// Out-of-line test-gated `mod x;` declarations: (name, explicit `#[path]`, names of the
    /// inline modules that enclose the declaration, outermost first).
    files: Vec<(String, Option<String>, Vec<String>)>,
}

/// Whether `attr` (`#[cfg(…)]`, from stripped source) compiles its item only under `cfg(test)`:
/// `test` itself, or `all(…)` with `test` among its direct arguments. `any(test, …)` and
/// `not(test)` also build outside tests, so they do not qualify.
fn is_test_gate(attr: &str) -> bool {
    let Some(predicate) = attr
        .strip_prefix("#[cfg(")
        .and_then(|rest| rest.strip_suffix(")]"))
    else {
        return false;
    };
    let predicate: String = predicate.chars().filter(|c| !c.is_whitespace()).collect();
    if predicate == "test" {
        return true;
    }
    let Some(args) = predicate
        .strip_prefix("all(")
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    let (mut depth, mut start) = (0usize, 0);
    let mut direct = Vec::new();
    for (i, c) in args.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                direct.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    direct.push(&args[start..]);
    direct.contains(&"test")
}

/// Names of the inline `mod x { … }` blocks that contain `offset`, outermost first.
fn enclosing_mods(code: &str, offset: usize) -> Vec<String> {
    let mut chain = Vec::new();
    let mut from = 0;
    while let Some(n) = code[from..offset].find("mod ") {
        let at = from + n;
        from = at + "mod ".len();
        if code[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let name_at = skip_ws(code, at + "mod".len());
        let name = ident(code, name_at);
        let open = skip_ws(code, name_at + name.len());
        if !name.is_empty()
            && code.as_bytes().get(open) == Some(&b'{')
            && matching(code, open, (b'{', b'}')) > offset
        {
            chain.push(name.to_owned());
        }
    }
    chain
}

/// Find test-gated module items in stripped source. `original` supplies `#[path]` values.
fn test_modules(code: &str, original: &str) -> TestModules {
    let mut found = TestModules {
        inline: Vec::new(),
        files: Vec::new(),
    };
    let mut from = 0;
    while let Some(at) = code[from..].find("#[cfg(").map(|n| from + n) {
        let gate_end = matching(code, at + 1, (b'[', b']'));
        from = gate_end;
        if !is_test_gate(&code[at..gate_end]) {
            continue;
        }
        let mut i = skip_ws(code, gate_end);
        let mut path_attr = None;
        // Further attributes on the same item.
        while code[i..].starts_with("#[") {
            let end = matching(code, i + 1, (b'[', b']'));
            let attr = &original[i..end];
            if let Some(rest) = attr.strip_prefix("#[path") {
                path_attr = rest.split('"').nth(1).map(str::to_owned);
            }
            i = skip_ws(code, end);
        }
        if code[i..].starts_with("pub") {
            i = skip_ws(code, i + 3);
            if code[i..].starts_with('(') {
                i = skip_ws(code, matching(code, i, (b'(', b')')));
            }
        }
        if !(code[i..].starts_with("mod") && code[i + 3..].starts_with(|c: char| c.is_whitespace()))
        {
            continue;
        }
        i = skip_ws(code, i + 3);
        let name = ident(code, i).to_owned();
        i = skip_ws(code, i + name.len());
        match code.as_bytes().get(i) {
            Some(b'{') => {
                let end = matching(code, i, (b'{', b'}'));
                found.inline.push(at..end);
                from = end;
            }
            Some(b';') => found
                .files
                .push((name, path_attr, enclosing_mods(code, at))),
            _ => {}
        }
    }
    found
}

/// Directory in which `file`'s child modules live.
fn child_dir(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap().to_path_buf();
    match file.file_stem().and_then(|s| s.to_str()) {
        Some("mod" | "lib" | "main") => parent,
        Some(stem) => parent.join(stem),
        None => parent,
    }
}

/// Resolve every `#[cfg(test)]` out-of-line module (and its submodule tree) to exempt paths.
fn exempt_paths(roots: &[PathBuf]) -> Result<BTreeSet<PathBuf>, String> {
    let mut all = Vec::new();
    for src in roots {
        rust_files(src, &mut all)?;
    }
    let mut exempt = BTreeSet::new();
    for file in &all {
        let original =
            fs::read_to_string(file).map_err(|e| format!("unreadable {}: {e}", file.display()))?;
        let code = strip(&original);
        if code.trim_start().starts_with("#![cfg(test)]") {
            exempt.insert(file.clone());
            exempt.insert(child_dir(file));
        }
        for (name, path_attr, inline) in test_modules(&code, &original).files {
            let dir = file.parent().unwrap();
            let candidates = match path_attr {
                Some(path) => vec![dir.join(path)],
                None => {
                    // A declaration inside `mod x { … }` looks in `x/` below the file's own
                    // child directory.
                    let base = inline
                        .iter()
                        .fold(child_dir(file), |dir, module| dir.join(module));
                    vec![
                        base.join(format!("{name}.rs")),
                        base.join(&name).join("mod.rs"),
                    ]
                }
            };
            for candidate in candidates.into_iter().filter(|c| c.is_file()) {
                exempt.insert(child_dir(&candidate));
                exempt.insert(candidate);
            }
        }
    }
    Ok(exempt)
}

fn line_of(code: &str, offset: usize) -> usize {
    code[..offset].bytes().filter(|&c| c == b'\n').count() + 1
}

/// Every crate-level module dependency in stripped source, as (byte offset, module name):
/// - `crate::m…`, `$crate::m…` and `::crate::m…` paths, with arbitrary whitespace around `::`;
/// - bare module references such as `use crate::m;` and `use crate::m as x;` (the module is
///   recorded whatever follows its name, so aliases and grouped items are covered);
/// - `crate::{…}` groups, recording each top-level item of the group (`m::…`, `m as x`, `m`);
/// - `self::` / `super::` chains, resolved against `module_path` (the file's own module, e.g.
///   `["ui", "composer"]`; empty at the crate root). A relative path that stays inside the
///   file's own top-level module records that module, which is never a forbidden target of
///   that module's own rule.
///
/// Known gaps: glob imports (`use super::*;`) followed by bare names, and inline `mod` blocks
/// inside a file (relative paths resolve from the file's module, not the inline module).
fn crate_refs(code: &str, module_path: &[String]) -> Vec<(usize, String)> {
    let b = code.as_bytes();
    let mut refs = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !(b[i].is_ascii_alphabetic() || b[i] == b'_') {
            i += 1;
            continue;
        }
        let word = ident(code, i);
        i += word.len();
        if !matches!(word, "crate" | "self" | "super") {
            continue;
        }
        if !code[skip_ws(code, i)..].starts_with("::") {
            continue;
        }
        // `base` is the module the path is anchored at: the crate root for `crate::`, the
        // file's module for `self::`/`super::`, which pops one level per `super`.
        let mut base: Vec<String> = if word == "crate" {
            Vec::new()
        } else {
            module_path.to_vec()
        };
        let mut keyword = word;
        let mut at = skip_ws(code, i);
        let (seg_start, seg) = loop {
            if keyword == "super" {
                base.pop();
            }
            let seg_start = skip_ws(code, at + 2);
            let seg = ident(code, seg_start);
            let after = skip_ws(code, seg_start + seg.len());
            if matches!(seg, "self" | "super") && code[after..].starts_with("::") {
                keyword = seg;
                at = after;
                continue;
            }
            break (seg_start, seg);
        };
        if seg.is_empty() {
            if code[seg_start..].starts_with('{') {
                let end = matching(code, seg_start, (b'{', b'}'));
                let body = &code[seg_start + 1..end - 1];
                let (mut depth, mut start) = (0usize, 0usize);
                let mut items = Vec::new();
                for (n, c) in body.bytes().enumerate() {
                    match c {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        b',' if depth == 0 => {
                            items.push((start, &body[start..n]));
                            start = n + 1;
                        }
                        _ => {}
                    }
                }
                items.push((start, &body[start..]));
                for (offset, item) in items {
                    let lead = item.len() - item.trim_start().len();
                    let name = ident(item, lead);
                    if name.is_empty() || matches!(name, "self" | "super") {
                        continue;
                    }
                    let module = base.first().cloned().unwrap_or_else(|| name.to_owned());
                    refs.push((seg_start + 1 + offset + lead, module));
                }
                i = end;
            }
        } else {
            let module = base.first().cloned().unwrap_or_else(|| seg.to_owned());
            refs.push((seg_start, module));
            i = seg_start + seg.len();
        }
    }
    refs
}

/// Violations of `forbidden` in one source text, ignoring exempt `#[cfg(test)]` module bodies.
/// `module_path` is the file's own module (used to resolve `self::`/`super::`).
fn violations_in(
    original: &str,
    forbidden: &[&str],
    module_path: &[String],
) -> Vec<(usize, String)> {
    violations_in_except(original, forbidden, &[], module_path)
}

/// Like [`violations_in`], but a reference whose full path is one of `allowed` (for example
/// `config::schema`, matched on whole path segments) is not a violation even when its top-level
/// module is forbidden. Everything else under a forbidden module still is.
fn violations_in_except(
    original: &str,
    forbidden: &[&str],
    allowed: &[&str],
    module_path: &[String],
) -> Vec<(usize, String)> {
    let code = strip(original);
    let tests = test_modules(&code, original).inline;
    crate_refs(&code, module_path)
        .into_iter()
        .filter(|(offset, name)| {
            forbidden.contains(&name.as_str())
                && !tests.iter().any(|r| r.contains(offset))
                && !is_allowed_path(&code, *offset, allowed)
        })
        .map(|(offset, name)| (line_of(&code, offset), name))
        .collect()
}

/// Whether the path starting at `offset` (for example `config::schema::registry`) is, or lies
/// under, one of the `allowed` paths. Segments must match exactly: `config::schemas` is not
/// `config::schema`.
fn is_allowed_path(code: &str, offset: usize, allowed: &[&str]) -> bool {
    let path: String = code[offset..]
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
        .collect();
    allowed
        .iter()
        .any(|a| path == *a || path.starts_with(&format!("{a}::")))
}

/// Module path of a source file relative to `src`: `ui/composer.rs` → `["ui", "composer"]`,
/// `ui/mod.rs` and `ui.rs` → `["ui"]`, `main.rs` → `[]`.
fn module_path_of(src: &Path, file: &Path) -> Vec<String> {
    let rel = file.strip_prefix(src).unwrap_or(file);
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(last) = parts.pop() {
        match last.strip_suffix(".rs") {
            Some("mod" | "lib" | "main") => {}
            Some(stem) => parts.push(stem.to_owned()),
            None => parts.push(last),
        }
    }
    parts
}

#[test]
fn modules_respect_their_dependency_boundaries() {
    // (module, forbidden `crate::<x>` paths, allowed sub-paths of a forbidden module)
    let rules: &[(&str, &[&str], &[&str])] = &[
        // ui reads the settings declarations (`config::schema`), persists settings through
        // `config::settings_store`, and uses the dial directory layout (`config::paths`), the
        // Laya provider constants (`config::laya`) and the per-model image settings types and
        // limits that `/image settings` edits (`config::image`). Any other `config` path is a new
        // dependency and must be added here deliberately.
        (
            "ui",
            &["learn", "tools", "config"],
            &[
                "config::schema",
                "config::settings_store",
                "config::paths",
                "config::laya",
                "config::image",
            ],
        ),
        ("llm", &["ui", "learn", "tools"], &[]),
        ("tools", &["ui", "learn", "agent"], &[]),
        ("runtime", &["ui", "learn", "agent", "tools", "llm"], &[]),
        ("storage", &["ui"], &[]),
        ("config", &["ui", "learn"], &[]),
        ("agent", &["ui"], &[]),
        ("learn", &["ui"], &[]),
        (
            "workspace_copy",
            &["agent", "benchmark", "learn", "config", "llm", "ui"],
            &[],
        ),
    ];
    let roots = source_roots(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|error| panic!("architecture guard: {error}"));
    let exempt = exempt_paths(&roots).unwrap_or_else(|error| panic!("architecture guard: {error}"));
    let mut violations = Vec::new();
    for (module, forbidden, allowed) in rules {
        let files = match module_files(&roots, module) {
            Ok(files) => files,
            Err(error) => {
                violations.push(format!("module root `{module}`: {error}"));
                continue;
            }
        };
        for file in files {
            if exempt.iter().any(|path| file.starts_with(path)) {
                continue;
            }
            let text = fs::read_to_string(&file)
                .unwrap_or_else(|error| panic!("unreadable {}: {error}", file.display()));
            let root = roots.iter().find(|r| file.starts_with(r)).unwrap();
            let module_path = module_path_of(root, &file);
            for (line, bad) in violations_in_except(&text, forbidden, allowed, &module_path) {
                violations.push(format!(
                    "{}:{line}: {module} must not use {bad}",
                    file.strip_prefix(root).unwrap().display()
                ));
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn production_code_after_a_test_module_is_still_scanned() {
    let source = r#"
use crate::config::Config;
#[cfg(test)]
mod tests {
    use crate::tools::EditMode;
    fn braces() -> &'static str { "}}}" }
    fn chars() -> char { '}' }
}
// crate::tools::ignored in a comment
fn after() -> &'static str { let _ = "crate::tools::in_string"; crate::tools::helper() }
use crate::{config::types::Harness, tools::EditMode};
"#;
    let found = violations_in(source, &["tools"], &[]);
    assert_eq!(
        found,
        vec![(10, "tools".to_owned()), (11, "tools".to_owned())]
    );
}

#[test]
fn test_gated_non_module_items_are_scanned() {
    let source = "#[cfg(test)]\nuse crate::tools::EditMode;\n#[cfg(test)]\npub(crate) mod fake {\n    use crate::tools::X;\n}\n";
    assert_eq!(
        violations_in(source, &["tools"], &[]),
        vec![(2, "tools".to_owned())]
    );
}

#[test]
fn import_forms_are_recognised() {
    let source = "use crate::tools;\n\
        use crate::tools as t;\n\
        use crate :: config :: Config;\n\
        use crate::\n    tools::EditMode;\n\
        use crate::{config::Config, tools as t2, ui::{Thing}};\n\
        pub(crate) use crate::tools::Helper;\n\
        use ::crate::tools::Global;\n\
        macro_rules! call { () => { $crate::tools::run() } }\n";
    assert_eq!(
        violations_in(source, &["tools"], &[]),
        vec![
            (1, "tools".to_owned()),
            (2, "tools".to_owned()),
            (5, "tools".to_owned()),
            (6, "tools".to_owned()),
            (7, "tools".to_owned()),
            (8, "tools".to_owned()),
            (9, "tools".to_owned()),
        ]
    );
}

#[test]
fn lookalikes_and_non_paths_are_not_dependencies() {
    let source = "// use crate::tools;\nlet s = \"crate::tools::x\";\nuse crate::toolsmith::X;\nuse my_crate::tools::Y;\nuse crate::config::tools_like;\npub(crate) mod z {}\n";
    assert!(violations_in(source, &["tools"], &[]).is_empty());
}

#[test]
fn allowed_sub_paths_of_a_forbidden_module_are_permitted() {
    let source = "use crate::config::schema::Kind;\n\
        use crate::config::types::Config;\n\
        use crate::{config::schema, config::types::Harness};\n\
        fn f() -> usize { crate::config::schema::registry().len() + crate::config::schemas::x() }\n\
        use crate::config::schema_extra::Y;\n";
    assert_eq!(
        violations_in_except(source, &["config"], &["config::schema"], &[]),
        vec![
            (2, "config".to_owned()),
            (3, "config".to_owned()),
            (4, "config".to_owned()),
            (5, "config".to_owned()),
        ]
    );
    // Without an allowlist every reference is a violation, as before.
    assert_eq!(violations_in(source, &["config"], &[]).len(), 7);
}

fn module_chain(path: &str) -> Vec<String> {
    path.split('/').map(str::to_owned).collect()
}

#[test]
fn relative_paths_resolve_against_the_file_module() {
    let source = "use super::tools::X;\nuse self::tools::Y;\n";
    // src/ui.rs: `super` is the crate root.
    assert_eq!(
        violations_in(source, &["tools"], &module_chain("ui")),
        vec![(1, "tools".to_owned())]
    );
    // src/ui/composer.rs: `super` is `ui`, so this is `ui::tools`, not `crate::tools`.
    assert!(violations_in(source, &["tools"], &module_chain("ui/composer")).is_empty());
    let grand = "use super::super::tools::X;\n";
    assert_eq!(
        violations_in(grand, &["tools"], &module_chain("ui/composer")),
        vec![(1, "tools".to_owned())]
    );
}

#[test]
fn module_path_follows_the_file_layout() {
    let src = Path::new("src");
    assert_eq!(
        module_path_of(src, Path::new("src/ui/composer.rs")),
        module_chain("ui/composer")
    );
    assert_eq!(
        module_path_of(src, Path::new("src/ui/mod.rs")),
        module_chain("ui")
    );
    assert_eq!(
        module_path_of(src, Path::new("src/ui.rs")),
        module_chain("ui")
    );
    assert!(module_path_of(src, Path::new("src/main.rs")).is_empty());
}

#[test]
fn missing_and_unreadable_module_roots_are_reported() {
    let root = std::env::temp_dir().join(format!("dial-arch-roots-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("present")).unwrap();
    fs::write(root.join("present/a.rs"), "").unwrap();
    let missing = module_files(std::slice::from_ref(&root), "absent");
    assert!(
        matches!(&missing, Err(error) if error.contains("not found")),
        "{missing:?}"
    );
    assert_eq!(
        module_files(std::slice::from_ref(&root), "present").unwrap(),
        vec![root.join("present/a.rs")]
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores directory permissions; only assert when the directory really is unreadable.
        if fs::read_dir(&locked).is_err() {
            assert!(matches!(
                module_files(std::slice::from_ref(&root), "locked"),
                Err(error) if error.contains("unreadable")
            ));
        }
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn out_of_line_test_modules_are_exempt_files() {
    let root = std::env::temp_dir().join(format!("dial-arch-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("llm/nested")).unwrap();
    fs::write(
        root.join("llm/mod.rs"),
        "#[cfg(test)]\nmod tests;\nmod nested;\n",
    )
    .unwrap();
    fs::write(root.join("llm/tests.rs"), "use crate::tools::X;\n").unwrap();
    fs::write(
        root.join("llm/nested.rs"),
        "#[cfg(test)]\n#[path = \"nested/t.rs\"]\nmod t;\n",
    )
    .unwrap();
    fs::write(root.join("llm/nested/t.rs"), "use crate::tools::Y;\n").unwrap();
    let exempt = exempt_paths(std::slice::from_ref(&root)).unwrap();
    let _ = fs::remove_dir_all(&root);
    assert!(exempt.contains(&root.join("llm/tests.rs")));
    assert!(exempt.contains(&root.join("llm/nested/t.rs")));
    assert!(
        !exempt
            .iter()
            .any(|p| root.join("llm/mod.rs").starts_with(p))
    );
    assert!(
        !exempt
            .iter()
            .any(|p| root.join("llm/nested.rs").starts_with(p))
    );
}

/// Offsets of `#[test]`, `#[tokio::test]` and similar test-function attributes in stripped source.
fn test_attributes(code: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = code[from..].find("#[").map(|n| from + n) {
        let end = matching(code, at + 1, (b'[', b']'));
        let attr = &code[at + 2..end - 1];
        let name = attr.split('(').next().unwrap_or(attr).trim();
        if name == "test" || name.ends_with("::test") {
            found.push(at);
        }
        from = at + 2;
    }
    found
}

/// Test sources are separate files: no test-gated module body is written inline, and `#[test]`
/// functions only appear in files reached through a test-gated `mod x;` declaration.
#[test]
fn tests_live_in_their_own_files() {
    let roots = source_roots(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|error| panic!("architecture guard: {error}"));
    let exempt = exempt_paths(&roots).unwrap_or_else(|error| panic!("architecture guard: {error}"));
    let mut files = Vec::new();
    for src in &roots {
        rust_files(src, &mut files).unwrap_or_else(|error| panic!("architecture guard: {error}"));
    }
    files.sort();
    let mut violations = Vec::new();
    for file in files {
        let original = fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("unreadable {}: {error}", file.display()));
        let code = strip(&original);
        let root = roots.iter().find(|r| file.starts_with(r)).unwrap();
        let name = file.strip_prefix(root).unwrap().display().to_string();
        for range in test_modules(&code, &original).inline {
            violations.push(format!(
                "{name}:{}: inline test module; move its body to its own file and declare it \
                 with `mod <name>;`",
                line_of(&code, range.start)
            ));
        }
        if !exempt.iter().any(|path| file.starts_with(path)) {
            for offset in test_attributes(&code) {
                violations.push(format!(
                    "{name}:{}: test function outside a test module file",
                    line_of(&code, offset)
                ));
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn inline_test_modules_and_stray_test_functions_are_detected() {
    let inline = "fn a() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n\
        #[cfg(all(test, unix))]\nmod unix_tests { }\n";
    let code = strip(inline);
    assert_eq!(test_modules(&code, inline).inline.len(), 2);
    assert_eq!(test_attributes(&code).len(), 1);

    let out_of_line = "#[cfg(test)]\nmod tests;\n#[cfg(any(test, feature = \"x\"))]\nmod shared {}\n\
        #[cfg(not(test))]\nmod prod {}\n";
    let code = strip(out_of_line);
    let found = test_modules(&code, out_of_line);
    assert!(found.inline.is_empty());
    assert_eq!(found.files.len(), 1);

    let tokio = "#[tokio::test(flavor = \"multi_thread\")]\nasync fn t() {}\n// #[test]\n\
        let s = \"#[test]\";\n#[derive(Debug)]\nstruct S;\n";
    assert_eq!(test_attributes(&strip(tokio)).len(), 1);
}

#[test]
fn test_gates_are_recognised_only_when_they_imply_cfg_test() {
    assert!(is_test_gate("#[cfg(test)]"));
    assert!(is_test_gate("#[cfg(all(test, unix))]"));
    assert!(is_test_gate(
        "#[cfg(all(unix, test, not(feature = \"desktop\")))]"
    ));
    assert!(!is_test_gate("#[cfg(any(test, unix))]"));
    assert!(!is_test_gate("#[cfg(not(test))]"));
    assert!(!is_test_gate("#[cfg(all(unix, not(test)))]"));
    assert!(!is_test_gate("#[cfg(feature = \"test\")]"));
}

#[test]
fn test_files_declared_inside_inline_modules_resolve_below_that_module() {
    let root = std::env::temp_dir().join(format!("dial-arch-nested-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("platform/time")).unwrap();
    fs::write(
        root.join("platform/mod.rs"),
        "pub mod time {\n    #[cfg(test)]\n    mod tests;\n}\n",
    )
    .unwrap();
    fs::write(root.join("platform/time/tests.rs"), "#[test]\nfn t() {}\n").unwrap();
    let exempt = exempt_paths(std::slice::from_ref(&root)).unwrap();
    let _ = fs::remove_dir_all(&root);
    assert!(exempt.contains(&root.join("platform/time/tests.rs")));
}

/// Workspace crates in layer order, lowest first (see ARCHITECTURE.md). A crate may depend on the
/// crates before it in this list and on nothing else; an unlisted crate is a violation, so a new
/// crate is placed in the order deliberately.
const CRATE_LAYERS: &[&str] = &[
    "dial-build-info",
    "dial-core",
    "dial-llm",
    "dial-storage",
    "dial-tools",
    "dial-computer",
    "dial-agent",
    "dial-ui",
    "dial-app",
];

/// Workspace-crate dependencies of one crate's manifest that are not in the layers below it.
/// Dev-dependencies count too: a test dependency on a higher crate is the same cycle risk.
fn crate_layer_violations(name: &str, manifest: &str) -> Vec<String> {
    let Some(layer) = CRATE_LAYERS.iter().position(|c| *c == name) else {
        return vec![format!("crates/{name}: not listed in CRATE_LAYERS")];
    };
    let parsed: toml::Table = toml::from_str(manifest)
        .unwrap_or_else(|error| panic!("crates/{name}/Cargo.toml: {error}"));
    let mut tables = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        tables.extend(parsed.get(key).and_then(|v| v.as_table()));
    }
    for target in parsed
        .get("target")
        .and_then(|v| v.as_table())
        .into_iter()
        .flatten()
    {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            tables.extend(target.1.get(key).and_then(|v| v.as_table()));
        }
    }
    let mut violations = Vec::new();
    for table in tables {
        for dep in table.keys().filter(|k| k.starts_with("dial-")) {
            match CRATE_LAYERS.iter().position(|c| c == dep) {
                Some(below) if below < layer => {}
                _ => violations.push(format!("crates/{name} must not depend on {dep}")),
            }
        }
    }
    violations
}

/// Workspace crate names: every `crates/<name>/` directory that holds a manifest.
fn workspace_crate_names(manifest_dir: &Path) -> Result<BTreeSet<String>, String> {
    let crates = manifest_dir.join("crates");
    let entries = fs::read_dir(&crates)
        .map_err(|e| format!("unreadable crates directory {}: {e}", crates.display()))?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("unreadable entry in {}: {e}", crates.display()))?;
        if entry.path().join("Cargo.toml").is_file() {
            names.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(names)
}

/// The root manifest's `workspace.members` patterns.
fn workspace_members(manifest_dir: &Path) -> Result<Vec<String>, String> {
    let path = manifest_dir.join("Cargo.toml");
    let text =
        fs::read_to_string(&path).map_err(|e| format!("unreadable {}: {e}", path.display()))?;
    let parsed: toml::Table =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let members = parsed
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .ok_or_else(|| format!("{}: no workspace.members array", path.display()))?;
    members
        .iter()
        .map(|m| {
            m.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{}: non-string workspace member {m}", path.display()))
        })
        .collect()
}

/// Whether one `workspace.members` pattern selects `path`: an exact path, or `dir/*` for each
/// direct child of `dir`.
fn member_matches(pattern: &str, path: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(dir) => path
            .strip_prefix(dir)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|child| !child.is_empty() && !child.contains('/')),
        None => pattern == path,
    }
}

/// Registry and workspace discovery must agree in both directions: every layer names an existing
/// crate that is a workspace member, every crate with a manifest is listed, and none is listed
/// twice.
fn registry_violations(
    layers: &[&str],
    crates: &BTreeSet<String>,
    members: &[String],
) -> Vec<String> {
    let mut violations = Vec::new();
    let mut seen = BTreeSet::new();
    for layer in layers {
        if !seen.insert(*layer) {
            violations.push(format!("CRATE_LAYERS lists {layer} more than once"));
        }
        if !crates.contains(*layer) {
            violations.push(format!(
                "CRATE_LAYERS lists {layer}, but crates/{layer}/Cargo.toml does not exist"
            ));
        }
    }
    for name in crates {
        if !layers.contains(&name.as_str()) {
            violations.push(format!(
                "crates/{name} has a manifest but is not in CRATE_LAYERS"
            ));
        }
        let path = format!("crates/{name}");
        if !members.iter().any(|m| member_matches(m, &path)) {
            violations.push(format!(
                "crates/{name} is not selected by workspace.members"
            ));
        }
    }
    violations
}

#[test]
fn crates_depend_only_on_lower_layers() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let crates = manifest_dir.join("crates");
    let names = workspace_crate_names(manifest_dir).unwrap_or_else(|e| panic!("{e}"));
    let mut violations = Vec::new();
    for name in &names {
        let text = fs::read_to_string(crates.join(name).join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("unreadable crates/{name}/Cargo.toml: {e}"));
        violations.extend(crate_layer_violations(name, &text));
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

/// Every crate is discovered and registered exactly once, and every registered layer exists.
#[test]
fn workspace_crates_and_layer_registry_agree() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let names = workspace_crate_names(manifest_dir).unwrap_or_else(|e| panic!("{e}"));
    let members = workspace_members(manifest_dir).unwrap_or_else(|e| panic!("{e}"));
    let violations = registry_violations(CRATE_LAYERS, &names, &members);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

/// Modules have one conventional definition in every source root (`src/` and `crates/*/src/`).
#[test]
fn modules_have_one_conventional_definition() {
    let violations = layout_violations(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|error| panic!("architecture guard: {error}"));
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn conventional_layout_accepts_leaf_and_mod_files_and_rejects_ambiguity() {
    let base = std::env::temp_dir().join(format!("dial-arch-layout-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let clean = base.join("clean");
    let dirty = base.join("dirty");
    for manifest in [&clean, &dirty] {
        fs::create_dir_all(manifest.join("src")).unwrap();
        fs::create_dir_all(manifest.join("crates/app/src")).unwrap();
    }
    // Accepted: leaf `foo.rs` with children in `foo/`, and `foo/mod.rs` alone.
    fs::create_dir_all(clean.join("src/leaf")).unwrap();
    fs::write(clean.join("src/leaf.rs"), "").unwrap();
    fs::write(clean.join("src/leaf/children.rs"), "").unwrap();
    fs::create_dir_all(clean.join("crates/app/src/modern")).unwrap();
    fs::write(clean.join("crates/app/src/modern/mod.rs"), "").unwrap();
    fs::write(clean.join("crates/app/src/modern/inner.rs"), "").unwrap();
    // Rejected: the same module defined by both `top.rs` and `top/mod.rs`, in the root package
    // and in a crate source root.
    fs::write(dirty.join("src/top.rs"), "").unwrap();
    fs::create_dir_all(dirty.join("src/top")).unwrap();
    fs::write(dirty.join("src/top/mod.rs"), "").unwrap();
    fs::write(dirty.join("crates/app/src/nest.rs"), "").unwrap();
    fs::create_dir_all(dirty.join("crates/app/src/nest")).unwrap();
    fs::write(dirty.join("crates/app/src/nest/mod.rs"), "").unwrap();
    let accepted = layout_violations(&clean);
    let rejected = layout_violations(&dirty);
    let _ = fs::remove_dir_all(&base);
    assert_eq!(accepted.unwrap(), Vec::<String>::new());
    let found = rejected.unwrap();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().any(|v| v.starts_with("top.rs:")), "{found:?}");
    assert!(found.iter().any(|v| v.starts_with("nest.rs:")), "{found:?}");
}

#[test]
fn member_patterns_select_direct_children_only() {
    assert!(member_matches("crates/*", "crates/app"));
    assert!(!member_matches("crates/*", "crates"));
    assert!(!member_matches("crates/*", "crates/"));
    assert!(!member_matches("crates/*", "crates/app/inner"));
    assert!(member_matches("crates/app", "crates/app"));
    assert!(!member_matches("crates/app", "crates/other"));
    assert!(!member_matches(".", "crates/app"));
}

#[test]
fn registry_reports_unlisted_missing_duplicate_and_unselected_crates() {
    let crates: BTreeSet<String> = ["dial-core", "dial-extra"].map(str::to_owned).into();
    let all = ["crates/*".to_owned()];
    assert_eq!(
        registry_violations(&["dial-core", "dial-ghost", "dial-core"], &crates, &all),
        vec![
            "CRATE_LAYERS lists dial-ghost, but crates/dial-ghost/Cargo.toml does not exist"
                .to_owned(),
            "CRATE_LAYERS lists dial-core more than once".to_owned(),
            "crates/dial-extra has a manifest but is not in CRATE_LAYERS".to_owned(),
        ]
    );
    let only_core = ["crates/dial-core".to_owned()];
    assert_eq!(
        registry_violations(&["dial-core", "dial-extra"], &crates, &only_core),
        vec!["crates/dial-extra is not selected by workspace.members".to_owned()]
    );
    assert!(registry_violations(&["dial-core", "dial-extra"], &crates, &all).is_empty());
}

#[test]
fn workspace_discovery_is_read_from_the_manifests() {
    let base = std::env::temp_dir().join(format!("dial-arch-registry-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("crates/dial-a")).unwrap();
    fs::create_dir_all(base.join("crates/dial-b")).unwrap();
    fs::write(base.join("crates/dial-a/Cargo.toml"), "").unwrap();
    fs::write(base.join("crates/loose.txt"), "").unwrap();
    fs::write(
        base.join("Cargo.toml"),
        "[workspace]\nmembers = [\".\", \"crates/*\"]\n",
    )
    .unwrap();
    let names = workspace_crate_names(&base);
    let members = workspace_members(&base);
    let _ = fs::remove_dir_all(&base);
    assert_eq!(
        names.unwrap(),
        ["dial-a".to_owned()].into_iter().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        members.unwrap(),
        vec![".".to_owned(), "crates/*".to_owned()]
    );
}

#[test]
fn crate_manifests_are_checked_against_the_layer_order() {
    let ok = "[dependencies]\ndial-core = { path = \"../dial-core\" }\n";
    assert!(crate_layer_violations("dial-llm", ok).is_empty());
    let up = "[dev-dependencies]\ndial-app.workspace = true\n";
    assert_eq!(
        crate_layer_violations("dial-core", up),
        vec!["crates/dial-core must not depend on dial-app".to_owned()]
    );
    let target = "[target.'cfg(unix)'.dependencies]\ndial-ui.workspace = true\n";
    assert_eq!(
        crate_layer_violations("dial-agent", target),
        vec!["crates/dial-agent must not depend on dial-ui".to_owned()]
    );
    assert_eq!(
        crate_layer_violations("dial-new", ok),
        vec!["crates/dial-new: not listed in CRATE_LAYERS".to_owned()]
    );
}
