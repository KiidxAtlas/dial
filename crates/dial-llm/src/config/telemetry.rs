//! `[telemetry]` settings.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `[telemetry]`: OTLP export. Disabled by default; no data leaves the machine unless enabled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Export traces, metrics and logs to an OTLP/HTTP collector.
    pub enabled: bool,
    /// Collector base URL such as `http://localhost:4318`; `/v1/traces`, `/v1/metrics` and
    /// `/v1/logs` are appended. Falls back to `OTEL_EXPORTER_OTLP_ENDPOINT`.
    pub endpoint: Option<String>,
    pub traces: bool,
    pub metrics: bool,
    pub logs: bool,
    /// Extra request headers (for example collector auth). Merged with `OTEL_EXPORTER_OTLP_HEADERS`.
    pub headers: BTreeMap<String, String>,
    /// `service.name` resource attribute; `OTEL_SERVICE_NAME` wins when set.
    pub service_name: String,
    /// Periodic flush for long-lived processes (RPC/ACP servers, the TUI).
    pub flush_interval_secs: u64,
    /// Spans/log records buffered before an early flush.
    pub max_batch: usize,
    /// Per-export HTTP timeout.
    pub timeout_secs: u64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            traces: true,
            metrics: true,
            logs: true,
            headers: BTreeMap::new(),
            service_name: "dial".into(),
            flush_interval_secs: 30,
            max_batch: 512,
            timeout_secs: 10,
        }
    }
}
