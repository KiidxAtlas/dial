//! Which language server handles which file.
//!
//! Built-in defaults are looked up on `PATH` only. `[lsp.<language>]` config entries replace the
//! default of the same name and take priority over every default for the extensions they list.

use crate::config::types::LspServerConfig;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A program to try (first found on `PATH` wins) and the arguments to start it with.
type Candidate = (&'static str, &'static [&'static str]);

struct Builtin {
    language: &'static str,
    extensions: &'static [&'static str],
    candidates: &'static [Candidate],
    install: &'static str,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        language: "rust",
        extensions: &["rs"],
        candidates: &[("rust-analyzer", &[])],
        install: "run `rustup component add rust-analyzer`",
    },
    Builtin {
        language: "typescript",
        extensions: &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        candidates: &[("typescript-language-server", &["--stdio"])],
        install: "run `npm install -g typescript-language-server typescript`",
    },
    Builtin {
        language: "python",
        extensions: &["py"],
        candidates: &[("pyright-langserver", &["--stdio"]), ("pylsp", &[])],
        install: "install pyright (`npm install -g pyright`) or python-lsp-server (`pip install python-lsp-server`)",
    },
    Builtin {
        language: "go",
        extensions: &["go"],
        candidates: &[("gopls", &[])],
        install: "run `go install golang.org/x/tools/gopls@latest`",
    },
    Builtin {
        language: "c",
        extensions: &["c", "cc", "cpp", "cxx", "h", "hpp", "hh", "hxx"],
        candidates: &[("clangd", &[])],
        install: "install clangd (it ships with LLVM and the Xcode command line tools)",
    },
    Builtin {
        language: "lua",
        extensions: &["lua"],
        candidates: &[("lua-language-server", &[])],
        install: "install lua-language-server",
    },
];

/// LSP `languageId` for a file extension; `fallback` (the server's language name) otherwise.
pub fn language_id<'a>(extension: &str, fallback: &'a str) -> &'a str {
    match extension {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "c" => "c",
        "cc" | "cpp" | "cxx" | "h" | "hpp" | "hh" | "hxx" => "cpp",
        "lua" => "lua",
        _ => fallback,
    }
}

enum Launch {
    Configured { program: PathBuf, args: Vec<String> },
    Builtin(&'static Builtin),
}

struct Entry {
    language: String,
    extensions: Vec<String>,
    launch: Launch,
}

/// A server ready to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub language: String,
    pub program: PathBuf,
    pub args: Vec<String>,
}

pub struct Servers {
    /// Configured entries first, then built-ins: the first entry claiming an extension wins.
    entries: Vec<Entry>,
}

fn normalize_extension(extension: &str) -> String {
    extension.trim_start_matches('.').to_ascii_lowercase()
}

pub fn extension_of(file: &str) -> Option<String> {
    Path::new(file)
        .extension()
        .and_then(OsStr::to_str)
        .map(normalize_extension)
}

/// Directories of the `PATH` environment variable.
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default()
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A program with a path separator is taken as is; a bare name is searched in `dirs`.
fn find_program(program: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return is_executable(program).then(|| program.to_owned());
    }
    dirs.iter()
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

impl Servers {
    pub fn new(config: &BTreeMap<String, LspServerConfig>) -> Self {
        let mut entries: Vec<Entry> = config
            .iter()
            .map(|(language, server)| Entry {
                language: language.clone(),
                extensions: server
                    .extensions
                    .iter()
                    .map(|e| normalize_extension(e))
                    .collect(),
                launch: Launch::Configured {
                    program: server.program.clone(),
                    args: server.args.clone(),
                },
            })
            .collect();
        entries.extend(
            BUILTINS
                .iter()
                .filter(|b| !config.contains_key(b.language))
                .map(|builtin| Entry {
                    language: builtin.language.to_owned(),
                    extensions: builtin.extensions.iter().map(|e| e.to_string()).collect(),
                    launch: Launch::Builtin(builtin),
                }),
        );
        Self { entries }
    }

    fn entry_for(&self, file: &str) -> Result<&Entry, String> {
        let extension = extension_of(file).ok_or_else(|| {
            format!("cannot pick a language server for {file:?}: it has no file extension")
        })?;
        self.entries.iter().find(|entry| entry.extensions.contains(&extension)).ok_or_else(|| {
            format!(
                "no language server is configured for .{extension} files; add one in dial's config, \
                 e.g. [lsp.mylang] with program, args and extensions = [\"{extension}\"]"
            )
        })
    }

