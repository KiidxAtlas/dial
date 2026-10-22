//! Pure parts of `/review`: diff chunking, validation of reviewer output, merging, verdict and
//! rendering. Nothing here touches the model, git or the workspace, so every rule is unit-tested.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{collections::HashSet, ops::Range, path::Path};

/// Largest slice of the captured diff given to one reviewer task.
pub(super) const CHUNK_BYTES: usize = 20_000;

const NAMES: [&str; 4] = ["critical", "high", "normal", "low"];
const STALE: &str = "WARNING: target changed or its current identity could not be rechecked; findings describe the captured snapshot only.";
const FRESH: &str =
    "Target identity unchanged at completion; this is review, not test verification.";

/// A line that starts a new unit of review: one file's diff, or the unstaged half of a snapshot.
fn boundary(line: &str) -> bool {
    line.starts_with("diff --git ") || line.starts_with("UNSTAGED SNAPSHOT:")
}

/// Contiguous byte ranges covering all of `text`. Whole files are packed together up to `limit`
/// bytes; only a single file larger than `limit` is cut by bytes (on a character boundary).
pub(super) fn chunk_diff(text: &str, limit: usize) -> Vec<Range<usize>> {
    let mut starts = vec![0];
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if offset > 0 && boundary(line) {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts.push(text.len());
    let mut chunks = Vec::new();
    let mut begin = 0;
    for pair in starts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if end - start > limit {
            if begin < start {
                chunks.push(begin..start);
            }
            let mut at = start;
            while at < end {
                let mut stop = (at + limit).min(end);
                while !text.is_char_boundary(stop) {
                    stop -= 1;
                }
                if stop == at {
                    stop = at + 1;
                    while !text.is_char_boundary(stop) {
                        stop += 1;
                    }
                }
                chunks.push(at..stop);
                at = stop;
            }
            begin = end;
        } else if end - begin > limit {
            chunks.push(begin..start);
            begin = start;
        }
    }
    if begin < text.len() {
        chunks.push(begin..text.len());
    }
    chunks
}

/// One reviewed chunk: the reviewer's verdict and validated findings, or a failure note.
pub(super) struct ChunkReview {
    pub range: Range<usize>,
    /// `findings`, `no_findings` or `inconclusive`.
    pub verdict: String,
    pub summary: String,
    pub findings: Vec<Value>,
}

impl ChunkReview {
    /// A chunk whose reviewer failed or was incomplete: it cannot support a clean verdict.
    pub(super) fn failed(range: Range<usize>, why: impl Into<String>) -> Self {
        Self {
            range,
            verdict: "inconclusive".into(),
            summary: why.into(),
            findings: Vec::new(),
        }
    }

    pub(super) fn inconclusive(&self) -> bool {
        self.verdict == "inconclusive"
    }
}

/// Validate one reviewer answer against the captured diff and stamp the target identity.
pub(super) fn parse_chunk(
    range: Range<usize>,
    mut value: Value,
    captured: &str,
    identity: &str,
) -> Result<ChunkReview> {
    for finding in value["findings"]
        .as_array_mut()
        .context("Missing findings")?
    {
        let path = finding["path"].as_str().context("Missing finding path")?;
        anyhow::ensure!(
            !Path::new(path).is_absolute()
                && !Path::new(path)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "Finding path is outside target"
        );
        anyhow::ensure!(
            captured.contains(&format!("+++ b/{path}\n"))
                || captured.contains(&format!("--- a/{path}\n")),
            "Finding path is not in the captured diff"
        );
        anyhow::ensure!(
            finding["end_line"].as_u64() >= finding["start_line"].as_u64(),
            "Finding range is reversed"
        );
        finding["target_identity"] = json!(identity);
    }
    anyhow::ensure!(
        value["verdict"] != "no_findings"
            || value["findings"].as_array().is_some_and(Vec::is_empty),
        "No-findings verdict conflicts with findings"
    );
    let findings = match value["findings"].take() {
        Value::Array(findings) => findings,
        _ => Vec::new(),
    };
    Ok(ChunkReview {
        range,
        verdict: value["verdict"]
            .as_str()
            .unwrap_or("inconclusive")
            .to_owned(),
        summary: value["summary"].as_str().unwrap_or_default().to_owned(),
        findings,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    DoNotShip,
    Inconclusive,
    ShipWithFixes,
    Ship,
}

impl Verdict {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::DoNotShip => "do not ship",
            Self::Inconclusive => "inconclusive",
            Self::ShipWithFixes => "ship with fixes",
            Self::Ship => "ship",
        }
    }

    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::DoNotShip => "do_not_ship",
            Self::Inconclusive => "inconclusive",
            Self::ShipWithFixes => "ship_with_fixes",
            Self::Ship => "ship",
        }
    }
}

fn priority(finding: &Value) -> u64 {
    finding["priority"].as_u64().unwrap_or(3).min(3)
}

fn confidence(finding: &Value) -> f64 {
    finding["confidence"].as_f64().unwrap_or(0.0)
}

fn identity_key(finding: &Value) -> (&str, u64, u64, &str) {
    (
        finding["path"].as_str().unwrap_or_default(),
        finding["start_line"].as_u64().unwrap_or(0),
        finding["end_line"].as_u64().unwrap_or(0),
        finding["title"].as_str().unwrap_or_default(),
    )
}

