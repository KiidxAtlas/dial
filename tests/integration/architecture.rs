//! Executable module-boundary checks (see ARCHITECTURE.md). Each rule lists a module and the
//! sibling modules it must not depend on.
//!
//! Only `#[cfg(test)]` modules (inline bodies, out-of-line module files and their submodules,
//! and files marked `#![cfg(test)]`) are exempt; production code after a test module is still
//! scanned. Comments and string/char literal contents are ignored.

use std::collections::BTreeSet;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every Rust file of a top-level module: `src/<m>.rs` plus everything under `src/<m>/`.
fn module_files(src: &Path, module: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let flat = src.join(format!("{module}.rs"));
    if flat.is_file() {
        files.push(flat);
    }
    rust_files(&src.join(module), &mut files);
    files.sort();
    files
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
    /// Byte ranges of inline `#[cfg(test)] mod x { … }` bodies.
    inline: Vec<Range<usize>>,
    /// Out-of-line `#[cfg(test)] mod x;` declarations: (name, explicit `#[path]`).
    files: Vec<(String, Option<String>)>,
}

/// Find `#[cfg(test)]`-gated module items in stripped source. `original` supplies `#[path]` values.
fn test_modules(code: &str, original: &str) -> TestModules {
    let mut found = TestModules {
        inline: Vec::new(),
        files: Vec::new(),
    };
    let mut from = 0;
    while let Some(at) = code[from..].find("#[cfg(test)]").map(|n| from + n) {
        from = at + "#[cfg(test)]".len();
        let mut i = skip_ws(code, from);
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
            Some(b';') => found.files.push((name, path_attr)),
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
fn exempt_paths(src: &Path) -> BTreeSet<PathBuf> {
    let mut all = Vec::new();
    rust_files(src, &mut all);
    let mut exempt = BTreeSet::new();
    for file in &all {
        let original = fs::read_to_string(file).unwrap();
        let code = strip(&original);
        if code.trim_start().starts_with("#![cfg(test)]") {
            exempt.insert(file.clone());
            exempt.insert(child_dir(file));
        }
        for (name, path_attr) in test_modules(&code, &original).files {
            let dir = file.parent().unwrap();
            let candidates = match path_attr {
                Some(path) => vec![dir.join(path)],
                None => {
                    let base = child_dir(file);
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
    exempt
}

fn line_of(code: &str, offset: usize) -> usize {
    code[..offset].bytes().filter(|&c| c == b'\n').count() + 1
}

/// Every `crate::<module>` dependency in stripped source: direct paths and every top-level item
/// of `crate::{…}` groups. Returns (byte offset, module).
fn crate_refs(code: &str) -> Vec<(usize, String)> {
    let mut refs = Vec::new();
    let mut from = 0;
    while let Some(at) = code[from..].find("crate::").map(|n| from + n) {
        from = at + "crate::".len();
        if at > 0 && {
            let prev = code.as_bytes()[at - 1];
            prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'$'
        } {
            continue;
        }
        let i = skip_ws(code, from);
        if code[i..].starts_with('{') {
            let end = matching(code, i, (b'{', b'}'));
            let body = &code[i + 1..end - 1];
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
                if !name.is_empty() && item[lead + name.len()..].trim_start().starts_with("::") {
                    refs.push((i + 1 + offset + lead, name.to_owned()));
                }
            }
        } else {
            let name = ident(code, i);
            if !name.is_empty() && code[i + name.len()..].starts_with("::") {
                refs.push((at, name.to_owned()));
            }
        }
    }
    refs
}

/// Violations of `forbidden` in one source text, ignoring exempt `#[cfg(test)]` module bodies.
fn violations_in(original: &str, forbidden: &[&str]) -> Vec<(usize, String)> {
    let code = strip(original);
    let tests = test_modules(&code, original).inline;
    crate_refs(&code)
        .into_iter()
        .filter(|(offset, name)| {
            forbidden.contains(&name.as_str()) && !tests.iter().any(|r| r.contains(offset))
        })
        .map(|(offset, name)| (line_of(&code, offset), name))
        .collect()
}

#[test]
fn modules_respect_their_dependency_boundaries() {
    // (module, forbidden `crate::<x>` paths)
    let rules: &[(&str, &[&str])] = &[
        ("ui", &["learn", "tools", "config"]),
        ("llm", &["ui", "learn", "tools"]),
        ("tools", &["ui", "learn", "agent"]),
        ("runtime", &["ui", "learn", "agent", "tools", "llm"]),
        ("storage", &["ui"]),
        ("config", &["ui", "learn"]),
        ("agent", &["ui"]),
        ("learn", &["ui"]),
        (
            "workspace_copy",
            &["agent", "benchmark", "learn", "config", "llm", "ui"],
        ),
    ];
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let exempt = exempt_paths(&src);
    let mut violations = Vec::new();
    for (module, forbidden) in rules {
        for file in module_files(&src, module) {
            if exempt.iter().any(|path| file.starts_with(path)) {
                continue;
            }
            let text = fs::read_to_string(&file).unwrap();
            for (line, bad) in violations_in(&text, forbidden) {
                violations.push(format!(
                    "{}:{line}: {module} must not use {bad}",
                    file.strip_prefix(&src).unwrap().display()
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
    let found = violations_in(source, &["tools"]);
    assert_eq!(
        found,
        vec![(10, "tools".to_owned()), (11, "tools".to_owned())]
    );
}

#[test]
fn test_gated_non_module_items_are_scanned() {
    let source = "#[cfg(test)]\nuse crate::tools::EditMode;\n#[cfg(test)]\npub(crate) mod fake {\n    use crate::tools::X;\n}\n";
    assert_eq!(
        violations_in(source, &["tools"]),
        vec![(2, "tools".to_owned())]
    );
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
    let exempt = exempt_paths(&root);
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
