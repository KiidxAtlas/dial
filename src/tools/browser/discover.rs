//! Fallback discovery of an already-installed Chromium.
//!
//! Used when the Chromium build the configured Playwright expects is missing (for example a
//! `browser.playwright_module` pinned to an older Playwright whose `chromium-<rev>` was never
//! downloaded). Complete Playwright builds in the known browser caches win, newest revision
//! first; an installed Chrome, Chromium or Edge is the last resort.

use std::path::{Path, PathBuf};

/// Executables inside a `chromium-<rev>/chrome-<platform>` directory, per platform.
#[cfg(target_os = "macos")]
const BUILD_EXECUTABLES: &[&str] = &[
    "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    "Chromium.app/Contents/MacOS/Chromium",
];
#[cfg(windows)]
const BUILD_EXECUTABLES: &[&str] = &["chrome.exe"];
#[cfg(not(any(target_os = "macos", windows)))]
const BUILD_EXECUTABLES: &[&str] = &["chrome"];

/// Playwright writes this marker once a browser download is fully extracted.
const COMPLETE_MARKER: &str = "INSTALLATION_COMPLETE";

/// First installed Chromium: the newest complete Playwright build under `roots`, else the first
/// existing system browser.
pub(super) fn installed_chromium(roots: &[PathBuf]) -> Option<PathBuf> {
    find(roots, &system_browsers())
}

fn find(roots: &[PathBuf], system: &[PathBuf]) -> Option<PathBuf> {
    let mut builds = roots
        .iter()
        .flat_map(|root| std::fs::read_dir(root).into_iter().flatten().flatten())
        .filter_map(|entry| {
            let name = entry.file_name();
            let revision = name
                .to_str()?
                .strip_prefix("chromium-")?
                .parse::<u32>()
                .ok()?;
            Some((revision, entry.path()))
        })
        .collect::<Vec<_>>();
    builds.sort_by_key(|build| std::cmp::Reverse(build.0));
    builds
        .iter()
        .filter(|(_, dir)| dir.join(COMPLETE_MARKER).is_file())
        .find_map(|(_, dir)| build_executable(dir))
        .or_else(|| system.iter().find(|path| path.is_file()).cloned())
}

fn build_executable(build: &Path) -> Option<PathBuf> {
    std::fs::read_dir(build)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("chrome-"))
        .flat_map(|entry| {
            BUILD_EXECUTABLES
                .iter()
                .map(move |exe| entry.path().join(exe))
        })
        .find(|path| path.is_file())
}

/// Playwright browser caches to search: dial's private runtime, `PLAYWRIGHT_BROWSERS_PATH` and
/// Playwright's per-user default cache.
pub(super) fn playwright_roots(runtime_browsers: &Path) -> Vec<PathBuf> {
    let mut roots = vec![runtime_browsers.to_path_buf()];
    if let Some(path) = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH").filter(|p| p != "0") {
        roots.push(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let default = if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library/Caches/ms-playwright"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("ms-playwright"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".cache")))
            .map(|p| p.join("ms-playwright"))
    };
    roots.extend(default);
    roots.dedup();
    roots
}

fn system_browsers() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if cfg!(target_os = "macos") {
        let apps = [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ];
        let mut bases = vec![PathBuf::from("/Applications")];
        bases.extend(std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Applications")));
        for base in &bases {
            paths.extend(apps.iter().map(|app| base.join(app)));
        }
    } else if cfg!(windows) {
        let suffixes = [
            r"Google\Chrome\Application\chrome.exe",
            r"Microsoft\Edge\Application\msedge.exe",
        ];
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(var) {
                paths.extend(suffixes.iter().map(|s| PathBuf::from(&base).join(s)));
            }
        }
    } else {
        for name in [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ] {
            for dir in ["/usr/bin", "/usr/local/bin", "/snap/bin"] {
                paths.push(Path::new(dir).join(name));
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests;
