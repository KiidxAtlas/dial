//! Verification and repair: completion checks, verification receipts, repair and mutation-cycle guards.

pub(in crate::agent) mod auto_escalation;
pub(in crate::agent) mod checks;
pub(in crate::agent) mod completion;
pub(in crate::agent) mod evidence;
pub(in crate::agent) mod mutation_cycles;
pub(in crate::agent) mod repair;
pub(crate) mod repair_evidence;
pub(in crate::agent) mod requirements;
pub(in crate::agent) mod reviewworkflow;
pub(in crate::agent) mod verification;
pub(in crate::agent) mod verification_receipts;
pub(in crate::agent) mod verified_examples;
pub(in crate::agent) mod visual;
