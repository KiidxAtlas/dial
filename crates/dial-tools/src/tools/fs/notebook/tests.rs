use super::*;

#[test]
fn a_notebook_mutation_reports_its_committed_path() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let path = dir.path().join("book.ipynb");
    std::fs::write(
        &path,
        json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[]}).to_string(),
    )
    .unwrap();
    let config = Config::default();
    let authorize = crate::tools::testkit::authorize_in_workspace(&ws, "notebook");
    let context = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: Some(&authorize),
        confirm: None,
    };
    let notebook = Notebook::default();
    let read = json!({"action":"read","path":"book.ipynb"});
    assert!(!notebook.may_modify_workspace(&read));
    let listing = notebook.call(&context, &read);
    assert!(!listing.is_error, "{}", listing.text);
    let revision = listing.data.as_ref().unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_string();
    let insert = json!({"action":"insert","path":"book.ipynb","revision":revision,"index":0,"cell_type":"markdown","source":"# Title\n"});
    assert!(notebook.may_modify_workspace(&insert));
    let cancel = AtomicBool::new(false);
    let output = crate::tools::exec::execution::run(&cancel, || notebook.call(&context, &insert));
    assert!(!output.is_error, "{}", output.text);
    assert!(std::fs::read_to_string(&path).unwrap().contains("# Title"));
    // The publish protocol leaves neither the staged file nor the captured original behind.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".dial-notebook-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert_eq!(
        output.execution.changed_paths,
        [std::fs::canonicalize(&path).unwrap()]
    );
}

/// Runs a notebook insert against `book.ipynb` with `hook` installed at the publish seams.
/// Returns the tool output; the hook runs on this thread, between the checks and the rename/link.
#[cfg(unix)]
fn publish_with_hook(
    dir: &tempfile::TempDir,
    ws: &Workspace,
    hook: impl Fn(&str) + 'static,
) -> ToolOutput {
    let config = Config::default();
    let authorize = crate::tools::testkit::authorize_in_workspace(ws, "notebook");
    let context = ToolContext {
        workspace: ws,
        config: &config,
        mutation_authorizer: Some(&authorize),
        confirm: None,
    };
    let notebook = Notebook::default();
    let read = notebook.call(&context, &json!({"action":"read","path":"book.ipynb"}));
    assert!(!read.is_error, "{}", read.text);
    let revision = read.data.as_ref().unwrap()["revision"].as_str().unwrap();
    let insert = json!({"action":"insert","path":"book.ipynb","revision":revision,"index":0,"cell_type":"markdown","source":"# Mine\n"});
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            HOOK.with(|h| *h.borrow_mut() = None);
        }
    }
    let _clear = Clear;
    HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    let cancel = AtomicBool::new(false);
    let _ = dir;
    crate::tools::exec::execution::run(&cancel, || notebook.call(&context, &insert))
}

#[cfg(unix)]
fn seed_notebook(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("book.ipynb");
    std::fs::write(
        &path,
        json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[]}).to_string(),
    )
    .unwrap();
    path
}

fn dial_files(dir: &tempfile::TempDir) -> Vec<String> {
    std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".dial-notebook-"))
        .collect()
}

#[cfg(unix)]
#[test]
fn a_notebook_replaced_by_another_writer_before_publication_is_preserved() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let path = seed_notebook(&dir);
    let theirs = r#"{"nbformat":4,"nbformat_minor":5,"metadata":{"by":"other"},"cells":[]}"#;
    let hook_dir = dir.path().to_path_buf();
    let output = publish_with_hook(&dir, &ws, move |stage| {
        if stage == "before-capture" {
            // Another writer atomically replaces the notebook after it was loaded and verified.
            let tmp = hook_dir.join("other.tmp");
            std::fs::write(&tmp, theirs).unwrap();
            std::fs::rename(&tmp, hook_dir.join("book.ipynb")).unwrap();
        }
    });
    assert!(output.is_error, "{}", output.text);
    assert!(
        output.text.contains("changed during publication"),
        "{}",
        output.text
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
    assert!(dial_files(&dir).is_empty(), "{:?}", dial_files(&dir));
    assert!(output.execution.changed_paths.is_empty());
}

#[cfg(unix)]
#[test]
fn a_file_appearing_at_the_destination_during_publication_is_never_overwritten() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let path = seed_notebook(&dir);
    let original = std::fs::read_to_string(&path).unwrap();
    let theirs = "someone else's file";
    let hook_dir = dir.path().to_path_buf();
    let output = publish_with_hook(&dir, &ws, move |stage| {
        if stage == "before-install" {
            // The original is already renamed aside; a racing writer creates the destination.
            std::fs::write(hook_dir.join("book.ipynb"), theirs).unwrap();
        }
    });
    assert!(output.is_error, "{}", output.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
    // The original cannot be restored over the racing file, so the error names where it is kept.
    let kept = dial_files(&dir);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].starts_with(".dial-notebook-backup-"), "{kept:?}");
    assert!(output.text.contains(&kept[0]), "{}", output.text);
    assert_eq!(
        std::fs::read_to_string(dir.path().join(&kept[0])).unwrap(),
        original
    );
}
