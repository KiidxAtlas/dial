//! Folded stacks: integer sampling weights, not durations.
use super::{MAX_RECORDS, MAX_TEXT};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn inspect(text: &str, limit: usize) -> Result<Value> {
    let mut functions: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    let mut total = 0u64;
    let mut stacks = 0usize;
    let mut blank_lines = 0usize;
    for (index, line) in text.lines().enumerate() {
        ensure!(index < MAX_RECORDS, "folded line count exceeds 20000");
        if line.trim().is_empty() {
            blank_lines += 1;
            continue;
        }
        let (stack, weight) = line
            .rsplit_once(' ')
            .context("folded line requires stack SPACE integer weight")?;
        ensure!(
            !weight.is_empty() && weight.bytes().all(|b| b.is_ascii_digit()),
            "folded weight must be a nonnegative integer"
        );
        let weight: u64 = weight.parse().context("folded weight exceeds u64")?;
        let frames: Vec<_> = stack.split(';').collect();
        ensure!(frames.len() <= 256, "folded stack exceeds 256 frames");
        ensure!(
            frames.iter().all(|s| !s.is_empty() && s.len() <= MAX_TEXT),
            "folded frames must contain 1..=4096 bytes"
        );
        total = total
            .checked_add(weight)
            .context("total folded weight overflow")?;
        let leaf = frames.last().context("empty folded stack")?;
        // Recursion must not inflate a function's share of sampled stack weight.
        let unique: BTreeSet<_> = frames.iter().copied().collect();
        for frame in unique {
            let counts = functions.entry(frame).or_default();
            counts.0 = counts
                .0
                .checked_add(weight)
                .context("inclusive folded weight overflow")?;
            if frame == *leaf {
                counts.1 = counts
                    .1
                    .checked_add(weight)
                    .context("self folded weight overflow")?;
            }
        }
        stacks += 1;
    }
    let count = functions.len();
    let mut rows: Vec<_> = functions.into_iter().collect();
    rows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
    let rows: Vec<_> = rows.into_iter().take(limit).map(|(name,(inclusive,own))| json!({"function":name,"inclusive_weight":inclusive.to_string(),"self_weight":own.to_string()})).collect();
    Ok(
        json!({"format":"folded_stacks","weight_unit":"source_integer_count_not_wall_time","total_weight":total.to_string(),"functions":rows,
        "coverage":{"stack_records":stacks,"blank_lines":blank_lines,"functions":count,"rows_omitted":count.saturating_sub(limit)},
        "limitations":["Inclusive weight counts a function once per stack even if recursive; self weight is assigned only to the leaf. Inclusive weights overlap and must not be summed as total CPU time.","No sampling period, clock, thread identity, source symbol resolution or wall-time conversion is inferred. Frame text is preserved as an untrusted source label."]}),
    )
}
