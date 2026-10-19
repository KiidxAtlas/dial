//! Turn execution: the step loop, dispatch of model tool calls, call bookkeeping and steering.

pub(in crate::agent) mod call_summary;
pub mod calls;
pub(in crate::agent) mod dispatch;
pub(in crate::agent) mod edit_runtime;
pub(in crate::agent) mod engine;
pub(in crate::agent) mod failure_guidance;
pub(in crate::agent) mod steering;
pub(in crate::agent) mod tool_compat;
pub(in crate::agent) mod tool_exposure;
pub(in crate::agent) mod tool_runner;
pub(in crate::agent) mod turn;
pub(in crate::agent) mod turn_loop;
pub(in crate::agent) mod turn_recovery;
