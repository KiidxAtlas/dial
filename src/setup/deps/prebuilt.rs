//! Verified direct downloads of prebuilt runtimes, the fallback when no package manager is present.
//!
//! Each archive is the project's own release asset, pinned by version and SHA-256 per platform.
//! The download is refused unless its digest matches, then extracted into a staging directory
//! inside Dial's install directory. Extraction rejects absolute paths, `..`, hard links and
//! symlinks that leave the install directory. Nothing is fetched unless the user confirms the plan.

use super::run::{download_to, sha256_file};
use anyhow::{Context, Result, bail, ensure};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// Largest archive Dial will unpack, before and after decompression.
const MAX_EXTRACTED_BYTES: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `.tar.gz`: one top-level folder, then the files.
    TarGz,
    /// `.zip`: one top-level folder, then the files.
    Zip,
}

/// One release archive for one platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asset {
    /// `darwin-arm64`, `darwin-x64`, `linux-x64` or `linux-arm64` (see [`current_platform`]).
    pub platform: &'static str,
    pub url: &'static str,
    /// Lowercase hex SHA-256 of the archive, copied from the project's `SHASUMS256.txt`.
    pub sha256: &'static str,
    pub format: Format,
}

/// A pinned release: which archive to fetch per platform, and where the program lands once the
/// top-level folder is removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prebuilt {
    pub version: &'static str,
    /// Program path relative to the install directory, such as `bin/node`.
    pub binary: &'static str,
    pub assets: &'static [Asset],
}

impl Prebuilt {
    /// The archive for `platform`, if this release ships one.
    pub fn asset(&self, platform: &str) -> Option<&'static Asset> {
        self.assets.iter().find(|asset| asset.platform == platform)
    }
}

/// Platform key of the running machine, or `None` when no prebuilt archive exists for it.
pub fn current_platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        _ => None,
    }
}

/// Node.js 22 LTS, from nodejs.org. Its `bin/npm` is a relative symlink, so links are kept.
pub const NODE: Prebuilt = Prebuilt {
    version: "22.23.3",
    binary: "bin/node",
    assets: &[
        Asset {
            platform: "darwin-arm64",
            url: "https://nodejs.org/dist/v22.23.3/node-v22.23.3-darwin-arm64.tar.gz",
            sha256: "23b25245dcfb9af7262f8ff142e9e2e0af025368117329e7a7458a51e5922f53",
            format: Format::TarGz,
        },
        Asset {
            platform: "darwin-x64",
            url: "https://nodejs.org/dist/v22.23.3/node-v22.23.3-darwin-x64.tar.gz",
            sha256: "8a677b0219178efd6eb0e475457c4afb452b521a92f6e67845a73bd85727f2a8",
            format: Format::TarGz,
        },
        Asset {
            platform: "linux-x64",
            url: "https://nodejs.org/dist/v22.23.3/node-v22.23.3-linux-x64.tar.gz",
            sha256: "1084aa36196bba4c3a5e69a1ee388a6e4ff729dad09445fbcd434b28fe3c24af",
            format: Format::TarGz,
        },
        Asset {
            platform: "linux-arm64",
            url: "https://nodejs.org/dist/v22.23.3/node-v22.23.3-linux-arm64.tar.gz",
            sha256: "5ced2d48d1d7198739b7f86804de0171aefb6823b684b12341d3321afc3cb0b2",
            format: Format::TarGz,
        },
    ],
};

