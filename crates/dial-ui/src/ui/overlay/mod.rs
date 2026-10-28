//! Full-screen panels (settings, model selector, providers). They draw into a ratatui `Frame` on the
//! alternate screen while open, and talk to the controller only through [`OverlayEvent`]: an overlay
//! never reads or writes configuration itself. The controller validates, persists and applies a
//! change, then reports back so the overlay can show the real outcome (including errors).
pub mod compute;
pub mod extensions;
pub mod git;
mod headers;
pub mod image_settings;
pub mod improve;
pub mod jobs;
mod model_download;
pub mod model_settings;
pub mod models;
pub mod palette;
pub mod path_browser;
pub mod providers;
pub mod settings;
pub mod shortcuts;
pub mod subagent_viewer;
pub mod subagents;

use crate::{
    config::settings_store::Scope,
    llm::model_config::{ModelSelection, ProviderSpec, UiConfig},
    ui::{
        keys::{Action, Keymap},
        status::StatusData,
        theme::Theme,
    },
};
use ratatui::text::Line;

use std::fmt;
pub enum OverlayEvent {
    None,
    Close,
    /// Set `path` (dotted config key) to `value` in `scope`. The controller answers via the overlay's
    /// `settled` method; nothing is assumed to have changed until then.
    Set {
        path: String,
        value: toml::Value,
        scope: Scope,
    },
    /// Remove the key from `scope` so the lower layer or default applies again.
    Unset {
        path: String,
        scope: Scope,
    },
    /// Make `selection` the model for a supported role in `model_config::ROLES`.
    AssignModel {
        role: String,
        selection: ModelSelection,
        scope: Scope,
    },
    /// Download the local model `value` names, then save it to `path` in `scope`. Nothing is
    /// saved when the download fails or is cancelled.
    DownloadSetting {
        path: String,
        value: String,
        scope: Scope,
    },
    /// Make `role` follow the main model again.
    ClearRole {
        role: String,
        scope: Scope,
    },
    AddProvider {
        id: String,
        spec: ProviderSpec,
        scope: Scope,
    },
    RemoveProvider {
        id: String,
        scope: Scope,
    },
    /// Install a missing dependency (see `setup::deps`) in the background, then save its settings. Ids
    /// of the form `embeddings:<model>` pull and wire a local embedding model instead (see `setup::embeddings`).
    SetupDep {
        id: String,
    },
    SaveCredential {
        provider: String,
        base_url: String,
        token: String,
    },
    OpenRouterLogin,
    AnthropicLogin,
    AnthropicSignOut,
    ChatGptLogin {
        client_id: Option<String>,
    },
    ChatGptCancel,
    ChatGptSelect {
        client_id: String,
    },
    ChatGptSignOut {
        client_id: String,
    },
    OAuthLogin {
        id: String,
    },
    OAuthSignOut {
        id: String,
    },
    /// Make a stored account of an OAuth provider the active one.
    OAuthSelectAccount {
        id: String,
        key: String,
    },
    /// Remove one stored account of an OAuth provider (the grant file goes with the last one).
    OAuthRemoveAccount {
        id: String,
        key: String,
    },
    RemoveCredential {
        base_url: String,
    },
    /// Rescan providers and models.
    Refresh,
    VerifyLora(ModelSelection),
    /// Preview a theme live without saving; `None` restores the configured theme.
    PreviewTheme(Option<String>),
    /// Validate and persist a global shortcut edit before applying it to the live keymap.
    Rebind {
        action: Action,
        chords: Vec<String>,
    },
    /// Free-form request to the application, e.g. `open:models`, `open:providers`.
    Action(String),
    /// Keystrokes or pasted text for an attached background process (raw terminal bytes).
    JobInput {
        name: String,
        bytes: Vec<u8>,
    },
}

impl fmt::Debug for OverlayEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Close => f.write_str("Close"),
            Self::SetupDep { id } => f.debug_struct("SetupDep").field("id", id).finish(),
            Self::Set { path, value, scope } => f
                .debug_struct("Set")
                .field("path", path)
                .field("value", value)
                .field("scope", scope)
                .finish(),
            Self::Unset { path, scope } => f
                .debug_struct("Unset")
                .field("path", path)
                .field("scope", scope)
                .finish(),
            Self::AssignModel {
                role,
                selection,
                scope,
            } => f
                .debug_struct("AssignModel")
                .field("role", role)
                .field("selection", selection)
                .field("scope", scope)
                .finish(),
            Self::DownloadSetting { path, value, scope } => f
                .debug_struct("DownloadSetting")
                .field("path", path)
                .field("value", value)
                .field("scope", scope)
                .finish(),
            Self::ClearRole { role, scope } => f
                .debug_struct("ClearRole")
                .field("role", role)
                .field("scope", scope)
                .finish(),
            Self::AddProvider { id, spec, scope } => f
                .debug_struct("AddProvider")
                .field("id", id)
                .field("spec", spec)
                .field("scope", scope)
                .finish(),
            Self::RemoveProvider { id, scope } => f
                .debug_struct("RemoveProvider")
                .field("id", id)
                .field("scope", scope)
                .finish(),
            Self::SaveCredential {
                provider, base_url, ..
            } => f
                .debug_struct("SaveCredential")
                .field("provider", provider)
                .field("base_url", base_url)
                .field("token", &"<redacted>")
                .finish(),
            Self::OpenRouterLogin => f.write_str("OpenRouterLogin"),
            Self::AnthropicLogin => f.write_str("AnthropicLogin"),
            Self::AnthropicSignOut => f.write_str("AnthropicSignOut"),
            Self::ChatGptLogin { .. } => f.write_str("ChatGptLogin"),
            Self::ChatGptCancel => f.write_str("ChatGptCancel"),
            Self::ChatGptSelect { .. } => f.write_str("ChatGptSelect"),
            Self::ChatGptSignOut { .. } => f.write_str("ChatGptSignOut"),
            Self::OAuthLogin { .. } => f.write_str("OAuthLogin"),
            Self::OAuthSignOut { .. } => f.write_str("OAuthSignOut"),
            Self::OAuthSelectAccount { .. } => f.write_str("OAuthSelectAccount"),
            Self::OAuthRemoveAccount { .. } => f.write_str("OAuthRemoveAccount"),
            Self::RemoveCredential { base_url } => f
                .debug_struct("RemoveCredential")
                .field("base_url", base_url)
                .finish(),
            Self::Refresh => f.write_str("Refresh"),
            Self::VerifyLora(selection) => f.debug_tuple("VerifyLora").field(selection).finish(),
            Self::PreviewTheme(name) => f.debug_tuple("PreviewTheme").field(name).finish(),
            Self::Rebind { action, chords } => f
                .debug_struct("Rebind")
                .field("action", action)
                .field("chords", chords)
                .finish(),
            Self::Action(action) => f.debug_tuple("Action").field(action).finish(),
            // Typed bytes may be secrets (passwords at a prompt): never log them.
            Self::JobInput { name, bytes } => f
                .debug_struct("JobInput")
                .field("name", name)
                .field("bytes", &bytes.len())
                .finish(),
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
