//! Terminal UI: inline (scrollback-native) rendering, themeable components, remappable keys.
//!
//! Layout of responsibilities:
//! - `term`        inline terminal driver: history rows go to the terminal's own scrollback; only the
//!                 live bottom region is repainted; overlays use the alternate screen
//! - `theme`       color/style tokens, symbol presets, theme files
//! - `keys`        remappable key bindings
//! - `composer`    the input box with the status line embedded in its border and the `/` menu
//! - `statusline`  status segments, presets and separators
//! - `transcript`  conversation blocks rendered to styled rows; `markdown`, `welcome`
//! - `overlay`     settings, model selector, providers
pub mod composer;
pub mod editor;
pub mod keys;
pub mod markdown;
pub mod overlay;
pub mod settings_schema;
pub mod status;
pub mod statusline;
pub mod term;
pub mod text;
pub mod theme;
pub mod transcript;
pub mod welcome;

/// Styled terminal rows, one `Line` per physical row (already wrapped to the target width).
pub type Rows = Vec<ratatui::text::Line<'static>>;
