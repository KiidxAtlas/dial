use super::*;
use crate::config::types::Harness;

fn sample_manifest(kind: &str, base: &str, files: Vec<Entry>) -> Manifest {
    Manifest {
        format: FORMAT.into(),
        version: VERSION,
        kind: kind.into(),
        base: base.into(),
        source: json!({}),
        files,
    }
}

fn sample_adapter() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "adapter_config.json",
            br#"{"base_model_name_or_path":"/exporter/model","r":8}"#.to_vec(),
        ),
        ("adapters.safetensors", b"weights".to_vec()),
    ]
}

fn write_sources(root: &Path, files: &[(&str, Vec<u8>)]) -> Vec<(String, PathBuf)> {
    files
        .iter()
        .map(|(name, bytes)| {
            let path = root.join(name);
            fs::write(&path, bytes).unwrap();
            ((*name).to_owned(), path)
        })
        .collect()
}

fn pack_adapter(root: &Path, base: &str, files: &[(&str, Vec<u8>)]) -> PathBuf {
    let sources = write_sources(root, files);
    let manifest = sample_manifest("lora", base, entries(&sources).unwrap());
    let pack = root.join("adapter.dialpack");
    write_pack(&pack, &manifest, &sources).unwrap();
    pack
}

/// Writes member names verbatim into tar headers, bypassing the tar crate's path checks, so
/// the reader's own validation is what gets exercised.
fn raw_pack(out: &Path, manifest: &Manifest, members: &[(&str, &[u8], tar::EntryType)]) {
    let encoder = GzEncoder::new(fs::File::create(out).unwrap(), Compression::default());
    let mut tar = tar::Builder::new(encoder);
    append_bytes(
        &mut tar,
        "manifest.json",
        &serde_json::to_vec(manifest).unwrap(),
    )
    .unwrap();
    for (name, data, kind) in members {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(*kind);
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        tar.append(&header, *data).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
}

#[test]
fn export_import_roundtrip_preserves_verified_bytes() {
    let root = tempfile::tempdir().unwrap();
    let pack = pack_adapter(root.path(), "aaaa", &sample_adapter());
    let (manifest, dir) = open_pack(
        &pack,
        &root.path().join("packs"),
        "lora",
        "aaaa",
        MAX_PACK_BYTES,
    )
    .unwrap();
    assert_eq!(manifest.kind, "lora");
    for (name, bytes) in sample_adapter() {
        assert_eq!(fs::read(dir.path().join(name)).unwrap(), bytes);
    }
}

#[test]
fn tampered_member_fails_checksum() {
    let root = tempfile::tempdir().unwrap();
    let sources = write_sources(root.path(), &sample_adapter());
    let manifest = sample_manifest("lora", "aaaa", entries(&sources).unwrap());
    // Same length, different bytes: only the checksum can catch this.
    fs::write(root.path().join("adapters.safetensors"), b"WEIGHTS").unwrap();
    let pack = root.path().join("tampered.dialpack");
    write_pack(&pack, &manifest, &sources).unwrap();
    let err = open_pack(
        &pack,
        &root.path().join("packs"),
        "lora",
        "aaaa",
        MAX_PACK_BYTES,
    )
    .unwrap_err();
    assert!(err.to_string().contains("failed its checksum"), "{err:#}");
}

#[test]
fn rejects_traversal_absolute_and_nested_members() {
    let root = tempfile::tempdir().unwrap();
    let pack = root.path().join("bad.dialpack");
    for bad in ["files/../evil", "/abs/evil", "files/sub/evil"] {
        raw_pack(
            &pack,
            &sample_manifest("lora", "aaaa", vec![]),
            &[(bad, &b"x"[..], tar::EntryType::Regular)],
        );
        let err = open_pack(
            &pack,
            &root.path().join("packs"),
            "lora",
            "aaaa",
            MAX_PACK_BYTES,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unexpected member"),
            "{bad}: {err:#}"
        );
    }
}

#[test]
fn rejects_symlink_members() {
    let root = tempfile::tempdir().unwrap();
    let pack = root.path().join("link.dialpack");
    raw_pack(
        &pack,
        &sample_manifest("lora", "aaaa", vec![]),
        &[("files/link", &b""[..], tar::EntryType::Symlink)],
    );
    let err = open_pack(
        &pack,
        &root.path().join("packs"),
        "lora",
        "aaaa",
        MAX_PACK_BYTES,
    )
    .unwrap_err();
    assert!(err.to_string().contains("non-regular member"), "{err:#}");
}

#[test]
fn rejects_members_not_listed_in_manifest() {
    let root = tempfile::tempdir().unwrap();
    let sources = write_sources(root.path(), &sample_adapter());
    let manifest = sample_manifest("lora", "aaaa", entries(&sources).unwrap());
    let pack = root.path().join("extra.dialpack");
    raw_pack(
        &pack,
        &manifest,
        &[
            (
                "files/adapter_config.json",
                &b"{}"[..],
                tar::EntryType::Regular,
            ),
            (
                "files/adapters.safetensors",
                &b"weights"[..],
                tar::EntryType::Regular,
            ),
            ("files/extra", &b"x"[..], tar::EntryType::Regular),
        ],
    );
    let err = open_pack(
        &pack,
        &root.path().join("packs"),
        "lora",
        "aaaa",
        MAX_PACK_BYTES,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("do not match its manifest"),
        "{err:#}"
    );
}

#[test]
fn rejects_packs_over_the_size_limit() {
    let root = tempfile::tempdir().unwrap();
    let pack = pack_adapter(root.path(), "aaaa", &sample_adapter());
    let err = open_pack(&pack, &root.path().join("packs"), "lora", "aaaa", 16).unwrap_err();
    assert!(err.to_string().contains("size limit"), "{err:#}");
}

#[test]
fn rejects_pack_for_a_different_base_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let sources = write_sources(root.path(), &[(STEERING_FILE, b"{}".to_vec())]);
    let mut manifest = sample_manifest("steering", "aaaa", vec![]);
    manifest.files = entries(&sources).unwrap();
    let pack = root.path().join("p.dialpack");
    write_pack(&pack, &manifest, &sources).unwrap();
    let err = open_pack(
        &pack,
        &root.path().join("packs"),
        "steering",
        "bbbb",
        MAX_PACK_BYTES,
    )
    .unwrap_err();
    assert!(err.to_string().contains("fingerprint mismatch"), "{err:#}");
    assert!(
        open_pack(
            &pack,
            &root.path().join("packs"),
            "steering",
            "aaaa",
            MAX_PACK_BYTES
        )
        .is_ok()
    );
}

