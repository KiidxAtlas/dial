//! Child agents and workers: subagents, delegation, live tasks, mailboxes, isolation and development copies.

pub(in crate::agent) mod child;
pub mod child_transcript;
pub(in crate::agent) mod delegation;
pub(in crate::agent) mod development_copy;
pub(crate) mod isolation;
pub(in crate::agent) mod live_tasks;
pub(crate) mod mailbox;
pub mod subagents;
pub(in crate::agent) mod task_packet;
pub(in crate::agent) mod task_slots;
pub(in crate::agent) mod worker_resume;
