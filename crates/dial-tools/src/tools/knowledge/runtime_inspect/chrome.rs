//! Chrome synchronous duration events; pairing is structural, never a causal claim.
use super::{MAX_RECORDS, text};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn micros(v: &Value, key: &str) -> Result<f64> {
    let n = v
        .get(key)
        .and_then(Value::as_f64)
        .with_context(|| format!("missing numeric {key}"))?;
    ensure!(
        n.is_finite() && (0.0..=9_007_199_254_740_991.0).contains(&n),
        "{key} outside safe nonnegative microsecond range"
    );
    Ok(n)
}
fn id(v: &Value, key: &str) -> Result<Value> {
    let id = v.get(key).context("Chrome events require pid and tid")?;
    ensure!(
        id.as_i64().is_some()
            || id.as_u64().is_some()
            || id
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 4096),
        "Chrome pid/tid must be integer or bounded string"
    );
    Ok(id.clone())
}
struct Begin {
    name: String,
    ts: f64,
    index: usize,
}

pub(super) fn inspect(root: &Value, limit: usize) -> Result<Value> {
    let events = root
        .as_array()
        .or_else(|| root.get("traceEvents").and_then(Value::as_array))
        .context("Chrome trace requires event array or traceEvents array")?;
    ensure!(
        events.len() <= MAX_RECORDS,
        "Chrome event count exceeds 20000"
    );
    let mut stacks: BTreeMap<(String, String), Vec<Begin>> = BTreeMap::new();
    let mut spans = Vec::new();
    let mut unsupported = BTreeMap::<String, usize>::new();
    let mut unmatched_ends = 0;
    for (index, event) in events.iter().enumerate() {
        let phase = text(event, "ph")?;
        ensure!(
            phase.len() == 1 && phase.is_ascii(),
            "Chrome phase must be one ASCII character"
        );
        if !matches!(phase, "X" | "B" | "E") {
            *unsupported.entry(phase.into()).or_default() += 1;
            continue;
        }
        let pid = id(event, "pid")?;
        let tid = id(event, "tid")?;
        let key = (pid.to_string(), tid.to_string());
        let ts = micros(event, "ts")?;
        match phase {
            "X" => {
                let dur = micros(event, "dur")?;
                ensure!(
                    ts + dur <= 9_007_199_254_740_991.0,
                    "Chrome end timestamp exceeds safe range"
                );
                spans.push(json!({"name":text(event,"name")?,"pid":pid,"tid":tid,"start_us":ts,"duration_us":dur,"source_event":index,"pairing":"complete_X"}));
            }
            "B" => stacks.entry(key).or_default().push(Begin {
                name: text(event, "name")?.into(),
                ts,
                index,
            }),
            "E" => {
                if let Some(begin) = stacks.get_mut(&key).and_then(Vec::pop) {
                    ensure!(ts >= begin.ts, "Chrome end precedes paired begin");
                    spans.push(json!({"name":begin.name,"pid":pid,"tid":tid,"start_us":begin.ts,"duration_us":ts-begin.ts,"source_event":begin.index,"end_event":index,"pairing":"per_pid_tid_LIFO_source_order"}));
                } else {
                    unmatched_ends += 1;
                }
            }
            _ => unreachable!(),
        }
    }
    let unmatched_begins: usize = stacks.values().map(Vec::len).sum();
    spans.sort_by(|a, b| {
        b["duration_us"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&a["duration_us"].as_f64().unwrap_or(0.0))
    });
    let total = spans.len();
    spans.truncate(limit);
    Ok(
        json!({"format":"chrome_trace","time_unit":"microseconds","slowest_spans":spans,
        "coverage":{"events":events.len(),"interpreted_spans":total,"rows_omitted":total.saturating_sub(limit),"unsupported_events_by_phase":unsupported,"unmatched_begins":unmatched_begins,"unmatched_ends":unmatched_ends},
        "limitations":["Only synchronous X/B/E duration events are interpreted; args, metadata, async and flow events are omitted. displayTimeUnit does not change Chrome trace timestamp units.","B/E pairing uses source-order per-process/thread LIFO, without inferring parenthood or causality. Fractional microseconds use f64, not exact decimal arithmetic; values beyond safe integer range are rejected.","Input timestamps do not establish wall-clock synchronization or total CPU time."]}),
    )
}
