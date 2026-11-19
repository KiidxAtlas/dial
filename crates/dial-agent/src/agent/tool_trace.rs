//! Main-agent tool trace: schemas offered on successful replies, calls admitted to dispatch, and a
//! bounded per-call record. Worker, extension and compaction-internal calls are outside this trace.
//! Records hold digests and names only: raw argument values and tool output never enter the trace.
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Most per-call records one main-agent run keeps. Later calls only increment `calls_omitted`.
pub const MAX_CALL_RECORDS: usize = 256;
/// Tool name used when the model named a tool that was not offered on the request.
pub const UNOFFERED_TOOL: &str = "unoffered";

/// One call that reached the dispatcher, in arrival order.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolCallRecord {
    /// 1-based position among main-agent calls that reached dispatch.
    pub ordinal: u64,
    /// Offered tool name, or `unoffered` for any other name.
    pub tool: String,
    /// SHA-256 of the canonical argument JSON. Stable across identical inputs; values are not kept.
    pub input_sha256: String,
    /// Top-level argument names that the offered schema declares, sorted. Names the model invented
    /// are never kept, because a key name can carry a secret as readily as a value.
    pub input_keys: Vec<String>,
    /// Top-level argument names the schema does not declare; only their count is recorded.
    pub input_keys_omitted: u64,
    /// Wall time of the dispatch that produced the result; 0 when nothing ran.
    pub duration_ms: u64,
    /// `ok` or `error`, from the result the model received.
    pub outcome: &'static str,
    /// False for form rejections, pre-dispatch skips and results served from a cache.
    pub executed: bool,
    /// `reused` when a read was answered from the held result; otherwise `none`.
    pub cache: &'static str,
    /// SHA-256 of the error text when `outcome` is `error`. The text itself is never recorded.
    pub error_sha256: Option<String>,
}

/// What the dispatcher observed for one call, passed as one value to keep recording uniform.
pub struct CallObservation<'a> {
    /// Argument names the offered schema declares for this tool; empty when it was not offered.
    pub known_keys: &'a [String],
    pub tool: &'a str,
    pub offered: bool,
    pub arguments: &'a Value,
    pub seconds: f64,
    pub is_error: bool,
    pub error_text: &'a str,
    pub executed: bool,
    pub reused: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ToolTraceCounts {
    /// Main-agent replies received, including text-only and portable-guidance-discarded replies.
    /// Stream failures, pre-reply regenerations and reasoning interceptions are outside this count.
    pub successful_requests: u64,
    /// Requests whose offered schema set named each tool; one count per request per tool.
    pub offered: BTreeMap<String, u64>,
    /// Admitted calls that reached dispatch, per tool. Rejected forms and pre-dispatch skips are excluded.
    pub invoked: BTreeMap<String, u64>,
    /// Every call that reached dispatch, including rejected, skipped and reused ones, up to
    /// `MAX_CALL_RECORDS`. Use `invoked` for execution counts; this list explains each call.
    pub calls: Vec<ToolCallRecord>,
    /// Calls seen after `calls` filled up.
    pub calls_omitted: u64,
}

impl ToolTraceCounts {
    pub fn record_offered(&mut self, schemas: &[Value]) {
        self.successful_requests += 1;
        let names: BTreeSet<&str> = schemas
            .iter()
            .filter_map(|schema| schema["function"]["name"].as_str())
            .collect();
        for name in names {
            *self.offered.entry(name.to_owned()).or_default() += 1;
        }
    }

    pub fn record_invoked(&mut self, name: &str) {
        *self.invoked.entry(name.to_owned()).or_default() += 1;
    }

    pub fn record_call(&mut self, observation: CallObservation<'_>) {
        if self.calls.len() >= MAX_CALL_RECORDS {
            self.calls_omitted += 1;
            return;
        }
        let mut input_keys: Vec<String> = Vec::new();
        let mut input_keys_omitted = 0;
        for key in observation
            .arguments
            .as_object()
            .into_iter()
            .flat_map(|object| object.keys())
        {
            if observation.known_keys.contains(key) {
                input_keys.push(key.clone());
            } else {
                input_keys_omitted += 1;
            }
        }
        input_keys.sort();
        let error_sha256 = observation.is_error.then(|| digest(observation.error_text));
        self.calls.push(ToolCallRecord {
            ordinal: self.calls.len() as u64 + self.calls_omitted + 1,
            tool: if observation.offered {
                observation.tool.to_owned()
            } else {
                UNOFFERED_TOOL.to_owned()
            },
            input_sha256: digest(&observation.arguments.to_string()),
            input_keys,
            input_keys_omitted,
            duration_ms: (observation.seconds.max(0.0) * 1000.0).round() as u64,
            outcome: if observation.is_error { "error" } else { "ok" },
            executed: observation.executed,
            cache: if observation.reused { "reused" } else { "none" },
            error_sha256,
        });
    }
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests;
