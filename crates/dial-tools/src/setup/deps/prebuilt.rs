//! Verified direct downloads of prebuilt runtimes, the fallback when no package manager is present.
//!
//! Each archive is the project's own release asset, pinned by version and SHA-256 per platform.
//! The download is refused unless its digest matches, then extracted into a staging directory
//! inside Dial's install directory. Extraction rejects absolute paths, `..`, hard links and
//! symlinks that leave the install directory. Nothing is fetched unless the user confirms the plan.

use super::run::download_to;
use crate::runtime::fingerprint::sha256_file;
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
    /// `.zip` with the files at its root and no top-level folder (llama.cpp's Windows builds).
    FlatZip,
}

/// One release archive for one platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asset {
    /// Platform key from [`current_platform`].
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
    let target_env = if cfg!(target_env = "gnu") {
        "gnu"
    } else if cfg!(target_env = "musl") {
        "musl"
    } else {
        ""
    };
    platform_key(std::env::consts::OS, std::env::consts::ARCH, target_env)
}

fn platform_key(os: &str, arch: &str, target_env: &str) -> Option<&'static str> {
    match (os, arch, target_env) {
        ("macos", "aarch64", _) => Some("darwin-arm64"),
        ("macos", "x86_64", _) => Some("darwin-x64"),
        ("linux", "x86_64", "gnu") => Some("linux-x64"),
        ("linux", "aarch64", "gnu") => Some("linux-arm64"),
        ("linux", "x86_64", "musl") => Some("linux-x64-musl"),
        ("linux", "aarch64", "musl") => Some("linux-arm64-musl"),
        ("windows", "x86_64", _) => Some("win32-x64"),
        ("windows", "aarch64", _) => Some("win32-arm64"),
        _ => None,
    }
}

/// Node.js 22 LTS (arm64 musl from Node unofficial-builds); both sources publish SHASUMS256.txt.
/// Its `bin/npm` is a relative symlink, so links are kept.
pub const NODE: Prebuilt = Prebuilt {
    version: "22.23.3",
    binary: if cfg!(windows) {
        "node.exe"
    } else {
        "bin/node"
    },
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
        Asset {
            platform: "linux-x64-musl",
            url: "https://nodejs.org/dist/v22.23.3/node-v22.23.3-linux-x64-musl.tar.gz",
            sha256: "633f49a4f388c5f9564d56d9c0f5ffa394346d92ca0076fe21cf776ece1988bc",
            format: Format::TarGz,
        },
        Asset {
            platform: "linux-arm64-musl",
            url: "https://unofficial-builds.nodejs.org/download/release/v22.23.3/node-v22.23.3-linux-arm64-musl.tar.gz",
            sha256: "8cec03a498d883739d1dc9b924fda2f86944705b54ac707bf48a637483774c70",
            format: Format::TarGz,
        },
        Asset {
            platform: "win32-x64",
            url: "https://nodejs.org/dist/v22.23.3/node-v22.23.3-win-x64.zip",
            sha256: "2b0ff57b049cda1bbcea2240eec20467018713c1efe1f7360c2681859b90ed71",
            format: Format::Zip,
        },
        Asset {
            platform: "win32-arm64",
            url: "https://nodejs.org/dist/v22.23.3/node-v22.23.3-win-arm64.zip",
            sha256: "33dad22e4cef5ee8f9fbb1b0d037fdacd0e56d12a4580f0d63f68b894deab535",
            format: Format::Zip,
        },
    ],
};

