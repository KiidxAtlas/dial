//! Changed-lines reads: when a file changed after an earlier read whose rows are still visible to
//! the model, only the lines that differ need to be sent again.
//!
//! The controller owns visibility: it passes the earlier revision and the line runs of it that are
//! still shown in full. This module maps those runs onto the current text with the same line map
//! that carries edit witnesses across versions, and plans which current lines are already known
//! (unchanged, visible in an earlier call) and which must be read. Unchanged covered lines become
//! edit witnesses of the current version, exactly as carried witnesses do after an external edit.

use super::select::{DEFAULT_READ_LINES, MAX_READ_LINE};
use crate::tools::edit::{edit_source, hashline};
use crate::tools::{Workspace, present};
use serde_json::{Value, json};

/// More covered runs than this make the mapping note cost more than it saves.
const MAX_COVERED_RUNS: usize = 24;

/// One run of current lines that is unchanged from a still-visible earlier read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoveredRun {
    /// Current 1-based inclusive lines.
    pub current: (u32, u32),
    /// The same lines' numbers in the earlier read.
    pub earlier: (u32, u32),
    pub call_id: String,
}

/// How a read of a changed file can reuse an earlier read of it.
#[derive(Debug)]
pub struct DeltaPlan {
    /// Workspace-relative path of the read target.
    pub path: String,
    /// Raw SHA-256 of the current version.
    pub revision: String,
    /// Current tag of the file after the covered lines were witnessed.
    pub tag: String,
    pub covered: Vec<CoveredRun>,
    /// Current lines that changed (or were never visible) and must be read.
    pub missing: Vec<(u32, u32)>,
}

impl DeltaPlan {
    /// The note that maps covered current lines onto the earlier call's rows.
    pub fn note(&self, earlier_revision: &str) -> String {
        let runs = self
            .covered
            .iter()
            .map(|run| {
                let (a, b) = run.current;
                let (c, d) = run.earlier;
                let current = if a == b {
                    a.to_string()
                } else {
                    format!("{a}-{b}")
                };
                let earlier = if c == d {
                    c.to_string()
                } else {
                    format!("{c}-{d}")
                };
                if current == earlier {
                    format!("{current} (call {})", run.call_id)
                } else {
                    format!("{current} = earlier {earlier} (call {})", run.call_id)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let short = earlier_revision.get(..12).unwrap_or(earlier_revision);
        format!(
            "[Read delta] {} changed since revision {short}…; current lines {runs} are unchanged and still visible there, so only the changed lines are sent. Current tag: [{}#{}]. Pass `fresh: true` for every row.",
            self.path, self.path, self.tag
        )
    }
}

impl Workspace {
    /// Plan a changed-lines read of `args` against an earlier revision whose still-visible runs
    /// are `known` (`((first, last), call_id)`, earlier line numbers). `None` when the current
    /// version is that revision, the earlier text is no longer retained, the change is too large
    /// to map, or nothing requested is covered; the caller then performs an ordinary read.
    pub fn read_delta_plan(
        &self,
        args: &Value,
        earlier_revision: &str,
        known: &[((u32, u32), String)],
    ) -> Option<DeltaPlan> {
        if args.get("fresh").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        // An unqualified whole-file read is planned as its first page of rows.
        let mut probe = args.clone();
        if !present(args, "offset") && !present(args, "limit") {
            probe["limit"] = json!(DEFAULT_READ_LINES);
        }
        let selection = self.native_read_selection(&probe)?;
        if selection.revision == earlier_revision {
            return None;
        }
        let digest = edit_source::parse_revision(earlier_revision)?;
        let path = self.resolve_read(&selection.path).ok()?;
        let earlier = self.store().text_with_digest(&path, &digest)?;
        let raw = std::fs::read_to_string(&path).ok()?;
        // The plan must describe exactly the version the selection identified; CRLF and BOM
        // sources have a projection that differs from their raw bytes, so they read normally.
        if edit_source::revision_hex(&raw) != selection.revision
            || raw.contains('\r')
            || raw.starts_with('\u{feff}')
        {
            return None;
        }
        let map = hashline::unchanged_line_map(&earlier, &raw)?;
        let lines = hashline::split_lines(&raw);
        // Current line → (earlier line, call) for unchanged lines an earlier visible read showed.
        let mut from_earlier: Vec<Option<(u32, &str)>> = vec![None; lines.len() + 1];
        for (index, current) in map.iter().enumerate() {
            let Some(current) = *current else { continue };
            let Ok(line) = u32::try_from(index + 1) else {
                continue;
            };
            // A row cut at the line limit was never shown whole, so it is not known.
            if lines[current].chars().count() > MAX_READ_LINE {
                continue;
            }
            if let Some((_, id)) = known
                .iter()
                .find(|((first, last), _)| *first <= line && line <= *last)
            {
                from_earlier[current + 1] = Some((line, id.as_str()));
            }
        }
        let mut covered: Vec<CoveredRun> = Vec::new();
        let mut missing: Vec<(u32, u32)> = Vec::new();
        for &(first, last) in &selection.ranges {
            for line in first..=last {
                match from_earlier.get(line as usize).copied().flatten() {
                    Some((earlier_line, id)) => match covered.last_mut() {
                        Some(run)
                            if run.current.1 + 1 == line
                                && run.earlier.1 + 1 == earlier_line
                                && run.call_id == id =>
                        {
                            run.current.1 = line;
                            run.earlier.1 = earlier_line;
                        }
                        _ => covered.push(CoveredRun {
                            current: (line, line),
                            earlier: (earlier_line, earlier_line),
                            call_id: id.to_owned(),
                        }),
                    },
                    None => match missing.last_mut() {
                        Some(run) if run.1 + 1 == line => run.1 = line,
                        _ => missing.push((line, line)),
                    },
                }
            }
        }
        if covered.is_empty() || covered.len() > MAX_COVERED_RUNS {
            return None;
        }
        // The covered rows are visible verbatim in the earlier call, so they witness the current
        // version just as a carried witness does.
        let witnessed: Vec<(usize, usize)> = covered
            .iter()
            .map(|run| (run.current.0 as usize, run.current.1 as usize))
            .collect();
        let tag = self.store().record_version(&path, &raw, None, &witnessed);
        Some(DeltaPlan {
            path: edit_source::display_path(selection.path),
            revision: selection.revision,
            tag,
            covered,
            missing,
        })
    }
}

#[cfg(test)]
mod tests;
