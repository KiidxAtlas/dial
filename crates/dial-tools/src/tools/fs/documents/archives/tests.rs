use super::*;
use std::io::{Cursor, Write};

const HELLO: &str = "hello archive\n";

fn tar_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, *data).unwrap();
    }
    builder.into_inner().unwrap()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn dir_record(buf: &mut [u8], lba: u32, size: u32, flags: u8, name: &[u8]) -> usize {
    let len = (33 + name.len() + 1) & !1;
    buf[0] = len as u8;
    buf[2..6].copy_from_slice(&lba.to_le_bytes());
    buf[6..10].copy_from_slice(&lba.to_be_bytes());
    buf[10..14].copy_from_slice(&size.to_le_bytes());
    buf[14..18].copy_from_slice(&size.to_be_bytes());
    buf[25] = flags;
    buf[32] = name.len() as u8;
    buf[33..33 + name.len()].copy_from_slice(name);
    len
}

/// Minimal ISO 9660 image: one primary volume descriptor, a root directory at sector 18,
/// and one file at sector 19.
fn iso_image(name: &str, data: &[u8]) -> Vec<u8> {
    const SECTOR: usize = 2048;
    let mut img = vec![0u8; 20 * SECTOR];
    let pvd = 16 * SECTOR;
    img[pvd] = 1;
    img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
    img[pvd + 6] = 1;
    dir_record(&mut img[pvd + 156..], 18, SECTOR as u32, 2, &[0]);
    let term = 17 * SECTOR;
    img[term] = 255;
    img[term + 1..term + 6].copy_from_slice(b"CD001");
    img[term + 6] = 1;

    let dir = 18 * SECTOR;
    let mut at = dir;
    at += dir_record(&mut img[at..], 18, SECTOR as u32, 2, &[0]);
    at += dir_record(&mut img[at..], 18, SECTOR as u32, 2, &[1]);
    let file_name = format!("{name};1");
    dir_record(
        &mut img[at..],
        19,
        data.len() as u32,
        0,
        file_name.as_bytes(),
    );
    img[19 * SECTOR..19 * SECTOR + data.len()].copy_from_slice(data);
    img
}

fn rar_stored(name: &str, data: &[u8]) -> Vec<u8> {
    use rars::rar50::{ArchiveEntry, Rar50Writer, WriterOptions};
    use rars::{ArchiveVersion, EntrySource, FeatureSet};
    let options = WriterOptions::new(ArchiveVersion::Rar50, FeatureSet::store_only())
        .with_compression_level(0);
    let entry = ArchiveEntry::new(
        name.as_bytes().to_vec(),
        EntrySource::from_bytes(std::sync::Arc::<[u8]>::from(data.to_vec())),
    );
    Rar50Writer::new(options)
        .entries(vec![entry])
        .finish()
        .unwrap()
}