/// Bun 1.4.3, from its GitHub release. The zip holds one `bun` executable.
pub const BUN: Prebuilt = Prebuilt {
    version: "1.4.3",
    binary: if cfg!(windows) { "bun.exe" } else { "bun" },
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
        Asset {
            platform: "linux-x64-musl",
            url: "https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/bun-linux-x64-musl-baseline.zip",
            sha256: "4b5b0b5503f6ebf15161e8c56eb5a263134c05cd98a86dc11ee43ac5bda9862e",
            format: Format::Zip,
        },
        Asset {
            platform: "linux-arm64-musl",
            url: "https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/bun-linux-aarch64-musl.zip",
            sha256: "8de8a0003cc2707bb555887e3d450e24e1edf3f605f491393e89ed61a8853420",
            format: Format::Zip,
        },
        Asset {
            platform: "win32-x64",
            url: "https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/bun-windows-x64-baseline.zip",
            sha256: "d7a7d3eb216614544aab0cde2d8d9f2e4e87839bf5a30a1cd11d077f02d84e0c",
            format: Format::Zip,
        },
        Asset {
            platform: "win32-arm64",
            url: "https://github.com/oven-sh/bun/releases/download/bun-v1.4.3/bun-windows-aarch64.zip",
            sha256: "5e8f142ef722cbc9f67dc068a166e46ee72be8307b38cac7186589d2face668f",
            format: Format::Zip,
        },
    ],
};

/// uv 0.13.1, from its GitHub release. The tarball holds `uv` and `uvx`.
pub const UV: Prebuilt = Prebuilt {
    version: "0.13.1",
    binary: if cfg!(windows) { "uv.exe" } else { "uv" },
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
        Asset {
            platform: "linux-x64-musl",
            url: "https://github.com/astral-sh/uv/releases/download/0.13.1/uv-x86_64-unknown-linux-musl.tar.gz",
            sha256: "6c86040a8f045ab9adc9e3ec4cdafa03fd00e9a988ed2bf28971c7fc2d0debd0",
            format: Format::TarGz,
        },
        Asset {
            platform: "linux-arm64-musl",
            url: "https://github.com/astral-sh/uv/releases/download/0.13.1/uv-aarch64-unknown-linux-musl.tar.gz",
            sha256: "345a8308e4d1835cdfaadf692836fd3137a6d45864773f05ad7bf59442b1ba16",
            format: Format::TarGz,
        },
        Asset {
            platform: "win32-x64",
            url: "https://github.com/astral-sh/uv/releases/download/0.13.1/uv-x86_64-pc-windows-msvc.zip",
            sha256: "3b2661a463a5baea7f5076ea2b25ab71d77f36a6c0d809e5350651792f80a672",
            format: Format::FlatZip,
        },
        Asset {
            platform: "win32-arm64",
            url: "https://github.com/astral-sh/uv/releases/download/0.13.1/uv-aarch64-pc-windows-msvc.zip",
            sha256: "ec7846a5939b4654afee24f2d82bf6fde71bd7a6952d7748f1135ab301dd0a49",
            format: Format::FlatZip,
        },
    ],
};

/// llama.cpp b11669 (llama-server), from its GitHub release. Windows x64 ships the server flat at
/// the archive root; Linux x64 wraps it in one folder. Both digests were checked against the
/// release's asset digests.
pub const LLAMA_CPP: Prebuilt = Prebuilt {
    version: "b11669",
    binary: if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    },
    assets: &[
        Asset {
            platform: "win32-x64",
            url: "https://github.com/ggml-org/llama.cpp/releases/download/b11669/llama-b11669-bin-win-cpu-x64.zip",
            sha256: "37b3efcf962847ca869745fd4ccf7f5e7e8dacfeff9fa3523b4ca8cdc66d632b",
            format: Format::FlatZip,
        },
        Asset {
            platform: "linux-x64",
            url: "https://github.com/ggml-org/llama.cpp/releases/download/b11669/llama-b11669-bin-ubuntu-x64.tar.gz",
            sha256: "cf07c1c112d709bd67e522017471de57ed0ec8bea031d732c5422351c07f65ac",
            format: Format::TarGz,
        },
    ],
};