/// Bun 1.4.3, from its GitHub release. The zip holds one `bun` executable.
pub const BUN: Prebuilt = Prebuilt {
    version: "1.4.3",
    binary: "bun",
    assets: &[
        Asset {
            platform: "darwin-arm64",
            url: "https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/bun-darwin-aarch64.zip",
            sha256: "80afd4c069b46a6fa8f3ec520bebc750a7843e4a8d5b9583d29ad436c00a3403",
            format: Format::Zip,
        },
        Asset {
            platform: "darwin-x64",
            url: "https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/bun-darwin-x64.zip",
            sha256: "62660e6438eb872278877acca77e1152d5d031324df368d4a81b428211489d2d",
            format: Format::Zip,
        },
        Asset {
            platform: "linux-x64",
            url: "https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/bun-linux-x64.zip",
            sha256: "3e730528d89775f03f87f05c06787c4df4099d0a37f3f252e7b829cb23816e18",
            format: Format::Zip,
        },
        Asset {
            platform: "linux-arm64",
            url: "https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/bun-linux-aarch64.zip",
            sha256: "efa9813da5ed72423bf847f916e8d2c47c0d776add972354026a75e10da9aa21",
            format: Format::Zip,
        },
    ],
};

/// uv 0.13.1, from its GitHub release. The tarball holds `uv` and `uvx`.
pub const UV: Prebuilt = Prebuilt {
    version: "0.13.1",
    binary: "uv",
    assets: &[
        Asset {
            platform: "darwin-arm64",
            url: "https://github.com/astral-sh/uv/releases/download/0.13.1/uv-aarch64-apple-darwin.tar.gz",
            sha256: "cfe454573412ff7380d15fc7dae5997ab53ffce72fce0a78e91ddbc84001acd2",
            format: Format::TarGz,
        },
        Asset {
            platform: "darwin-x64",
            url: "https://github.com/astral-sh/uv/releases/download/0.13.1/uv-x86_64-apple-darwin.tar.gz",
            sha256: "bb7b805bdcf3d0bc8c74ceb94569e6b3e8aa35bd492acdbb9d46bf46d9d4ff6b",
            format: Format::TarGz,
        },
        Asset {
            platform: "linux-x64",
            url: "https://github.com/astral-sh/uv/releases/download/0.13.1/uv-x86_64-unknown-linux-gnu.tar.gz",
            sha256: "eadd9d27c134ab5ac1ffd4473b97a741407f3ea238288f03121fc4deddf90220",
            format: Format::TarGz,
        },
        Asset {
            platform: "linux-arm64",
            url: "https://github.com/astral-sh/uv/releases/download/0.13.1/uv-aarch64-unknown-linux-gnu.tar.gz",
            sha256: "9041c73c99fdc5892cf128140beb354eca9231d502068cc36e6cb53a8e59ad14",
            format: Format::TarGz,
        },
    ],
};

/// Downloads `asset` into `dest` (the install directory). The archive goes to a `.download` file
/// beside `dest`, is checked against its pinned digest, unpacked into `<dest>.part`, and only then
/// replaces `dest`. Any failure leaves no partial install behind.
pub fn fetch(
    asset: &Asset,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    let parent = dest.parent().context("install directory has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let name = dest
        .file_name()
        .context("install directory has no name")?
        .to_string_lossy()
        .into_owned();
    let archive = parent.join(format!("{name}.download"));
    let staging = parent.join(format!("{name}.part"));
    let result: Result<()> = (|| {
        progress(&format!("downloading {}", asset.url));
        download_to(asset.url, &archive, 0, cancel, progress)?;
        let got = sha256_file(&archive)?;
        ensure!(
            got == asset.sha256,
            "download checksum mismatch: expected {}, got {got}",
            asset.sha256
        );
        progress("archive verified (sha256 ok)");
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;
        match asset.format {
            Format::TarGz => extract_tar_gz(&archive, &staging)?,
            Format::Zip => extract_zip(&archive, &staging)?,
        }
        let _ = fs::remove_dir_all(dest);
        fs::rename(&staging, dest).with_context(|| format!("installing {}", dest.display()))?;
        Ok(())
    })();
    let _ = fs::remove_file(&archive);
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

/// The path inside the archive without its top-level folder. `None` for that folder itself.
/// Anything that is not a plain name (`..`, absolute paths, prefixes) is an error.
fn inside_top(raw: &Path) -> Result<Option<PathBuf>> {
    let mut parts = Vec::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            _ => bail!("unsafe path in archive: {}", raw.display()),
        }
    }
    if parts.len() <= 1 {
        return Ok(None);
    }
    Ok(Some(parts[1..].iter().collect()))
}

