use super::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Observed(Mutex<Vec<std::path::PathBuf>>);

impl crate::tools::WriteObserver for Observed {
    fn before_write(&self, abs: &Path) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(abs.to_path_buf());
        Ok(())
    }
}

fn digest(text: &str) -> Option<[u8; 32]> {
    Some(Sha256::digest(text.as_bytes()).into())
}

#[test]
fn restores_through_the_observed_transaction_and_drops_witnesses() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "new\n").unwrap();
    let observed = Arc::new(Observed::default());
    let ws = Workspace::new(dir.path())
        .unwrap()
        .with_observer(observed.clone());
    let path = ws.resolve("a.txt").unwrap();
    ws.store().record(&path, "new\n", &[(1, 1)]);

    let output = restore(
        &ws,
        &path,
        digest("new\n"),
        Some(b"old\n".to_vec()),
        "before its last change",
    );
    assert!(!output.is_error, "{}", output.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
    assert_eq!(output.execution.changed_paths, vec![path.clone()]);
    assert_eq!(*observed.0.lock().unwrap(), vec![path.clone()]);
    // The stale witness is gone; the next edit needs a fresh read.
    assert!(
        ws.store()
            .witness_flags(&path, "new\n")
            .iter()
            .all(|seen| !seen)
    );
}

#[test]
fn refuses_a_file_that_changed_since_planning() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "outside\n").unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("a.txt").unwrap();
    let output = restore(&ws, &path, digest("new\n"), Some(b"old\n".to_vec()), "x");
    assert!(output.is_error);
    assert!(output.text.contains("changed since"), "{}", output.text);
    assert!(output.execution.changed_paths.is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "outside\n");
}

#[test]
fn deletes_a_file_that_did_not_exist_before() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("new.txt"), "created\n").unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("new.txt").unwrap();
    let output = restore(&ws, &path, digest("created\n"), None, "at turn start");
    assert!(!output.is_error, "{}", output.text);
    assert!(!path.exists());
    assert_eq!(output.execution.changed_paths, vec![path]);
}
