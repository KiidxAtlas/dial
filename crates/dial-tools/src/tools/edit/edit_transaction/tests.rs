use super::*;
use crate::tools::edit::revert::restore;
use crate::tools::{ClientFileIo, Workspace, WriteObserver};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Scripted editor client. `buffer` is the unsaved text `read_text` returns; a successful
/// `write_text` replaces it, as an editor would.
#[derive(Default)]
struct Editor {
    read: bool,
    write: bool,
    fail: bool,
    fail_after_write: bool,
    buffer: Mutex<String>,
    written: Mutex<Vec<(PathBuf, String)>>,
}

impl ClientFileIo for Editor {
    fn reads(&self) -> bool {
        self.read
    }
    fn writes(&self) -> bool {
        self.write
    }
    fn read_text(&self, _path: &Path) -> Result<String, String> {
        Ok(self.buffer.lock().unwrap().clone())
    }
    fn write_text(&self, path: &Path, content: &str) -> Result<(), String> {
        if self.fail {
            return Err("editor refused the write".into());
        }
        self.written
            .lock()
            .unwrap()
            .push((path.to_path_buf(), content.to_owned()));
        *self.buffer.lock().unwrap() = content.to_owned();
        if self.fail_after_write {
            return Err("editor applied the buffer but its acknowledgement was lost".into());
        }
        Ok(())
    }
    fn write_text_confirmed(
        &self,
        path: &Path,
        content: &str,
    ) -> Result<(), crate::tools::ClientWriteError> {
        if self.fail {
            return Err(crate::tools::ClientWriteError::NotWritten(
                "editor refused the write before delivery".into(),
            ));
        }
        self.write_text(path, content)
            .map_err(crate::tools::ClientWriteError::Unknown)
    }
}

#[derive(Default)]
struct Observed(Mutex<Vec<PathBuf>>);

impl WriteObserver for Observed {
    fn before_write(&self, abs: &Path) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(abs.to_path_buf());
        Ok(())
    }
}

fn digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

/// A project whose `a.txt` holds `disk` on disk, with `editor` as the ACP client when given.
fn project(editor: Option<Arc<Editor>>) -> (tempfile::TempDir, Workspace, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "disk\n").unwrap();
    let mut ws = Workspace::new(dir.path()).unwrap();
    if let Some(editor) = editor {
        ws.set_client_files(editor);
    }
    let path = ws.resolve("a.txt").unwrap();
    (dir, ws, path)
}

fn editor(read: bool, write: bool) -> Arc<Editor> {
    Arc::new(Editor {
        read,
        write,
        buffer: Mutex::new("buffer\n".into()),
        ..Default::default()
    })
}

#[test]
fn bridged_reads_serve_the_editor_buffer_only_when_both_directions_are_advertised() {
    let cases = [
        (true, true, "buffer\n"),
        (true, false, "disk\n"),
        (false, true, "disk\n"),
        (false, false, "disk\n"),
    ];
    for (read, write, expected) in cases {
        let (_dir, ws, path) = project(Some(editor(read, write)));
        let original = super::read(&ws, &path).unwrap().unwrap();
        assert_eq!(original.text, expected, "read={read} write={write}");
        let mut streamed = String::new();
        open_text(&ws, &path)
            .unwrap()
            .read_to_string(&mut streamed)
            .unwrap();
        assert_eq!(streamed, expected, "streamed read={read} write={write}");
    }
}

#[test]
fn bridged_write_is_checkpointed_then_published_by_the_client_and_undone_through_it() {
    let editor = editor(true, true);
    let (_dir, ws, path) = project(Some(editor.clone()));
    let observed = Arc::new(Observed::default());
    let ws = ws.with_observer(observed.clone());

    let change = Change::load(&ws, path.clone(), Some("new\n".into())).unwrap();
    commit(&ws, &[change]).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "disk\n");
    assert_eq!(
        *editor.written.lock().unwrap(),
        vec![(path.clone(), "new\n".to_owned())]
    );
    assert_eq!(*observed.0.lock().unwrap(), vec![path.clone()]);

    let undo = restore(
        &ws,
        &path,
        Some(digest("new\n")),
        Some(b"buffer\n".to_vec()),
        "before the edit",
    );
    assert!(!undo.is_error, "{}", undo.text);
    assert_eq!(*editor.buffer.lock().unwrap(), "buffer\n");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "disk\n");
    assert_eq!(observed.0.lock().unwrap().len(), 2);
}

