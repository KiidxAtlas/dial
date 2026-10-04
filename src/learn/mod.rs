//! The self-improvement loop: harvest trajectories, propose and mutate harnesses, build a
//! curriculum, verify results, and admit changes only through the evaluation gate.

pub mod auto;
pub mod curriculum;
pub mod gate;
pub mod harvest;
pub mod improve;
pub mod mutate;
pub mod probe;
pub mod verify;
pub mod worker;
