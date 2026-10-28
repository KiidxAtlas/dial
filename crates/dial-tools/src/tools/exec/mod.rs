//! Command execution: background jobs, sandboxed shell and bash, eval/kernel cells and their preludes.

pub mod background;
pub mod bash_interceptor;
pub mod cargo_target;
pub mod eval;
pub mod execution;
pub mod git_guard;
pub mod kernel;
pub mod minimizer;
pub mod preludes;
pub mod sandbox;
pub mod shell;
pub mod signals;
pub mod speculation;
