use super::*;
use crossterm::event::{KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn settle(browser: &mut PathBrowser) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while browser.loading {
        browser.poll();
        assert!(
            std::time::Instant::now() < deadline,
            "filesystem worker did not finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn open(kind: PathKind, current: &str, base: &Path) -> PathBrowser {
    let mut browser = PathBrowser::open(kind, current, base);
    settle(&mut browser);
    browser
}

fn press(browser: &mut PathBrowser, code: KeyCode) -> BrowseStep {
    settle(browser);
    let step = browser.key(&key(code), &Keymap::defaults());
    settle(browser);
    if code == KeyCode::Enter && browser.validated.is_some() {
        return browser.key(&key(code), &Keymap::defaults());
    }
    step
}

fn type_text(browser: &mut PathBrowser, text: &str) {
    for c in text.chars() {
        assert_eq!(press(browser, KeyCode::Char(c)), BrowseStep::Stay);
    }
}

fn labels(browser: &PathBrowser) -> Vec<String> {
    browser
        .view(50)
        .rows
        .into_iter()
        .map(|(label, _)| label)
        .collect()
}

fn tree() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("bin")).unwrap();
    std::fs::create_dir_all(root.path().join("lib/deep")).unwrap();
    std::fs::write(root.path().join("bin/tool"), "").unwrap();
    std::fs::write(root.path().join("notes.txt"), "").unwrap();
    std::fs::write(root.path().join(".hidden"), "").unwrap();
    root
}

#[test]
fn folders_come_first_and_hidden_entries_need_a_dot_filter() {
    let root = tree();
    let mut browser = open(PathKind::File, "", root.path());
    assert_eq!(
        labels(&browser),
        ["../ (up one folder)", "bin/", "lib/", "notes.txt"]
    );
    type_text(&mut browser, ".h");
    assert_eq!(labels(&browser), [".hidden"]);
}

#[test]
fn enter_opens_folders_and_chooses_files() {
    let root = tree();
    let mut browser = open(PathKind::File, "", root.path());
    type_text(&mut browser, "bin");
    assert_eq!(press(&mut browser, KeyCode::Enter), BrowseStep::Stay);
    assert_eq!(browser.dir(), root.path().join("bin"));
    // The filter resets on entering a folder; the cursor starts on "../".
    assert_eq!(press(&mut browser, KeyCode::Down), BrowseStep::Stay);
    assert_eq!(
        press(&mut browser, KeyCode::Enter),
        BrowseStep::Chosen(root.path().join("bin/tool"))
    );
}

#[test]
fn opening_at_a_saved_file_selects_it_and_left_goes_up_to_the_folder_it_came_from() {
    let root = tree();
    let saved = root.path().join("bin/tool");
    let mut browser = open(PathKind::Executable, saved.to_str().unwrap(), root.path());
    assert_eq!(browser.dir(), root.path().join("bin"));
    assert_eq!(
        press(&mut browser, KeyCode::Enter),
        BrowseStep::Chosen(saved)
    );
    assert_eq!(press(&mut browser, KeyCode::Left), BrowseStep::Stay);
    assert_eq!(browser.dir(), root.path());
    assert_eq!(
        press(&mut browser, KeyCode::Enter),
        BrowseStep::Stay,
        "the cursor returns to bin/, which Enter reopens"
    );
    assert_eq!(browser.dir(), root.path().join("bin"));
}

#[test]
fn directory_settings_list_only_folders_and_choose_the_shown_folder() {
    let root = tree();
    let mut browser = open(PathKind::Directory, "lib", root.path());
    assert_eq!(browser.dir(), root.path().join("lib"));
    let rows = labels(&browser);
    assert!(rows[0].starts_with("Use this folder: "));
    assert_eq!(&rows[1..], ["../ (up one folder)", "deep/"]);
    assert_eq!(
        press(&mut browser, KeyCode::Enter),
        BrowseStep::Chosen(root.path().join("lib"))
    );
}

