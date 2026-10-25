//! Journal schema versions.
//!
//! The session header carries an integer `schema_version` (distinct from the free-form `version`
//! field, which records the Dial release that wrote the file). A header without the field is a
//! legacy journal, version 0. Older journals are upgraded in memory as they load and the file is
//! never rewritten; a journal newer than this build understands is refused with a clear error
//! instead of being half-read.
//!
//! Version history:
//! - 0: journals written before the marker existed.
//! - 1: the marker itself. No record changed shape, so upgrading from 0 rewrites nothing; `label`
//!   records are new in the format but optional, and older builds skip unknown record types.
//!
//! A future version that changes a record's shape adds a [`RecordStep`] to [`RECORD_STEPS`] and
//! bumps [`CURRENT_SCHEMA_VERSION`].

use anyhow::{Result, bail};
use serde_json::Value;

/// The schema version this build writes and the newest one it reads.
pub(super) const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Rewrites one record from schema `from` to `from + 1`.
pub(super) struct RecordStep {
    pub from: u32,
    pub run: fn(&mut Value),
}

/// Record upgrades in ascending `from` order. Empty while version 1 only added the marker.
const RECORD_STEPS: &[RecordStep] = &[];

/// The schema version a session header declares: absent means legacy (0).
pub(super) fn header_version(header: &Value) -> Result<u32> {
    let Some(declared) = header.get("schema_version") else {
        return Ok(0);
    };
    let Some(version) = declared.as_u64().and_then(|v| u32::try_from(v).ok()) else {
        bail!("session header has an invalid schema_version ({declared})");
    };
    if version > CURRENT_SCHEMA_VERSION {
        bail!(
            "session journal uses schema version {version}, newer than the version {CURRENT_SCHEMA_VERSION} this Dial understands; update Dial to open it"
        );
    }
    Ok(version)
}

/// Upgrades a record read from a `version` journal to the current shape, in memory.
pub(super) fn upgrade_record(version: u32, record: &mut Value) {
    apply(RECORD_STEPS, version, CURRENT_SCHEMA_VERSION, record);
}

fn apply(steps: &[RecordStep], from: u32, to: u32, record: &mut Value) {
    for step in steps {
        if step.from >= from && step.from < to {
            (step.run)(record);
        }
    }
}

#[cfg(test)]
mod tests;
