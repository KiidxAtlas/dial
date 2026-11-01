use super::*;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn fake_bin(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn config(entries: &[(&str, &Path, &[&str])]) -> LspConfig {
    LspConfig {
        servers: entries
            .iter()
            .map(|(name, program, extensions)| {
                (
                    name.to_string(),
                    LspServerConfig {
                        program: program.to_path_buf(),
                        args: Some(vec!["--flag".into()]),
                        extensions: Some(extensions.iter().map(|e| e.to_string()).collect()),
                        ..LspServerConfig::default()
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
        ..LspConfig::default()
    }
}

fn project(markers: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for marker in markers {
        fs::write(dir.path().join(marker), "").unwrap();
    }
    dir
}

#[test]
fn installed_dirs_lists_dial_tool_bins_only_when_present() {
    let user = tempfile::tempdir().unwrap();
    assert!(installed_dirs(user.path()).is_empty());
    assert!(installed_dirs(Path::new("")).is_empty());
    let bin = user.path().join("tools/gopls/bin");
    let npm = user
        .path()
        .join("tools/yaml-language-server/node_modules/.bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&npm).unwrap();
    fs::create_dir_all(user.path().join("tools/empty")).unwrap();
    assert_eq!(installed_dirs(user.path()), vec![bin, npm]);
}

#[test]
fn installable_names_missing_builtin_servers_with_recipes() {
    let ws = project(&["Cargo.toml"]);
    let root = fs::canonicalize(ws.path()).unwrap();
    let servers = Servers::new(&LspConfig::default());
    let file = root.join("src/lib.rs");
    let empty = tempfile::tempdir().unwrap();
    let dirs = [empty.path().to_path_buf()];
    assert_eq!(
        servers.installable(&root, &file, &dirs),
        vec!["rust-analyzer"]
    );
    fake_bin(empty.path(), "rust-analyzer");
    assert!(servers.installable(&root, &file, &dirs).is_empty());
}

#[test]
fn installable_skips_configured_servers() {
    let ws = project(&["Cargo.toml"]);
    let root = fs::canonicalize(ws.path()).unwrap();
    let missing = root.join("nope");
    let servers = Servers::new(&config(&[("rust-analyzer", &missing, &["rs"])]));
    let dirs = [root.clone()];
    assert!(
        servers
            .installable(&root, &root.join("lib.rs"), &dirs)
            .is_empty()
    );
}

#[test]
fn bundled_defaults_parse_with_dozens_of_servers() {
    assert!(DEFAULTS.len() >= 50);
    assert!(
        DEFAULTS
            .iter()
            .any(|d| d.name == "rust-analyzer" && d.root_markers.contains(&"Cargo.toml".into()))
    );
}

#[test]
fn defaults_need_root_markers_and_the_nearest_root_wins() {
    let bin = tempfile::tempdir().unwrap();
    let ra = fake_bin(bin.path(), "rust-analyzer");
    let ws = project(&["Cargo.toml"]);
    let root = fs::canonicalize(ws.path()).unwrap();
    fs::create_dir_all(root.join("crates/a/src")).unwrap();
    fs::write(root.join("crates/a/Cargo.toml"), "").unwrap();
    let servers = Servers::new(&LspConfig::default());
    let dirs = [bin.path().to_path_buf()];
    let got = servers
        .select(&root, &root.join("crates/a/src/lib.rs"), &dirs)
        .unwrap();
    assert_eq!(
        (got.language.as_str(), got.program, got.root),
        ("rust-analyzer", ra, root.join("crates/a"))
    );
    // A marker above the workspace roots the server there (a workspace nested in a larger
    // project), and temp directories can sit inside a checkout that has a Cargo.toml. Swap in
    // a marker that cannot exist so the unrooted error is checked without ambient ancestors.
    let mut config = LspConfig::default();
    config.servers.insert(
        "rust-analyzer".into(),
        LspServerConfig {
            root_markers: Some(vec!["dial-absent.marker".into()]),
            ..LspServerConfig::default()
        },
    );
    let bare = project(&[]);
    let bare = fs::canonicalize(bare.path()).unwrap();
    let error = Servers::new(&config)
        .select(&bare, &bare.join("main.rs"), &dirs)
        .unwrap_err();
    assert!(
        error.contains("root marker") && error.contains("dial-absent.marker"),
        "{error}"
    );
}

#[test]
fn project_local_bins_beat_path_and_linters_come_last() {
    let bin = tempfile::tempdir().unwrap();
    fake_bin(bin.path(), "pyright-langserver");
    let ws = project(&["pyproject.toml", "ruff.toml"]);
    let root = fs::canonicalize(ws.path()).unwrap();
    fs::create_dir_all(root.join(".venv/bin")).unwrap();
    let local = fake_bin(&root.join(".venv/bin"), "ruff");
    let servers = Servers::new(&LspConfig::default());
    let all = servers
        .select_all(&root, &root.join("a.py"), &[bin.path().to_path_buf()])
        .unwrap();
    assert_eq!(all[0].language, "pyright");
    let last = all.last().unwrap();
    assert!(last.linter && last.program == local, "{all:?}");
}

#[test]
fn missing_default_names_what_to_install() {
    let empty = tempfile::tempdir().unwrap();
    let ws = project(&["Cargo.toml"]);
    let root = fs::canonicalize(ws.path()).unwrap();
    let error = Servers::new(&LspConfig::default())
        .select(&root, &root.join("lib.rs"), &[empty.path().to_path_buf()])
        .unwrap_err();
    assert!(
        error.contains("rust-analyzer") && error.contains("rustup") && error.contains("[lsp."),
        "{error}"
    );
}

#[test]
fn rust_analyzer_falls_back_to_rustup_which_in_the_server_root() {
    let tools = tempfile::tempdir().unwrap();
    let toolchain = tools.path().join("toolchains/pinned/bin");
    fs::create_dir_all(&toolchain).unwrap();
    let ra = fake_bin(&toolchain, "rust-analyzer");
    let ws = project(&["Cargo.toml"]);
    let root = fs::canonicalize(ws.path()).unwrap();
    // The fake rustup answers only when run in the project root with auto-install disabled.
    let rustup = tools.path().join("rustup");
    fs::write(
        &rustup,
        format!(
            "#!/bin/sh\n[ \"$1 $2\" = 'which rust-analyzer' ] && [ \"$(pwd -P)\" = '{}' ] && [ \"$RUSTUP_AUTO_INSTALL\" = 0 ] && echo '{}' && exit 0\nexit 1\n",
            root.display(),
            ra.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&rustup, fs::Permissions::from_mode(0o755)).unwrap();
    let dirs = [tools.path().to_path_buf()];
    let servers = Servers::new(&LspConfig::default());
    let got = servers.select(&root, &root.join("lib.rs"), &dirs).unwrap();
    assert_eq!((got.language.as_str(), got.program), ("rust-analyzer", ra));
    assert!(
        servers
            .installable(&root, &root.join("lib.rs"), &dirs)
            .is_empty()
    );

    // Without a toolchain answer, the error lists the directories searched.
    fs::write(&rustup, "#!/bin/sh\nexit 1\n").unwrap();
    let error = servers
        .select(&root, &root.join("lib.rs"), &dirs)
        .unwrap_err();
    assert!(
        error.contains(&format!("searched {}", tools.path().display())),
        "{error}"
    );
}

#[test]
fn pyenv_shims_resolve_to_the_active_version_or_are_skipped() {
    let pyenv_root = tempfile::tempdir().unwrap();
    let shims = pyenv_root.path().join("shims");
    let real_dir = pyenv_root.path().join("versions/3.14/bin");
    fs::create_dir_all(&shims).unwrap();
    fs::create_dir_all(&real_dir).unwrap();
    fs::create_dir_all(pyenv_root.path().join("bin")).unwrap();
    fake_bin(&shims, "pyright-langserver");
    let real = fake_bin(&real_dir, "pyright-langserver");
    let pyenv = pyenv_root.path().join("bin/pyenv");
    let answer = |active: bool| {
        let body = if active {
            format!(
                "[ \"$1 $2\" = 'which pyright-langserver' ] && echo '{}' && exit 0\nexit 1",
                real.display()
            )
        } else {
            "echo 'pyenv: pyright-langserver: command not found' >&2\nexit 127".to_owned()
        };
        fs::write(&pyenv, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&pyenv, fs::Permissions::from_mode(0o755)).unwrap();
    };
    let ws = project(&["pyproject.toml"]);
    let root = fs::canonicalize(ws.path()).unwrap();
    let file = root.join("a.py");
    let servers = Servers::new(&LspConfig::default());

    answer(true);
    let got = servers
        .select(&root, &file, std::slice::from_ref(&shims))
        .unwrap();
    assert_eq!(
        (got.language.as_str(), got.program),
        ("pyright", real.clone())
    );

    // The active version lacks the command: the shim is skipped, so a later directory wins and
    // otherwise the server counts as missing instead of failing at initialize.
    answer(false);
    let later = tempfile::tempdir().unwrap();
    let fallback = fake_bin(later.path(), "pyright-langserver");
    let got = servers
        .select(&root, &file, &[shims.clone(), later.path().to_path_buf()])
        .unwrap();
    assert_eq!(got.program, fallback);
    let missing = servers
        .select_all(&root, &file, std::slice::from_ref(&shims))
        .map(|all| all.into_iter().map(|s| s.program).collect::<Vec<_>>());
    assert!(
        missing
            .as_ref()
            .is_err_and(|error| error.contains("pyright")),
        "{missing:?}"
    );
}

#[test]
fn unknown_and_missing_extensions_explain_how_to_configure() {
    let servers = Servers::new(&LspConfig::default());
    assert!(
        servers
            .names_for("a.unknownext")
            .unwrap_err()
            .contains("[lsp.mylang]")
    );
    assert!(
        servers
            .names_for("LICENSE")
            .unwrap_err()
            .contains("no file extension")
    );
}

#[test]
fn configured_extensions_replace_defaults_and_overrides_inherit() {
    let bin = tempfile::tempdir().unwrap();
    fake_bin(bin.path(), "clangd");
    let zls = fake_bin(bin.path(), "my-zls");
    let ws = project(&["Makefile"]);
    let root = fs::canonicalize(ws.path()).unwrap();
    let servers = Servers::new(&config(&[("zig", &zls, &[".zig", "H"])]));
    let dirs = [bin.path().to_path_buf()];
    assert_eq!(
        servers
            .select(&root, &root.join("a.zig"), &dirs)
            .unwrap()
            .language,
        "zig"
    );
    assert_eq!(
        servers
            .select_all(&root, &root.join("a.h"), &dirs)
            .unwrap()
            .len(),
        1,
        "config replaces clangd for .h"
    );
    assert_eq!(
        servers
            .select(&root, &root.join("a.c"), &dirs)
            .unwrap()
            .language,
        "clangd"
    );
    let mut overridden = LspConfig::default();
    overridden.servers.insert(
        "clangd".into(),
        LspServerConfig {
            args: Some(vec![]),
            ..LspServerConfig::default()
        },
    );
    let got = Servers::new(&overridden)
        .select(&root, &root.join("a.c"), &dirs)
        .unwrap();
    assert!(got.args.is_empty() && got.program.ends_with("clangd"));
    overridden.servers.get_mut("clangd").unwrap().disabled = true;
    assert!(
        Servers::new(&overridden)
            .select(&root, &root.join("a.c"), &dirs)
            .is_err()
    );
}

#[test]
fn configured_program_missing_is_a_clear_error() {
    let ws = project(&[]);
    let root = fs::canonicalize(ws.path()).unwrap();
    let servers = Servers::new(&config(&[(
        "zig",
        Path::new("/definitely/not/here/zls"),
        &["zig"],
    )]));
    let error = servers.select(&root, &root.join("a.zig"), &[]).unwrap_err();
    assert!(
        error.contains("/definitely/not/here/zls") && error.contains("[lsp.zig]"),
        "{error}"
    );
}

#[test]
fn wildcard_root_markers_match_one_level() {
    let ws = project(&["demo.cabal"]);
    assert!(has_markers(ws.path(), &["*.cabal"]));
    assert!(!has_markers(ws.path(), &["*.sln"]));
    assert!(wildcard("a*c?", "abbbcd") && !wildcard("a*c", "abd"));
}

#[test]
fn language_ids_follow_the_extension() {
    assert_eq!(language_id("tsx", "typescript"), "typescriptreact");
    assert_eq!(language_id("hpp", "c"), "cpp");
    assert_eq!(language_id("weird", "zig"), "zig");
}
