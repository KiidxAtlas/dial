//! Binary-only command implementations dispatched from `main.rs`. They are not part of the
//! library facade: `dial` the library never reaches them.
pub mod benchmark;
pub mod improve;
pub mod update;
