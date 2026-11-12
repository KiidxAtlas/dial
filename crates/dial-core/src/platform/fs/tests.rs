use super::*;
use std::path::PathBuf;

const PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

fn only(names: &'static [&'static str]) -> impl Fn(&Path) -> bool {
    move |path| names.iter().any(|name| path == Path::new(name))
}

#[test]
fn windows_bare_names_resolve_to_their_pathext_form() {
    let npx =
        executable_candidate_with(Path::new("bin/npx"), Some(PATHEXT), only(&["bin/npx.CMD"]));
    assert_eq!(npx, Some(PathBuf::from("bin/npx.CMD")));
    let analyzer = executable_candidate_with(
        Path::new("bin/rust-analyzer"),
        Some(PATHEXT),
        only(&["bin/rust-analyzer.EXE"]),
    );
    assert_eq!(analyzer, Some(PathBuf::from("bin/rust-analyzer.EXE")));
}

#[test]
fn suffixes_are_appended_not_substituted_and_exact_names_win() {
    let dotted = executable_candidate_with(
        Path::new("python3.12"),
        Some(PATHEXT),
        only(&["python3.EXE", "python3.12.EXE"]),
    );
    assert_eq!(dotted, Some(PathBuf::from("python3.12.EXE")));
    let exact = executable_candidate_with(
        Path::new("tool.exe"),
        Some(PATHEXT),
        only(&["tool.exe", "tool.exe.EXE"]),
    );
    assert_eq!(exact, Some(PathBuf::from("tool.exe")));
}

#[test]
fn without_pathext_only_the_exact_path_counts() {
    assert_eq!(
        executable_candidate_with(Path::new("npx"), None, only(&["npx.CMD"])),
        None
    );
    assert_eq!(
        executable_candidate_with(Path::new("npx"), Some(";;.;"), only(&["npx", "npx."])),
        Some(PathBuf::from("npx"))
    );
}

#[test]
fn find_program_searches_path_entries_in_order() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let name = if cfg!(windows) { "tool.cmd" } else { "tool" };
    for dir in [&first, &second] {
        let file = dir.path().join(name);
        std::fs::write(&file, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let search = std::env::join_paths([second.path(), first.path()]).unwrap();
    let found = find_program(Path::new("tool"), Some(&search)).unwrap();
    assert_eq!(found.parent(), Some(second.path()));
    assert_eq!(find_program(Path::new("missing-tool"), Some(&search)), None);
    assert_eq!(find_program(Path::new(""), Some(&search)), None);
}

#[test]
fn launch_path_keeps_the_name_when_nothing_matches_or_off_windows() {
    let dir = tempfile::tempdir().unwrap();
    let name = if cfg!(windows) { "tool.cmd" } else { "tool" };
    let file = dir.path().join(name);
    std::fs::write(&file, "").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let search = Some(dir.path().as_os_str());
    let launched = launch_path(Path::new("tool"), search);
    if cfg!(windows) {
        // The suffix comes from `PATHEXT` (`.CMD`), and Windows names are case-insensitive, so the
        // resolved path names the same file with the extension's casing.
        assert_eq!(launched.parent(), file.parent());
        assert!(
            launched
                .to_string_lossy()
                .eq_ignore_ascii_case(&file.to_string_lossy()),
            "{launched:?} is not {file:?}"
        );
    } else {
        // Unix `Command` searches PATH itself; the name is passed through.
        assert_eq!(launched, PathBuf::from("tool"));
    }
    assert_eq!(
        launch_path(Path::new("missing"), search),
        PathBuf::from("missing")
    );
}