/// stable-diffusion.cpp `master-1106-532d2c1` (`sd-server`), from its GitHub release. Every archive
/// holds the server and its shared libraries flat at the root (no symlinks); the digests are the
/// release's asset digests. These are the CPU (Linux, Windows) and Metal (macOS arm64) builds;
/// Vulkan, CUDA and ROCm builds from the same release can be used through `DIAL_SD_SERVER`.
pub const SD_CPP: Prebuilt = Prebuilt {
    version: "master-1106-532d2c1",
    binary: if cfg!(windows) {
        "sd-server.exe"
    } else {
        "sd-server"
    },
    assets: &[
        Asset {
            platform: "darwin-arm64",
            url: "https://github.com/leejet/stable-diffusion.cpp/releases/download/master-1106-532d2c1/sd-master-532d2c1-bin-Darwin-macOS-26.7.2-arm64.zip",
            sha256: "8146d6ffc75f8ff8f64b62a80fffee8fb714ab65e368e202084135fa07cc6f36",
            format: Format::FlatZip,
        },
        Asset {
            platform: "linux-x64",
            url: "https://github.com/leejet/stable-diffusion.cpp/releases/download/master-1106-532d2c1/sd-master-532d2c1-bin-Linux-Ubuntu-26.04-x86_64.zip",
            sha256: "0a02f7f51feb0691128f60cf6ff1c686ec23089561cfe7c3b72645f446646e13",
            format: Format::FlatZip,
        },
        Asset {
            platform: "win32-x64",
            url: "https://github.com/leejet/stable-diffusion.cpp/releases/download/master-1106-532d2c1/sd-master-532d2c1-bin-win-cpu-x64.zip",
            sha256: "abef332cfca7861cc567910e79bedb70bfb2920bb4eedac97fcbb6db477f1596",
            format: Format::FlatZip,
        },
    ],
};

/// MLX-Serve 26.11.3 (`mlx-serve`), from its GitHub release: the command-line server with its MLX
/// libraries in `lib/` under one top-level folder. Built for macOS on Apple Silicon only; the
/// digest is the release's asset digest.
pub const MLX_SERVE: Prebuilt = Prebuilt {
    version: "26.11.3",
    binary: "mlx-serve",
    assets: &[Asset {
        platform: "darwin-arm64",
        url: "https://github.com/ddalcu/mlx-serve/releases/download/v26.11.3/mlx-serve-bin-macos-arm64.tar.gz",
        sha256: "a45db31a0a670dc551ed2f220719dc991a2ca13aaae9ec7d2a857625da7f32e4",
        format: Format::TarGz,
    }],
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
            Format::Zip => extract_zip(&archive, &staging, true)?,
            Format::FlatZip => extract_zip(&archive, &staging, false)?,
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
pub(super) fn inside_top(raw: &Path, top_folder: bool) -> Result<Option<PathBuf>> {
    let mut parts = Vec::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            _ => bail!("unsafe path in archive: {}", raw.display()),
        }
    }
    let rest = if top_folder {
        if parts.len() <= 1 {
            return Ok(None);
        }
        &parts[1..]
    } else {
        &parts[..]
    };
    if rest.is_empty() {
        return Ok(None);
    }
    Ok(Some(rest.iter().collect()))
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
        let Some(rel) = inside_top(&raw, true)? else {
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
    std::os::unix::fs::symlink(target, link).with_context(|| format!("creating {}", link.display()))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, link: &Path) -> Result<()> {
    bail!("symlinks are not supported here: {}", link.display())
}

fn extract_zip(archive: &Path, dest: &Path, top_folder: bool) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("reading {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("reading the runtime archive")?;
    let mut total: u64 = 0;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).context("reading the runtime archive")?;
        let raw = std::path::PathBuf::from(
            entry
                .name()
                .context("the runtime archive has a member name that is not UTF-8")?
                .as_ref(),
        );
        let Some(rel) = inside_top(&raw, top_folder)? else {
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
            bail!(
                "symlinks are not allowed in this archive: {}",
                raw.display()
            );
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
mod tests;
