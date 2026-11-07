use super::*;

#[test]
fn preserved_inputs_are_independent_and_do_not_pollute_changed_files_or_undo() {
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let checkpoints =
        Checkpoints::new(private.path().join("checkpoints"), root.path().to_owned()).unwrap();
    let artifacts =
        crate::storage::artifact::ArtifactStore::new(&private.path().join("artifacts")).unwrap();
    let turn = checkpoints
        .begin_conversation_turn("recover", "session")
        .unwrap();
    let path = root.path().join("main.db-wal");
    fs::write(&path, b"original").unwrap();
    let receipt = checkpoints
        .preserve_input(turn, &path, b"original", &artifacts)
        .unwrap();
    fs::write(&path, b"changed").unwrap();
    assert!(checkpoints.changed_files(turn).is_empty());
    assert!(
        checkpoints.list().is_empty(),
        "preservation is not an undoable edit"
    );
    assert_eq!(
        artifacts.exact_bytes(&receipt.artifact, 1024).unwrap(),
        b"original"
    );
}