/// Whether a symlink at `link_at` (relative to the install directory) with target `target`
/// stays inside the install directory. Checked lexically, before anything is created.
fn link_stays_inside(link_at: &Path, target: &Path) -> bool {
    let mut depth: usize = link_at.components().count().saturating_sub(1);
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            _ => return false,
        }
    }
    true
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("reading {}", archive.display()))?;
    let reader = flate2::read::GzDecoder::new(file).take(MAX_EXTRACTED_BYTES);
    let mut tar = tar::Archive::new(reader);
    tar.set_preserve_permissions(true);
    for entry in tar.entries().context("reading the runtime archive")? {
        let mut entry = entry?;
        let raw = entry.path()?.into_owned();
        let Some(rel) = inside_top(&raw)? else {
            continue;
        };
        let target = dest.join(&rel);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&target)?;
        } else if kind.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            entry.set_preserve_permissions(true);
            entry.unpack(&target)?;
        } else if kind.is_symlink() {
            let link = entry
                .link_name()?
                .context("symlink without a target")?
                .into_owned();
            ensure!(
                link_stays_inside(&rel, &link),
                "symlink leaves the install directory: {}",
                raw.display()
            );
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            symlink(&link, &target)?;
        } else {
            bail!("unsupported archive entry {kind:?} at {}", raw.display());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("creating {}", link.display()))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, link: &Path) -> Result<()> {
    bail!("symlinks are not supported here: {}", link.display())
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("reading {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("reading the runtime archive")?;
    let mut total: u64 = 0;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).context("reading the runtime archive")?;
        let raw = std::path::PathBuf::from(entry.name());
        let Some(rel) = inside_top(&raw)? else {
            continue;
        };
        let target = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("symlinks are not allowed in this archive: {}", raw.display());
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out =
            File::create(&target).with_context(|| format!("creating {}", target.display()))?;
        total += io::copy(&mut (&mut entry).take(MAX_EXTRACTED_BYTES + 1), &mut out)?;
        ensure!(
            total <= MAX_EXTRACTED_BYTES,
            "archive expands beyond {MAX_EXTRACTED_BYTES} bytes"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = entry.unix_mode().unwrap_or(0o644);
            let mode = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
                    "darwin-arm64" | "darwin-x64" | "linux-x64" | "linux-arm64"
                ));
            }
            assert!(prebuilt.asset("darwin-arm64").is_some());
            assert!(prebuilt.asset("plan9-mips").is_none());
        }
    }

    #[test]
    fn top_folder_is_removed_and_traversal_is_refused() {
        assert_eq!(inside_top(Path::new("node-v1/")).unwrap(), None);
        assert_eq!(
            inside_top(Path::new("node-v1/bin/node")).unwrap(),
            Some(PathBuf::from("bin/node"))
        );
        assert!(inside_top(Path::new("node-v1/../etc/passwd")).is_err());
        assert!(inside_top(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn symlinks_must_stay_inside_the_install_directory() {
        assert!(link_stays_inside(Path::new("bin/npm"), Path::new("../lib/npm-cli.js")));
        assert!(!link_stays_inside(Path::new("bin/npm"), Path::new("../../etc/passwd")));
        assert!(!link_stays_inside(Path::new("bin/npm"), Path::new("/usr/bin/env")));
        assert!(!link_stays_inside(Path::new("a"), Path::new("../x")));
    }

    fn tgz(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("runtime.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
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
        assert!(inside_top(Path::new("top/../x")).is_err());
    }

    #[test]
    fn zip_extraction_strips_the_top_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.zip");
        let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .unix_permissions(0o755);
        writer.start_file("bun-test/bun", options).unwrap();
        writer.write_all(b"#!/bin/sh\n").unwrap();
        writer.finish().unwrap();
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        extract_zip(&path, &dest).unwrap();
        assert!(dest.join("bun").is_file());
    }
}
