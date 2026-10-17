//! Stealth patch for the built-in browser.
//!
//! The in-page patch scripts under `runtimes/browser/stealth/` load in the order listed in
//! `index.json`. `bootstrap.js` is the injection wrapper with a marker where the patch
//! bodies are spliced in.

/// Environment override for the stealth setting: `"1"` forces on, `"0"` forces off.
/// Any other value (or unset) defers to the configured setting.
const STEALTH_ENV: &str = "DIAL_BROWSER_STEALTH";

/// In-page patch scripts as `(file name, body)`, in load order.
pub const STEALTH_SCRIPTS: &[(&str, &str)] = &[
    (
        "00_stealth_tampering.txt",
        include_str!("../../../../../runtimes/browser/stealth/00_stealth_tampering.txt"),
    ),
    (
        "01_stealth_activity.txt",
        include_str!("../../../../../runtimes/browser/stealth/01_stealth_activity.txt"),
    ),
    (
        "02_stealth_hairline.txt",
        include_str!("../../../../../runtimes/browser/stealth/02_stealth_hairline.txt"),
    ),
    (
        "03_stealth_botd.txt",
        include_str!("../../../../../runtimes/browser/stealth/03_stealth_botd.txt"),
    ),
    (
        "04_stealth_iframe.txt",
        include_str!("../../../../../runtimes/browser/stealth/04_stealth_iframe.txt"),
    ),
    (
        "05_stealth_webgl.txt",
        include_str!("../../../../../runtimes/browser/stealth/05_stealth_webgl.txt"),
    ),
    (
        "06_stealth_screen.txt",
        include_str!("../../../../../runtimes/browser/stealth/06_stealth_screen.txt"),
    ),
    (
        "07_stealth_fonts.txt",
        include_str!("../../../../../runtimes/browser/stealth/07_stealth_fonts.txt"),
    ),
    (
        "08_stealth_audio.txt",
        include_str!("../../../../../runtimes/browser/stealth/08_stealth_audio.txt"),
    ),
    (
        "09_stealth_locale.txt",
        include_str!("../../../../../runtimes/browser/stealth/09_stealth_locale.txt"),
    ),
    (
        "10_stealth_plugins.txt",
        include_str!("../../../../../runtimes/browser/stealth/10_stealth_plugins.txt"),
    ),
    (
        "11_stealth_hardware.txt",
        include_str!("../../../../../runtimes/browser/stealth/11_stealth_hardware.txt"),
    ),
    (
        "12_stealth_codecs.txt",
        include_str!("../../../../../runtimes/browser/stealth/12_stealth_codecs.txt"),
    ),
    (
        "13_stealth_worker.txt",
        include_str!("../../../../../runtimes/browser/stealth/13_stealth_worker.txt"),
    ),
];

const BOOTSTRAP: &str = include_str!("../../../../../runtimes/browser/stealth/bootstrap.js");
const BOOTSTRAP_SCRIPTS_MARKER: &str = "/*@@STEALTH_PATCH_SCRIPTS@@*/";

const ENABLE_AUTOMATION_FLAG: &str = "--enable-automation";

/// Chromium switches suppressed through Puppeteer/Playwright `ignoreDefaultArgs`.
const STEALTH_IGNORE_DEFAULT_ARGS: &[&str] = &[
    ENABLE_AUTOMATION_FLAG,
    "--disable-extensions",
    "--disable-default-apps",
    "--disable-component-extensions-with-background-pages",
    "--disable-popup-blocking",
    "--disable-client-side-phishing-detection",
    "--allow-pre-commit-input",
    "--disable-ipc-flooding-protection",
    "--metrics-recording-only",
];

/// Whether the stealth patch is active. `setting` is the configured value; the
/// `DIAL_BROWSER_STEALTH` environment variable (`"0"`/`"1"`) overrides it.
pub fn enabled_from_config(setting: bool) -> bool {
    resolve_enabled(setting, std::env::var(STEALTH_ENV).ok().as_deref())
}

fn resolve_enabled(setting: bool, env: Option<&str>) -> bool {
    match env.map(str::trim) {
        Some("1") => true,
        Some("0") => false,
        _ => setting,
    }
}

/// Chromium launch args that suppress automation signals: `--disable-blink-features=AutomationControlled`.
pub fn launch_args() -> Vec<&'static str> {
    vec!["--disable-blink-features=AutomationControlled"]
}

/// Default Chromium switches to drop at launch.
/// Microsoft Edge keeps `--enable-automation` because it can exit before CDP opens
/// when that switch is stripped.
pub fn ignored_default_args(executable_path: Option<&str>) -> Vec<&'static str> {
    let edge = is_microsoft_edge_executable(executable_path);
    STEALTH_IGNORE_DEFAULT_ARGS
        .iter()
        .copied()
        .filter(|arg| !(edge && *arg == ENABLE_AUTOMATION_FLAG))
        .collect()
}

fn is_microsoft_edge_executable(executable_path: Option<&str>) -> bool {
    let Some(path) = executable_path.filter(|path| !path.is_empty()) else {
        return false;
    };
    let normalized = path.replace('\\', "/").to_lowercase();
    let name = normalized.rsplit('/').next().unwrap_or("");
    name == "msedge.exe" || name == "microsoft edge" || name.starts_with("microsoft-edge")
}

/// Page-side bootstrap source for `addInitScript` / `evaluateOnNewDocument`: the
/// wrapper with every entry of [`STEALTH_SCRIPTS`] spliced in load order, each in
/// its own `try`/`catch` so one failing patch does not stop the rest.
pub fn injection_script() -> String {
    let joint = STEALTH_SCRIPTS
        .iter()
        .map(|(_, body)| format!("\n\t\ttry {{\n\t\t\t{body};\n\t\t}} catch (e) {{}}\n\t"))
        .collect::<Vec<_>>()
        .join(";\n");
    BOOTSTRAP.replace(BOOTSTRAP_SCRIPTS_MARKER, &joint)
}

#[cfg(test)]
mod tests;
