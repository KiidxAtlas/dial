//! Progress test for automatic verification repairs. A repair round that leaves the same confirmed
//! diagnosis outstanding and every declared input unchanged stops with the failure still visible.
//! Changed inputs still get verified; stopping never earns a pass. The effect epoch alone is not
//! evidence of a change: a reverted edit moves it without changing any declared input.
//! Infrastructure, permission and unverified problems never reach this test: they are absent from
//! `CheckRun::actionable`, and no acceptance condition is weakened here.
use std::hash::{Hash, Hasher};

/// The state a repair round must change to be worth another round. `I` is one declared input
/// identity per check, `None` where a check declares no complete contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Fingerprint<I> {
    diagnosis: u64,
    revision: [u64; 3],
    inputs: Vec<Option<I>>,
}

impl<I: Clone + PartialEq> Fingerprint<I> {
    pub(super) fn new(diagnoses: &[String], revision: [u64; 3], inputs: Vec<Option<I>>) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        diagnoses.hash(&mut hasher);
        Self {
            diagnosis: hasher.finish(),
            revision,
            inputs,
        }
    }

    /// The same diagnosis remains and nothing a rerun reads moved. When every check declares its
    /// inputs, those identities decide alone, so an edit that leaves them unchanged is no progress.
    /// Otherwise an undeclared input is unknown, and only the effect epoch can show that it moved.
    pub(super) fn repeats(&self, after: &Self) -> bool {
        if self.diagnosis != after.diagnosis || self.inputs != after.inputs {
            return false;
        }
        let declared = !self.inputs.is_empty() && self.inputs.iter().all(Option::is_some);
        declared || self.revision == after.revision
    }

    /// Stable, bounded identifier of the diagnosis for the journal.
    pub(super) fn key(&self) -> String {
        format!("{:016x}", self.diagnosis)
    }
}

#[cfg(test)]
mod tests;