#[test]
fn rejects_kind_mismatch() {
    let root = tempfile::tempdir().unwrap();
    let sources = write_sources(root.path(), &[(STEERING_FILE, b"{}".to_vec())]);
    let mut manifest = sample_manifest("steering", "aaaa", vec![]);
    manifest.files = entries(&sources).unwrap();
    let pack = root.path().join("p.dialpack");
    write_pack(&pack, &manifest, &sources).unwrap();
    assert!(
        open_pack(
            &pack,
            &root.path().join("packs"),
            "lora",
            "aaaa",
            MAX_PACK_BYTES
        )
        .is_err()
    );
}

#[test]
fn steering_import_respects_the_shared_pending_vector_lock() {
    let root = tempfile::tempdir().unwrap();
    let checkpoint = root.path().join("checkpoint");
    fs::create_dir_all(&checkpoint).unwrap();
    fs::write(checkpoint.join("config.json"), b"{}").unwrap();
    fs::write(checkpoint.join("model.safetensors"), b"w").unwrap();
    let config = Config {
        model: checkpoint.display().to_string(),
        state_dir: root.path().join("state"),
        ..Config::default()
    };
    let sources = write_sources(root.path(), &[(STEERING_FILE, b"{}".to_vec())]);
    let manifest = sample_manifest(
        "steering",
        &pinned_base(&config).unwrap(),
        entries(&sources).unwrap(),
    );
    let pack = root.path().join("steering.dialpack");
    write_pack(&pack, &manifest, &sources).unwrap();
    let name = format!("imported-{}", &manifest.files[0].sha256[..12]);
    let pending = steering::pending_path(&config, &name).unwrap();
    let lock = steering::lock_vector(&pending).unwrap();
    crate::storage::settings_store::write_private(&pending, b"existing vector").unwrap();
    for replace in [false, true] {
        let error = import_steering(&config, &pack, replace).unwrap_err();
        assert!(
            error.to_string().contains("another steering job"),
            "{error:#}"
        );
        assert_eq!(fs::read(&pending).unwrap(), b"existing vector");
        assert!(!pending.with_extension("staging.json").exists());
    }
    drop(lock);
    assert!(import_steering(&config, &pack, false).is_err());
    assert_eq!(fs::read(&pending).unwrap(), b"existing vector");
    assert!(!pending.with_extension("staging.json").exists());
}

#[test]
fn imported_adapter_is_a_candidate_and_never_activates() {
    let root = tempfile::tempdir().unwrap();
    let checkpoint = root.path().join("checkpoint");
    fs::create_dir_all(&checkpoint).unwrap();
    fs::write(checkpoint.join("config.json"), b"{}").unwrap();
    fs::write(checkpoint.join("model.safetensors"), b"w").unwrap();
    let config = Config {
        model: checkpoint.display().to_string(),
        state_dir: root.path().join("state"),
        ..Config::default()
    };
    let base = pinned_base(&config).unwrap();
    let pack = pack_adapter(root.path(), &base, &sample_adapter());
    let store = Store::open(root.path(), "model", "coding", &Harness::default()).unwrap();
    let active = store.active().unwrap();

    let imported = import_adapter(&config, &store, &pack).unwrap();
    assert_eq!(imported.status, "candidate");
    assert_eq!(imported.parent, Some(active.id));
    assert_eq!(store.active().unwrap().id, active.id);
    assert_eq!(store.version(imported.id).unwrap().status, "candidate");

    let adapter = imported.adapter.clone().unwrap();
    let localized: Value =
        serde_json::from_slice(&fs::read(adapter.join("adapter_config.json")).unwrap()).unwrap();
    assert_eq!(localized["base_model_name_or_path"], config.model.as_str());
    assert!(
        import_adapter(&config, &store, &pack).is_err(),
        "re-import must be refused"
    );
}
