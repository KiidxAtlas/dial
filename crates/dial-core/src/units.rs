//! Human-readable sizes shared by the CLI, approval prompts, storage labels and model reports.

const KIB: f64 = 1024.0;
const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// A byte count for people: `512 B` below one KiB, then one decimal in the largest binary unit
/// that keeps the value below 1024, e.g. `1.5 KiB`, `3.0 MiB`, `5.2 GiB`.
pub fn human_bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= KIB && unit < UNITS.len() - 1 {
        value /= KIB;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests;
