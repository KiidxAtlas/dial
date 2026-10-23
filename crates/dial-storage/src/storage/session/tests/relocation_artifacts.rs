use super::*;
use crate::storage::artifact::{ArtifactStore, uris};
use sha2::{Digest, Sha256};

fn target(e: &Env, project: &Path) -> Layout {
    Layout::with_user_dir(project, e._home.path().join("dial")).unwrap()
}

fn feedback(store: &Path, uri: &str, text: &str) {
    let id = uri.strip_prefix("artifact://").unwrap();
    let dir = store.parent().unwrap().join("annotations").join(id);
    crate::platform::atomic::private_dir(&dir).unwrap();
    let bytes = serde_json::to_vec(&json!({"schema_version":1,"artifact":uri,"sha256":id,
        "start":null,"end":null,"note":text,"created_at_unix_ms":1,"origin":"human_cli"}))
    .unwrap();
    crate::platform::atomic::write_private(
        &dir.join(format!("{:x}.json", Sha256::digest(&bytes))),
        &bytes,
    )
    .unwrap();
}

#[test]
fn moved_text_artifacts_and_feedback_survive_source_retirement_and_reopen() {
    let e = env();
    let mut session = new_session(&e);
    let root = artifact_root(session.path().unwrap()).unwrap();
    let store = ArtifactStore::new(&root).unwrap();
    let uri = store.retain("retained tool output α").unwrap();
    let unrelated = store.retain("private unrelated output").unwrap();
    feedback(&root, &uri, "source feedback");
    session.append(assistant(&format!("output {uri}"))).unwrap();
    session.append(user(&"x".repeat(MAX_LINE * 2))).unwrap();
    let oversized = uris(&session.messages().last().unwrap().to_string())[0].to_owned();
    let project = tempfile::tempdir().unwrap();
    let target = target(&e, project.path());
    let target_root = artifact_root(&target.sessions_dir().join("placeholder.jsonl")).unwrap();
    let destination = ArtifactStore::new(&target_root).unwrap();
    destination.retain("retained tool output α").unwrap();
    feedback(&target_root, &uri, "existing target feedback");
    let moved = session.relocate_to(&target).unwrap();
    let path = moved.path().unwrap().to_owned();
    session.remove_file().unwrap();
    fs::remove_dir_all(&root).unwrap();
    drop(moved);
    let reopened = Session::open(&path).unwrap();
    assert!(!reopened.messages().is_empty());
    assert!(
        destination
            .call(&json!({"uri":uri}))
            .text
            .contains("retained tool output α")
    );
    assert!(
        !destination
            .call(&json!({"uri":oversized,"offset":0,"bytes":32}))
            .is_error
    );
    assert_eq!(destination.info(&uri).unwrap()["state"], "retained");
    let notes = destination.call(&json!({"uri":uri,"annotations":true}));
    assert!(!notes.is_error, "{}", notes.text);
    assert_eq!(notes.data.unwrap()["total"], 2);
    assert!(destination.retrieve(&unrelated, 1, 1).is_err());
}

#[test]
fn missing_reference_refuses_move_and_preserves_source() {
    let e = env();
    let mut session = new_session(&e);
    let uri = format!("artifact://{}", "a".repeat(64));
    session
        .append(json!({"role":"tool","content":"retained output is absent",
        "dial_evidence":{"artifact":uri}}))
        .unwrap();
    let src = session.path().unwrap().to_owned();
    let before = fs::read(&src).unwrap();
    let project = tempfile::tempdir().unwrap();
    let target = target(&e, project.path());
    let error = session.relocate_to(&target).err().unwrap();
    assert!(format!("{error:#}").contains("unavailable in this project's retained store"));
    assert_eq!(fs::read(&src).unwrap(), before);
    assert!(
        !target
            .sessions_dir()
            .join(src.file_name().unwrap())
            .exists()
    );
}

#[test]
fn failed_reopen_keeps_source_artifacts_and_metadata() {
    let e = env();
    let mut session = new_session(&e);
    let store = ArtifactStore::new(&artifact_root(session.path().unwrap()).unwrap()).unwrap();
    let uri = store.retain("still available").unwrap();
    session.append(user(&uri)).unwrap();
    let before = store.info(&uri).unwrap();
    let project = tempfile::tempdir().unwrap();
    let target = target(&e, project.path());
    assert!(
        session
            .relocate_with(&target, |_| anyhow::bail!("injected reopen failure"))
            .is_err()
    );
    assert!(session.path().unwrap().exists());
    assert!(
        store
            .retrieve(&uri, 1, 1)
            .unwrap()
            .contains("still available")
    );
    assert_eq!(store.info(&uri).unwrap(), before);
}

