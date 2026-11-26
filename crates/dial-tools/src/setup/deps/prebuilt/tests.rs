use super::*;
use std::io::Write;

#[test]
fn every_pinned_asset_has_a_full_digest_and_a_known_platform() {
    for prebuilt in [NODE, BUN, UV] {
        for asset in prebuilt.assets {
            assert_eq!(asset.sha256.len(), 64, "{}", asset.url);
            assert!(asset.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(asset.url.starts_with("https://"));
            assert!(matches!(
                asset.platform,
                "darwin-arm64"
                    | "darwin-x64"
                    | "linux-x64"
                    | "linux-arm64"
                    | "linux-x64-musl"
                    | "linux-arm64-musl"
                    | "win32-x64"
                    | "win32-arm64"
            ));
        }
        assert!(prebuilt.asset("darwin-arm64").is_some());
        assert!(prebuilt.asset("plan9-mips").is_none());
    }
}

#[test]
fn release_platforms_distinguish_linux_libcs_and_reject_unknown_targets() {
    for (os, arch, abi, expected) in [
        ("linux", "x86_64", "gnu", Some("linux-x64")),
        ("linux", "aarch64", "gnu", Some("linux-arm64")),
        ("linux", "x86_64", "musl", Some("linux-x64-musl")),
        ("linux", "aarch64", "musl", Some("linux-arm64-musl")),
        ("linux", "x86_64", "uclibc", None),
        ("linux", "riscv64", "musl", None),
        ("macos", "aarch64", "", Some("darwin-arm64")),
        ("macos", "x86_64", "", Some("darwin-x64")),
        ("windows", "aarch64", "msvc", Some("win32-arm64")),
        ("windows", "x86_64", "msvc", Some("win32-x64")),
        ("freebsd", "x86_64", "", None),
    ] {
        assert_eq!(platform_key(os, arch, abi), expected, "{os} {arch} {abi}");
    }
}

#[test]
fn musl_runtimes_use_native_archives_and_do_not_offer_glibc_model_servers() {
    for platform in ["linux-x64-musl", "linux-arm64-musl"] {
        for (runtime, format) in [
            (NODE, Format::TarGz),
            (BUN, Format::Zip),
            (UV, Format::TarGz),
        ] {
            let asset = runtime.asset(platform).unwrap();
            assert!(asset.url.contains("-musl"), "{}", asset.url);
            assert_eq!(asset.format, format);
        }
        for runtime in [LLAMA_CPP, SD_CPP] {
            assert!(runtime.asset(platform).is_none());
        }
    }
}

#[test]
fn windows_setup_runtimes_have_pinned_native_archives_for_each_architecture() {
    for arch in ["win32-x64", "win32-arm64"] {
        let asset = BUN.asset(arch).expect("missing Windows Bun archive");
        assert_eq!(asset.format, Format::Zip);
        assert!(asset.url.contains("/bun-windows-"));
        assert_eq!(NODE.asset(arch).unwrap().format, Format::Zip);
        assert_eq!(UV.asset(arch).unwrap().format, Format::FlatZip);
    }
}

#[test]
fn top_folder_is_removed_and_traversal_is_refused() {
    assert_eq!(inside_top(Path::new("node-v1/"), true).unwrap(), None);
    assert_eq!(
        inside_top(Path::new("node-v1/bin/node"), true).unwrap(),
        Some(PathBuf::from("bin/node"))
    );
    assert!(inside_top(Path::new("node-v1/../etc/passwd"), true).is_err());
    assert!(inside_top(Path::new("/etc/passwd"), true).is_err());
}

#[test]
fn flat_archives_keep_every_member_at_its_path() {
    assert_eq!(
        inside_top(Path::new("llama-server.exe"), false).unwrap(),
        Some(PathBuf::from("llama-server.exe"))
    );
    assert_eq!(
        inside_top(Path::new("llama-b1/llama-server"), true).unwrap(),
        Some(PathBuf::from("llama-server"))
    );
}

#[test]
fn llama_cpp_pins_each_platform_archive_with_its_layout() {
    assert_eq!(
        LLAMA_CPP.asset("win32-x64").unwrap().format,
        Format::FlatZip
    );
    assert_eq!(LLAMA_CPP.asset("linux-x64").unwrap().format, Format::TarGz);
    for asset in LLAMA_CPP.assets {
        assert_eq!(asset.sha256.len(), 64, "{}", asset.url);
        assert!(
            asset
                .url
                .starts_with("https://github.com/ggml-org/llama.cpp/")
        );
    }
}

#[test]
fn sd_cpp_pins_a_flat_archive_for_each_supported_platform() {
    for platform in ["darwin-arm64", "linux-x64", "win32-x64"] {
        let asset = SD_CPP.asset(platform).unwrap();
        assert_eq!(asset.format, Format::FlatZip, "{platform}");
        assert_eq!(asset.sha256.len(), 64, "{}", asset.url);
        assert!(asset.url.starts_with(
            "https://github.com/leejet/stable-diffusion.cpp/releases/download/master-1106-532d2c1/"
        ));
    }
}

#[test]
fn mlx_serve_pins_its_apple_silicon_archive_only() {
    let asset = MLX_SERVE.asset("darwin-arm64").unwrap();
    assert_eq!(asset.format, Format::TarGz);
    assert_eq!(asset.sha256.len(), 64);
    assert!(
        asset
            .url
            .starts_with("https://github.com/ddalcu/mlx-serve/releases/download/v26.11.3/")
    );
    for platform in ["darwin-x64", "linux-x64", "linux-arm64", "win32-x64"] {
        assert!(MLX_SERVE.asset(platform).is_none(), "{platform}");
    }
}

#[test]
fn symlinks_must_stay_inside_the_install_directory() {
    assert!(link_stays_inside(
        Path::new("bin/npm"),
        Path::new("../lib/npm-cli.js")
    ));
    assert!(!link_stays_inside(
        Path::new("bin/npm"),
        Path::new("../../etc/passwd")
    ));
    assert!(!link_stays_inside(
        Path::new("bin/npm"),
        Path::new("/usr/bin/env")
    ));
    assert!(!link_stays_inside(Path::new("a"), Path::new("../x")));
}

fn tgz(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
    let path = dir.join("runtime.tar.gz");
    let encoder =
        flate2::write::GzEncoder::new(File::create(&path).unwrap(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for (name, body) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, name, *body).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();
    path
}

#[test]
fn tar_extraction_strips_the_top_folder_and_keeps_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let archive = tgz(tmp.path(), &[("top/bin/tool", b"#!/bin/sh\n")]);
    let dest = tmp.path().join("out");
    fs::create_dir_all(&dest).unwrap();
    extract_tar_gz(&archive, &dest).unwrap();
    assert!(dest.join("bin/tool").is_file());
}

#[test]
fn zip_names_with_parent_directories_are_refused() {
    assert!(inside_top(Path::new("top/../x"), true).is_err());
}

#[test]
fn zip_extraction_strips_the_top_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("runtime.zip");
    let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
    writer.start_file("bun-test/bun", options).unwrap();
    writer.write_all(b"#!/bin/sh\n").unwrap();
    writer.finish().unwrap();
    let dest = tmp.path().join("out");
    fs::create_dir_all(&dest).unwrap();
    extract_zip(&path, &dest, true).unwrap();
    assert!(dest.join("bun").is_file());
}

#[test]
fn flat_zip_extraction_keeps_files_at_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("runtime.zip");
    let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
    writer.start_file("llama-server.exe", options).unwrap();
    writer.write_all(b"MZ").unwrap();
    writer.finish().unwrap();
    let dest = tmp.path().join("out");
    fs::create_dir_all(&dest).unwrap();
    extract_zip(&path, &dest, false).unwrap();
    assert!(dest.join("llama-server.exe").is_file());
}
