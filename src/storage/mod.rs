//! Persistent state: filesystem layout, sessions, long-term memory, skills, checkpoints,
//! the trajectory store and settings files.

pub mod annotations;
pub mod anonymize;
pub mod archive;
pub mod checkpoint;
pub mod codebase;
pub mod dump;
pub mod experience;
pub mod export;
pub mod foreign;
pub mod history;
pub mod image_refs;
pub mod memory;
pub mod memory_backend;
pub mod process_record;
pub mod provenance;
pub mod redact;
pub mod retention;
pub mod session;
pub mod session_pins;
pub mod share;
pub mod skills;
pub mod store;

pub(crate) mod media;