fn cab_archive(name: &str, data: &[u8]) -> Vec<u8> {
    let mut builder = ::cab::CabinetBuilder::new();
    let folder = builder.add_folder(::cab::CompressionType::None);
    folder.add_file(name);
    let mut writer = builder.build(Cursor::new(Vec::new())).unwrap();
    while let Some(mut file) = writer.next_file().unwrap() {
        file.write_all(data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn safe_names_reject_traversal_absolute_and_backslash_paths() {
    assert!(safe("dir/file.txt"));
    assert!(!safe("../escape.txt"));
    assert!(!safe("dir/../../escape.txt"));
    assert!(!safe("/etc/passwd"));
    assert!(!safe("dir\\file.txt"));
    assert!(!safe(""));
}

#[test]
fn single_stream_xz_bz2_zst_decode_to_text() {
    let xz = include_bytes!("tests/fixtures/hello.txt.xz");
    let bz2 = include_bytes!("tests/fixtures/hello.txt.bz2");
    let zst = include_bytes!("tests/fixtures/hello.txt.zst");
    assert_eq!(read_bytes("xz", "hello.txt.xz", xz, None).unwrap(), HELLO);
    assert_eq!(
        read_bytes("bz2", "hello.txt.bz2", bz2, None).unwrap(),
        HELLO
    );
    assert_eq!(
        read_bytes("zst", "hello.txt.zst", zst, None).unwrap(),
        HELLO
    );
}

#[test]
fn compressed_stream_rejects_member_selector() {
    let xz = include_bytes!("tests/fixtures/hello.txt.xz");
    assert!(read_bytes("xz", "hello.txt.xz", xz, Some("x")).is_err());
}

#[test]
fn tar_compression_follows_container_suffix() {
    assert_eq!(tar_compression("a.tar.gz"), "gzip");
    assert_eq!(tar_compression("a.TGZ"), "gzip");
    assert_eq!(tar_compression("a.tar.xz"), "xz");
    assert_eq!(tar_compression("a.tbz2"), "bz2");
    assert_eq!(tar_compression("a.tar.zst"), "zst");
    assert_eq!(tar_compression("a.tar"), "none");
}

#[test]
fn tar_gzip_lists_and_selects_member() {
    let data = tar_with(&[("docs/a.txt", b"alpha"), ("docs/b.txt", b"beta")]);
    let bytes = gzip(&data);
    let listing = read_bytes("tar", "bundle.tar.gz", &bytes, None).unwrap();
    assert!(listing.contains("docs/a.txt"));
    assert!(listing.contains("docs/b.txt"));
    assert_eq!(
        read_bytes("tar", "bundle.tar.gz", &bytes, Some("docs/b.txt")).unwrap(),
        "beta"
    );
}

#[test]
fn tar_rejects_duplicate_member_names() {
    let data = tar_with(&[("a.txt", b"one"), ("a.txt", b"two")]);
    assert!(read_bytes("tar", "dup.tar", &data, None).is_err());
}

#[test]
fn tar_gzip_declared_expansion_over_cap_is_rejected() {
    let size = (MAX_DECODED + 1) as usize;
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(size as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, "big.bin", std::io::repeat(0).take(size as u64))
        .unwrap();
    let tar_bytes = builder.into_inner().unwrap();
    let bytes = gzip(&tar_bytes);
    assert!(read_bytes("tar", "bomb.tar.gz", &bytes, Some("big.bin")).is_err());
}

#[test]
fn seven_zip_reads_member_content() {
    let bytes = include_bytes!("tests/fixtures/single_file.7z");
    assert_eq!(
        read_bytes("7z", "single_file.7z", bytes, Some("file.txt")).unwrap(),
        "this is a file\n"
    );
}

fn seven_zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
    for (name, data) in entries {
        writer
            .push_archive_entry(ArchiveEntry::new_file(name), Some(Cursor::new(*data)))
            .unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[test]
fn seven_zip_lists_and_reports_missing_members() {
    let bytes = include_bytes!("tests/fixtures/single_file.7z");
    assert_eq!(
        read_bytes("7z", "single_file.7z", bytes, None).unwrap(),
        "file.txt\t15 bytes\n"
    );
    assert!(
        read_bytes("7z", "single_file.7z", bytes, Some("absent.txt"))
            .unwrap_err()
            .contains("not found")
    );
}

#[test]
fn seven_zip_validates_unselected_names_after_selected_content() {
    for name in [
        "../escape.txt",
        "/absolute.txt",
        "dir\\escape.txt",
        "C:/drive.txt",
        "ok.txt",
    ] {
        let bytes = seven_zip_with(&[("ok.txt", b"safe"), (name, b"unsafe")]);
        for member in [None, Some("ok.txt")] {
            let error = read_bytes("7z", "unsafe.7z", &bytes, member).unwrap_err();
            assert!(error.contains("unsafe or duplicate"), "{name}: {error}");
        }
    }
}

#[test]
fn seven_zip_rejects_member_over_content_limit() {
    let data = vec![b'a'; (super::super::MAX_CONTENT + 1) as usize];
    let bytes = seven_zip_with(&[("large.txt", &data)]);
    // Listing does not impose the selected-text cap.
    assert!(read_bytes("7z", "large.7z", &bytes, None).is_ok());
    assert!(
        read_bytes("7z", "large.7z", &bytes, Some("large.txt"))
            .unwrap_err()
            .contains("exceeds 8 MiB")
    );
}

#[test]
fn seven_zip_rejects_entry_count_over_limit() {
    let names: Vec<_> = (0..=MAX_ENTRIES).map(|n| format!("{n}.txt")).collect();
    let entries: Vec<_> = names.iter().map(|name| (name.as_str(), &b""[..])).collect();
    let bytes = seven_zip_with(&entries);
    assert!(
        read_bytes("7z", "many.7z", &bytes, Some("0.txt"))
            .unwrap_err()
            .contains("10000 member limit")
    );
}

#[test]
fn cab_reads_member_content() {
    let bytes = cab_archive("notes.txt", HELLO.as_bytes());
    assert_eq!(
        read_bytes("cab", "pack.cab", &bytes, Some("notes.txt")).unwrap(),
        HELLO
    );
}

#[test]
fn iso_reads_member_content() {
    let bytes = iso_image("HELLO.TXT", HELLO.as_bytes());
    assert_eq!(
        read_bytes("iso", "disc.iso", &bytes, Some("HELLO.TXT")).unwrap(),
        HELLO
    );
}

#[test]
fn rar_reads_stored_member_content() {
    let bytes = rar_stored("notes.txt", HELLO.as_bytes());
    assert_eq!(
        read_bytes("rar", "pack.rar", &bytes, Some("notes.txt")).unwrap(),
        HELLO
    );
}

#[test]
fn rar_missing_member_is_an_error() {
    let bytes = rar_stored("notes.txt", HELLO.as_bytes());
    assert!(read_bytes("rar", "pack.rar", &bytes, Some("absent.txt")).is_err());
}
