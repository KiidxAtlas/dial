//! Terminal UI: inline (scrollback-native) rendering, themeable components, remappable keys.
//!
//! Layout of responsibilities:
//! - `term`        inline terminal driver: history rows go to the terminal's own scrollback; only the
//!   live bottom region is repainted; overlays use the alternate screen
//! - `theme`       color/style tokens, symbol presets, theme files
//! - `keys`        remappable key bindings
//! - `composer`    the input box with the status line embedded in its border and the `/` menu
//! - `statusline`  status segments, presets and separators
//! - `transcript`  conversation blocks rendered to styled rows; `markdown`, `welcome`
//! - `tasks`       the live todo panel pinned above the composer
//! - `overlay`     settings, model selector, providers
pub mod chrome;
mod clipboard;
pub mod command_surface;
pub mod composer;
pub mod editor;
pub mod fx;
pub mod images;
pub mod input;
pub mod keys;
mod lanes;
pub mod markdown;
mod notice;
pub mod overlay;
pub mod panel;
pub mod plugin_slots;
pub mod questions;
pub mod rail;
pub mod recovery;
pub mod session_picker;
pub mod status;
pub mod statusline;
pub mod subagent_hub;
pub mod subagent_model;
pub mod tasks;
pub mod term;
pub mod term_links;
pub mod term_state;
pub mod termcaps;
pub mod text;
pub mod theme;
pub mod tool_cards;
pub mod transcript;
pub mod tree_picker;
pub mod welcome;
pub mod word_complete;
pub mod worker_pins;

/// Styled terminal rows, one `Line` per physical row (already wrapped to the target width).
pub type Rows = Vec<ratatui::text::Line<'static>>;
