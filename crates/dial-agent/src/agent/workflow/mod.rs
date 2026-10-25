//! Workflow state: todos, plan files, questions, exploration, progress, investigations and solve mode.

pub(in crate::agent) mod efficiency;
pub(in crate::agent) mod evidence;
pub mod exploration;
pub(in crate::agent) mod investigation;
pub mod plan_files;
pub(in crate::agent) mod progress;
pub mod questions;
pub(in crate::agent) mod reasoning;
pub(in crate::agent) mod recovery_checkpoint;
pub(in crate::agent) mod research;
pub mod solve;
pub mod todo;
pub(in crate::agent) mod todo_recovery;
pub(in crate::agent) mod working_state;
