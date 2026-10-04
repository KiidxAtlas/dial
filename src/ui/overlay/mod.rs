//! Full-screen panels (settings, model selector, providers). They draw into a ratatui `Frame` on the
//! alternate screen while open, and talk to the controller only through [`OverlayEvent`]: an overlay
//! never reads or writes configuration itself. The controller validates, persists and applies a
//! change, then reports back so the overlay can show the real outcome (including errors).
pub mod models;
pub mod providers;
pub mod settings;

use crate::{
    llm::model_config::{ModelSelection, ProviderSpec, UiConfig},
    storage::settings_store::Scope,
    ui::{keys::Keymap, status::StatusData, theme::Theme},
};
use ratatui::text::Line;

use std::fmt;
pub enum OverlayEvent {
    None,
    Close,
    /// Set `path` (dotted config key) to `value` in `scope`. The controller answers via the overlay's
    /// `settled` method; nothing is assumed to have changed until then.
    Set { path: String, value: toml::Value, scope: Scope },
    /// Remove the key from `scope` so the lower layer or default applies again.
    Unset { path: String, scope: Scope },
    /// Make `selection` the model for `role` (`main`, `feedback`, `subagent`, `compaction`).
    AssignModel { role: String, selection: ModelSelection, scope: Scope },
    /// Make `role` follow the main model again.
    ClearRole { role: String, scope: Scope },
    AddProvider { id: String, spec: ProviderSpec, scope: Scope },
    RemoveProvider { id: String, scope: Scope },
    SaveCredential { provider: String, base_url: String, token: String },
    OpenRouterLogin,
    ChatGptLogin { client_id: Option<String> },
    ChatGptCancel,
    ChatGptSelect { client_id: String },
    ChatGptSignOut { client_id: String },
    RemoveCredential { base_url: String },
    /// Rescan providers and models.
    Refresh,
    /// Preview a theme live without saving; `None` restores the configured theme.
    PreviewTheme(Option<String>),
    /// Free-form request to the application, e.g. `open:models`, `open:providers`.
    Action(String),
}

impl fmt::Debug for OverlayEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Close => f.write_str("Close"),
            Self::Set { path, value, scope } => f.debug_struct("Set").field("path", path).field("value", value).field("scope", scope).finish(),
            Self::Unset { path, scope } => f.debug_struct("Unset").field("path", path).field("scope", scope).finish(),
            Self::AssignModel { role, selection, scope } => f.debug_struct("AssignModel").field("role", role).field("selection", selection).field("scope", scope).finish(),
            Self::ClearRole { role, scope } => f.debug_struct("ClearRole").field("role", role).field("scope", scope).finish(),
            Self::AddProvider { id, spec, scope } => f.debug_struct("AddProvider").field("id", id).field("spec", spec).field("scope", scope).finish(),
            Self::RemoveProvider { id, scope } => f.debug_struct("RemoveProvider").field("id", id).field("scope", scope).finish(),
            Self::SaveCredential { provider, base_url, .. } => f.debug_struct("SaveCredential").field("provider", provider).field("base_url", base_url).field("token", &"<redacted>").finish(),
            Self::OpenRouterLogin => f.write_str("OpenRouterLogin"),
            Self::ChatGptLogin { .. } => f.write_str("ChatGptLogin"),
            Self::ChatGptCancel => f.write_str("ChatGptCancel"),
            Self::ChatGptSelect { .. } => f.write_str("ChatGptSelect"),
            Self::ChatGptSignOut { .. } => f.write_str("ChatGptSignOut"),
            Self::RemoveCredential { base_url } => f.debug_struct("RemoveCredential").field("base_url", base_url).finish(),
            Self::Refresh => f.write_str("Refresh"),
            Self::PreviewTheme(name) => f.debug_tuple("PreviewTheme").field(name).finish(),
            Self::Action(action) => f.debug_tuple("Action").field(action).finish(),
        }
    }
}

/// Everything an overlay needs to draw consistently with the rest of the UI.
pub struct DrawCtx<'a> {
    pub theme: &'a Theme,
    pub keys: &'a Keymap,
    pub status: &'a StatusData,
    /// Renders the composer status line for a hypothetical config at a width, so the settings panel
    /// can show a live preview of status-line options before they are saved.
    pub preview_status: &'a dyn Fn(&UiConfig, u16) -> Line<'static>,
}
