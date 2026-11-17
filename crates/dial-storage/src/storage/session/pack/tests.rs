use super::*;
use crate::storage::session::{Resumed, SessionMeta};
use serde_json::json;
use std::io::Read;

struct Env {
    _home: tempfile::TempDir,
    project: tempfile::TempDir,
    layout: Layout,
}

fn env() -> Env {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    Env {
        _home: home,
        project,
        layout,
    }
}

/// A second project under the same user directory, as two projects of one user always share.
fn env_sharing(of: &Env) -> Env {
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), of.layout.user_dir.clone()).unwrap();
    Env {
        _home: tempfile::tempdir().unwrap(),
        project,
        layout,
    }
}

fn session_with_artifact(e: &Env) -> (Session, String) {
    let mut session = Session::create(&e.layout, SessionMeta { model: "m".into() }).unwrap();
    session
        .append(json!({"role": "user", "content": "hello"}))
        .unwrap();
    let path = session.path().unwrap().to_path_buf();
    let uri = ArtifactStore::new(&artifact_root(&path).unwrap())
        .unwrap()
        .retain("blob payload")
        .unwrap();
    session
        .append(json!({"role": "user", "content": format!("see {uri}")}))
        .unwrap();
    (session, uri)
}

fn evidence_chain(store: &ArtifactStore) -> (String, Vec<String>) {
    use sha2::{Digest, Sha256};
    let mut parents = Vec::new();
    let mut dependencies = Vec::new();
    for bytes in [b"parent bytes".as_slice(), b"child bytes".as_slice()] {
        let evidence = serde_json::from_value(json!({
            "source":"file:///not-fetched", "location":{"kind":"whole"},
            "method":"read", "producer":"pack-test", "kind":"observation",
            "summary":"retained", "representation":{"media_type":"text/plain",
            "sha256":format!("{:x}", Sha256::digest(bytes))}, "limitations":[],
            "context":{"parents":parents}
        }))
        .unwrap();
        let citation = store.retain_evidence(&evidence, bytes).unwrap();
        dependencies.push(store.evidence_record(&citation).unwrap().representation_uri);
        dependencies.push(citation.clone());
        parents = vec![citation];
    }
    (parents.remove(0), dependencies)
}

#[test]
fn manifest_only_journal_packs_and_resumes_transitive_evidence() {
    let src = env();
    let mut session = Session::create(&src.layout, SessionMeta { model: "m".into() }).unwrap();
    let path = session.path().unwrap().to_owned();
    let store = ArtifactStore::new(&artifact_root(&path).unwrap()).unwrap();
    let (citation, dependencies) = evidence_chain(&store);
    session
        .append(json!({"role":"user", "content":citation}))
        .unwrap();
    let archive = src.project.path().join("evidence.zip");
    assert_eq!(
        pack_session(&src.layout, &path, &archive)
            .unwrap()
            .artifacts,
        4
    );
    let dst = env();
    let unpacked = unpack_session(&dst.layout, &archive).unwrap();
    let imported = ArtifactStore::new(&artifact_root(&unpacked.session_path).unwrap()).unwrap();
    for uri in dependencies {
        assert_eq!(
            fs::read(imported.blob_path(&uri).unwrap()).unwrap(),
            fs::read(store.blob_path(&uri).unwrap()).unwrap()
        );
    }
    assert!(Session::open(&unpacked.session_path).is_ok());
}

#[test]
fn missing_transitive_representation_refuses_pack() {
    let src = env();
    let mut session = Session::create(&src.layout, SessionMeta { model: "m".into() }).unwrap();
    let path = session.path().unwrap().to_owned();
    let store = ArtifactStore::new(&artifact_root(&path).unwrap()).unwrap();
    let (citation, dependencies) = evidence_chain(&store);
    fs::remove_file(store.blob_path(&dependencies[0]).unwrap()).unwrap();
    session
        .append(json!({"role":"user", "content":citation}))
        .unwrap();
    assert!(pack_session(&src.layout, &path, &src.project.path().join("missing.zip")).is_err());
}

