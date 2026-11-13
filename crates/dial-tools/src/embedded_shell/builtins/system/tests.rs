use super::super::{Io, RunFn};
use super::*;
use std::time::Duration;
use tempfile::tempdir;

fn run(f: RunFn, cwd: &Path, env: &[(String, String)], args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let mut input: &[u8] = b"";
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = f(
        &mut Io {
            name: "cmd",
            cwd,
            env,
            synopsis: "",
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            declined: None,
        },
        &args,
    );
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn env_of(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Runs `sleep` with the given operands on a fresh runtime and returns its exit code.
fn run_sleep(args: &[&str]) -> i32 {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(sleep_in_tempdir(args))
}

async fn sleep_in_tempdir(args: &[&str]) -> i32 {
    let dir = tempdir().unwrap();
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let mut input: &[u8] = b"";
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    sleep(
        &mut Io {
            name: "sleep",
            cwd: dir.path(),
            env: &[],
            synopsis: "",
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            declined: None,
        },
        &args,
    )
    .await
}

#[test]
fn sleep_parses_suffixes_and_rejects_bad_intervals() {
    let cases: &[(&str, Option<Duration>)] = &[
        ("0", Some(Duration::ZERO)),
        ("1", Some(Duration::from_secs(1))),
        ("0.5", Some(Duration::from_millis(500))),
        ("2s", Some(Duration::from_secs(2))),
        ("0.5m", Some(Duration::from_secs(30))),
        ("1h", Some(Duration::from_secs(3600))),
        ("1d", Some(Duration::from_secs(86_400))),
        ("", None),
        ("s", None),
        ("-1", None),
        ("1x", None),
        ("inf", None),
        ("nan", None),
        ("1e400", None),
    ];
    for (text, expected) in cases {
        assert_eq!(parse_interval(text), *expected, "interval {text:?}");
    }
}

#[test]
fn sleep_exit_codes_follow_the_usage_table() {
    let cases: &[(&[&str], i32)] = &[
        (&["0.01"], 0),
        (&["0.01", "0.01"], 0),
        (&["--", "0"], 0),
        (&["soon"], 1),
        (&["-x"], 2),
        (&[], 2),
        (&["--"], 2),
    ];
    for (args, expected) in cases {
        assert_eq!(run_sleep(args), *expected, "args {args:?}");
    }
}

#[test]
fn sleep_stops_when_its_future_is_dropped() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // A one-hour sleep must end when the surrounding timeout (the shell abort) drops it.
    let stopped = runtime.block_on(async {
        tokio::time::timeout(Duration::from_millis(20), sleep_in_tempdir(&["3600"]))
            .await
            .is_err()
    });
    assert!(stopped);
}

/// An executable named `name` in `dir`, as the platform runs it: `name` on Unix, `name.exe` on
/// Windows (where `which` finds it through `PATHEXT`). Returns the file name it created.
fn install(dir: &Path, name: &str) -> String {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let path = dir.join(&file);
    fs::write(&path, "x").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    file
}

#[test]
fn which_finds_executables_on_the_shell_path_only() {
    let first = tempdir().unwrap();
    let second = tempdir().unwrap();
    let tool = install(first.path(), "tool");
    let other = install(second.path(), "tool");
    let path = std::env::join_paths([first.path(), second.path()])
        .unwrap()
        .into_string()
        .unwrap();
    let env = env_of(&[("PATH", &path)]);
    let cwd = first.path();
    let shown = |dir: &Path, file: &str| dir.join(file).display().to_string() + "\n";
    let cases: &[(&[&str], i32, String)] = &[
        (&["tool"], 0, shown(first.path(), &tool)),
        (
            &["-a", "tool"],
            0,
            shown(first.path(), &tool) + &shown(second.path(), &other),
        ),
        (&["missing"], 1, String::new()),
        (&["tool", "missing"], 1, shown(first.path(), &tool)),
    ];
    for (args, code, stdout) in cases {
        let (got, out, _) = run(which, cwd, &env, args);
        assert_eq!(
            (got, out.as_str()),
            (*code, stdout.as_str()),
            "args {args:?}"
        );
    }
    let (code, _, err) = run(which, cwd, &env, &["missing"]);
    assert_eq!(code, 1);
    assert!(err.starts_with("cmd: no missing in ("), "{err}");
    assert_eq!(run(which, cwd, &env, &[]).0, 2);
    assert_eq!(run(which, cwd, &env, &["-x", "tool"]).0, 2);
}

#[test]
fn which_ignores_the_host_path_and_non_executable_files() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("plain"), "text").unwrap();
    let env = env_of(&[("PATH", &dir.path().display().to_string())]);
    // Without an execute bit (on Unix) or an executable suffix (on Windows) nothing is found.
    assert_eq!(run(which, dir.path(), &env, &["plain"]).0, 1);
    // The host's own PATH is never consulted: an empty shell PATH finds nothing.
    assert_eq!(run(which, dir.path(), &[], &["sh"]).0, 1);
}

#[test]
fn which_resolves_names_with_a_separator_where_they_point() {
    let dir = tempdir().unwrap();
    let tool = install(dir.path(), "tool");
    let name = format!("./{tool}");
    let (code, out, _) = run(which, dir.path(), &[], &[name.as_str()]);
    assert_eq!((code, out.as_str()), (0, &format!("./{tool}\n")[..]));
}

