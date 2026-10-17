use super::*;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn exit_codes_survive_artifact_footers() {
    assert_eq!(
        exit_code("ok\n[exit code: 0]\n[complete output: artifact://abc]\n"),
        Some(0)
    );
    assert_eq!(exit_code("failed\n[exit code: 3]\n"), Some(3));
    assert_eq!(exit_code("[exit code: nope]\n"), None);
}

static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("focused-check-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn routes_and_deduplicates_supported_languages_in_stable_order() {
    let root = TempDir::new();
    fs::write(root.0.join("Cargo.toml"), "").unwrap();
    fs::create_dir_all(root.0.join("scripts")).unwrap();
    fs::write(root.0.join("scripts/check.py"), "value = 1\n").unwrap();
    let changed = [
        PathBuf::from("src/main.rs"),
        PathBuf::from("scripts/check.py"),
        PathBuf::from("Cargo.toml"),
        PathBuf::from("scripts/check.py"),
        PathBuf::from("notes.txt"),
        PathBuf::from("config/custom.toml"),
        PathBuf::from("lib/module.rs"),
    ];

    let checks = plan(&root.0, Path::new("/usr/bin/python3"), &changed);

    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0].name, "cargo check");
    assert_eq!(
        checks[0].command,
        "cargo check --offline --all-targets --quiet"
    );
    assert_eq!(checks[0].timeout_secs, 0);
    assert_eq!(checks[1].name, "python AST: scripts/check.py");
    assert!(checks[1].command.contains("'scripts/check.py'"));
    assert_eq!(checks[1].timeout_secs, 0);
}

#[test]
fn renamed_or_deleted_python_files_get_no_impossible_syntax_check() {
    let root = TempDir::new();
    fs::create_dir_all(root.0.join("pkg")).unwrap();
    fs::write(root.0.join("pkg/new_name.py"), "value = 1\n").unwrap();
    let changed = [
        PathBuf::from("pkg/old_name.py"),
        PathBuf::from("pkg/new_name.py"),
        PathBuf::from("pkg/deleted.py"),
    ];
    let checks = plan(&root.0, Path::new("/usr/bin/python3"), &changed);
    // Only the destination of the rename is parsed; the removed paths are never read.
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].name, "python AST: pkg/new_name.py");
    assert!(checks[0].command.contains("'pkg/new_name.py'"));
    assert!(obsolete_check(
        &root.0,
        &FocusedCheck {
            name: "python AST: pkg/old_name.py".into(),
            command: String::new(),
            timeout_secs: 0,
        }
    ));
    assert!(!obsolete_check(&root.0, &checks[0]));
}

#[test]
fn shell_quotes_executable_and_path_with_spaces_and_single_quotes() {
    let root = TempDir::new();
    fs::create_dir_all(root.0.join("dir with space")).unwrap();
    fs::write(root.0.join("dir with space/a'b.py"), "value = 1\n").unwrap();
    let changed = [PathBuf::from("dir with space/a'b.py")];

    #[cfg(unix)]
    let python = Path::new("/opt/python's bin/python");
    #[cfg(windows)]
    let python = Path::new(r"C:\opt\python's bin\python.exe");
    let checks = plan(&root.0, python, &changed);
    assert_eq!(checks.len(), 1);
    #[cfg(unix)]
    assert_eq!(
        checks[0].command,
        "'/opt/python'\"'\"'s bin/python' '-I' '-c' 'import ast, pathlib, sys; p = sys.argv[1]; ast.parse(pathlib.Path(p).read_text(encoding='\"'\"'utf-8'\"'\"'), filename=p)' 'dir with space/a'\"'\"'b.py'"
    );
    #[cfg(windows)]
    assert_eq!(
        checks[0].command,
        "& 'C:\\opt\\python''s bin\\python.exe' '-I' '-c' 'import ast, pathlib, sys; p = sys.argv[1]; ast.parse(pathlib.Path(p).read_text(encoding=''utf-8''), filename=p)' 'dir with space\\a''b.py'"
    );
}

#[test]
fn unrelated_paths_produce_no_checks() {
    let root = TempDir::new();
    let changed = [PathBuf::from("README.md"), PathBuf::from("image.png")];

    assert!(plan(&root.0, Path::new("python3"), &changed).is_empty());
}
