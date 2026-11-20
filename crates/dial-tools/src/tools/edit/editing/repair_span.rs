//! Isolate model-visible edit source without changing the proposal on disk.
use super::EditRepairRequest;
use similar::{DiffTag, TextDiff};
use std::{
    ops::Range,
    path::Path,
    time::{Duration, Instant},
};

/// Byte ranges on complete line boundaries in the two versions of an edit.
#[derive(Debug)]
pub struct RepairSpan {
    pub before: Range<usize>,
    pub proposed: Range<usize>,
}

impl EditRepairRequest {
    /// Find a single changed hunk whose local undo restores parsing of the complete proposal.
    /// A one-hunk edit needs no trial parse. Ambiguous or expensive multi-hunk trials skip the
    /// helper rather than sending the union of unrelated edits. Never mutates either source.
    pub fn repair_span(&self) -> Result<RepairSpan, &'static str> {
        let equal_bytes = |(a, b): &(&str, &str)| a == b;
        let prefix: usize = self
            .before
            .split_inclusive('\n')
            .zip(self.proposed.split_inclusive('\n'))
            .take_while(equal_bytes)
            .map(|(s, _)| s.len())
            .sum();
        let old = &self.before[prefix..];
        let new = &self.proposed[prefix..];
        let suffix: usize = old
            .split_inclusive('\n')
            .rev()
            .zip(new.split_inclusive('\n').rev())
            .take_while(equal_bytes)
            .map(|(s, _)| s.len())
            .sum();
        let old = &old[..old.len() - suffix];
        let new = &new[..new.len() - suffix];
        // Bound the line diff before tokenization, independently of the model input limit.
        if old.len().max(new.len()) > 1024 * 1024 {
            return Err("change too large to isolate for repair");
        }
        let timeout = Duration::from_millis(100);
        let started = Instant::now();
        let diff = TextDiff::configure().timeout(timeout).diff_lines(old, new);
        // A timed-out diff may coalesce separate changes into one coarse replacement.
        if started.elapsed() >= timeout {
            return Err("line diff exceeded edit repair budget");
        }
        let changes: Vec<_> = diff
            .ops()
            .iter()
            .filter(|op| op.tag() != DiffTag::Equal)
            .take(9)
            .collect();
        if changes.len() == 1 {
            return Ok(RepairSpan {
                before: prefix..prefix + old.len(),
                proposed: prefix..prefix + new.len(),
            });
        }
        if changes.is_empty() || changes.len() > 8 {
            return Err("no isolated edit repair within parse budget");
        }
        let byte_range = |lines: &[&str], range: Range<usize>| {
            let start = prefix + lines[..range.start].iter().map(|s| s.len()).sum::<usize>();
            start..start + lines[range].iter().map(|s| s.len()).sum::<usize>()
        };
        let mut found = None;
        let mut remaining_bytes = 4 * 1024 * 1024usize;
        for op in changes {
            let before = byte_range(diff.old_slices(), op.old_range());
            let proposed = byte_range(diff.new_slices(), op.new_range());
            let trial_len = self.proposed.len() - proposed.len() + before.len();
            remaining_bytes = remaining_bytes
                .checked_sub(trial_len)
                .ok_or("no isolated edit repair within parse budget")?;
            let mut trial = self.proposed.clone();
            trial.replace_range(proposed.clone(), &self.before[before.clone()]);
            if super::super::syntax::report(Path::new(&self.path), &trial)["valid"] == true {
                if found.is_some() {
                    return Err("more than one edit can undo the parse regression");
                }
                found = Some(RepairSpan { before, proposed });
            }
        }
        found.ok_or("no single edit isolates the parse regression")
    }
}
