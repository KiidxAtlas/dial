//! Versioned source references for interpreted data; evidence never grants execution authority.
//!
//! A representation digest identifies the bytes inspected or returned, not necessarily the original
//! source. A rendered page, sampled frame or captured log window is not proof of the whole source.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Wire version of the evidence envelope stored under `ToolOutput.data.evidence`.
pub const SCHEMA_VERSION: u32 = 1;
/// Maximum evidence records in one result; producers must bound their own extraction work too.
pub const MAX_ITEMS: usize = 64;

/// A bounded envelope, independent of tool execution status and verification facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceBundle {
    pub schema_version: u32,
    pub items: Vec<Evidence>,
}

impl EvidenceBundle {
    /// Construct and validate a version-one envelope.
    pub fn new(items: Vec<Evidence>) -> Result<Self> {
        let bundle = Self {
            schema_version: SCHEMA_VERSION,
            items,
        };
        bundle.validate()?;
        Ok(bundle)
    }

    /// Validate both version and bounds before consuming deserialized evidence.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported evidence schema version"
        );
        ensure!(
            !self.items.is_empty() && self.items.len() <= MAX_ITEMS,
            "evidence must contain 1..={MAX_ITEMS} items"
        );
        for item in &self.items {
            item.validate()?;
        }
        Ok(())
    }
}

/// One producer's observation or inference, with a retraceable source and representation identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    /// Authorized source locator, scoped to the originating workspace/session. Never fetched here.
    pub source: String,
    pub location: Location,
    pub method: Method,
    /// Parser, converter or model identifier; this label is not an attestation.
    pub producer: String,
    pub kind: EvidenceKind,
    /// What was obtained, not an instruction to execute.
    pub summary: String,
    pub representation: Representation,
    /// Explicit gaps such as sampling, approximate seeking or lack of audio interpretation.
    pub limitations: Vec<String>,
    #[serde(default, skip_serializing_if = "EvidenceContext::is_empty")]
    pub context: EvidenceContext,
}

impl Evidence {
    /// Check structural bounds, not truth, permission, source freshness or model confidence.
    pub fn validate(&self) -> Result<()> {
        bounded(&self.source, 4096, "source")?;
        bounded(&self.producer, 256, "producer")?;
        bounded(&self.summary, 8192, "summary")?;
        bounded(&self.representation.media_type, 256, "media type")?;
        validate_digest(&self.representation.sha256)?;
        ensure!(
            self.limitations.len() <= 16,
            "too many evidence limitations"
        );
        for limitation in &self.limitations {
            bounded(limitation, 2048, "limitation")?;
        }
        self.location.validate()?;
        self.context.validate()
    }
}

fn bounded(value: &str, max: usize, name: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= max,
        "{name} must contain 1..={max} bytes"
    );
    ensure!(
        !value
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "{name} contains unsupported control characters"
    );
    Ok(())
}

/// Optional provenance claims. These do not establish graph existence, authority or synchrony.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EvidenceContext {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SourceSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<Clock>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub alignments: Vec<ClockAlignment>,
}

impl EvidenceContext {
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
            && self.snapshot.is_none()
            && self.coverage.is_none()
            && self.clock.is_none()
            && self.alignments.is_empty()
    }

    pub fn validate(&self) -> Result<()> {
        validate_refs(&self.parents)?;
        ensure!(
            self.alignments.len() <= MAX_ITEMS,
            "too many clock alignments"
        );
        if let Some(snapshot) = &self.snapshot {
            snapshot.validate()?;
        }
        if let Some(coverage) = &self.coverage {
            coverage.validate()?;
        }
        if let Some(clock) = &self.clock {
            clock.validate()?;
        }
        for alignment in &self.alignments {
            alignment.validate()?;
        }
        Ok(())
    }
}

/// Identity of ORIGINAL source bytes, distinct from the inspected representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSnapshot {
    pub sha256: String,
    pub bytes: u64,
    pub captured_at_unix_ms: u64,
    /// Optional retained original bytes, not a manifest or representation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

impl SourceSnapshot {
    pub fn validate(&self) -> Result<()> {
        validate_digest(&self.sha256)?;
        if let Some(artifact) = &self.artifact {
            validate_artifact_ref(artifact)?;
            ensure!(
                artifact.strip_prefix("artifact://") == Some(self.sha256.as_str()),
                "snapshot artifact must identify original source bytes"
            );
        }
        Ok(())
    }
}

/// Producer-reported extraction coverage, not proof of completeness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub extent: CoverageExtent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inspected: Vec<Location>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted: Vec<Location>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
}

