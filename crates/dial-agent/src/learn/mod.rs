//! The self-improvement loop: harvest trajectories, propose and mutate harnesses, build a
//! curriculum, verify results, and admit changes only through the evaluation gate.

pub mod auto;
pub mod compute;
pub mod compute_controls;
pub mod curriculum;
pub mod distill;
pub mod download;
pub mod gate;
pub mod harvest;
pub mod improve;
pub mod mutate;
pub mod probe;
pub mod processes;
pub mod resources;
pub mod session;
pub mod signals;
pub mod verify;
pub mod worker;
pub mod workflow;

pub mod profiles;
pub mod repository;
pub mod task_policy;
