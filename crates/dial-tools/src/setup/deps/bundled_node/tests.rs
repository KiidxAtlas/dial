use super::*;
use flate2::{Compression, write::GzEncoder};

const PROGRAM: &[u8] = b"#!/bin/sh\necho bundled node\n";

/// A tiny gzip payload standing in for `node.gz`, with the digest and size its metadata records.
struct Fixture {
    gz: Vec<u8>,
    sha256: String,
    size: u64,
}

impl Fixture {
    fn new(raw: &[u8]) -> Self {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(raw).unwrap();
        Self {
            gz: encoder.finish().unwrap(),
            sha256: format!("{:x}", Sha256::digest(raw)),
            size: raw.len() as u64,
        }
    }

    fn bundle(&self) -> Bundle<'_> {
        Bundle {
            version: "24.22.3",
            sha256: &self.sha256,
            size: self.size,
            gz: &self.gz,
        }
    }
}

fn provision(user_dir: &Path, fixture: &Fixture) -> Result<PathBuf> {
    provision_in(user_dir, &fixture.bundle())
}

#[test]
fn roundtrip_extracts_once_then_reuses_the_verified_copy() {
    let user_dir = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(PROGRAM);
    let first = provision(user_dir.path(), &fixture).unwrap();
    assert_eq!(fs::read(&first).unwrap(), PROGRAM);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let second = provision(user_dir.path(), &fixture).unwrap();
    assert_eq!(first, second);
    let staged: Vec<_> = fs::read_dir(first.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(staged.is_empty(), "staged files left behind: {staged:?}");
}

#[test]
fn tampered_truncated_or_extended_runtime_fails_closed_without_rewrite() {
    type Mutate = fn(&mut Vec<u8>);
    let cases: [(&str, Mutate); 3] = [
        ("tampered", |bytes| bytes[0] ^= 0xff),
        ("truncated", |bytes| bytes.truncate(bytes.len() / 2)),
        ("extended", |bytes| bytes.push(b'\n')),
    ];
    for (name, mutate) in cases {
        let user_dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(PROGRAM);
        let target = provision(user_dir.path(), &fixture).unwrap();
        let mut corrupted = fs::read(&target).unwrap();
        mutate(&mut corrupted);
        fs::write(&target, &corrupted).unwrap();
        assert!(
            provision(user_dir.path(), &fixture).is_err(),
            "{name} was accepted"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            corrupted,
            "{name} was rewritten"
        );
    }
}

#[test]
fn payload_that_does_not_match_its_recorded_digest_publishes_nothing() {
    let user_dir = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::new(PROGRAM);
    fixture.sha256 = format!("{:x}", Sha256::digest(b"other program"));
    assert!(provision(user_dir.path(), &fixture).is_err());
    let root = user_dir.path().join("cache").join("runtimes").join("node");
    let leftovers: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .flat_map(|dir| fs::read_dir(dir.unwrap().path()).unwrap())
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(leftovers.is_empty(), "unexpected files: {leftovers:?}");
}

#[test]
fn directory_at_the_runtime_path_is_refused() {
    let user_dir = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(PROGRAM);
    let target = provision(user_dir.path(), &fixture).unwrap();
    fs::remove_file(&target).unwrap();
    fs::create_dir(&target).unwrap();
    assert!(provision(user_dir.path(), &fixture).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_at_the_runtime_path_is_refused_even_when_it_points_at_valid_bytes() {
    let user_dir = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(PROGRAM);
    let target = provision(user_dir.path(), &fixture).unwrap();
    let elsewhere = user_dir.path().join("elsewhere");
    fs::write(&elsewhere, PROGRAM).unwrap();
    fs::remove_file(&target).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &target).unwrap();
    assert!(provision(user_dir.path(), &fixture).is_err());
}