#[test]
fn client_write_error_is_a_tool_error_and_no_local_write_follows() {
    let editor = Arc::new(Editor {
        read: true,
        write: true,
        fail: true,
        buffer: Mutex::new("buffer\n".into()),
        ..Default::default()
    });
    let (_dir, ws, path) = project(Some(editor.clone()));
    let change = Change::load(&ws, path.clone(), Some("new\n".into())).unwrap();
    let output =
        crate::tools::exec::execution::run(&std::sync::atomic::AtomicBool::new(false), || {
            crate::runtime::tool_output::ToolOutput::err(commit(&ws, &[change]).unwrap_err())
        });
    assert_eq!(
        output.execution.effects,
        crate::runtime::execution::Effects::Known
    );
    assert!(output.execution.changed_paths.is_empty());
    assert!(output.text.contains("was not written"), "{}", output.text);
    let error = output.text;
    assert!(
        error.contains("editor refused the write") && error.contains("no local write"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "disk\n");
    assert!(editor.written.lock().unwrap().is_empty());
}

#[test]
fn unadvertised_writes_stay_local() {
    for (read, write) in [(true, false), (false, false)] {
        let editor = editor(read, write);
        let (_dir, ws, path) = project(Some(editor.clone()));
        let change = Change::load(&ws, path.clone(), Some("new\n".into())).unwrap();
        commit(&ws, &[change]).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert!(editor.written.lock().unwrap().is_empty());
    }
}

#[test]
fn only_plain_text_inside_the_roots_is_bridged() {
    let (dir, mut ws, _) = project(None);
    ws.set_client_files(editor(true, true));
    let base = dir.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let cases = [
        (base.join("notes.txt"), true),
        (base.join("pic.png"), false),
        (base.join("book.pdf"), false),
        (base.join("cell.ipynb"), false),
        (
            outside.path().canonicalize().unwrap().join("notes.txt"),
            false,
        ),
    ];
    for (path, bridged) in cases {
        assert_eq!(
            client_target(&ws, &path).is_some(),
            bridged,
            "{}",
            path.display()
        );
    }
}

#[test]
fn mixed_bridged_and_local_batches_are_refused_before_any_write() {
    let editor = editor(true, true);
    let (dir, ws, path) = project(Some(editor.clone()));
    std::fs::write(dir.path().join("pic.png"), b"png").unwrap();
    let pic = ws.resolve("pic.png").unwrap();
    let changes = [
        Change::load(&ws, path.clone(), Some("new\n".into())).unwrap(),
        Change::load(&ws, pic.clone(), Some("changed".into())).unwrap(),
    ];
    let error = commit(&ws, &changes).unwrap_err();
    assert!(error.contains("mixes"), "{error}");
    assert!(editor.written.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "disk\n");
    assert_eq!(std::fs::read(&pic).unwrap(), b"png");
}

#[test]
fn bridged_files_cannot_be_deleted_locally() {
    let editor = editor(true, true);
    let (_dir, ws, path) = project(Some(editor.clone()));
    let change = Change::load(&ws, path.clone(), None).unwrap();
    let error = commit(&ws, &[change]).unwrap_err();
    assert!(error.contains("cannot be deleted"), "{error}");
    assert!(path.exists());
    assert!(editor.written.lock().unwrap().is_empty());
}

#[test]
fn already_applied_buffer_edit_never_notifies_the_observer_or_editor_writer() {
    let editor = editor(true, true);
    let (_dir, ws, path) = project(Some(editor.clone()));
    let observed = Arc::new(Observed::default());
    let ws = ws
        .with_edit_mode(crate::tools::EditMode::Replace)
        .with_observer(observed.clone());
    ws.store().record_all(&path, "buffer\n");
    let output = crate::tools::edit::editing::edit_output(
        &ws,
        &serde_json::json!({"path":"a.txt", "old_string":"buffer", "new_string":"buffer"}),
    )
    .unwrap();
    assert!(!output.is_error, "{}", output.text);
    assert_eq!(output.data.unwrap()["edit"]["outcome"], "already_applied");
    assert!(editor.written.lock().unwrap().is_empty());
    assert!(observed.0.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "disk\n");
    assert_eq!(*editor.buffer.lock().unwrap(), "buffer\n");
}

#[test]
fn applied_client_write_with_lost_acknowledgement_has_unknown_effects() {
    let editor = Arc::new(Editor {
        read: true,
        write: true,
        fail_after_write: true,
        buffer: Mutex::new("buffer\n".into()),
        ..Default::default()
    });
    let (_dir, ws, path) = project(Some(editor.clone()));
    let change = Change::load(&ws, path, Some("new\n".into())).unwrap();
    let output =
        crate::tools::exec::execution::run(&std::sync::atomic::AtomicBool::new(false), || {
            crate::tools::ToolOutput::err(commit(&ws, &[change]).unwrap_err())
        });
    assert_eq!(*editor.buffer.lock().unwrap(), "new\n");
    assert_eq!(
        output.execution.effects,
        crate::runtime::execution::Effects::Unknown
    );
    assert!(!output.text.contains("was not written"), "{}", output.text);
    assert!(output.text.contains("not confirmed"), "{}", output.text);
}

/// A local multi-target commit replaces, creates and deletes as asked, keeps permissions and a
/// second hard link on the original inode, and leaves no staging or backup entries behind.
#[cfg(unix)]
#[test]
fn local_commit_installs_every_target_and_leaves_no_staging_entries() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, ws, a) = project(None);
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::fs::hard_link(&a, dir.path().join("alias.txt")).unwrap();
    std::fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
    let gone = ws.resolve("gone.txt").unwrap();
    let created = ws.resolve("new.txt").unwrap();
    let changes = [
        Change::load(&ws, a.clone(), Some("edited\n".into())).unwrap(),
        Change::load(&ws, gone.clone(), None).unwrap(),
        Change::load(&ws, created.clone(), Some("fresh\n".into())).unwrap(),
    ];
    commit(&ws, &changes).unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "edited\n");
    assert_eq!(
        std::fs::metadata(&a).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("alias.txt")).unwrap(),
        "disk\n"
    );
    assert!(!gone.exists());
    assert_eq!(std::fs::read_to_string(&created).unwrap(), "fresh\n");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".dial-edit-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