impl Coverage {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.inspected.len() <= MAX_ITEMS && self.omitted.len() <= MAX_ITEMS,
            "too many coverage regions"
        );
        ensure!(
            self.failures.len() <= MAX_ITEMS,
            "too many coverage failures"
        );
        for region in self.inspected.iter().chain(&self.omitted) {
            region.validate()?;
        }
        for failure in &self.failures {
            bounded(failure, 2048, "coverage failure")?;
        }
        if self.extent == CoverageExtent::Complete {
            ensure!(
                !self.inspected.is_empty()
                    && self.omitted.is_empty()
                    && !self.truncated
                    && self.failures.is_empty(),
                "complete coverage requires inspected regions and no gaps or failures"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageExtent {
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clock {
    pub id: String,
    pub origin: ClockOrigin,
    /// Producer-supplied timezone label, retained verbatim; no timezone database is consulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl Clock {
    pub fn validate(&self) -> Result<()> {
        bounded(&self.id, 256, "clock id")?;
        if let Some(timezone) = &self.timezone {
            bounded(timezone, 256, "clock timezone")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockOrigin {
    Relative,
    Unix,
}

/// A producer assertion of target = source + offset, not verified clock synchrony.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockAlignment {
    pub from: String,
    pub to: String,
    pub offset_ms: i64,
    pub uncertainty_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub basis: Vec<String>,
}

impl ClockAlignment {
    pub fn validate(&self) -> Result<()> {
        bounded(&self.from, 256, "source clock")?;
        bounded(&self.to, 256, "target clock")?;
        ensure!(self.from != self.to, "self clock alignment is invalid");
        validate_refs(&self.basis)
    }

    /// Apply the asserted offset with checked signed arithmetic, retaining uncertainty.
    pub fn aligned_time(&self, source_ms: i64) -> Result<AlignedTime> {
        self.validate()?;
        let time_ms = source_ms
            .checked_add(self.offset_ms)
            .ok_or_else(|| anyhow::anyhow!("aligned time overflows signed milliseconds"))?;
        Ok(AlignedTime {
            time_ms,
            uncertainty_ms: self.uncertainty_ms,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlignedTime {
    pub time_ms: i64,
    pub uncertainty_ms: u64,
}

fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "sha256 must be 64 lowercase hexadecimal characters"
    );
    Ok(())
}

fn validate_artifact_ref(reference: &str) -> Result<()> {
    let digest = reference
        .strip_prefix("artifact://")
        .ok_or_else(|| anyhow::anyhow!("evidence reference must be artifact://<sha256>"))?;
    validate_digest(digest)
}

fn validate_refs(references: &[String]) -> Result<()> {
    ensure!(
        references.len() <= MAX_ITEMS,
        "too many evidence references"
    );
    for reference in references {
        validate_artifact_ref(reference)?;
    }
    Ok(())
}
/// Hash of the exact returned/inspected representation; never an implicit original-file digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Representation {
    pub media_type: String,
    pub sha256: String,
}

impl Representation {
    /// Identify bytes without retaining, fetching or interpreting them.
    pub fn of_bytes(media_type: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            media_type: media_type.into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }
    }
}

/// Evidence classification is a producer assertion, never a successful-check signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EvidenceKind {
    Observation,
    Inference,
}

/// How the representation or interpretation was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Method {
    Read,
    Render,
    Sample,
    Transcription,
    Ocr,
    Calculation,
    ModelInterpretation,
}

/// Source coordinates. Lines/pages are one-based; times are milliseconds from source start.
/// Regions use normalized millionths of the image, independent of later resizing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Location {
    Whole,
    Lines {
        start: u64,
        end: u64,
    },
    Page {
        number: u32,
    },
    /// Equal endpoints identify a point; a sampled preview must not claim a continuous interval.
    Time {
        start_ms: u64,
        end_ms: u64,
    },
    Region {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
}

impl Location {
    pub fn validate(&self) -> Result<()> {
        match *self {
            Self::Whole => {}
            Self::Lines { start, end } => {
                ensure!(start > 0 && end >= start, "invalid evidence line range")
            }
            Self::Page { number } => ensure!(number > 0, "evidence pages are one-based"),
            Self::Time { start_ms, end_ms } => {
                ensure!(end_ms >= start_ms, "invalid evidence time range")
            }
            Self::Region {
                x,
                y,
                width,
                height,
            } => {
                const SCALE: u32 = 1_000_000;
                ensure!(
                    width > 0
                        && height > 0
                        && x <= SCALE
                        && y <= SCALE
                        && width <= SCALE - x
                        && height <= SCALE - y,
                    "evidence region must fit normalized image bounds"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
