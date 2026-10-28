use serde::{Deserialize, Serialize};

#[cfg(any(test, dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
use super::error::{CoreResult, DesktopError};

/// One immediate child of a native application menu. Paths contain the actual
/// native labels, including ellipses; selection accepts normalized labels.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopMenuItem {
    pub title: String,
    pub path: Vec<String>,
    pub enabled: bool,
    pub checked: bool,
    pub has_submenu: bool,
    pub shortcut: Option<String>,
}

/// Delivery options shared with window input: omission follows task control.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct MenuSelectOptions {
    pub takeover: Option<bool>,
}

#[cfg(any(test, dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
pub(crate) fn validate_path(path: &[String], allow_empty: bool) -> CoreResult<()> {
    if (!allow_empty && path.is_empty()) || path.len() > 32 {
        return Err(DesktopError::invalid_target(
            "menu path must contain 1..32 labels",
        ));
    }
    if path
        .iter()
        .any(|label| label.trim().is_empty() || label.contains('\0'))
    {
        return Err(DesktopError::invalid_target(
            "menu path contains an empty or NUL label",
        ));
    }
    Ok(())
}

#[cfg(any(test, dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
fn normalized(label: &str) -> &str {
    let label = label.trim();
    label
        .strip_suffix("...")
        .or_else(|| label.strip_suffix('…'))
        .unwrap_or(label)
        .trim_end()
}

/// Exact case-insensitive labels win over ellipsis-normalized matches. Every
/// tier must have exactly one match: order and enabled state never break ties.
#[cfg(any(test, dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
pub(crate) fn match_index(items: &[DesktopMenuItem], label: &str) -> CoreResult<usize> {
    let folded = label.trim().to_lowercase();
    let mut exact = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.title.trim().to_lowercase() == folded);
    if let Some((index, _)) = exact.next() {
        return if exact.next().is_some() {
            Err(DesktopError::ax_failed(format!(
                "menu label '{label}' is ambiguous"
            )))
        } else {
            Ok(index)
        };
    }
    let folded = normalized(label).to_lowercase();
    let mut matches = items
        .iter()
        .enumerate()
        .filter(|(_, item)| normalized(&item.title).to_lowercase() == folded);
    match (matches.next(), matches.next()) {
        (Some((index, _)), None) => Ok(index),
        (None, _) => Err(DesktopError::ax_failed(format!(
            "menu item '{label}' was not found"
        ))),
        _ => Err(DesktopError::ax_failed(format!(
            "menu label '{label}' is ambiguous after ellipsis normalization"
        ))),
    }
}

#[cfg(any(test, dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
pub(crate) fn require_enabled(item: &DesktopMenuItem) -> CoreResult<()> {
    if item.enabled {
        Ok(())
    } else {
        Err(DesktopError::ax_failed(format!(
            "menu item '{}' is disabled; no command was dispatched",
            item.path.join(" > ")
        )))
    }
}

#[cfg(any(test, dial_desktop_macos, dial_desktop_windows, dial_desktop_linux))]
pub(crate) fn require_command(item: &DesktopMenuItem) -> CoreResult<()> {
    require_enabled(item)?;
    if item.has_submenu || item.title.trim().is_empty() {
        return Err(DesktopError::ax_failed(
            "select a named leaf menu command, not a submenu or separator",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