fn write_zip(path: &Path, entries: &[(String, Vec<u8>, Option<u32>)]) {
    let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
    for (name, bytes, mode) in entries {
        let mut options = zip::write::SimpleFileOptions::default();
        if let Some(mode) = mode {
            // `unix_permissions` keeps only 0o777; the file-type bits live in the high 16 bits.
            options = options.external_attributes(*mode << 16);
        }
        zip.start_file(name.as_str(), options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

/// Copies `src` to `dst`, replacing the manifest with `edit`ed JSON. Every other member is verbatim.
fn rewrite_manifest(src: &Path, dst: &Path, edit: impl Fn(&mut Value)) {
    let mut input = zip::ZipArchive::new(File::open(src).unwrap()).unwrap();
    let mut entries = Vec::new();
    for index in 0..input.len() {
        let mut entry = input.by_index(index).unwrap();
        let name = entry.name().unwrap().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        if name == MANIFEST {
            let mut manifest: Value = serde_json::from_slice(&bytes).unwrap();
            edit(&mut manifest);
            bytes = serde_json::to_vec(&manifest).unwrap();
        }
        entries.push((name, bytes, None));
    }
    write_zip(dst, &entries);
}

fn assert_nothing_installed(layout: &Layout) {
    let dir = layout.sessions_dir();
    let empty = !dir.exists() || fs::read_dir(&dir).unwrap().next().is_none();
    assert!(empty, "a refused archive must install nothing");
}

#[test]
fn round_trip_is_byte_identical_for_journal_blobs_and_transcripts() {
    let src = env();
    let (session, uri) = session_with_artifact(&src);
    let id = session.id().to_owned();
    let path = session.path().unwrap().to_path_buf();
    let blob_src = ArtifactStore::new(&artifact_root(&path).unwrap())
        .unwrap()
        .blob_path(&uri)
        .unwrap();
    let transcript_dir = subagent::transcript_dir(&src.layout, &id);
    fs::create_dir_all(&transcript_dir).unwrap();
    fs::write(
        transcript_dir.join("job-1-1.jsonl"),
        b"{\"type\":\"header\"}\n",
    )
    .unwrap();

    let archive = src.project.path().join("out.zip");
    let packed = pack_session(&src.layout, &path, &archive).unwrap();
    assert_eq!(packed.artifacts, 1);
    assert_eq!(packed.subagents, 1);

    let dst = env();
    let unpacked = unpack_session(&dst.layout, &archive).unwrap();
    assert_eq!(unpacked.session_id, id);
    assert_eq!(unpacked.artifacts, 1);
    assert_eq!(unpacked.subagents, 1);
    assert_eq!(
        fs::read(&unpacked.session_path).unwrap(),
        fs::read(&path).unwrap()
    );
    let blob_dst = ArtifactStore::new(&artifact_root(&unpacked.session_path).unwrap())
        .unwrap()
        .blob_path(&uri)
        .unwrap();
    assert_eq!(fs::read(blob_dst).unwrap(), fs::read(blob_src).unwrap());
    let transcript_dst = subagent::transcript_dir(&dst.layout, &id).join("job-1-1.jsonl");
    assert_eq!(
        fs::read(transcript_dst).unwrap(),
        b"{\"type\":\"header\"}\n"
    );
}

#[test]
fn unpack_refuses_to_overwrite_and_pack_never_replaces_an_archive() {
    let src = env();
    let (session, _uri) = session_with_artifact(&src);
    let path = session.path().unwrap().to_path_buf();
    let archive = src.project.path().join("out.zip");
    pack_session(&src.layout, &path, &archive).unwrap();
    assert!(pack_session(&src.layout, &path, &archive).is_err());

    let dst = env();
    let first = unpack_session(&dst.layout, &archive).unwrap();
    let before = fs::read(&first.session_path).unwrap();
    assert!(unpack_session(&dst.layout, &archive).is_err());
    assert_eq!(fs::read(&first.session_path).unwrap(), before);
}

#[test]
fn unpack_refuses_path_traversal_and_installs_nothing() {
    let dst = env();
    let archive = dst.project.path().join("evil.zip");
    write_zip(
        &archive,
        &[
            ("manifest.json".into(), b"{}".to_vec(), None),
            ("../escape.jsonl".into(), b"x".to_vec(), None),
        ],
    );
    assert!(unpack_session(&dst.layout, &archive).is_err());
    assert_nothing_installed(&dst.layout);
    assert!(!dst.layout.state_dir.join("escape.jsonl").exists());
}

#[test]
fn unpack_refuses_absolute_and_backslash_member_names() {
    let dst = env();
    for name in [
        "/etc/passwd",
        "sub\\..\\escape.jsonl",
        "subagents/../x.jsonl",
    ] {
        let archive = dst.project.path().join("bad.zip");
        write_zip(&archive, &[(name.to_owned(), b"x".to_vec(), None)]);
        assert!(unpack_session(&dst.layout, &archive).is_err(), "{name}");
    }
    assert_nothing_installed(&dst.layout);
}

#[test]
fn unpack_refuses_symlink_members() {
    let dst = env();
    let archive = dst.project.path().join("link.zip");
    write_zip(
        &archive,
        &[(
            "session.jsonl".into(),
            b"/etc/passwd".to_vec(),
            Some(0o120777),
        )],
    );
    let error = unpack_session(&dst.layout, &archive).unwrap_err();
    assert!(format!("{error:#}").contains("symlink"), "{error:#}");
    assert_nothing_installed(&dst.layout);
}

#[test]
fn oversized_members_are_refused_on_pack_and_unpack() {
    let src = env();
    let (session, _uri) = session_with_artifact(&src);
    let path = session.path().unwrap().to_path_buf();
    let archive = src.project.path().join("out.zip");
    let tight = Limits {
        member_bytes: 16,
        ..Limits::default()
    };
    assert!(pack_session_with(&src.layout, &path, &archive, tight).is_err());
    assert!(!archive.exists(), "a refused pack leaves no archive behind");

    pack_session(&src.layout, &path, &archive).unwrap();
    let dst = env();
    assert!(unpack_session_with(&dst.layout, &archive, tight).is_err());
    assert_nothing_installed(&dst.layout);

    let total_too_small = Limits {
        total_bytes: 16,
        ..Limits::default()
    };
    assert!(unpack_session_with(&dst.layout, &archive, total_too_small).is_err());
    assert_nothing_installed(&dst.layout);
}

#[test]
fn newer_archive_format_and_schema_are_refused() {
    let src = env();
    let (session, _uri) = session_with_artifact(&src);
    let path = session.path().unwrap().to_path_buf();
    let archive = src.project.path().join("out.zip");
    pack_session(&src.layout, &path, &archive).unwrap();

    let newer_format = src.project.path().join("newer-format.zip");
    rewrite_manifest(&archive, &newer_format, |m| {
        m["version"] = json!(PACK_VERSION + 1);
    });
    let dst = env();
    let error = unpack_session(&dst.layout, &newer_format).unwrap_err();
    assert!(format!("{error:#}").contains("newer"), "{error:#}");

    let newer_schema = src.project.path().join("newer-schema.zip");
    rewrite_manifest(&archive, &newer_schema, |m| {
        m["schema_version"] = json!(CURRENT_SCHEMA_VERSION + 1);
    });
    let error = unpack_session(&dst.layout, &newer_schema).unwrap_err();
    assert!(format!("{error:#}").contains("newer"), "{error:#}");
    assert_nothing_installed(&dst.layout);
}

#[test]
fn corrupt_member_bytes_fail_manifest_verification() {
    let src = env();
    let (session, _uri) = session_with_artifact(&src);
    let path = session.path().unwrap().to_path_buf();
    let archive = src.project.path().join("out.zip");
    pack_session(&src.layout, &path, &archive).unwrap();

    let tampered = src.project.path().join("tampered.zip");
    let mut input = zip::ZipArchive::new(File::open(&archive).unwrap()).unwrap();
    let mut entries = Vec::new();
    for index in 0..input.len() {
        let mut entry = input.by_index(index).unwrap();
        let name = entry.name().unwrap().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        if name == SESSION_MEMBER {
            bytes.extend_from_slice(b"\n{\"type\":\"message\"}\n");
        }
        entries.push((name, bytes, None));
    }
    write_zip(&tampered, &entries);
    let dst = env();
    assert!(unpack_session(&dst.layout, &tampered).is_err());
    assert_nothing_installed(&dst.layout);
}

#[test]
fn stale_moved_away_source_forks_a_sibling_on_open() {
    let src = env();
    let mut session = Session::create(&src.layout, SessionMeta { model: "m".into() }).unwrap();
    session
        .append(json!({"role": "user", "content": "before the move"}))
        .unwrap();
    let source_path = session.path().unwrap().to_path_buf();
    let original_id = session.id().to_owned();
    let stale_bytes = fs::read(&source_path).unwrap();

    let target = env_sharing(&src);
    let moved = session.relocate_to(&target.layout).unwrap();
    assert_eq!(moved.id(), original_id);
    drop(moved);
    drop(session);

    // A stale copy reappears where the journal used to live (e.g. restored from a backup).
    fs::write(&source_path, &stale_bytes).unwrap();
    match Session::open_or_fork(&src.layout, &source_path).unwrap() {
        Resumed::Forked { source_id, session } => {
            assert_eq!(source_id, original_id);
            assert_ne!(session.id(), original_id);
        }
        Resumed::Owned(_) => panic!("a moved-away source must fork, not resume in place"),
    }
}

#[test]
fn unpacked_source_path_is_recorded_so_the_original_forks() {
    let src = env();
    let (session, _uri) = session_with_artifact(&src);
    let source_path = session.path().unwrap().to_path_buf();
    let original_id = session.id().to_owned();
    let archive = src.project.path().join("out.zip");
    pack_session(&src.layout, &source_path, &archive).unwrap();
    drop(session);

    // The source still exists here; unpacking into another project moves the session identity.
    let dst = env_sharing(&src);
    let unpacked = unpack_session(&dst.layout, &archive).unwrap();
    assert_eq!(unpacked.session_id, original_id);
    match Session::open_or_fork(&src.layout, &source_path).unwrap() {
        Resumed::Forked { source_id, .. } => assert_eq!(source_id, original_id),
        Resumed::Owned(_) => panic!("the stale source must fork after an unpack elsewhere"),
    }
}

#[test]
fn orphan_companions_refuse_unpack_even_when_absent_from_archive() {
    let src = env();
    let session = Session::create(&src.layout, SessionMeta { model: "m".into() }).unwrap();
    let archive = src.project.path().join("orphan.zip");
    pack_session(&src.layout, session.path().unwrap(), &archive).unwrap();
    for kind in ["side", "stream", "workers"] {
        let dst = env();
        fs::create_dir_all(dst.layout.sessions_dir()).unwrap();
        let target = dst
            .layout
            .sessions_dir()
            .join(session.path().unwrap().file_name().unwrap());
        let companion = match kind {
            "stream" => crate::runtime::stream_journal::path(&target),
            "workers" => {
                let dir = subagent::transcript_dir(&dst.layout, session.id());
                fs::create_dir_all(&dir).unwrap();
                dir.join("job-1-1.jsonl")
            }
            _ => side_thread::path_for(&target),
        };
        fs::write(&companion, b"existing orphan").unwrap();
        assert!(unpack_session(&dst.layout, &archive).is_err());
        assert!(!target.exists());
        assert_eq!(fs::read(companion).unwrap(), b"existing orphan");
    }
}

#[test]
fn worker_only_artifacts_travel_with_transcripts() {
    let src = env();
    let session = Session::create(&src.layout, SessionMeta { model: "m".into() }).unwrap();
    let store = ArtifactStore::new(&artifact_root(session.path().unwrap()).unwrap()).unwrap();
    let uri = store.retain("worker-only payload").unwrap();
    let transcripts = subagent::transcript_dir(&src.layout, session.id());
    fs::create_dir_all(&transcripts).unwrap();
    fs::write(
        transcripts.join("job-1-1.jsonl"),
        format!("{}\n", json!({"content": uri})),
    )
    .unwrap();
    let archive = src.project.path().join("worker.zip");
    let packed = pack_session(&src.layout, session.path().unwrap(), &archive).unwrap();
    assert_eq!(packed.artifacts, 1);
    let dst = env();
    let unpacked = unpack_session(&dst.layout, &archive).unwrap();
    let imported = ArtifactStore::new(&artifact_root(&unpacked.session_path).unwrap()).unwrap();
    assert_eq!(
        fs::read(imported.blob_path(&uri).unwrap()).unwrap(),
        b"worker-only payload"
    );
}

#[test]
fn ledger_failure_only_cleans_up_definitely_unpublished_unpack() {
    for published in [false, true] {
        let src = env();
        let session = Session::create(&src.layout, SessionMeta { model: "m".into() }).unwrap();
        let archive = src.project.path().join("ledger.zip");
        let source = session.path().unwrap().to_path_buf();
        let transcripts = subagent::transcript_dir(&src.layout, session.id());
        fs::create_dir_all(&transcripts).unwrap();
        fs::write(transcripts.join("job-1-1.jsonl"), b"{}\n").unwrap();
        pack_session(&src.layout, &source, &archive).unwrap();
        let dst = env();
        if !published {
            crate::platform::atomic::private_dir(&dst.layout.user_dir).unwrap();
            fs::create_dir(crate::config::paths::session_transfers_file(
                &dst.layout.user_dir,
            ))
            .unwrap();
        }
        let error = unpack_with_record(
            &dst.layout,
            &archive,
            Limits::default(),
            |layout, id, from, to| {
                transfers::record(layout, id, from, to)?;
                anyhow::bail!("injected ledger directory sync failure");
            },
        )
        .unwrap_err();
        let target = dst.layout.sessions_dir().join(source.file_name().unwrap());
        assert_eq!(target.exists(), published, "{error:#}");
        assert!(source.exists());
        if published {
            assert!(format!("{error:#}").contains("no rollback claimed"));
            assert_eq!(fs::read(target).unwrap(), fs::read(source).unwrap());
        } else {
            assert!(!subagent::transcript_dir(&dst.layout, session.id()).exists());
            fs::remove_dir(crate::config::paths::session_transfers_file(
                &dst.layout.user_dir,
            ))
            .unwrap();
            assert_eq!(unpack_session(&dst.layout, &archive).unwrap().subagents, 1);
        }
    }
}

/// Remove a blob and all declarations of it, keeping the remaining manifest self-consistent.
fn omit_archive_artifact(src: &Path, dst: &Path, omitted: &str) {
    let member = format!(
        "{ARTIFACT_PREFIX}{}",
        crate::storage::artifact::artifact_id(omitted).unwrap()
    );
    let mut input = zip::ZipArchive::new(File::open(src).unwrap()).unwrap();
    let mut entries = Vec::new();
    for index in 0..input.len() {
        let mut entry = input.by_index(index).unwrap();
        let name = entry.name().unwrap().into_owned();
        if name == member {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        if name == MANIFEST {
            let mut manifest: Manifest = serde_json::from_slice(&bytes).unwrap();
            manifest.members.retain(|entry| entry.name != member);
            manifest.artifacts.retain(|uri| uri != omitted);
            bytes = serde_json::to_vec(&manifest).unwrap();
        }
        entries.push((name, bytes, None));
    }
    write_zip(dst, &entries);
}

#[test]
fn unpack_requires_direct_and_nested_artifacts_from_every_transcript_carrier() {
    for carrier in ["session", "worker", "stream"] {
        let src = env();
        let mut session = Session::create(&src.layout, SessionMeta { model: "m".into() }).unwrap();
        let path = session.path().unwrap().to_owned();
        let store = ArtifactStore::new(&artifact_root(&path).unwrap()).unwrap();
        let (citation, dependencies) = evidence_chain(&store);
        match carrier {
            "session" => {
                session
                    .append(json!({"role":"user", "content":citation}))
                    .unwrap();
            }
            "worker" => {
                let dir = subagent::transcript_dir(&src.layout, session.id());
                fs::create_dir_all(&dir).unwrap();
                fs::write(
                    dir.join("job-1-1.jsonl"),
                    format!("{}\n", json!({"content":citation})),
                )
                .unwrap();
            }
            "stream" => {
                let tree = crate::runtime::Tree::default();
                tree.journal(crate::runtime::stream_journal::Journal::open(&path).unwrap());
                let worker = tree.start("worker", "artifact dependency", None);
                // Replay must join append frames before classifying the reference.
                let (head, tail) = citation.split_at(citation.len() / 2);
                worker.append(head);
                worker.append(tail);
            }
            _ => unreachable!(),
        }
        let archive = src.project.path().join("complete.zip");
        assert_eq!(
            pack_session(&src.layout, &path, &archive)
                .unwrap()
                .artifacts,
            4
        );
        let complete = env();
        assert_eq!(
            unpack_session(&complete.layout, &archive)
                .unwrap()
                .artifacts,
            4
        );
        for omitted in [&citation, &dependencies[0]] {
            let tampered = src.project.path().join("omitted.zip");
            omit_archive_artifact(&archive, &tampered, omitted);
            let dst = env();
            // Existing destination cache entries must not conceal an incomplete portable archive.
            let cache_root = artifact_root(&dst.layout.sessions_dir().join("check.jsonl")).unwrap();
            let cache = ArtifactStore::new(&cache_root).unwrap();
            store.transfer_to(&cache, omitted).unwrap();
            let error = unpack_session(&dst.layout, &tampered).unwrap_err();
            let diagnostic = format!("{error:#}");
            assert!(
                diagnostic.contains("missing required artifact"),
                "{carrier}: {diagnostic}"
            );
            assert!(diagnostic.contains(omitted), "{carrier}: {diagnostic}");
            assert_nothing_installed(&dst.layout);
            assert!(!subagent::transcript_dir(&dst.layout, session.id()).exists());
            assert!(!crate::config::paths::session_transfers_file(&dst.layout.user_dir).exists());
        }
    }
}
