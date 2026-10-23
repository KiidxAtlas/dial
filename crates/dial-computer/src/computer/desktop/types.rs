use base64::Engine;
use serde::{Deserialize, Serialize, Serializer};

fn serialize_base64<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Monitor geometry in both global logical desktop coordinates and composite
/// screenshot pixels.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopDisplay {
    pub id: String,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub pixel_x: u32,
    pub pixel_y: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub is_primary: bool,
}

/// One capturable top-level window in global logical desktop coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopWindow {
    /// Backend-defined opaque window id, valid as a capture target while the
    /// window lives. Numeric on X11/Win32/macOS; a composite AT-SPI string on
    /// Wayland (e.g. `atspi::1.31:/org/a11y/atspi/accessible/1`). Never parse
    /// it.
    pub id: String,
    /// Window title; may be empty for untitled windows.
    pub title: String,
    /// Owning application name.
    pub app: String,
    /// Owning process id when the platform exposes it.
    pub pid: Option<u32>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Whether the window currently holds input focus.
    pub focused: bool,
}

/// A bounded discovery listing; explicit ID lookup is not limited by this cap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopWindowSnapshot {
    pub windows: Vec<DesktopWindow>,
    pub truncated: bool,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopCapture {
    /// PNG-encoded image bytes; serialized as standard base64.
    #[serde(serialize_with = "serialize_base64")]
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Pre-scaling capture width in native pixels; equals `width` when unscaled.
    pub source_width: u32,
    /// Pre-scaling capture height in native pixels; equals `height` when
    /// unscaled.
    pub source_height: u32,
    /// Dimensions of the full screenshot coordinate frame used by pointer input.
    pub coordinate_width: u32,
    pub coordinate_height: u32,
    /// Region in the full screenshot's coordinates; zoom pixels are not input
    /// coordinates.
    pub region: Option<CaptureRegion>,
    pub target: String,
    pub displays: Vec<DesktopDisplay>,
    pub backend: String,
    pub display_server: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopCapabilities {
    pub backend: String,
    pub display_server: Option<String>,
    pub capture: bool,
    pub input: bool,
    pub ax: bool,
    pub background_window_input: bool,
    /// Whether window input accepts `takeover: true` (briefly activate the
    /// target and post real input).
    pub takeover: bool,
    pub applications: bool,
    pub menus: bool,
    pub held_input: bool,
    pub spaces: bool,
    /// Native global Escape cancellation while input/control ownership is held.
    /// Wayland requires the host interrupt action instead.
    pub global_escape: bool,
    pub capture_permission: String,
    pub input_permission: String,
    pub ax_permission: String,
    pub display_count: u32,
}

impl DesktopCapabilities {
    pub(crate) fn unavailable() -> Self {
        Self {
            backend: "unavailable".to_string(),
            display_server: None,
            capture: false,
            input: false,
            ax: false,
            background_window_input: false,
            takeover: false,
            applications: super::applications::supported(),
            menus: false,
            held_input: false,
            spaces: false,
            global_escape: false,
            capture_permission: "unavailable".to_string(),
            input_permission: "unavailable".to_string(),
            ax_permission: "unavailable".to_string(),
            display_count: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSessionOptions {
    pub display: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureCaps {
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
}

/// Rectangle in pixels of the most recent full screenshot of the same target.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureRegion {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PointerOptions {
    pub button: Option<String>,
    pub count: Option<u32>,
    pub modifiers: Option<Vec<String>>,
    /// Arbitrary keys held for the duration of a drag.
    pub keys: Option<Vec<String>>,
    /// Briefly activate the target window and post real input instead of the
    /// default background delivery.
    pub takeover: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HoldOptions {
    /// Duration in seconds, from zero through 100.
    pub duration: f64,
    pub button: Option<String>,
    pub keys: Option<Vec<String>>,
    pub takeover: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopControlState {
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopObservation {
    pub capture: DesktopCapture,
    pub accessibility: AxSnapshot,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxNode {
    #[serde(rename = "ref")]
    pub ref_: String,
    pub role: String,
    pub native_role: String,
    pub title: Option<String>,
    pub value: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub focused: bool,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub actions: Option<Vec<String>>,
    pub child_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxSnapshot {
    pub text: String,
    pub node_count: u32,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxSnapshotOptions {
    pub max_depth: Option<u32>,
    pub max_nodes: Option<u32>,
    pub all: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxQuery {
    pub role: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Desktop,
    /// Canonical target key, including the reserved `display:` prefix.
    Display(String),
    Window(String),
}

impl Target {
    pub(crate) fn parse(value: &str) -> Self {
        if value.eq_ignore_ascii_case("desktop") {
            Self::Desktop
        } else if let Some(selector) = value.strip_prefix("display:") {
            let selector = selector.trim();
            let selector = if selector.eq_ignore_ascii_case("active") || selector.is_empty() {
                "active"
            } else if selector.eq_ignore_ascii_case("all") {
                "all"
            } else {
                selector
            };
            Self::Display(format!("display:{selector}"))
        } else {
            Self::Window(value.to_string())
        }
    }

    pub(crate) fn key(&self) -> &str {
        match self {
            Self::Desktop => "desktop",
            Self::Window(id) | Self::Display(id) => id,
        }
    }

    pub(crate) fn display_selector(&self) -> Option<DisplaySelector> {
        match self {
            Self::Display(key) => Some(DisplaySelector::parse(Some(
                key["display:".len()..].to_string(),
            ))),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DisplaySelector {
    Active,
    All,
    Id(String),
}

impl DisplaySelector {
    pub(crate) fn parse(display: Option<String>) -> Self {
        match display.as_deref().map(str::trim) {
            None | Some("") => Self::Active,
            Some(value) if value.eq_ignore_ascii_case("active") => Self::Active,
            Some(value) if value.eq_ignore_ascii_case("all") => Self::All,
            Some(id) => Self::Id(id.to_string()),
        }
    }

    pub(crate) fn select(
        &self,
        mut displays: Vec<DesktopDisplay>,
        focused: Option<&DesktopWindow>,
    ) -> super::error::CoreResult<Vec<DesktopDisplay>> {
        match self {
            Self::All => {}
            Self::Id(id) => displays.retain(|display| display.id == *id),
            Self::Active => {
                let selected = focused
                    .and_then(|window| {
                        displays
                            .iter()
                            .enumerate()
                            .map(|(index, display)| {
                                let left = i64::from(window.x).max(i64::from(display.x));
                                let top = i64::from(window.y).max(i64::from(display.y));
                                let right = (i64::from(window.x) + i64::from(window.width))
                                    .min(i64::from(display.x) + i64::from(display.width));
                                let bottom = (i64::from(window.y) + i64::from(window.height))
                                    .min(i64::from(display.y) + i64::from(display.height));
                                let area =
                                    (right - left).max(0) as u64 * (bottom - top).max(0) as u64;
                                (index, area, display.is_primary)
                            })
                            .filter(|(_, area, _)| *area > 0)
                            .max_by_key(|(_, area, primary)| (*area, *primary))
                            .map(|(index, ..)| index)
                    })
                    .or_else(|| displays.iter().position(|display| display.is_primary))
                    .or_else(|| (!displays.is_empty()).then_some(0));
                if let Some(index) = selected {
                    let display = displays.swap_remove(index);
                    displays.clear();
                    displays.push(display);
                }
            }
        }
        if displays.is_empty() {
            return Err(match self {
                Self::Id(id) => super::error::DesktopError::invalid_target(format!(
                    "selected display id '{id}' is not active"
                )),
                Self::Active | Self::All => {
                    super::error::DesktopError::capture_failed("no active displays were found")
                }
            });
        }
        Ok(displays)
    }
}

#[cfg(test)]
mod tests;
