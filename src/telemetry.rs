//! Usage events from the agent loop, fanned out off the agent thread to the local stats ledger
//! (`crate::stats`) and, when enabled, to an OpenTelemetry collector over OTLP/HTTP JSON
//! (traces, metrics and logs).
//!
//! Events carry counts, names and durations only — never prompts, replies, tool arguments or
//! tool output — so neither sink can leak conversation content. Recording never blocks or fails a
//! turn: events go over a channel to one background thread, which batches OTLP records (flushed
//! when a batch fills, every `flush_interval_secs`, and on `shutdown`).
use crate::config::types::Config;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

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

// ---------------------------------------------------------------- events

#[derive(Debug, Clone)]
pub struct Request {
    pub session: String,
    pub project: String,
    pub model: String,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
    pub cost_usd: Option<f64>,
    pub duration: Duration,
    pub finish_reason: String,
    pub tool_calls: usize,
    pub error: bool,
    pub end: SystemTime,
    /// Final-wire fingerprint: separates client prompt changes from provider cache misses.
    pub wire: Option<crate::llm::wire::Observation>,
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub status: crate::tools::ToolStatus,
    pub effects: crate::tools::outcome::Effects,
    pub session: String,
    pub project: String,
    pub tool: String,
    pub duration: Duration,
    pub error: bool,
    pub end: SystemTime,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub session: String,
    pub project: String,
    pub outcome: String,
    pub steps: u64,
    pub duration: Duration,
    pub end: SystemTime,
}

#[derive(Debug, Clone)]
pub enum Event {
    Request(Box<Request>),
    Tool(Tool),
    Turn(Turn),
}

impl Event {
    fn end(&self) -> SystemTime {
        match self {
            Event::Request(r) => r.end,
            Event::Tool(t) => t.end,
            Event::Turn(t) => t.end,
        }
    }
    fn duration(&self) -> Duration {
        match self {
            Event::Request(r) => r.duration,
            Event::Tool(t) => t.duration,
            Event::Turn(t) => t.duration,
        }
    }
}

// ---------------------------------------------------------------- agent-facing API

/// One accepted model response.
pub fn request(
    config: &Config,
    hermetic: bool,
    session: &str,
    model: &str,
    reply: &crate::llm::Reply,
    cost_usd: Option<f64>,
    elapsed: Duration,
) {
    if hermetic {
        return;
    }
    emit(
        config,
        Event::Request(Box::new(Request {
            session: session.into(),
            project: project_key(config),
            model: model.into(),
            input: reply.prompt_tokens,
            output: reply.completion_tokens,
            cache_read: reply.cached_prompt_tokens,
            cache_write: reply.cache_creation_tokens,
            reasoning: reply.reasoning_tokens,
            cost_usd: cost_usd.filter(|c| c.is_finite() && *c >= 0.0),
            duration: elapsed,
            finish_reason: reply.finish_reason.clone(),
            tool_calls: reply.tool_calls.len(),
            error: false,
            end: SystemTime::now(),
            wire: reply.wire.clone(),
        })),
    );
}

/// A model request that ended in an error (after transport retries). The row's finish reason is
/// `error:<class>` so stats can separate rate limits and stalls from invalid requests.
pub fn request_failed(
    config: &Config,
    hermetic: bool,
    session: &str,
    model: &str,
    elapsed: Duration,
    error: &anyhow::Error,
) {
    if hermetic {
        return;
    }
    emit(
        config,
        Event::Request(Box::new(Request {
            session: session.into(),
            project: project_key(config),
            model: model.into(),
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
            reasoning: None,
            cost_usd: None,
            duration: elapsed,
            finish_reason: format!("error:{}", crate::llm::errors::classify(error).label()),
            tool_calls: 0,
            error: true,
            end: SystemTime::now(),
            wire: None,
        })),
    );
}

/// One executed tool call.
pub fn tool(
    config: &Config,
    hermetic: bool,
    session: &str,
    name: &str,
    seconds: f64,
    status: crate::tools::ToolStatus,
) {
    tool_with_effects(
        config,
        hermetic,
        session,
        name,
        seconds,
        status,
        crate::tools::outcome::Effects::Known,
    );
}

pub fn tool_with_effects(
    config: &Config,
    hermetic: bool,
    session: &str,
    name: &str,
    seconds: f64,
    status: crate::tools::ToolStatus,
    effects: crate::tools::outcome::Effects,
) {
    if hermetic {
        return;
    }
    emit(
        config,
        Event::Tool(Tool {
            status,
            effects,
            session: session.into(),
            project: project_key(config),
            tool: name.into(),
            duration: Duration::try_from_secs_f64(seconds).unwrap_or_default(),
            error: status.failed() || effects == crate::tools::outcome::Effects::Unknown,
            end: SystemTime::now(),
        }),
    );
}

/// One finished user turn (closes its trace).
pub fn turn(
    config: &Config,
    hermetic: bool,
    session: &str,
    outcome: &str,
    steps: u64,
    elapsed: Duration,
) {
    if hermetic {
        return;
    }
    emit(
        config,
        Event::Turn(Turn {
            session: session.into(),
            project: project_key(config),
            outcome: outcome.into(),
            steps,
            duration: elapsed,
            end: SystemTime::now(),
        }),
    );
}