/// Findings of every chunk merged into one ordered list, with the verdict they support.
pub(super) struct Merged {
    pub verdict: Verdict,
    /// Findings per priority, P0 first.
    pub counts: [usize; 4],
    pub findings: Vec<Value>,
}

impl Merged {
    /// Sort by priority (most severe first) then confidence (highest first), then drop findings
    /// that repeat an earlier path, range and title. Sorting first keeps the strongest duplicate.
    pub(super) fn new(chunks: &[ChunkReview], stale: bool) -> Self {
        let mut findings: Vec<Value> = chunks
            .iter()
            .flat_map(|chunk| chunk.findings.iter().cloned())
            .collect();
        findings.sort_by(|a, b| {
            priority(a)
                .cmp(&priority(b))
                .then_with(|| confidence(b).total_cmp(&confidence(a)))
                .then_with(|| identity_key(a).cmp(&identity_key(b)))
        });
        let mut seen = HashSet::new();
        findings.retain(|finding| {
            let (path, start, end, title) = identity_key(finding);
            seen.insert((path.to_owned(), start, end, title.to_owned()))
        });
        let mut counts = [0; 4];
        for finding in &mut findings {
            counts[priority(finding) as usize] += 1;
            finding["stale_snapshot"] = json!(stale);
        }
        let verdict = if counts[0] + counts[1] > 0 {
            Verdict::DoNotShip
        } else if chunks.iter().any(ChunkReview::inconclusive) {
            Verdict::Inconclusive
        } else if counts[2] + counts[3] > 0 {
            Verdict::ShipWithFixes
        } else {
            Verdict::Ship
        };
        Self {
            verdict,
            counts,
            findings,
        }
    }
}

/// The readable report: identity, freshness, verdict, counts and findings grouped P0..P3.
pub(super) fn render(
    identity: &str,
    stale: bool,
    chunks: &[ChunkReview],
    merged: &Merged,
) -> String {
    let [p0, p1, p2, p3] = merged.counts;
    let inconclusive = chunks.iter().filter(|chunk| chunk.inconclusive()).count();
    let mut out = format!(
        "Target {identity}\n{}\nVerdict: {}\nFindings: {} (P0 {p0}, P1 {p1}, P2 {p2}, P3 {p3}) from {} chunk(s), {inconclusive} inconclusive\n",
        if stale { STALE } else { FRESH },
        merged.verdict.label(),
        merged.findings.len(),
        chunks.len(),
    );
    for (level, name) in NAMES.iter().enumerate() {
        let group: Vec<&Value> = merged
            .findings
            .iter()
            .filter(|finding| priority(finding) == level as u64)
            .collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("\nP{level} {name} ({})\n", group.len()));
        for (number, finding) in group.iter().enumerate() {
            let (path, start, end, title) = identity_key(finding);
            out.push_str(&format!(
                "  {}. {title} ({path}:{start}-{end}, confidence {:.2})\n",
                number + 1,
                confidence(finding)
            ));
            for line in finding["body"].as_str().unwrap_or_default().lines() {
                out.push_str(&format!("     {line}\n"));
            }
        }
    }
    let notes: Vec<_> = chunks
        .iter()
        .enumerate()
        .filter(|(_, chunk)| !chunk.summary.trim().is_empty())
        .collect();
    if !notes.is_empty() {
        out.push_str("\nChunk notes\n");
        for (index, chunk) in notes {
            out.push_str(&format!(
                "  {}/{} [{}] {}\n",
                index + 1,
                chunks.len(),
                chunk.verdict,
                chunk.summary.trim()
            ));
        }
    }
    out
}

/// The machine-readable report for programmatic callers.
pub(super) fn to_json(
    identity: &str,
    stale: bool,
    chunks: &[ChunkReview],
    merged: &Merged,
) -> Value {
    let [p0, p1, p2, p3] = merged.counts;
    json!({
        "target_identity": identity,
        "stale_snapshot": stale,
        "verdict": merged.verdict.wire(),
        "counts": {"P0": p0, "P1": p1, "P2": p2, "P3": p3},
        "chunks": chunks.iter().enumerate().map(|(index, chunk)| json!({
            "index": index + 1,
            "bytes": [chunk.range.start, chunk.range.end],
            "verdict": chunk.verdict,
            "summary": chunk.summary,
        })).collect::<Vec<_>>(),
        "findings": merged.findings,
    })
}

/// Turn one entry of the task batch's `jobs` array into a chunk review. A reviewer that did not
/// finish, or whose answer fails validation, leaves the chunk inconclusive rather than aborting
/// the chunks that did complete.
pub(super) fn from_job(
    range: Range<usize>,
    job: &Value,
    captured: &str,
    identity: &str,
) -> ChunkReview {
    let status = job["status"].as_str().unwrap_or("failed");
    if status != "done" {
        let why = job["result"].as_str().unwrap_or_default();
        return ChunkReview::failed(
            range,
            format!(
                "reviewer {status}: {}",
                crate::agent::workers::subagents::clip(why, 500)
            ),
        );
    }
    let value = match &job["result"] {
        Value::String(text) => {
            serde_json::from_str(text).context("Reviewer returned invalid structured output")
        }
        value => Ok(value.clone()),
    };
    match value.and_then(|value| parse_chunk(range.clone(), value, captured, identity)) {
        Ok(chunk) => chunk,
        Err(error) => ChunkReview::failed(range, format!("reviewer output rejected: {error:#}")),
    }
}

#[cfg(test)]
mod tests;
