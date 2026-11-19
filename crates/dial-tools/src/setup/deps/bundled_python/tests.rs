use super::*;
use flate2::{Compression, write::GzEncoder};

fn archive(extra: Option<(&str, tar::EntryType)>) -> Vec<u8> {
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    for (path, kind) in std::iter::once((PROGRAM, tar::EntryType::Regular)).chain(extra) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o755);
        header.set_size(0);
        tar.append_data(&mut header, path, io::empty()).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

#[test]
fn extracts_a_fresh_tree_and_cleans_up_on_drop_or_failure() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().canonicalize().unwrap();
    let valid = archive(None);
    let runtime = extract(&parent, &valid, 65_536).unwrap();
    assert!(runtime.executable().is_file());
    fs::write(runtime.executable(), b"modified").unwrap();
    let other = extract(&parent, &valid, 65_536).unwrap();
    assert!(fs::read(other.executable()).unwrap().is_empty());
    drop((other, runtime));
    for entry in [
        (PROGRAM, tar::EntryType::Regular),
        ("alias", tar::EntryType::Symlink),
    ] {
        assert!(extract(&parent, &archive(Some(entry)), 65_536).is_err());
    }
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
}
