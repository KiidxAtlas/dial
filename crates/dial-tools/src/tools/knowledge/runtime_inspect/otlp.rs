//! OTLP JSON export spans with exact integer nanoseconds and explicit links only.
use super::{MAX_RECORDS, array, text};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn nanos(v: &Value, field: &str) -> Result<u64> {
    let s = text(v, field)?;
    ensure!(
        s.bytes().all(|b| b.is_ascii_digit()),
        "{field} must be a decimal nanosecond string"
    );
    s.parse()
        .with_context(|| format!("{field} exceeds u64 nanoseconds"))
}
fn id(v: &Value, field: &str, length: usize) -> Result<String> {
    let s = text(v, field)?;
    ensure!(
        s.len() == length
            && s.bytes().all(|b| b.is_ascii_hexdigit())
            && s.bytes().any(|b| b != b'0'),
        "{field} must be nonzero {length}-digit hex"
    );
    Ok(s.to_ascii_lowercase())
}
struct Span {
    row: Value,
    trace: String,
    id: String,
    parent: Option<String>,
    duration: u64,
}
pub(super) fn inspect(root: &Value, limit: usize) -> Result<Value> {
    let mut spans = Vec::new();
    let resources = array(root, "resourceSpans")?;
    let mut containers = 0usize;
    for resource in resources {
        containers += 1;
        ensure!(
            containers <= MAX_RECORDS,
            "OTLP container count exceeds 20000"
        );
        let mut service = None;
        if let Some(attrs) = resource.pointer("/resource/attributes") {
            for attr in attrs
                .as_array()
                .context("resource attributes must be an array")?
            {
                if attr["key"] == "service.name" {
                    ensure!(service.is_none(), "duplicate service.name attribute");
                    service = Some(text(
                        attr.get("value").context("missing service value")?,
                        "stringValue",
                    )?);
                }
            }
        }
        for scope in array(resource, "scopeSpans")? {
            containers += 1;
            ensure!(
                containers <= MAX_RECORDS,
                "OTLP container count exceeds 20000"
            );
            for span in array(scope, "spans")? {
                ensure!(spans.len() < MAX_RECORDS, "OTLP span count exceeds 20000");
                let trace = id(span, "traceId", 32)?;
                let span_id = id(span, "spanId", 16)?;
                let parent = match span.get("parentSpanId") {
                    None => None,
                    Some(Value::String(s)) if s.is_empty() || s == "0000000000000000" => None,
                    Some(_) => Some(id(span, "parentSpanId", 16)?),
                };
                let start = nanos(span, "startTimeUnixNano")?;
                let end = nanos(span, "endTimeUnixNano")?;
                let duration = end.checked_sub(start).context("OTLP end precedes start")?;
                let status = match span.pointer("/status/code") {
                    None => "unset",
                    Some(v) if v == 0 || v == "STATUS_CODE_UNSET" => "unset",
                    Some(v) if v == 1 || v == "STATUS_CODE_OK" => "ok",
                    Some(v) if v == 2 || v == "STATUS_CODE_ERROR" => "error",
                    Some(_) => anyhow::bail!("unknown OTLP status code"),
                };
                spans.push(Span {
                    row:json!({"name":text(span,"name")?,"trace_id":trace,"span_id":span_id,"parent_span_id":parent,"service_name":service,"status":status,"start_unix_ns":start.to_string(),"end_unix_ns":end.to_string(),"duration_ns":duration.to_string()}),
                    trace, id:span_id, parent, duration,
                });
            }
        }
    }
    let mut indices = BTreeMap::new();
    for (i, span) in spans.iter().enumerate() {
        ensure!(
            indices
                .insert((span.trace.as_str(), span.id.as_str()), i)
                .is_none(),
            "duplicate OTLP trace/span ID"
        );
    }
    let parents: Vec<Option<usize>> = spans
        .iter()
        .map(|s| {
            s.parent
                .as_deref()
                .and_then(|p| indices.get(&(s.trace.as_str(), p)).copied())
        })
        .collect();
    let orphans = spans
        .iter()
        .zip(&parents)
        .filter(|(s, p)| s.parent.is_some() && p.is_none())
        .count();
    // Each node has at most one explicit parent; global colors keep cycle scanning linear.
    let mut colors = vec![0u8; spans.len()];
    let mut cyclic = vec![false; spans.len()];
    for start in 0..spans.len() {
        if colors[start] != 0 {
            continue;
        }
        let mut path = Vec::new();
        let mut current = Some(start);
        while let Some(i) = current {
            if colors[i] != 0 {
                if colors[i] == 1
                    && let Some(position) = path.iter().position(|&n| n == i)
                {
                    for &n in &path[position..] {
                        cyclic[n] = true;
                    }
                }
                break;
            }
            colors[i] = 1;
            path.push(i);
            current = parents[i];
        }
        for i in path {
            colors[i] = 2;
        }
    }
    let cycle_members = cyclic.iter().filter(|&&v| v).count();
    let roots = spans.iter().filter(|s| s.parent.is_none()).count();
    let errors = spans.iter().filter(|s| s.row["status"] == "error").count();
    let total = spans.len();
    let mut rows: Vec<_> = spans
        .into_iter()
        .enumerate()
        .map(|(i, mut s)| {
            s.row["parent_resolution"] = json!(if s.parent.is_none() {
                "root"
            } else if parents[i].is_none() {
                "orphan"
            } else {
                "present"
            });
            s.row["cycle_member"] = json!(cyclic[i]);
            (s.duration, s.row)
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    let rows: Vec<_> = rows.into_iter().take(limit).map(|(_, v)| v).collect();
    Ok(
        json!({"format":"otlp_json","time_unit":"nanoseconds_decimal_strings","slowest_spans":rows,
        "coverage":{"spans":total,"rows_omitted":total.saturating_sub(limit),"roots":roots,"orphan_parent_links":orphans,"cycle_members":cycle_members,"error_status_spans":errors},
        "limitations":["Only resourceSpans/scopeSpans/spans are supported. Events, links, attributes other than service.name, instrumentation metadata and dropped counts are not interpreted.","Parents are matched only by explicit parent ID within the same trace. Missing parents may reflect partial exports; cyclic links are reported, not repaired. IDs and service names are source claims, not causal proof.","No exclusive time or cross-service clock alignment is inferred; source-reported error status is not an independently diagnosed failure."]}),
    )
}
