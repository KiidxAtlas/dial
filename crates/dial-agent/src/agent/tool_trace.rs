//! Main-agent tool trace: schemas offered on successful replies and calls admitted to dispatch.
//! Worker, extension and compaction-internal calls are outside this trace by design.
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default, Clone, Serialize)]
pub struct ToolTraceCounts {
    /// Main-agent replies received, including text-only and portable-guidance-discarded replies.
    /// Stream failures, pre-reply regenerations and reasoning interceptions are outside this count.
    pub successful_requests: u64,
    /// Requests whose offered schema set named each tool; one count per request per tool.
    pub offered: BTreeMap<String, u64>,
    /// Admitted calls that reached dispatch, per tool. Rejected forms and pre-dispatch skips are excluded.
    pub invoked: BTreeMap<String, u64>,
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
}

#[cfg(test)]
mod tests;