/// Stats rows are keyed like session directories (`projects/<name>-<hash>`), so runtime usage
/// joins the transcripts of the same project.
fn project_key(config: &Config) -> String {
    config
        .state_dir
        .file_name()
        .or_else(|| config.project_root.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Push buffered records out now (bounded by the export timeout).
pub fn flush() {
    let Some(tx) = hub_sender(false) else { return };
    let (done, wait) = mpsc::channel();
    if tx.send(Msg::Flush(done)).is_ok() {
        let _ = wait.recv_timeout(Duration::from_secs(15));
    }
}

/// Flush and stop the background thread. Called once before the process exits.
pub fn shutdown() {
    let hub = HUB.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(hub) = hub {
        let (done, wait) = mpsc::channel();
        if hub.tx.send(Msg::Stop(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(15));
        }
    }
}

// ---------------------------------------------------------------- routing / hub

#[derive(Debug, Clone, PartialEq)]
struct Route {
    stats: Option<PathBuf>,
    otlp: Option<OtlpSettings>,
}

impl Route {
    fn from_config(config: &Config) -> Self {
        Self {
            stats: (config.stats.enabled && config.user_dir.is_absolute())
                .then(|| crate::stats::db_path(&config.user_dir)),
            otlp: OtlpSettings::resolve(&config.telemetry),
        }
    }
}

enum Msg {
    Event(Box<Route>, Event),
    Flush(mpsc::Sender<()>),
    Stop(mpsc::Sender<()>),
}

struct Hub {
    tx: mpsc::Sender<Msg>,
}

static HUB: Mutex<Option<Hub>> = Mutex::new(None);

fn hub_sender(spawn: bool) -> Option<mpsc::Sender<Msg>> {
    let mut slot = HUB.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() && spawn {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("dial-telemetry".into())
            .spawn(move || run(rx))
            .ok()?;
        *slot = Some(Hub { tx });
    }
    slot.as_ref().map(|hub| hub.tx.clone())
}

fn emit(config: &Config, event: Event) {
    let route = Route::from_config(config);
    if route.stats.is_none() && route.otlp.is_none() {
        return;
    }
    if let Some(tx) = hub_sender(true) {
        let _ = tx.send(Msg::Event(Box::new(route), event));
    }
}

fn run(rx: mpsc::Receiver<Msg>) {
    let mut stats: Option<(PathBuf, crate::stats::Db)> = None;
    let mut failed_open: Option<PathBuf> = None;
    let mut exporter: Option<Exporter> = None;
    loop {
        let wait = exporter
            .as_ref()
            .map(|e| e.next_flush.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(3600));
        match rx.recv_timeout(wait) {
            Ok(Msg::Event(route, event)) => {
                if let Some(path) = &route.stats {
                    if stats.as_ref().is_none_or(|(open, _)| open != path) {
                        match crate::stats::Db::open(path) {
                            Ok(db) => {
                                stats = Some((path.clone(), db));
                                failed_open = None;
                            }
                            Err(error) => {
                                stats = None;
                                if failed_open.as_ref() != Some(path) {
                                    eprintln!(
                                        "dial: stats database unavailable at {}: {error:#}",
                                        path.display()
                                    );
                                    failed_open = Some(path.clone());
                                }
                            }
                        }
                    }
                    if let Some((_, db)) = &mut stats {
                        let _ = record_stats(db, &event);
                    }
                }
                if exporter.as_ref().map(|e| &e.settings) != route.otlp.as_ref() {
                    if let Some(old) = &mut exporter {
                        old.flush();
                    }
                    exporter = route.otlp.clone().map(Exporter::new);
                }
                if let Some(e) = &mut exporter {
                    e.observe(&event);
                    if e.pending() >= e.settings.max_batch {
                        e.flush();
                    }
                }
            }
            Ok(Msg::Flush(done)) => {
                if let Some(e) = &mut exporter {
                    e.flush();
                }
                let _ = done.send(());
            }
            Ok(Msg::Stop(done)) => {
                if let Some(e) = &mut exporter {
                    e.close_open_turns();
                    e.flush();
                }
                let _ = done.send(());
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(e) = &mut exporter {
                    e.flush();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(e) = &mut exporter {
                    e.flush();
                }
                return;
            }
        }
    }
}

fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn unix_ns(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn record_stats(db: &mut crate::stats::Db, event: &Event) -> Result<()> {
    match event {
        Event::Request(r) => db.record_request(&crate::stats::RequestRecord {
            ts_ms: unix_ms(r.end),
            session: r.session.clone(),
            project: r.project.clone(),
            model: r.model.clone(),
            input: r.input.unwrap_or(0),
            output: r.output.unwrap_or(0),
            cache_read: r.cache_read.unwrap_or(0),
            cache_write: r.cache_write.unwrap_or(0),
            reasoning: r.reasoning.unwrap_or(0),
            reported: r.input.is_some() && r.output.is_some(),
            cost_usd: r.cost_usd,
            duration_ms: r.duration.as_millis() as u64,
            finish_reason: r.finish_reason.clone(),
            error: r.error,
            wire: r.wire.as_ref().map(|w| crate::stats::WireRecord {
                transport: if w.chained {
                    format!("{}_chained", w.transport)
                } else {
                    w.transport.into()
                },
                invalidation: w.invalidation.into(),
                common_prompt_bytes: w.common_prompt_bytes as u64,
                prompt_bytes: w.prompt_bytes as u64,
            }),
        }),
        Event::Tool(t) => db.record_tool(&crate::stats::ToolRecord {
            status: Some(t.status),
            ts_ms: unix_ms(t.end),
            session: t.session.clone(),
            project: t.project.clone(),
            tool: t.tool.clone(),
            duration_ms: t.duration.as_millis() as u64,
            error: t.error,
        }),
        Event::Turn(_) => Ok(()),
    }
}

// ---------------------------------------------------------------- OTLP settings

#[derive(Debug, Clone, PartialEq)]
pub struct OtlpSettings {
    traces_url: Option<String>,
    metrics_url: Option<String>,
    logs_url: Option<String>,
    headers: Vec<(String, String)>,
    resource: Vec<(String, String)>,
    flush: Duration,
    max_batch: usize,
    timeout: Duration,
}

impl OtlpSettings {
    /// None when export is disabled or no usable endpoint is configured.
    pub fn resolve(config: &TelemetryConfig) -> Option<Self> {
        Self::resolve_with(config, |name| {
            std::env::var(name).ok().filter(|v| !v.trim().is_empty())
        })
    }

    fn resolve_with(
        config: &TelemetryConfig,
        env: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let base = config
            .endpoint
            .clone()
            .or_else(|| env("OTEL_EXPORTER_OTLP_ENDPOINT"))
            .map(|base| base.trim().trim_end_matches('/').to_owned());
        let signal = |on: bool, var: &str, path: &str| -> Option<String> {
            if !on {
                return None;
            }
            let url = env(var).or_else(|| base.as_ref().map(|base| format!("{base}{path}")))?;
            validate_endpoint(&url).ok().map(|_| url)
        };
        let traces_url = signal(
            config.traces,
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "/v1/traces",
        );
        let metrics_url = signal(
            config.metrics,
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            "/v1/metrics",
        );
        let logs_url = signal(config.logs, "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "/v1/logs");
        if traces_url.is_none() && metrics_url.is_none() && logs_url.is_none() {
            return None;
        }
        let mut headers: Vec<(String, String)> = env("OTEL_EXPORTER_OTLP_HEADERS")
            .map(|raw| parse_pairs(&raw))
            .unwrap_or_default();
        for (name, value) in &config.headers {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            headers.push((name.clone(), value.clone()));
        }
        let mut resource = vec![
            (
                "service.name".to_owned(),
                env("OTEL_SERVICE_NAME").unwrap_or_else(|| config.service_name.clone()),
            ),
            (
                "service.version".to_owned(),
                env!("CARGO_PKG_VERSION").to_owned(),
            ),
            ("process.pid".to_owned(), std::process::id().to_string()),
        ];
        for (key, value) in env("OTEL_RESOURCE_ATTRIBUTES")
            .map(|raw| parse_pairs(&raw))
            .unwrap_or_default()
        {
            if key == "service.name" && env("OTEL_SERVICE_NAME").is_some() {
                continue;
            }
            resource.retain(|(existing, _)| existing != &key);
            resource.push((key, value));
        }
        Some(Self {
            traces_url,
            metrics_url,
            logs_url,
            headers,
            resource,
            flush: Duration::from_secs(config.flush_interval_secs.max(1)),
            max_batch: config.max_batch.max(1),
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
        })
    }
}

/// `OTEL_*` list syntax: `key=value,key2=value2` with percent-encoded values.
fn parse_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_owned(), percent_decode(value.trim())))
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn validate_endpoint(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url).with_context(|| format!("invalid OTLP endpoint {url:?}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("OTLP endpoint must be an http(s) URL with a host");
    }
    Ok(())
}

// ---------------------------------------------------------------- OTLP exporter

const SCOPE: &str = "dial.agent";
const TOKEN_BOUNDS: &[f64] = &[
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];
const SECONDS_BOUNDS: &[f64] = &[
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];
const MS_BOUNDS: &[f64] = &[
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0,
    60000.0, 300000.0,
];

/// (name, unit, description, histogram bounds or None for a monotonic delta sum, double-valued sum)
type MetricSpec = (
    &'static str,
    &'static str,
    &'static str,
    Option<&'static [f64]>,
    bool,
);

/// Every metric the exporter emits.
const METRICS: &[MetricSpec] = &[
    (
        "gen_ai.client.token.usage",
        "{token}",
        "Token usage reported by GenAI chat calls.",
        Some(TOKEN_BOUNDS),
        false,
    ),
    (
        "gen_ai.client.operation.duration",
        "s",
        "GenAI chat call duration.",
        Some(SECONDS_BOUNDS),
        false,
    ),
    (
        "dial.agent.chat.cost.usd",
        "USD",
        "Observed API cost for priced chat calls.",
        None,
        true,
    ),
    (
        "dial.agent.chat.calls",
        "{call}",
        "Chat calls completed.",
        None,
        false,
    ),
    (
        "dial.agent.tool.calls",
        "{call}",
        "Tool calls completed.",
        None,
        false,
    ),
    (
        "dial.agent.tool.duration",
        "ms",
        "Tool call latency.",
        Some(MS_BOUNDS),
        false,
    ),
    (
        "dial.agent.runs",
        "{run}",
        "Completed agent turns.",
        None,
        false,
    ),
    (
        "dial.agent.steps",
        "{step}",
        "Agent loop steps completed inside turns.",
        None,
        false,
    ),
    (
        "dial.agent.errors",
        "{error}",
        "Errors observed in chat and tool execution.",
        None,
        false,
    ),
];

type Attrs = Vec<(&'static str, String)>;

#[derive(Debug, Clone)]
struct Hist {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    buckets: Vec<u64>,
}

impl Hist {
    fn new(bounds: &[f64]) -> Self {
        Self {
            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            buckets: vec![0; bounds.len() + 1],
        }
    }
    fn record(&mut self, bounds: &[f64], value: f64) {
        self.count += 1;
        self.sum += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        // Bucket i covers (bounds[i-1], bounds[i]]; the last bucket is (bounds[last], +inf).
        let index = bounds
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(bounds.len());
        self.buckets[index] += 1;
    }
}

struct TurnSpan {
    trace: u128,
    span: u64,
    start_ns: u64,
}

struct Exporter {
    settings: OtlpSettings,
    agent: ureq::Agent,
    next_flush: Instant,
    spans: Vec<Value>,
    logs: Vec<Value>,
    window_start_ns: u64,
    sums: BTreeMap<(&'static str, Attrs), f64>,
    hists: BTreeMap<(&'static str, Attrs), Hist>,
    turns: HashMap<String, TurnSpan>,
    #[cfg(test)]
    last_error: Option<String>,
}

fn nonzero_u128() -> u128 {
    loop {
        let id = rand::random::<u128>();
        if id != 0 {
            return id;
        }
    }
}

fn nonzero_u64() -> u64 {
    loop {
        let id = rand::random::<u64>();
        if id != 0 {
            return id;
        }
    }
}

fn attr_value(value: &Value) -> Value {
    match value {
        Value::Bool(b) => json!({"boolValue": b}),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({"intValue": n.to_string()}),
        Value::Number(n) => json!({"doubleValue": n.as_f64().unwrap_or(0.0)}),
        Value::String(s) => json!({"stringValue": s}),
        other => json!({"stringValue": other.to_string()}),
    }
}

/// OTLP/JSON `KeyValue` list; nulls are omitted.
fn attributes(pairs: &[(&str, Value)]) -> Value {
    Value::Array(
        pairs
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| json!({"key": key, "value": attr_value(value)}))
            .collect(),
    )
}

impl Exporter {
    fn new(settings: OtlpSettings) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(settings.timeout))
            .build()
            .into();
        Self {
            next_flush: Instant::now() + settings.flush,
            settings,
            agent,
            spans: Vec::new(),
            logs: Vec::new(),
            window_start_ns: unix_ns(SystemTime::now()),
            sums: BTreeMap::new(),
            hists: BTreeMap::new(),
            turns: HashMap::new(),
            #[cfg(test)]
            last_error: None,
        }
    }

    fn pending(&self) -> usize {
        self.spans.len() + self.logs.len()
    }

    fn add(&mut self, name: &'static str, attrs: Attrs, value: f64) {
        *self.sums.entry((name, attrs)).or_insert(0.0) += value;
    }

    fn histogram(&mut self, name: &'static str, attrs: Attrs, value: f64) {
        let bounds = METRICS
            .iter()
            .find(|m| m.0 == name)
            .and_then(|m| m.3)
            .unwrap_or(&[]);
        self.hists
            .entry((name, attrs))
            .or_insert_with(|| Hist::new(bounds))
            .record(bounds, value);
    }

    /// The open turn span for this session, created on its first event.
    fn turn_for(&mut self, session: &str, start_ns: u64) -> (u128, u64) {
        let turn = self
            .turns
            .entry(session.to_owned())
            .or_insert_with(|| TurnSpan {
                trace: nonzero_u128(),
                span: nonzero_u64(),
                start_ns,
            });
        turn.start_ns = turn.start_ns.min(start_ns);
        (turn.trace, turn.span)
    }

    fn observe(&mut self, event: &Event) {
        let end_ns = unix_ns(event.end());
        let start_ns =
            end_ns.saturating_sub(u64::try_from(event.duration().as_nanos()).unwrap_or(u64::MAX));
        match event {
            Event::Request(r) => {
                let (trace, parent) = self.turn_for(&r.session, start_ns);
                let span = nonzero_u64();
                let base: Attrs = vec![
                    ("gen_ai.operation.name", "chat".into()),
                    ("gen_ai.request.model", r.model.clone()),
                ];
                if r.error {
                    let mut attrs = base.clone();
                    attrs.push(("error.type", "chat".into()));
                    self.add("dial.agent.errors", attrs.clone(), 1.0);
                    self.histogram(
                        "gen_ai.client.operation.duration",
                        attrs,
                        r.duration.as_secs_f64(),
                    );
                } else {
                    for (kind, value) in [
                        ("input", r.input),
                        ("output", r.output),
                        ("cache_read_input", r.cache_read),
                        ("cache_write_input", r.cache_write),
                        ("reasoning_output", r.reasoning),
                    ] {
                        if let Some(value) = value.filter(|v| *v > 0) {
                            let mut attrs = base.clone();
                            attrs.push(("gen_ai.token.type", kind.into()));
                            self.histogram("gen_ai.client.token.usage", attrs, value as f64);
                        }
                    }
                    self.histogram(
                        "gen_ai.client.operation.duration",
                        base.clone(),
                        r.duration.as_secs_f64(),
                    );
                    if let Some(cost) = r.cost_usd.filter(|c| *c > 0.0) {
                        self.add("dial.agent.chat.cost.usd", base.clone(), cost);
                    }
                    let mut attrs = base;
                    attrs.push(("gen_ai.response.finish_reason", r.finish_reason.clone()));
                    self.add("dial.agent.chat.calls", attrs, 1.0);
                }
                if self.settings.traces_url.is_some() {
                    let attrs = attributes(&[
                        ("gen_ai.operation.name", json!("chat")),
                        ("gen_ai.request.model", json!(r.model)),
                        ("gen_ai.usage.input_tokens", json!(r.input)),
                        ("gen_ai.usage.output_tokens", json!(r.output)),
                        ("gen_ai.usage.cache_read_input_tokens", json!(r.cache_read)),
                        (
                            "gen_ai.usage.cache_creation_input_tokens",
                            json!(r.cache_write),
                        ),
                        ("gen_ai.usage.reasoning_tokens", json!(r.reasoning)),
                        ("gen_ai.response.finish_reasons", json!(r.finish_reason)),
                        ("dial.chat.tool_calls", json!(r.tool_calls)),
                        ("dial.chat.cost_usd", json!(r.cost_usd)),
                        ("session.id", json!(r.session)),
                        (
                            "error.type",
                            if r.error { json!("chat") } else { Value::Null },
                        ),
                    ]);
                    self.spans.push(span_json(
                        trace,
                        span,
                        Some(parent),
                        &format!("chat {}", r.model),
                        3,
                        start_ns,
                        end_ns,
                        attrs,
                        r.error,
                    ));
                }
                if r.error && self.settings.logs_url.is_some() {
                    self.logs.push(log_json(
                        end_ns,
                        17,
                        "ERROR",
                        "chat request failed",
                        "dial.chat.failed",
                        attributes(&[
                            ("gen_ai.request.model", json!(r.model)),
                            ("session.id", json!(r.session)),
                            ("dial.project", json!(r.project)),
                            (
                                "dial.chat.duration_ms",
                                json!(r.duration.as_millis() as u64),
                            ),
                        ]),
                        Some((trace, span)),
                    ));
                }
            }
            Event::Tool(t) => {
                let (trace, parent) = self.turn_for(&t.session, start_ns);
                let status = t.status.as_str();
                self.add(
                    "dial.agent.tool.calls",
                    vec![
                        ("gen_ai.tool.name", t.tool.clone()),
                        ("dial.tool.status", status.into()),
                    ],
                    1.0,
                );
                self.histogram(
                    "dial.agent.tool.duration",
                    vec![("gen_ai.tool.name", t.tool.clone())],
                    t.duration.as_secs_f64() * 1000.0,
                );
                if t.error {
                    self.add(
                        "dial.agent.errors",
                        vec![("error.type", "tool".into())],
                        1.0,
                    );
                }
                if self.settings.traces_url.is_some() {
                    let attrs = attributes(&[
                        ("gen_ai.operation.name", json!("execute_tool")),
                        ("gen_ai.tool.name", json!(t.tool)),
                        ("dial.tool.status", json!(status)),
                        ("dial.tool.effects", json!(t.effects)),
                        ("session.id", json!(t.session)),
                    ]);
                    self.spans.push(span_json(
                        trace,
                        nonzero_u64(),
                        Some(parent),
                        &format!("execute_tool {}", t.tool),
                        1,
                        start_ns,
                        end_ns,
                        attrs,
                        t.error,
                    ));
                }
            }
            Event::Turn(t) => {
                let (trace, span) = self.turn_for(&t.session, start_ns);
                let opened = self
                    .turns
                    .remove(&t.session)
                    .map_or(start_ns, |turn| turn.start_ns);
                let failed = t.outcome == "failed";
                self.add(
                    "dial.agent.runs",
                    vec![("dial.agent.outcome", t.outcome.clone())],
                    1.0,
                );
                if t.steps > 0 {
                    self.add("dial.agent.steps", Vec::new(), t.steps as f64);
                }
                let pairs = [
                    ("gen_ai.operation.name", json!("invoke_agent")),
                    ("gen_ai.agent.name", json!("dial")),
                    ("dial.agent.outcome", json!(t.outcome)),
                    ("dial.agent.step_count", json!(t.steps)),
                    (
                        "dial.agent.duration_ms",
                        json!(t.duration.as_millis() as u64),
                    ),
                    ("dial.project", json!(t.project)),
                    ("session.id", json!(t.session)),
                ];
                if self.settings.traces_url.is_some() {
                    self.spans.push(span_json(
                        trace,
                        span,
                        None,
                        "invoke_agent dial",
                        1,
                        opened.min(start_ns),
                        end_ns,
                        attributes(&pairs),
                        failed,
                    ));
                }
                if self.settings.logs_url.is_some() {
                    let (severity, text) = if failed { (17, "ERROR") } else { (9, "INFO") };
                    self.logs.push(log_json(
                        end_ns,
                        severity,
                        text,
                        "agent run completed",
                        "dial.agent.run.completed",
                        attributes(&pairs),
                        Some((trace, span)),
                    ));
                }
            }
        }
    }

    /// Turns still open at shutdown are closed as interrupted so their child spans keep a parent.
    fn close_open_turns(&mut self) {
        let now = SystemTime::now();
        let open: Vec<(String, u64)> = self
            .turns
            .iter()
            .map(|(session, turn)| (session.clone(), turn.start_ns))
            .collect();
        for (session, start_ns) in open {
            let duration = Duration::from_nanos(unix_ns(now).saturating_sub(start_ns));
            self.observe(&Event::Turn(Turn {
                session,
                project: String::new(),
                outcome: "interrupted".into(),
                steps: 0,
                duration,
                end: now,
            }));
        }
    }

    fn resource(&self) -> Value {
        json!({"attributes": Value::Array(self.settings.resource.iter()
            .map(|(key, value)| json!({"key": key, "value": {"stringValue": value}}))
            .collect())})
    }

    fn scope() -> Value {
        json!({"name": SCOPE, "version": env!("CARGO_PKG_VERSION")})
    }

    /// Drain buffered records into OTLP/JSON request bodies, one per enabled signal.
    fn take_payloads(&mut self, now_ns: u64) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        let spans = std::mem::take(&mut self.spans);
        if let Some(url) = &self.settings.traces_url
            && !spans.is_empty()
        {
            out.push((
                url.clone(),
                json!({"resourceSpans": [{
                    "resource": self.resource(),
                    "scopeSpans": [{"scope": Self::scope(), "spans": spans}],
                }]}),
            ));
        }
        let logs = std::mem::take(&mut self.logs);
        if let Some(url) = &self.settings.logs_url
            && !logs.is_empty()
        {
            out.push((
                url.clone(),
                json!({"resourceLogs": [{
                    "resource": self.resource(),
                    "scopeLogs": [{"scope": Self::scope(), "logRecords": logs}],
                }]}),
            ));
        }
        let sums = std::mem::take(&mut self.sums);
        let hists = std::mem::take(&mut self.hists);
        let start = std::mem::replace(&mut self.window_start_ns, now_ns);
        if let Some(url) = &self.settings.metrics_url
            && (!sums.is_empty() || !hists.is_empty())
        {
            let mut metrics = Vec::new();
            for (name, unit, description, bounds, double) in METRICS {
                let point_attrs = |attrs: &Attrs| {
                    attributes(
                        &attrs
                            .iter()
                            .map(|(k, v)| (*k, json!(v)))
                            .collect::<Vec<_>>(),
                    )
                };
                if let Some(bounds) = bounds {
                    let points: Vec<Value> = hists
                        .iter()
                        .filter(|((metric, _), _)| metric == name)
                        .map(|((_, attrs), hist)| json!({
                            "attributes": point_attrs(attrs),
                            "startTimeUnixNano": start.to_string(),
                            "timeUnixNano": now_ns.to_string(),
                            "count": hist.count.to_string(),
                            "sum": hist.sum,
                            "min": hist.min,
                            "max": hist.max,
                            "bucketCounts": hist.buckets.iter().map(u64::to_string).collect::<Vec<_>>(),
                            "explicitBounds": bounds,
                        }))
                        .collect();
                    if !points.is_empty() {
                        metrics.push(
                            json!({"name": name, "unit": unit, "description": description,
                            "histogram": {"dataPoints": points, "aggregationTemporality": 1}}),
                        );
                    }
                } else {
                    let points: Vec<Value> = sums
                        .iter()
                        .filter(|((metric, _), _)| metric == name)
                        .map(|((_, attrs), value)| {
                            let mut point = json!({
                                "attributes": point_attrs(attrs),
                                "startTimeUnixNano": start.to_string(),
                                "timeUnixNano": now_ns.to_string(),
                            });
                            if *double {
                                point["asDouble"] = json!(value);
                            } else {
                                point["asInt"] = json!((value.round() as i64).to_string());
                            }
                            point
                        })
                        .collect();
                    if !points.is_empty() {
                        metrics.push(json!({"name": name, "unit": unit, "description": description,
                            "sum": {"dataPoints": points, "aggregationTemporality": 1, "isMonotonic": true}}));
                    }
                }
            }
            out.push((
                url.clone(),
                json!({"resourceMetrics": [{
                    "resource": self.resource(),
                    "scopeMetrics": [{"scope": Self::scope(), "metrics": metrics}],
                }]}),
            ));
        }
        out
    }

    fn flush(&mut self) {
        self.next_flush = Instant::now() + self.settings.flush;
        for (url, body) in self.take_payloads(unix_ns(SystemTime::now())) {
            // Exports are best effort; a failed batch is dropped rather than growing without bound.
            let _result = self.post(&url, &body);
            #[cfg(test)]
            if let Err(error) = _result {
                self.last_error = Some(format!("{error:#}"));
            }
        }
    }

    fn post(&self, url: &str, body: &Value) -> Result<()> {
        let mut request = self
            .agent
            .post(url)
            .header("Content-Type", "application/json");
        for (name, value) in &self.settings.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .send(body.to_string())
            .map_err(|e| anyhow::anyhow!("OTLP export to {url} failed: {e}"))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            bail!("OTLP export to {url} failed: HTTP {status}");
        }
        Ok(())
    }
}

fn hex(value: u128, bytes: usize) -> String {
    format!("{value:0width$x}", width = bytes * 2)
}

#[allow(clippy::too_many_arguments)]
fn span_json(
    trace: u128,
    span: u64,
    parent: Option<u64>,
    name: &str,
    kind: u8,
    start_ns: u64,
    end_ns: u64,
    attributes: Value,
    error: bool,
) -> Value {
    json!({
        "traceId": hex(trace, 16),
        "spanId": hex(span as u128, 8),
        "parentSpanId": parent.map(|p| hex(p as u128, 8)).unwrap_or_default(),
        "name": name,
        "kind": kind,
        "startTimeUnixNano": start_ns.to_string(),
        "endTimeUnixNano": end_ns.max(start_ns).to_string(),
        "attributes": attributes,
        "status": if error { json!({"code": 2}) } else { json!({}) },
    })
}

fn log_json(
    time_ns: u64,
    severity: u8,
    severity_text: &str,
    body: &str,
    event_name: &str,
    attributes: Value,
    context: Option<(u128, u64)>,
) -> Value {
    let mut record = json!({
        "timeUnixNano": time_ns.to_string(),
        "observedTimeUnixNano": unix_ns(SystemTime::now()).to_string(),
        "severityNumber": severity,
        "severityText": severity_text,
        "body": {"stringValue": body},
        "eventName": event_name,
        "attributes": attributes,
    });
    if let Some((trace, span)) = context {
        record["traceId"] = json!(hex(trace, 16));
        record["spanId"] = json!(hex(span as u128, 8));
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    fn settings(base: &str) -> OtlpSettings {
        let config = TelemetryConfig {
            enabled: true,
            endpoint: Some(base.into()),
            ..Default::default()
        };
        OtlpSettings::resolve_with(&config, |_| None).unwrap()
    }

    fn request_event(session: &str, error: bool) -> Event {
        Event::Request(Box::new(Request {
            session: session.into(),
            project: "proj-1234abcd".into(),
            model: "test-model".into(),
            input: Some(1200),
            output: Some(300),
            cache_read: Some(1000),
            cache_write: None,
            reasoning: Some(0),
            cost_usd: Some(0.25),
            duration: Duration::from_millis(1500),
            finish_reason: "stop".into(),
            tool_calls: 1,
            error,
            end: SystemTime::now(),
            wire: None,
        }))
    }

    #[test]
    fn disabled_or_endpointless_config_exports_nothing() {
        assert!(
            OtlpSettings::resolve_with(&TelemetryConfig::default(), |_| Some(
                "http://x:4318".into()
            ))
            .is_none()
        );
        let enabled = TelemetryConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(OtlpSettings::resolve_with(&enabled, |_| None).is_none());
        let from_env = OtlpSettings::resolve_with(&enabled, |name| {
            (name == "OTEL_EXPORTER_OTLP_ENDPOINT").then(|| "http://collector:4318/".to_owned())
        })
        .unwrap();
        assert_eq!(
            from_env.traces_url.as_deref(),
            Some("http://collector:4318/v1/traces")
        );
        let bad = TelemetryConfig {
            enabled: true,
            endpoint: Some("ftp://x".into()),
            ..Default::default()
        };
        assert!(OtlpSettings::resolve_with(&bad, |_| None).is_none());
    }

    #[test]
    fn env_headers_and_resource_attributes_are_parsed() {
        let config = TelemetryConfig {
            enabled: true,
            endpoint: Some("https://otel.example".into()),
            headers: BTreeMap::from([("x-team".to_owned(), "core".to_owned())]),
            ..Default::default()
        };
        let resolved = OtlpSettings::resolve_with(&config, |name| match name {
            "OTEL_EXPORTER_OTLP_HEADERS" => Some("authorization=Bearer%20abc, x-team=old".into()),
            "OTEL_RESOURCE_ATTRIBUTES" => Some("deployment.environment=dev".into()),
            "OTEL_SERVICE_NAME" => Some("dial-ci".into()),
            _ => None,
        })
        .unwrap();
        assert!(
            resolved
                .headers
                .contains(&("authorization".into(), "Bearer abc".into()))
        );
        assert!(resolved.headers.contains(&("x-team".into(), "core".into())));
        assert!(!resolved.headers.contains(&("x-team".into(), "old".into())));
        assert!(
            resolved
                .resource
                .contains(&("service.name".into(), "dial-ci".into()))
        );
        assert!(
            resolved
                .resource
                .contains(&("deployment.environment".into(), "dev".into()))
        );
    }

    #[test]
    fn otlp_json_payloads_have_the_protocol_shape() {
        let mut exporter = Exporter::new(settings("http://127.0.0.1:4318"));
        exporter.observe(&request_event("s1", false));
        exporter.observe(&Event::Tool(Tool {
            status: crate::tools::ToolStatus::Error,
            effects: crate::tools::outcome::Effects::Known,
            session: "s1".into(),
            project: "proj-1234abcd".into(),
            tool: "read".into(),
            duration: Duration::from_millis(12),
            error: true,
            end: SystemTime::now(),
        }));
        exporter.observe(&Event::Turn(Turn {
            session: "s1".into(),
            project: "proj-1234abcd".into(),
            outcome: "completed".into(),
            steps: 2,
            duration: Duration::from_secs(3),
            end: SystemTime::now(),
        }));
        let payloads: BTreeMap<String, Value> = exporter
            .take_payloads(unix_ns(SystemTime::now()))
            .into_iter()
            .collect();
        assert_eq!(payloads.len(), 3);

        let traces = &payloads["http://127.0.0.1:4318/v1/traces"];
        let resource = &traces["resourceSpans"][0]["resource"]["attributes"];
        assert!(
            resource
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["key"] == "service.name" && a["value"]["stringValue"] == "dial")
        );
        let spans = traces["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 3);
        let root = spans
            .iter()
            .find(|s| s["name"] == "invoke_agent dial")
            .unwrap();
        assert_eq!(root["parentSpanId"], "");
        for span in spans {
            assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
            assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
            assert_eq!(span["traceId"], root["traceId"]);
            assert!(
                span["startTimeUnixNano"]
                    .as_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    <= span["endTimeUnixNano"]
                        .as_str()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
            );
            if span != root {
                assert_eq!(span["parentSpanId"], root["spanId"]);
            }
        }
        let chat = spans
            .iter()
            .find(|s| s["name"] == "chat test-model")
            .unwrap();
        assert_eq!(chat["kind"], 3);
        let input = chat["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "gen_ai.usage.input_tokens")
            .unwrap();
        assert_eq!(input["value"]["intValue"], "1200");
        let tool = spans
            .iter()
            .find(|s| s["name"] == "execute_tool read")
            .unwrap();
        assert_eq!(tool["status"]["code"], 2);

        let metrics = &payloads["http://127.0.0.1:4318/v1/metrics"]["resourceMetrics"][0]["scopeMetrics"]
            [0]["metrics"];
        let find = |name: &str| {
            metrics
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["name"] == name)
                .cloned()
                .unwrap()
        };
        let tokens = find("gen_ai.client.token.usage");
        assert_eq!(tokens["histogram"]["aggregationTemporality"], 1);
        let points = tokens["histogram"]["dataPoints"].as_array().unwrap();
        assert_eq!(
            points.len(),
            3,
            "zero-valued reasoning and absent cache writes are not recorded"
        );
        for point in points {
            assert_eq!(
                point["bucketCounts"].as_array().unwrap().len(),
                TOKEN_BOUNDS.len() + 1
            );
            assert_eq!(point["count"], "1");
        }
        let cost = find("dial.agent.chat.cost.usd");
        assert_eq!(cost["sum"]["dataPoints"][0]["asDouble"], 0.25);
        assert_eq!(cost["sum"]["isMonotonic"], true);
        let tool_calls = find("dial.agent.tool.calls");
        assert_eq!(tool_calls["sum"]["dataPoints"][0]["asInt"], "1");
        assert_eq!(
            find("dial.agent.steps")["sum"]["dataPoints"][0]["asInt"],
            "2"
        );

        let logs = &payloads["http://127.0.0.1:4318/v1/logs"]["resourceLogs"][0]["scopeLogs"][0]["logRecords"];
        assert_eq!(logs[0]["body"]["stringValue"], "agent run completed");
        assert_eq!(logs[0]["severityNumber"], 9);
        assert_eq!(logs[0]["traceId"], root["traceId"]);

        assert!(
            exporter
                .take_payloads(unix_ns(SystemTime::now()))
                .is_empty(),
            "buffers drain"
        );
    }

    #[test]
    fn histogram_buckets_follow_explicit_bounds() {
        let mut hist = Hist::new(&[1.0, 10.0]);
        for value in [0.5, 1.0, 5.0, 10.0, 11.0] {
            hist.record(&[1.0, 10.0], value);
        }
        assert_eq!(hist.buckets, vec![2, 2, 1]);
        assert_eq!(hist.count, 5);
        assert_eq!(hist.min, 0.5);
        assert_eq!(hist.max, 11.0);
    }

    #[test]
    fn flush_posts_json_to_the_signal_path() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            (&stream)
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                .unwrap();
            (line, body)
        });
        let config = TelemetryConfig {
            enabled: true,
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            metrics: false,
            logs: false,
            ..Default::default()
        };
        let mut exporter = Exporter::new(OtlpSettings::resolve_with(&config, |_| None).unwrap());
        exporter.observe(&request_event("s2", true));
        exporter.flush();
        let (line, body) = server.join().unwrap();
        assert!(line.starts_with("POST /v1/traces "), "{line}");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"]["code"],
            2
        );
        assert!(exporter.last_error.is_none());
    }
}
