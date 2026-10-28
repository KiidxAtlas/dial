//! Binary-only command implementations dispatched from `dispatch.rs`. They are not part of the
//! library facade: `dial` the library never reaches them.
pub(crate) mod auth;
pub mod benchmark;
pub(crate) mod doctor;
pub mod improve;
pub(crate) mod inspection;
pub(crate) mod laya;
pub(crate) mod schedule;
pub(crate) mod sessions;
pub(crate) mod skills;
pub(crate) mod steering;
pub mod update;
