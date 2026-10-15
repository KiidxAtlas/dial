//! Installed application discovery does not require capture or input
//! permission.

#[cfg(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::error::{CoreResult, DesktopError};

#[cfg(dial_desktop_linux)]
#[path = "linux/applications.rs"]
mod platform;
#[cfg(dial_desktop_macos)]
#[path = "macos/applications.rs"]
mod platform;
#[cfg(dial_desktop_windows)]
#[path = "win32/applications.rs"]
mod platform;

/// Installed application identity with currently observable process
/// information.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Application {
    pub id: String,
    pub name: String,
    pub path: String,
    pub running: bool,
    pub pid: Option<u32>,
}

/// Filters the native application inventory without requiring screen access.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationQuery {
    pub query: Option<String>,
    pub running_only: Option<bool>,
}

/// Controls whether launching an application deliberately activates it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationOpenOptions {
    pub activate: Option<bool>,
}

pub(crate) const fn supported() -> bool {
    cfg!(any(
        dial_desktop_macos,
        dial_desktop_windows,
        dial_desktop_linux
    ))
}

#[cfg(not(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux)))]
pub(crate) fn list(_options: ApplicationQuery) -> CoreResult<Vec<Application>> {
    Err(DesktopError::native_unavailable(
        "installed application discovery",
    ))
}

#[cfg(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
pub(crate) fn list(options: ApplicationQuery) -> CoreResult<Vec<Application>> {
    let mut apps = platform::list()?;
    let query = options.query.as_deref().unwrap_or_default().to_lowercase();
    apps.retain(|app| {
        (!options.running_only.unwrap_or(false) || app.running)
            && (query.is_empty()
                || app.name.to_lowercase().contains(&query)
                || app.id.to_lowercase().contains(&query)
                || app.path.to_lowercase().contains(&query))
    });
    apps.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.id.cmp(&b.id))
            .then(a.path.cmp(&b.path))
    });
    Ok(apps)
}

/// Resolve identity before display name; ambiguity never launches an arbitrary
/// app.
#[cfg(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux, test))]
fn resolve<'a>(apps: &'a [Application], input: &str) -> CoreResult<Option<&'a Application>> {
    let mut exact = apps
        .iter()
        .filter(|app| app.id == input || app.path == input);
    if let Some(first) = exact.next() {
        return unique(first, exact.next(), input).map(Some);
    }
    let folded = input.to_lowercase();
    let mut names = apps.iter().filter(|app| app.name.to_lowercase() == folded);
    names
        .next()
        .map(|first| unique(first, names.next(), input))
        .transpose()
}

#[cfg(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux, test))]
fn unique<'a>(
    first: &'a Application,
    second: Option<&Application>,
    input: &str,
) -> CoreResult<&'a Application> {
    if let Some(second) = second {
        return Err(DesktopError::invalid_target(format!(
            "application {input:?} is ambiguous; use an exact native path (matches include {:?} and \
			 {:?})",
            first.path, second.path,
        )));
    }
    Ok(first)
}

pub(crate) fn open(input: &str, options: ApplicationOpenOptions) -> CoreResult<Application> {
    if input.is_empty() || input.contains('\0') {
        return Err(DesktopError::invalid_target(
            "application identity must be nonempty and contain no NUL",
        ));
    }
    #[cfg(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
    {
        let apps = platform::list()?;
        // Native paths are checked before display names so a real package path is
        // never mistaken for a similarly named installed application.
        let app = if apps.iter().any(|app| app.id == input || app.path == input) {
            resolve(&apps, input)?
                .cloned()
                .ok_or_else(|| DesktopError::invalid_target("application disappeared"))?
        } else if Path::new(input).is_absolute() || input.contains(std::path::MAIN_SEPARATOR) {
            platform::from_path(Path::new(input))?
        } else if let Some(app) = resolve(&apps, input)?.cloned() {
            app
        } else if Path::new(input).exists() {
            platform::from_path(Path::new(input))?
        } else {
            return Err(DesktopError::invalid_target(format!(
                "application {input:?} was not found; use apps.list() or an existing native \
				 application path"
            )));
        };
        platform::open(app, options.activate.unwrap_or(false))
    }
    #[cfg(not(any(dial_desktop_macos, dial_desktop_windows, dial_desktop_linux)))]
    {
        let _ = options;
        Err(DesktopError::native_unavailable(
            "native application launch",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, path: &str) -> Application {
        Application {
            id: id.into(),
            name: name.into(),
            path: path.into(),
            running: false,
            pid: None,
        }
    }

    #[test]
    fn computer_identity_precedes_name_and_names_are_case_insensitive() {
        let apps = [
            app("org.example.Editor", "Edit", "/A.app"),
            app("other", "org.example.Editor", "/B.app"),
        ];
        assert_eq!(
            resolve(&apps, "org.example.Editor").unwrap().unwrap().path,
            "/A.app"
        );
        assert_eq!(
            resolve(&apps, "eDiT").unwrap().unwrap().id,
            "org.example.Editor"
        );
        assert!(resolve(&apps, "missing").unwrap().is_none());
    }

    #[test]
    fn computer_duplicate_names_and_bundle_id_copies_require_path() {
        let apps = [
            app("same", "Editor", "/A.app"),
            app("same", "editor", "/B.app"),
        ];
        assert!(resolve(&apps, "EDITOR").is_err());
        assert!(resolve(&apps, "same").is_err());
        assert_eq!(resolve(&apps, "/B.app").unwrap().unwrap().path, "/B.app");
    }
}