/// Canonical form of `path` as `realpath` prints it (without the Windows verbatim prefix).
fn canonical(path: &Path) -> PathBuf {
    plain(fs::canonicalize(path).unwrap())
}

#[test]
fn realpath_resolves_existing_and_missing_paths_by_mode() {
    let dir = tempdir().unwrap();
    let base = canonical(dir.path());
    fs::create_dir_all(dir.path().join("a/b")).unwrap();
    fs::write(dir.path().join("a/b/f"), "").unwrap();
    // Joined one component at a time so the separators are the platform's (`\` on Windows).
    let under = |parts: &[&str]| {
        parts
            .iter()
            .fold(base.clone(), |path, part| path.join(part))
            .display()
            .to_string()
            + "\n"
    };
    let file = under(&["a", "b", "f"]);
    let missing_leaf = under(&["a", "b", "missing"]);
    let nothing = under(&["x", "z"]);
    let cases: &[(&[&str], i32, String)] = &[
        (&["a/b/f"], 0, file.clone()),
        (&["-e", "a/b/f"], 0, file),
        (&["a/b/missing"], 0, missing_leaf.clone()),
        (&["-m", "a/b/missing"], 0, missing_leaf),
        (&["-e", "a/b/missing"], 1, String::new()),
        (&["-m", "x/y/../z"], 0, nothing),
        (&["x/y/z"], 1, String::new()),
        (&["-e", "-m", "a"], 2, String::new()),
        (&["-q", "a"], 2, String::new()),
        (&[], 2, String::new()),
    ];
    for (args, code, stdout) in cases {
        let (got, out, _) = run(realpath, dir.path(), &[], args);
        assert_eq!(
            (got, out.as_str()),
            (*code, stdout.as_str()),
            "args {args:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn realpath_follows_symbolic_links() {
    let dir = tempdir().unwrap();
    let base = canonical(dir.path());
    fs::create_dir_all(dir.path().join("real")).unwrap();
    fs::write(dir.path().join("real/f"), "").unwrap();
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
    let (code, out, _) = run(realpath, dir.path(), &[], &["link/f"]);
    assert_eq!(code, 0);
    assert_eq!(out, base.join("real/f").display().to_string() + "\n");
}

/// The path `mktemp` printed (its trailing newline removed).
fn created_name(out: &str) -> PathBuf {
    PathBuf::from(out.trim_end_matches('\n'))
}

#[test]
fn mktemp_creates_unique_files_named_from_the_template() {
    let dir = tempdir().unwrap();
    let (code, first, _) = run(mktemp, dir.path(), &[], &["data-XXXX"]);
    assert_eq!(code, 0);
    let (_, second, _) = run(mktemp, dir.path(), &[], &["data-XXXX"]);
    let (first, second) = (created_name(&first), created_name(&second));
    assert_ne!(first, second);
    assert!(first.is_file() && second.is_file());
    let name = first.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("data-") && name.len() == "data-".len() + 4,
        "{name}"
    );
}

#[test]
fn mktemp_places_and_shapes_names_by_options() {
    let dir = tempdir().unwrap();
    let base = canonical(dir.path());
    let temp = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("sub")).unwrap();
    let env = env_of(&[("TMPDIR", &temp.path().display().to_string())]);
    // (args, expected parent directory, expected file-name prefix, expected directory?)
    let cases: &[(&[&str], PathBuf, &str, bool)] = &[
        (&[], canonical(temp.path()), "tmp.", false),
        (&["-d"], canonical(temp.path()), "tmp.", true),
        (&["-p", "sub", "x.XXX"], base.join("sub"), "x.", false),
        (&["sub/t.XXX"], base.join("sub"), "t.", false),
        (
            &["-d", "-p", "sub", "dir-XXXXXX"],
            base.join("sub"),
            "dir-",
            true,
        ),
    ];
    for (args, parent, prefix, directory) in cases {
        let (code, out, err) = run(mktemp, dir.path(), &env, args);
        assert_eq!(code, 0, "args {args:?}: {err}");
        let made = created_name(&out);
        assert_eq!(
            made.parent().map(canonical),
            Some(parent.clone()),
            "args {args:?}"
        );
        let name = made.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(prefix), "args {args:?}: {name}");
        assert_eq!(made.is_dir(), *directory, "args {args:?}");
        assert_eq!(made.is_file(), !*directory, "args {args:?}");
    }
}

#[test]
fn mktemp_rejects_bad_templates_and_options() {
    let dir = tempdir().unwrap();
    // (args, exit code)
    let cases: &[(&[&str], i32)] = &[
        (&["name"], 1),
        (&["a.XX"], 1),
        (&["-p", "sub", "sub/x.XXX"], 1),
        (&["-q"], 2),
        (&["-p"], 2),
        (&["one.XXX", "two.XXX"], 2),
    ];
    for (args, expected) in cases {
        let (code, out, _) = run(mktemp, dir.path(), &[], args);
        assert_eq!((code, out.as_str()), (*expected, ""), "args {args:?}");
    }
}
