//! Published Dial releases: where they live, how this executable was installed, and how to
//! update it. `dial update` (and `/update`) installs; the interactive shell only reports that one
//! is available. [`plan`] decides how, so the command and the notice always agree.
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const REPOSITORY: &str = "KiidxAtlas/dial";

/// The version of this executable.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Ed25519 key that verifies signed release manifests. Only release builds embed it, so a source
/// build cannot self-update and does not check for releases.
pub const SIGNING_KEY: Option<&str> = option_env!("DIAL_RELEASE_PUBLIC_KEY");

/// Who owns the installed executable, judged from its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installation {
    Homebrew,
    Cargo,
    Nix,
    /// The release installer (`install.sh` / `install.ps1`) or a manual copy.
    Script,
}

impl Installation {
    pub fn of(path: &Path) -> Self {
        let text = path.to_string_lossy().replace('\\', "/");
        if text.contains("/Cellar/") || text.contains("/homebrew/") {
            Self::Homebrew
        } else if text.contains("/.cargo/bin/") {
            Self::Cargo
        } else if text.contains("/nix/store/") {
            Self::Nix
        } else {
            Self::Script
        }
    }

    pub fn current() -> Self {
        std::env::current_exe()
            .map(|path| Self::of(&path))
            .unwrap_or(Self::Script)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Homebrew => "Homebrew",
            Self::Cargo => "Cargo",
            Self::Nix => "Nix",
            Self::Script => "the release installer",
        }
    }

    /// The package-manager command that updates this installation, when Dial can run it itself.
    /// Dial is not published on crates.io, so a Cargo installation updates from the Git
    /// repository; `tag` pins that to a release.
    pub fn package_manager_argv(self, tag: Option<&str>) -> Option<Vec<String>> {
        let words: &[&str] = match self {
            Self::Homebrew => &["brew", "upgrade", "dial"],
            Self::Cargo => &["cargo", "install", "--locked", "--git"],
            Self::Nix | Self::Script => return None,
        };
        let mut argv: Vec<String> = words.iter().map(|&word| word.to_owned()).collect();
        if self == Self::Cargo {
            argv.push(format!("https://github.com/{REPOSITORY}"));
            if let Some(tag) = tag {
                argv.extend(["--tag".to_owned(), tag.to_owned()]);
            }
            argv.push("dial".to_owned());
        }
        Some(argv)
    }

    /// The command that updates this installation, for display.
    pub fn update_command(self) -> String {
        match self.package_manager_argv(None) {
            Some(argv) => argv.join(" "),
            None if self == Self::Nix => "update the dial flake input and rebuild".into(),
            None => "dial update".into(),
        }
    }
}

/// How `dial update` brings this executable up to date.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdatePlan {
    /// Download, verify and install a signed release over this executable.
    Release,
    /// Run the owning package manager (Homebrew or Cargo).
    PackageManager(Installation),
    /// Fast-forward the Git checkout this executable was built from and rebuild it (`make update`).
    Checkout(PathBuf),
    /// Dial cannot update this installation itself; the text says what to do instead.
    Manual(String),
}

/// Chooses the update method. `verified` is whether release signatures can be checked
/// ([`SIGNING_KEY`]); `checkout` is the source checkout this build came from, if it still exists.
pub fn plan(installation: Installation, verified: bool, checkout: Option<&Path>) -> UpdatePlan {
    match (installation, checkout) {
        (Installation::Homebrew, _) => UpdatePlan::PackageManager(installation),
        (Installation::Nix, _) => UpdatePlan::Manual(installation.update_command()),
        (Installation::Script, _) if verified => UpdatePlan::Release,
        // `make update` also refreshes `~/.cargo/bin/dial`, so a source build found there is
        // rebuilt from its checkout rather than replaced by `cargo install`.
        (Installation::Script | Installation::Cargo, Some(root)) => {
            UpdatePlan::Checkout(root.to_owned())
        }
        (Installation::Cargo, None) => UpdatePlan::PackageManager(installation),
        (Installation::Script, None) => UpdatePlan::Manual(format!(
            "install a signed release from https://github.com/{REPOSITORY}/releases/latest"
        )),
    }
}

/// The plan for the running executable.
pub fn current_plan() -> UpdatePlan {
    plan(
        Installation::current(),
        SIGNING_KEY.is_some(),
        source_checkout().as_deref(),
    )
}

/// The Dial source checkout this executable was compiled from, while it still exists. Package
/// builds (Homebrew's temporary build tree, CI) leave no checkout behind, and Cargo's own Git
/// checkouts belong to `cargo install`, so neither is reported.
pub fn source_checkout() -> Option<PathBuf> {
    // This crate lives at `<checkout>/crates/dial-app`.
    checkout_at(Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2)?)
}

fn checkout_at(root: &Path) -> Option<PathBuf> {
    let text = root.to_string_lossy().replace('\\', "/");
    if text.contains("/.cargo/") || text.contains("/git/checkouts/") {
        return None;
    }
    (root.join(".git").exists()
        && root.join("Makefile").is_file()
        && root.join("scripts/install-dev.py").is_file())
    .then(|| root.to_owned())
}

/// Normalizes a version or tag to a `v`-prefixed release tag and rejects anything that could
/// escape a URL path segment.
pub fn tag(version: &str) -> Result<String> {
    let tag = if version.starts_with('v') {
        version.to_owned()
    } else {
        format!("v{version}")
    };
    ensure!(
        tag.len() > 1
            && tag[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')),
        "invalid version tag"
    );
    Ok(tag)
}

/// The tag of the latest published release.
pub fn latest_tag(timeout: Duration) -> Result<String> {
    let value: serde_json::Value = ureq::get(&format!(
        "https://api.github.com/repos/{REPOSITORY}/releases/latest"
    ))
    .header("User-Agent", "dial-updater")
    .config()
    .timeout_global(Some(timeout))
    .build()
    .call()?
    .into_body()
    .read_json()?;
    let latest = value
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .context("GitHub latest release has no tag_name")?;
    tag(latest)
}

/// `major.minor.patch` and whether a pre-release suffix follows; build metadata is ignored.
fn parse(version: &str) -> Option<([u64; 3], bool)> {
    let version = version.strip_prefix('v').unwrap_or(version);
    let version = version.split('+').next()?;
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, !pre.is_empty()),
        None => (version, false),
    };
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let parsed = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some((parsed, pre))
}

/// Whether `candidate` is a strictly newer release than `current`. Unparseable versions are
/// never reported as newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse(candidate), parse(current)) {
        (Some((candidate, candidate_pre)), Some((current, current_pre))) => {
            candidate > current || (candidate == current && current_pre && !candidate_pre)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
