//! Built-in agent instruction sources, grouped by purpose.
//!
//! These modules own wording, not authority: runtime owners select and assemble prompts,
//! enforce budgets and approvals, and keep evidence separate from instructions.
//! See `README.md` for the ownership index and editing/verification workflow.

pub(crate) mod compaction;
pub(crate) mod modes;
pub(crate) mod recovery;
pub(crate) mod system;
pub(crate) mod turn_drafter;