#[test]
fn a_typed_path_opens_on_enter_and_unknown_paths_say_so() {
    let root = tree();
    let mut browser = open(PathKind::File, "", root.path());
    type_text(&mut browser, "lib/deep");
    assert_eq!(
        browser.view(10).note.as_deref(),
        Some("Enter checks/opens the typed path")
    );
    assert_eq!(press(&mut browser, KeyCode::Enter), BrowseStep::Stay);
    assert_eq!(browser.dir(), root.path().join("lib/deep"));
    type_text(&mut browser, "../../bin/tool");
    assert_eq!(
        press(&mut browser, KeyCode::Enter),
        BrowseStep::Chosen(root.path().join("lib/deep/../../bin/tool"))
    );
    type_text(&mut browser, "missing/file");
    assert_eq!(press(&mut browser, KeyCode::Enter), BrowseStep::Stay);
    assert!(browser.view(10).note.unwrap().starts_with("Cannot open"));
}

#[test]
fn esc_clears_the_filter_before_cancelling_and_ctrl_e_switches_to_typing() {
    let root = tree();
    let mut browser = open(PathKind::File, "", root.path());
    type_text(&mut browser, "no");
    assert_eq!(press(&mut browser, KeyCode::Esc), BrowseStep::Stay);
    assert_eq!(labels(&browser).len(), 4);
    assert_eq!(press(&mut browser, KeyCode::Esc), BrowseStep::Cancel);
    let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert_eq!(browser.key(&ctrl_e, &Keymap::defaults()), BrowseStep::Type);
}

#[cfg(unix)]
#[test]
fn program_settings_mark_executable_files() {
    use std::os::unix::fs::PermissionsExt;
    let root = tree();
    let tool = root.path().join("bin/tool");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let browser = open(PathKind::Executable, tool.to_str().unwrap(), root.path());
    assert!(labels(&browser).contains(&"tool  (program)".to_owned()));
}

#[test]
fn partial_listing_warning_survives_filtering() {
    let root = tempfile::tempdir().unwrap();
    for n in 0..=MAX_ENTRIES {
        std::fs::write(root.path().join(format!("file-{n}")), "").unwrap();
    }
    // The scan limit counts raw entries, including files omitted from folder-only browsers.
    for kind in [PathKind::File, PathKind::Directory] {
        let mut browser = open(kind, "", root.path());
        assert!(browser.view(10).note.unwrap().contains("first"));
        type_text(&mut browser, "no-match");
        assert!(
            browser.view(10).note.is_some(),
            "partial results must remain disclosed"
        );
        press(&mut browser, KeyCode::Esc);
        assert!(
            browser.view(10).note.is_some(),
            "clearing a filter must not erase listing status"
        );
    }
}

#[cfg(unix)]
#[test]
fn typed_parent_components_follow_symlinks_before_resolving_parent() {
    let root = tree();
    std::fs::create_dir_all(root.path().join("elsewhere/child")).unwrap();
    std::fs::write(root.path().join("elsewhere/target"), "").unwrap();
    std::os::unix::fs::symlink(
        root.path().join("elsewhere/child"),
        root.path().join("link"),
    )
    .unwrap();
    let mut browser = open(PathKind::File, "", root.path());
    type_text(&mut browser, "link/../target");
    let chosen = press(&mut browser, KeyCode::Enter);
    assert!(
        matches!(chosen, BrowseStep::Chosen(ref path) if std::fs::read_to_string(path).is_ok()),
        "{chosen:?}"
    );
}

#[cfg(windows)]
#[test]
fn native_windows_paths_are_paths_not_filters() {
    for path in [
        r"C:\Users\name",
        r"\\server\share\folder",
        r".\folder",
        r"..\folder",
        r"~\folder",
        "C:",
    ] {
        assert!(is_typed_path(path), "{path}");
    }
}

#[test]
fn editing_during_stalled_validation_invalidates_the_pending_choice() {
    let root = tree();
    let mut browser = open(PathKind::File, "", root.path());
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    browser.loader = Some(
        Loader::start(move |request, _| {
            let Request::Validate(path) = request else {
                panic!("unexpected request")
            };
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Reply::Validated(path)
        })
        .unwrap(),
    );
    browser.paste("bin/tool");
    assert_eq!(
        browser.key(&key(KeyCode::Enter), &Keymap::defaults()),
        BrowseStep::Stay
    );
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    // Neither drawing nor editing waits for validation, and edits invalidate its eventual result.
    assert!(browser.view(10).note.unwrap().contains("Checking"));
    browser.paste("-different");
    assert!(!browser.is_loading());
    assert!(browser.validated.is_none());
    release_tx.send(()).unwrap();
    browser.poll();
    assert_eq!(browser.filter.text, "bin/tool-different");
    assert!(browser.validated.is_none());
}