    /// Server for `file` (matched by extension), searching `dirs` for the program. A missing
    /// program is an actionable error naming what to install or configure.
    pub fn select(&self, file: &str, dirs: &[PathBuf]) -> Result<Resolved, String> {
        let entry = self.entry_for(file)?;
        match &entry.launch {
            Launch::Configured { program, args } => {
                let found = find_program(program, dirs).ok_or_else(|| {
                    format!(
                        "the language server for {} is configured as {:?} but that program was not found \
                         or is not executable; fix [lsp.{}] program in dial's config",
                        entry.language,
                        program.display(),
                        entry.language
                    )
                })?;
                Ok(Resolved {
                    language: entry.language.clone(),
                    program: found,
                    args: args.clone(),
                })
            }
            Launch::Builtin(builtin) => builtin
                .candidates
                .iter()
                .find_map(|(program, args)| {
                    find_program(Path::new(program), dirs).map(|found| Resolved {
                        language: entry.language.clone(),
                        program: found,
                        args: args.iter().map(|a| a.to_string()).collect(),
                    })
                })
                .ok_or_else(|| {
                    let names: Vec<&str> = builtin.candidates.iter().map(|(p, _)| *p).collect();
                    format!(
                        "no {} language server found on PATH (looked for {}); {}, or point \
                         [lsp.{}] program at one in dial's config",
                        entry.language,
                        names.join(", "),
                        builtin.install,
                        entry.language
                    )
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn config(entries: &[(&str, &Path, &[&str])]) -> BTreeMap<String, LspServerConfig> {
        entries
            .iter()
            .map(|(language, program, extensions)| {
                (
                    language.to_string(),
                    LspServerConfig {
                        program: program.to_path_buf(),
                        args: vec!["--flag".into()],
                        extensions: extensions.iter().map(|e| e.to_string()).collect(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn builtin_servers_are_found_by_extension_on_path() {
        let bin = tempfile::tempdir().unwrap();
        let ra = fake_bin(bin.path(), "rust-analyzer");
        let ts = fake_bin(bin.path(), "typescript-language-server");
        let servers = Servers::new(&BTreeMap::new());
        let dirs = [bin.path().to_path_buf()];
        let rust = servers.select("src/main.RS", &dirs).unwrap();
        assert_eq!(
            (rust.language.as_str(), rust.program, rust.args),
            ("rust", ra, vec![])
        );
        for file in ["a.ts", "a.tsx", "a.js", "a.jsx"] {
            let got = servers.select(file, &dirs).unwrap();
            assert_eq!(
                (got.program, got.args),
                (ts.clone(), vec!["--stdio".to_string()]),
                "{file}"
            );
        }
    }

    #[test]
    fn python_prefers_pyright_and_falls_back_to_pylsp() {
        let bin = tempfile::tempdir().unwrap();
        let pylsp = fake_bin(bin.path(), "pylsp");
        let dirs = [bin.path().to_path_buf()];
        let servers = Servers::new(&BTreeMap::new());
        assert_eq!(servers.select("a.py", &dirs).unwrap().program, pylsp);
        let pyright = fake_bin(bin.path(), "pyright-langserver");
        let got = servers.select("a.py", &dirs).unwrap();
        assert_eq!(
            (got.program, got.args),
            (pyright, vec!["--stdio".to_string()])
        );
    }

    #[test]
    fn missing_builtin_names_what_to_install() {
        let empty = tempfile::tempdir().unwrap();
        let servers = Servers::new(&BTreeMap::new());
        let error = servers
            .select("lib.rs", &[empty.path().to_path_buf()])
            .unwrap_err();
        assert!(
            error.contains("rust-analyzer")
                && error.contains("rustup")
                && error.contains("[lsp.rust]"),
            "{error}"
        );
    }

    #[test]
    fn non_executable_files_on_path_are_ignored() {
        let bin = tempfile::tempdir().unwrap();
        fs::write(bin.path().join("gopls"), "not executable").unwrap();
        let servers = Servers::new(&BTreeMap::new());
        assert!(servers.select("a.go", &[bin.path().to_path_buf()]).is_err());
    }

    #[test]
    fn unknown_and_missing_extensions_explain_how_to_configure() {
        let servers = Servers::new(&BTreeMap::new());
        assert!(
            servers
                .select("a.zig", &[])
                .unwrap_err()
                .contains("[lsp.mylang]")
        );
        assert!(
            servers
                .select("Makefile", &[])
                .unwrap_err()
                .contains("no file extension")
        );
    }

    #[test]
    fn config_overrides_a_builtin_of_the_same_language() {
        let bin = tempfile::tempdir().unwrap();
        fake_bin(bin.path(), "rust-analyzer");
        let custom = fake_bin(bin.path(), "my-ra");
        let servers = Servers::new(&config(&[("rust", &custom, &["rs"])]));
        let got = servers.select("a.rs", &[bin.path().to_path_buf()]).unwrap();
        assert_eq!(
            (got.program, got.args),
            (custom, vec!["--flag".to_string()])
        );
    }

    #[test]
    fn configured_extensions_win_over_builtins_and_extend_them() {
        let bin = tempfile::tempdir().unwrap();
        fake_bin(bin.path(), "clangd");
        let zls = fake_bin(bin.path(), "zls");
        let servers = Servers::new(&config(&[("zig", &zls, &[".zig", "H"])]));
        let dirs = [bin.path().to_path_buf()];
        assert_eq!(servers.select("a.zig", &dirs).unwrap().language, "zig");
        assert_eq!(
            servers.select("a.h", &dirs).unwrap().language,
            "zig",
            "config beats the C default for .h"
        );
        assert_eq!(servers.select("a.c", &dirs).unwrap().language, "c");
    }

    #[test]
    fn configured_program_missing_is_a_clear_error() {
        let servers = Servers::new(&config(&[(
            "zig",
            Path::new("/definitely/not/here/zls"),
            &["zig"],
        )]));
        let error = servers.select("a.zig", &[]).unwrap_err();
        assert!(
            error.contains("/definitely/not/here/zls") && error.contains("[lsp.zig]"),
            "{error}"
        );
    }

    #[test]
    fn configured_bare_program_names_are_searched_on_path() {
        let bin = tempfile::tempdir().unwrap();
        let zls = fake_bin(bin.path(), "zls");
        let servers = Servers::new(&config(&[("zig", Path::new("zls"), &["zig"])]));
        assert_eq!(
            servers
                .select("a.zig", &[bin.path().to_path_buf()])
                .unwrap()
                .program,
            zls
        );
    }

    #[test]
    fn language_ids_follow_the_extension() {
        assert_eq!(language_id("tsx", "typescript"), "typescriptreact");
        assert_eq!(language_id("hpp", "c"), "cpp");
        assert_eq!(language_id("zig", "zig"), "zig");
    }
}
