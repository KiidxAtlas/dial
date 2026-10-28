//! Agent-to-UI surface: UI events, chat text, the UI trait and state-aware UI.

pub(in crate::agent) mod chat;
mod request_timing;
pub(in crate::agent) mod state_ui;
pub(in crate::agent) mod surface;
pub mod ui_event;
