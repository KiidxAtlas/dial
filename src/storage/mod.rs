//! Persistent state: filesystem layout, sessions, long-term memory, skills, checkpoints,
//! the trajectory store and settings files.

pub mod annotations;
pub mod anonymize;
pub mod archive;
pub mod artifact;
pub mod checkpoint;
pub mod dump;
pub mod experience;
pub mod export;
pub mod foreign;
pub mod image_refs;
pub mod memory;
pub mod memory_backend;
pub mod redact;
pub mod repair_evidence;
pub mod retention;
pub mod session;
pub mod session_pins;
pub mod share;
pub mod skills;
pub mod store;
pub mod transcript;

pub(crate) mod media;