#[test]
fn reference_classification_ignores_prose_examples_and_nonexact_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let uri = format!("artifact://{}", "b".repeat(64));
    let mut references = HashSet::new();
    crate::storage::artifact::transfer::collect_references(
        &json!({"content":format!("For example `{uri}`; not a reference: {uri}f")}),
        dir.path(),
        &mut references,
    );
    assert!(references.is_empty());
    crate::storage::artifact::transfer::collect_references(
        &json!({"dial_evidence":{"artifact":uri}}),
        dir.path(),
        &mut references,
    );
    assert!(references.contains(&uri));
}

#[test]
fn stream_only_artifact_references_move_with_the_observation() {
    let e = env();
    let session = new_session(&e);
    let store = ArtifactStore::new(&artifact_root(session.path().unwrap()).unwrap()).unwrap();
    let uri = store.retain("stream-only output").unwrap();
    let tree = session.runtime();
    let observation = tree.start("assistant", "stream", None);
    observation.append(&uri[..30]);
    observation.append(&uri[30..]);
    let project = tempfile::tempdir().unwrap();
    let target = target(&e, project.path());
    let moved = session.relocate_to(&target).unwrap();
    let destination = ArtifactStore::new(&artifact_root(moved.path().unwrap()).unwrap()).unwrap();
    assert!(
        destination
            .retrieve(&uri, 1, 1)
            .unwrap()
            .contains("stream-only output")
    );
}

#[test]
fn corrupt_reference_aborts_without_retiring_source() {
    let e = env();
    let mut session = new_session(&e);
    let root = artifact_root(session.path().unwrap()).unwrap();
    let store = ArtifactStore::new(&root).unwrap();
    let uri = store.retain("original").unwrap();
    session.append(user(&uri)).unwrap();
    fs::write(
        root.join(uri.strip_prefix("artifact://").unwrap()),
        "corrupt",
    )
    .unwrap();
    let project = tempfile::tempdir().unwrap();
    let target = target(&e, project.path());
    assert!(
        format!("{:#}", session.relocate_to(&target).err().unwrap()).contains("corrupt content")
    );
    assert!(session.path().unwrap().exists());
    assert!(
        !target
            .sessions_dir()
            .join(session.path().unwrap().file_name().unwrap())
            .exists()
    );
}

#[test]
fn foreign_root_image_and_prose_examples_do_not_block_relocation() {
    let e = env();
    let mut session = new_session(&e);
    let foreign = tempfile::tempdir().unwrap();
    let foreign_store = ArtifactStore::new(foreign.path()).unwrap();
    let uri = foreign_store.retain("foreign image payload").unwrap();
    let snapshot = foreign_store.image_reference(&uri).unwrap();
    let example = format!("artifact://{}", "a".repeat(64));
    session
        .append(
            json!({"role":"user","content":format!("Example `{example}`, nonexact {example}f"),
        "dial_images":[{"sha256":snapshot["sha256"],"snapshot":snapshot}]}),
        )
        .unwrap();
    let project = tempfile::tempdir().unwrap();
    let target = target(&e, project.path());
    let moved = session.relocate_to(&target).unwrap();
    let path = moved.path().unwrap().to_owned();
    session.remove_file().unwrap();
    drop(moved);
    let reopened = Session::open(&path).unwrap();
    assert_eq!(
        reopened.messages()[0]["dial_images"][0]["snapshot"]["root"],
        json!(foreign.path())
    );
    assert!(
        foreign_store
            .retrieve(&uri, 1, 1)
            .unwrap()
            .contains("foreign image payload")
    );
}

#[test]
fn admitted_inactive_and_compacted_rows_keep_their_dependencies() {
    let e = env();
    let session = new_session(&e);
    let root = artifact_root(session.path().unwrap()).unwrap();
    let store = ArtifactStore::new(&root).unwrap();
    let inactive = store.retain("inactive branch output").unwrap();
    let compacted = store.retain("compacted output").unwrap();
    // Extra rows need not be active/reachable to own their retained evidence. The normal
    // session loader may skip them; relocation must inspect every admitted JSONL value.
    let mut file = OpenOptions::new()
        .append(true)
        .open(session.path().unwrap())
        .unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"message","id":"inactive", "parent":"absent",
        "message":{"role":"tool","dial_evidence":{"artifact":inactive}}})
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"compaction","id":"compacted", "parent":"absent",
        "summary":format!("[complete output: {compacted}]"),"prefix":[]})
    )
    .unwrap();
    // A torn tail containing a non-reference example is copied but not interpreted.
    file.write_all(b"{\"content\":\"artifact://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    file.sync_all().unwrap();
    let project = tempfile::tempdir().unwrap();
    let target = target(&e, project.path());
    let moved = session.relocate_to(&target).unwrap();
    let destination = ArtifactStore::new(&artifact_root(moved.path().unwrap()).unwrap()).unwrap();
    assert!(
        destination
            .retrieve(&inactive, 1, 1)
            .unwrap()
            .contains("inactive branch output")
    );
    assert!(
        destination
            .retrieve(&compacted, 1, 1)
            .unwrap()
            .contains("compacted output")
    );
}
