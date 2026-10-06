//! Executable module-boundary checks (see ARCHITECTURE.md). Each rule lists a module and the
//! sibling modules it must not depend on.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn modules_respect_their_dependency_boundaries() {
    // (module, forbidden `crate::<x>` paths)
    let rules: &[(&str, &[&str])] = &[
        ("ui", &["learn", "tools", "config"]),
        ("llm", &["ui", "learn", "tools"]),
        ("tools", &["ui", "learn"]),
        ("storage", &["ui"]),
        ("config", &["ui", "learn"]),
        ("agent", &["ui"]),
        ("learn", &["ui"]),
    ];
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for (module, forbidden) in rules {
        let mut files = Vec::new();
        rust_files(&src.join(module), &mut files);
        for file in files {
            let text = fs::read_to_string(&file).unwrap();
            for (n, line) in text.lines().enumerate() {
                let code = line.trim_start();
                // Test modules sit at the end of a file and may reach across boundaries.
                if code.starts_with("#[cfg(test)]") {
                    break;
                }
                if code.starts_with("//") {
                    continue;
                }
                for bad in *forbidden {
                    let direct = format!("crate::{bad}::");
                    let grouped = format!("crate::{{{bad}");
                    if code.contains(&direct) || code.contains(&grouped) {
                        violations.push(format!(
                            "{}:{}: {module} must not use {bad}",
                            file.strip_prefix(&src).unwrap().display(),
                            n + 1
                        ));
                    }
                }
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
