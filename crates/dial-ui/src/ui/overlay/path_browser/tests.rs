use super::*;
use crossterm::event::{KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(browser: &mut PathBrowser, code: KeyCode) -> BrowseStep {
    browser.key(&key(code), &Keymap::defaults())
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
    let mut browser = PathBrowser::open(PathKind::File, "", root.path());
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
    let mut browser = PathBrowser::open(PathKind::File, "", root.path());
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
    let mut browser = PathBrowser::open(PathKind::Executable, saved.to_str().unwrap(), root.path());
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
    let mut browser = PathBrowser::open(PathKind::Directory, "lib", root.path());
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
    let mut browser = PathBrowser::open(PathKind::File, "", root.path());
    type_text(&mut browser, "lib/deep");
    assert_eq!(
        browser.view(10).note.as_deref(),
        Some("Enter opens the typed path")
    );
    assert_eq!(press(&mut browser, KeyCode::Enter), BrowseStep::Stay);
    assert_eq!(browser.dir(), root.path().join("lib/deep"));
    type_text(&mut browser, "../../bin/tool");
    assert_eq!(
        press(&mut browser, KeyCode::Enter),
        BrowseStep::Chosen(root.path().join("bin/tool"))
    );
    type_text(&mut browser, "missing/file");
    assert_eq!(press(&mut browser, KeyCode::Enter), BrowseStep::Stay);
    assert!(browser.view(10).note.unwrap().ends_with("does not exist"));
}

#[test]
fn esc_clears_the_filter_before_cancelling_and_ctrl_e_switches_to_typing() {
    let root = tree();
    let mut browser = PathBrowser::open(PathKind::File, "", root.path());
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
    let browser = PathBrowser::open(PathKind::Executable, tool.to_str().unwrap(), root.path());
    assert!(labels(&browser).contains(&"tool  (program)".to_owned()));
}
